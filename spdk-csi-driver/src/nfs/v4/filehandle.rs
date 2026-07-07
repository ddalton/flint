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
// Handle Format v1 (variable length, up to 128 bytes):
// - Version (1 byte): 1
// - Instance ID (8 bytes): Server instance identifier
// - Path Hash (32 bytes): SHA-256 hash of path
// - Path (variable): Full path string (for verification)
//
// Handle Format v2 (fixed 17 bytes) — used when the path does not fit
// v1's 85-byte budget (RFC 8881 NFS4_FHSIZE is 128; long Spark part
// names + a volume-dir prefix blow past it and used to fail the OPEN
// with "Path too long for file handle" → client-visible EIO and
// un-deletable stripe debris):
// - Version (1 byte): 2
// - Instance ID (8 bytes)
// - File ID (8 bytes): random non-zero id; resolved through the
//   id↔path table (persisted via the state backend when attached, so
//   v2 handles survive restart like v1's embedded path does). RENAME
//   re-keys the table — a v2 handle stays valid across renames.
// v1 stays the format for paths that fit: it is stateless, and legacy
// striped pins rely on the DS extracting the path from the MDS handle
// (parse_path_lenient) — those paths are short by construction.

use super::protocol::Nfs4FileHandle;
use super::pseudo::{PseudoFilesystem, Export};
use crate::state_backend::{spawn_persist, FhMappingRecord, StateBackend};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn, info};

/// Fixed length of a v2 (id-based) filehandle.
const FH_V2_LEN: usize = 1 + 8 + 8;

/// Random non-zero file id for the v2 table. Same construction as the
/// placement layer's `allocate_file_id`.
fn allocate_fh_id() -> u64 {
    let (hi, lo) = uuid::Uuid::new_v4().as_u64_pair();
    match hi ^ lo {
        0 => 1,
        id => id,
    }
}

/// File handle manager - maps between paths and file handles
/// Why a file handle failed validation. The distinction is wire-visible
/// and load-bearing: a `Stale` handle is structurally valid but minted by
/// another server incarnation — answered with NFS4ERR_STALE, which kernel
/// clients recover from by re-walking the path and minting fresh handles.
/// `Malformed` handles get NFS4ERR_BADHANDLE, which clients treat as fatal
/// (observed as a permanent errno-521/ENOENT loop on live mounts when a
/// restarted server answered BadHandle for old-incarnation handles — RWX
/// cutover round, 2026-06-12).
#[derive(Debug, PartialEq)]
pub enum HandleError {
    Stale,
    Malformed(String),
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandleError::Stale => write!(f, "Stale file handle"),
            HandleError::Malformed(m) => write!(f, "{}", m),
        }
    }
}

pub struct FileHandleManager {
    /// Server instance ID (changes on restart to invalidate old handles)
    instance_id: u64,

    /// Cache of path -> handle mappings (for fast lookup)
    path_to_handle: Arc<RwLock<HashMap<PathBuf, Nfs4FileHandle>>>,

    /// Cache of handle -> path mappings (for reverse lookup)
    handle_to_path: Arc<RwLock<HashMap<Vec<u8>, PathBuf>>>,

    /// Root export path
    export_path: PathBuf,

    /// Pseudo-filesystem (RFC 7530 Section 7)
    pseudo_fs: Arc<PseudoFilesystem>,

    /// Export name in pseudo-filesystem
    export_name: String,

    /// id↔path table behind v2 (id-based) handles — the paths too long
    /// to embed. Mirrored to `backend` when one is attached.
    id_to_path: Arc<RwLock<HashMap<u64, PathBuf>>>,
    path_to_id: Arc<RwLock<HashMap<PathBuf, u64>>>,

    /// Persistence for the v2 table. Attached after construction
    /// (`attach_backend`) because the two servers build their pieces
    /// in different orders. Absent (tests, dev) = v2 handles don't
    /// survive restart — clients see NFS4ERR_STALE and re-walk.
    backend: RwLock<Option<Arc<dyn StateBackend>>>,
}

impl FileHandleManager {
    /// Generate a unique instance ID for this server deployment
    /// Uses timestamp with nanosecond precision
    fn generate_instance_id() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }
    
    /// Create a new file handle manager
    pub fn new(export_path: PathBuf) -> Self {
        Self::new_with_export_name(export_path, "volume".to_string())
    }
    
    /// Create a new file handle manager with custom export name
    pub fn new_with_export_name(export_path: PathBuf, export_name: String) -> Self {
        // Try to get shared instance_id from environment (for pNFS cluster)
        let instance_id = match std::env::var("PNFS_INSTANCE_ID") {
            Ok(id_str) => {
                id_str.parse::<u64>().unwrap_or_else(|_| {
                    warn!("Invalid PNFS_INSTANCE_ID, generating new one");
                    Self::generate_instance_id()
                })
            }
            Err(_) => Self::generate_instance_id(),
        };
        
        Self::new_with_instance_id(export_path, export_name, instance_id)
    }
    
    /// Create with explicit instance_id (for pNFS clusters)
    pub fn new_with_instance_id(export_path: PathBuf, export_name: String, instance_id: u64) -> Self {
        // Canonicalize the export path so relative inputs work with PUTROOTFH
        // and normalization checks. If canonicalize fails, keep the original
        // to avoid crashing; later operations will surface a clear error.
        let export_path = export_path
            .canonicalize()
            .unwrap_or(export_path);

        info!("🔧 FileHandleManager created:");
        info!("   Instance ID: {} (shared across pNFS cluster)", instance_id);
        info!("   Export path: {:?}", export_path);
        info!("   Export name: {}", export_name);
        
        // Create pseudo-filesystem (RFC 7530 Section 7)
        let pseudo_fs = Arc::new(PseudoFilesystem::new());
        
        // Register the export in pseudo-filesystem
        let export = Export::new(1, export_name.clone(), export_path.clone());
        if let Err(e) = pseudo_fs.add_export(export) {
            warn!("Failed to add export to pseudo-filesystem: {}", e);
        }

        Self {
            instance_id,
            path_to_handle: Arc::new(RwLock::new(HashMap::new())),
            handle_to_path: Arc::new(RwLock::new(HashMap::new())),
            export_path,
            pseudo_fs,
            export_name,
            id_to_path: Arc::new(RwLock::new(HashMap::new())),
            path_to_id: Arc::new(RwLock::new(HashMap::new())),
            backend: RwLock::new(None),
        }
    }

    /// Attach the persistence backend for the v2 id↔path table and
    /// load its persisted mappings. Call once at server construction,
    /// before the listener accepts — a client re-presenting a
    /// pre-restart v2 handle must find its mapping, not STALE.
    pub async fn attach_backend(&self, backend: Arc<dyn StateBackend>) {
        match backend.list_fh_mappings().await {
            Ok(records) => {
                let n = records.len();
                let mut ids = self.path_to_id.write().unwrap();
                let mut rev = self.id_to_path.write().unwrap();
                for r in records {
                    let path = PathBuf::from(&r.path);
                    ids.insert(path.clone(), r.file_id);
                    rev.insert(r.file_id, path);
                }
                if n > 0 {
                    info!("FileHandleManager loaded {} v2 fh mapping(s) from backend", n);
                }
            }
            Err(e) => warn!("FileHandleManager: loading v2 fh mappings failed: {}", e),
        }
        *self.backend.write().unwrap() = Some(backend);
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

    /// Get the root file handle (PUTROOTFH)
    ///
    /// Per RFC 7530 Section 7, this MUST return the pseudo-filesystem root,
    /// NOT the actual export directory.
    pub fn root_filehandle(&self) -> Result<Nfs4FileHandle, String> {
        // Return pseudo-root handle (RFC 7530 Section 7)
        Ok(self.pseudo_fs.get_pseudo_root_handle())
    }

    /// Alias for root_filehandle (NFSv4 terminology)
    pub fn get_root_fh(&self) -> Result<Nfs4FileHandle, String> {
        self.root_filehandle()
    }

    /// Get the export root path
    pub fn get_export_path(&self) -> &Path {
        &self.export_path
    }
    
    /// Check if a filehandle represents the pseudo-root
    pub fn is_pseudo_root(&self, handle: &Nfs4FileHandle) -> bool {
        self.pseudo_fs.is_pseudo_root(handle)
    }
    
    /// Get pseudo-filesystem reference
    pub fn get_pseudo_fs(&self) -> &Arc<PseudoFilesystem> {
        &self.pseudo_fs
    }
    
    /// Lookup an export by name (for LOOKUP from pseudo-root)
    pub fn lookup_export(&self, name: &str) -> Option<Export> {
        self.pseudo_fs.lookup_export(name)
    }
    
    /// Get export name
    pub fn get_export_name(&self) -> &str {
        &self.export_name
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
    pub fn validate_handle(&self, handle: &Nfs4FileHandle) -> Result<(), HandleError> {
        if handle.data.is_empty() {
            return Err(HandleError::Malformed("File handle is empty".to_string()));
        }

        // Check if this is a pseudo-root handle (special case)
        if self.is_pseudo_root(handle) {
            debug!("Validating pseudo-root handle: {} bytes", handle.data.len());
            return Ok(()); // Pseudo-root handles are always valid
        }

        // Regular filehandle validation
        match handle.data[0] {
            1 => {
                if handle.data.len() < 41 {
                    return Err(HandleError::Malformed("File handle too short".to_string()));
                }
            }
            2 => {
                if handle.data.len() != FH_V2_LEN {
                    return Err(HandleError::Malformed(format!(
                        "v2 file handle must be {} bytes, got {}",
                        FH_V2_LEN,
                        handle.data.len()
                    )));
                }
            }
            v => {
                return Err(HandleError::Malformed(format!(
                    "Unsupported file handle version: {}",
                    v
                )));
            }
        }

        // Extract instance ID
        let mut instance_bytes = [0u8; 8];
        instance_bytes.copy_from_slice(&handle.data[1..9]);
        let handle_instance = u64::from_be_bytes(instance_bytes);

        // Check if instance matches
        if handle_instance != self.instance_id {
            warn!("Stale file handle detected: instance {} != {}",
                  handle_instance, self.instance_id);
            return Err(HandleError::Stale);
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
            // Too long to embed — mint an id-based v2 handle instead.
            return Ok(self.v2_handle_for(path));
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

    /// Mint (or reuse) a v2 id-based handle for a path too long to
    /// embed. Allocates a random non-zero file id on first mint and
    /// mirrors the mapping to the state backend when attached.
    fn v2_handle_for(&self, path: &Path) -> Nfs4FileHandle {
        let existing = self.path_to_id.read().unwrap().get(path).copied();
        let id = match existing {
            Some(id) => id,
            None => {
                let mut ids = self.path_to_id.write().unwrap();
                let mut rev = self.id_to_path.write().unwrap();
                // Re-check under the write locks (mint races are real:
                // parallel LOOKUPs of the same long name).
                if let Some(&id) = ids.get(path) {
                    id
                } else {
                    let mut id = allocate_fh_id();
                    while rev.contains_key(&id) {
                        id = allocate_fh_id();
                    }
                    ids.insert(path.to_path_buf(), id);
                    rev.insert(id, path.to_path_buf());
                    if let Some(backend) = self.backend.read().unwrap().clone() {
                        let record = FhMappingRecord {
                            file_id: id,
                            path: path.to_string_lossy().into_owned(),
                        };
                        spawn_persist("fh_mapping", move || async move {
                            backend.put_fh_mapping(&record).await
                        });
                    }
                    debug!("Minted v2 filehandle id {:016x} for long path {:?}", id, path);
                    id
                }
            }
        };

        let mut data = Vec::with_capacity(FH_V2_LEN);
        data.push(2);
        data.extend_from_slice(&self.instance_id.to_be_bytes());
        data.extend_from_slice(&id.to_be_bytes());
        Nfs4FileHandle { data }
    }

    /// Extract the path bytes from a version-1 filehandle **without**
    /// checking the instance ID or recomputing the hash.
    ///
    /// This is the cross-instance path: a pNFS Data Server uses it to
    /// honor filehandles minted by the *Metadata Server*, whose
    /// `instance_id` and hash are by definition different from the DS's
    /// own. The DS trusts the MDS as the layout authority — the I/O
    /// caller is responsible for rebasing the returned path into the
    /// DS's own export tree (typically by basename).
    ///
    /// Do not use this for normal in-process FH resolution; use
    /// [`Self::filehandle_to_path`] which validates instance + hash.
    pub fn parse_path_lenient(handle: &Nfs4FileHandle) -> Result<PathBuf, String> {
        // Layout: version(1) | instance_id(8) | hash(32) | path_len(2) | path(N)
        if handle.data.is_empty() {
            return Err("File handle is empty".to_string());
        }
        if handle.data[0] != 1 {
            return Err(format!("Unsupported file handle version: {}", handle.data[0]));
        }
        if handle.data.len() < 43 {
            return Err("File handle too short".to_string());
        }
        let mut len_bytes = [0u8; 2];
        len_bytes.copy_from_slice(&handle.data[41..43]);
        let path_len = u16::from_be_bytes(len_bytes) as usize;
        if handle.data.len() < 43 + path_len {
            return Err("File handle truncated".to_string());
        }
        let path_str = std::str::from_utf8(&handle.data[43..43 + path_len])
            .map_err(|_| "Invalid path encoding".to_string())?;
        Ok(PathBuf::from(path_str))
    }

    /// Parse a file handle to extract the path
    fn parse_handle(&self, handle: &Nfs4FileHandle) -> Result<PathBuf, String> {
        // Validate first
        self.validate_handle(handle).map_err(|e| e.to_string())?;

        // v2 (id-based): resolve through the id↔path table. A missing
        // entry means the mapping didn't survive (no backend, or the
        // record was lost) — answered as stale so the client re-walks.
        if handle.data.first() == Some(&2) {
            let mut id_bytes = [0u8; 8];
            id_bytes.copy_from_slice(&handle.data[9..17]);
            let id = u64::from_be_bytes(id_bytes);
            return self
                .id_to_path
                .read()
                .unwrap()
                .get(&id)
                .cloned()
                .ok_or_else(|| "Stale file handle: unknown v2 file id".to_string());
        }

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

    /// A successful filesystem RENAME old→new: re-key the v2 id↔path
    /// table for the renamed node AND everything under it (directory
    /// renames move every descendant's path), so v2 handles stay valid
    /// across renames. Also drops the v1 path↔handle cache entries for
    /// the old subtree — v1 handles embed the path and legitimately go
    /// dead; serving them from cache would resolve to the dead path.
    pub fn note_fs_rename(&self, old_path: &Path, new_path: &Path) {
        // v1 caches: drop old-subtree entries (both directions).
        {
            let mut p2h = self.path_to_handle.write().unwrap();
            let mut h2p = self.handle_to_path.write().unwrap();
            let dead: Vec<PathBuf> = p2h
                .keys()
                .filter(|p| p.starts_with(old_path))
                .cloned()
                .collect();
            for p in dead {
                if let Some(h) = p2h.remove(&p) {
                    h2p.remove(&h.data);
                }
            }
        }
        // v2 table: re-key old subtree → new prefix, persist each.
        let mut ids = self.path_to_id.write().unwrap();
        let mut rev = self.id_to_path.write().unwrap();
        let moved: Vec<(PathBuf, u64)> = ids
            .iter()
            .filter(|(p, _)| p.starts_with(old_path))
            .map(|(p, &id)| (p.clone(), id))
            .collect();
        let backend = self.backend.read().unwrap().clone();
        for (old, id) in moved {
            let suffix = old.strip_prefix(old_path).expect("filtered by starts_with");
            let new = new_path.join(suffix);
            ids.remove(&old);
            ids.insert(new.clone(), id);
            rev.insert(id, new.clone());
            if let Some(backend) = backend.clone() {
                let record = FhMappingRecord {
                    file_id: id,
                    path: new.to_string_lossy().into_owned(),
                };
                spawn_persist("fh_mapping_rename", move || async move {
                    backend.put_fh_mapping(&record).await
                });
            }
        }
    }

    /// A successful filesystem REMOVE: drop v1 cache entries and v2
    /// mappings for the removed node and (for directories) everything
    /// under it. A recreated same-name file mints a fresh id — new
    /// file, new handle, per NFS semantics.
    pub fn note_fs_remove(&self, path: &Path) {
        {
            let mut p2h = self.path_to_handle.write().unwrap();
            let mut h2p = self.handle_to_path.write().unwrap();
            let dead: Vec<PathBuf> = p2h
                .keys()
                .filter(|p| p.starts_with(path))
                .cloned()
                .collect();
            for p in dead {
                if let Some(h) = p2h.remove(&p) {
                    h2p.remove(&h.data);
                }
            }
        }
        let mut ids = self.path_to_id.write().unwrap();
        let mut rev = self.id_to_path.write().unwrap();
        let dead: Vec<(PathBuf, u64)> = ids
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, &id)| (p.clone(), id))
            .collect();
        let backend = self.backend.read().unwrap().clone();
        for (p, id) in dead {
            ids.remove(&p);
            rev.remove(&id);
            if let Some(backend) = backend.clone() {
                spawn_persist("fh_mapping_delete", move || async move {
                    backend.delete_fh_mapping(id).await
                });
            }
        }
    }

    /// Normalize a path (resolve . and .., ensure within export)
    fn normalize_path(&self, path: &Path) -> Result<PathBuf, String> {
        // Convert to absolute path
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.export_path.join(path)
        };

        // IMPORTANT: Do NOT use canonicalize() because it follows symlinks!
        // We need to normalize . and .. without following symlinks
        let normalized = self.normalize_without_following_symlinks(&abs_path)?;

        // Ensure path is within export
        // Both paths should be canonicalized for proper comparison
        // (to handle cases where /tmp -> /private/tmp on macOS)
        let normalized_canon = normalized.canonicalize()
            .unwrap_or_else(|_| normalized.clone());
        let export_canon = self.export_path.canonicalize()
            .unwrap_or_else(|_| self.export_path.clone());
            
        if !normalized_canon.starts_with(&export_canon) {
            return Err("Path outside export".to_string());
        }

        Ok(normalized)
    }

    /// Normalize a path without following symlinks
    /// Resolves . and .. components but preserves symlinks
    fn normalize_without_following_symlinks(&self, path: &Path) -> Result<PathBuf, String> {
        use std::path::Component;
        
        let mut normalized = PathBuf::new();
        
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    normalized.push(component);
                }
                Component::CurDir => {
                    // Skip . (current directory)
                }
                Component::ParentDir => {
                    // Go up one level (..)
                    if !normalized.pop() {
                        return Err("Path goes above root".to_string());
                    }
                }
                Component::Normal(name) => {
                    normalized.push(name);
                }
            }
        }
        
        // Verify the path exists (but don't follow symlinks)
        if !normalized.symlink_metadata().is_ok() {
            return Err(format!("Path does not exist: {:?}", normalized));
        }
        
        Ok(normalized)
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
        let temp_dir = tempfile::TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_path_buf();

        let manager = FileHandleManager::new(temp_path.clone());
        let test_path = temp_path.join("test.txt");
        fs::write(&test_path, b"test").unwrap();

        // Generate handle
        let handle = manager.path_to_filehandle(&test_path).unwrap();

        // Resolve back
        let resolved_path = manager.filehandle_to_path(&handle).unwrap();

        // Compare canonicalized paths (handles symlinks like /var -> /private/var on macOS)
        assert_eq!(test_path.canonicalize().unwrap(), resolved_path.canonicalize().unwrap());

        // TempDir cleanup happens automatically
    }

    #[test]
    fn test_root_filehandle() {
        let temp_dir = std::env::temp_dir().join("nfsv4_root_test");
        fs::create_dir_all(&temp_dir).unwrap();
        let temp_dir = temp_dir.canonicalize().unwrap();

        let manager = FileHandleManager::new(temp_dir.clone());

        // Get root handle (this is the pseudo-root, not the export root)
        let root_handle = manager.root_filehandle().unwrap();

        // Pseudo-root should be recognized as such
        assert!(manager.is_pseudo_root(&root_handle));
        
        // Pseudo-root handles are special and don't resolve to a regular path
        // They represent the NFSv4 pseudo-filesystem root (RFC 7530 Section 7)

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_handle_validation() {
        // Use TempDir for automatic cleanup and shorter paths
        let temp_dir = tempfile::TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_path_buf();

        let manager1 = FileHandleManager::new(temp_path.clone());
        let manager2 = FileHandleManager::new(temp_path.clone());

        let test_path = temp_path.join("test.txt");
        fs::write(&test_path, b"test").unwrap();

        // Generate handle with manager1
        let handle = manager1.path_to_filehandle(&test_path).unwrap();

        // Should be valid for manager1
        assert!(manager1.validate_handle(&handle).is_ok());

        // Should be invalid for manager2 (different instance)
        assert!(manager2.validate_handle(&handle).is_err());

        // TempDir cleanup happens automatically
    }

    #[test]
    fn test_handle_deterministic() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_path_buf();

        let manager = FileHandleManager::new(temp_path.clone());
        let test_path = temp_path.join("test.txt");
        fs::write(&test_path, b"test").unwrap();

        // Generate handle twice
        let handle1 = manager.path_to_filehandle(&test_path).unwrap();
        let handle2 = manager.path_to_filehandle(&test_path).unwrap();

        // Should be identical
        assert_eq!(handle1.data, handle2.data);

        // TempDir cleanup happens automatically
    }

    #[test]
    fn test_cache() {
        let temp_dir = std::env::temp_dir().join("nfsv4_cache_test");
        fs::create_dir_all(&temp_dir).unwrap();
        let temp_dir = temp_dir.canonicalize().unwrap();

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

    fn long_name(prefix: &str) -> String {
        // Spark-shaped: well past v1's ~85-byte path budget on its own.
        format!(
            "{}-00000-a1b2c3d4-e5f6-7890-abcd-ef0123456789-c000.snappy.parquet.{}",
            prefix,
            "x".repeat(80)
        )
    }

    /// Long paths used to fail the mint outright ("Path too long for
    /// file handle" → client EIO). They now get a fixed-size v2 handle
    /// that round-trips, and short paths still mint v1 (stateless,
    /// legacy-pin compatible).
    #[test]
    fn long_path_mints_v2_and_round_trips() {
        let temp_dir = std::env::temp_dir().join("fh_v2_test");
        fs::create_dir_all(&temp_dir).unwrap();
        let manager = FileHandleManager::new(temp_dir.clone());

        let long_path = temp_dir.join(long_name("part"));
        fs::write(&long_path, b"parquet bytes").unwrap();
        let fh = manager.path_to_filehandle(&long_path).unwrap();
        assert_eq!(fh.data[0], 2, "long path must mint a v2 handle");
        assert_eq!(fh.data.len(), FH_V2_LEN);
        assert!(manager.validate_handle(&fh).is_ok());
        let resolved = manager.filehandle_to_path(&fh).unwrap();
        assert!(resolved.ends_with(long_path.file_name().unwrap()));

        // Deterministic: same path, same handle.
        assert_eq!(manager.path_to_filehandle(&long_path).unwrap().data, fh.data);

        let short_path = temp_dir.join("short.txt");
        fs::write(&short_path, b"x").unwrap();
        let fh1 = manager.path_to_filehandle(&short_path).unwrap();
        assert_eq!(fh1.data[0], 1, "short path keeps the v1 format");

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    /// v2 handles survive RENAME — including a parent-directory rename
    /// — and die with REMOVE.
    #[test]
    fn v2_handle_follows_rename_and_dies_with_remove() {
        let temp_dir = std::env::temp_dir().join("fh_v2_rename_test");
        fs::create_dir_all(&temp_dir).unwrap();
        let manager = FileHandleManager::new(temp_dir.clone());

        let stage = temp_dir.join("stage");
        fs::create_dir_all(&stage).unwrap();
        let file = stage.join(long_name("part"));
        fs::write(&file, b"parquet bytes").unwrap();
        let fh = manager.path_to_filehandle(&file).unwrap();
        assert_eq!(fh.data[0], 2);

        // Directory rename: the handle must resolve to the new home.
        let done = temp_dir.join("done");
        fs::rename(&stage, &done).unwrap();
        manager.note_fs_rename(&stage, &done);
        let resolved = manager.filehandle_to_path(&fh).unwrap();
        assert!(resolved.starts_with(&done), "v2 handle follows the dir rename: {:?}", resolved);

        // REMOVE forgets the mapping → stale, and a re-created file
        // gets a DIFFERENT handle (new file, new id).
        let new_home = done.join(file.file_name().unwrap());
        manager.note_fs_remove(&new_home);
        assert!(manager.filehandle_to_path(&fh).is_err());
        let fh2 = manager.path_to_filehandle(&new_home).unwrap();  // file still on disk
        assert_ne!(fh2.data, fh.data);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    /// With a backend attached, v2 mappings persist and a "restarted"
    /// manager (same backend, same instance id) resolves the old
    /// handle. Without persistence the restart answers stale.
    #[tokio::test]
    async fn v2_handles_survive_restart_via_backend() {
        let temp_dir = std::env::temp_dir().join("fh_v2_persist_test");
        fs::create_dir_all(&temp_dir).unwrap();
        let backend: std::sync::Arc<dyn StateBackend> =
            std::sync::Arc::new(crate::state_backend::MemoryBackend::new());

        let m1 = FileHandleManager::new_with_instance_id(temp_dir.clone(), "volume".into(), 42);
        m1.attach_backend(std::sync::Arc::clone(&backend)).await;
        let long_path = temp_dir.join(long_name("part"));
        fs::write(&long_path, b"parquet bytes").unwrap();
        let fh = m1.path_to_filehandle(&long_path).unwrap();
        assert_eq!(fh.data[0], 2);

        // spawn_persist is fire-and-forget; wait (bounded) for the record.
        let mut persisted = Vec::new();
        for _ in 0..200 {
            persisted = backend.list_fh_mappings().await.unwrap();
            if !persisted.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(persisted.len(), 1, "v2 mapping was never persisted");

        // "Restart": fresh manager, same instance id + backend.
        let m2 = FileHandleManager::new_with_instance_id(temp_dir.clone(), "volume".into(), 42);
        assert!(
            m2.filehandle_to_path(&fh).is_err(),
            "before load the mapping is unknown"
        );
        m2.attach_backend(std::sync::Arc::clone(&backend)).await;
        let resolved = m2.filehandle_to_path(&fh).unwrap();
        assert!(resolved.ends_with(long_path.file_name().unwrap()));

        fs::remove_dir_all(&temp_dir).unwrap();
    }
}
