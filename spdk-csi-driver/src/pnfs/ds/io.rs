//! Data Server I/O Operations
//!
//! Handles filesystem-based I/O for pNFS FILE layout (RFC 8881 Chapter 13).
//! Each DS mounts SPDK volumes via ublk and serves NFS file operations.
//!
//! # Architecture
//! 
//! ```text
//! DS NFS Server (this module)
//!   ↓ Standard file I/O (open, read, write, fsync)
//! Mounted Filesystem (ext4/xfs)
//!   ↓ Block I/O
//! ublk device (/dev/ublkb0)
//!   ↓ Local access
//! SPDK Logical Volume (with RAID-5/6)
//!   ↓ Direct access
//! Physical NVMe drives
//! ```
//!
//! # Protocol References
//! - RFC 8881 Section 13 - NFSv4.1 File Layout Type
//! - RFC 8881 Section 13.3 - "ranges of bytes from the file"
//! - RFC 8881 Section 18.22 - READ operation (file-level)
//! - RFC 8881 Section 18.32 - WRITE operation (file-level)
//! - RFC 8881 Section 18.3 - COMMIT operation (file-level)

use crate::pnfs::Result;
use crate::nfs::v4::filehandle::FileHandleManager;
use crate::nfs::v4::protocol::Nfs4FileHandle;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// I/O operation handler for data server
/// 
/// Performs filesystem-based I/O on locally mounted SPDK volumes.
/// This is the correct approach for pNFS FILE layout per RFC 8881.
pub struct IoOperationHandler {
    /// Base path where SPDK volume is mounted
    /// Example: /mnt/pnfs-data
    base_path: PathBuf,
    
    /// File handle manager (reused from standalone NFS server)
    fh_manager: Arc<FileHandleManager>,
}

impl IoOperationHandler {
    /// Create a new I/O operation handler
    /// 
    /// # Arguments
    /// * `base_path` - Mount point of the SPDK volume (e.g., /mnt/pnfs-data)
    pub fn new<P: AsRef<Path>>(base_path: P) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        
        // Verify mount point exists and is accessible
        if !base_path.exists() {
            return Err(crate::pnfs::Error::Config(
                format!("Data path does not exist: {:?}", base_path)
            ));
        }
        
        if !base_path.is_dir() {
            return Err(crate::pnfs::Error::Config(
                format!("Data path is not a directory: {:?}", base_path)
            ));
        }
        
        // Create file handle manager - reuse from existing NFS server
        let fh_manager = Arc::new(FileHandleManager::new(base_path.clone()));
        
        info!("I/O handler initialized with data path: {:?}", base_path);
        
        Ok(Self { base_path, fh_manager })
    }

    /// Handle READ operation (RFC 8881 Section 18.22)
    /// 
    /// Reads bytes from a file at the specified offset.
    /// This is standard filesystem I/O - exactly like standalone NFS.
    pub async fn read(
        &self,
        filehandle: &[u8],
        offset: u64,
        count: u32,
    ) -> Result<ReadResult> {
        debug!(
            "DS READ: fh={:?}, offset={}, count={}",
            &filehandle[0..4.min(filehandle.len())],
            offset,
            count
        );

        // Map filehandle to file path
        let file_path = self.filehandle_to_path(filehandle)?;
        
        // Standard file I/O - just like standalone NFS server!
        let mut file = File::open(&file_path)
            .map_err(|e| {
                warn!("Failed to open file {:?}: {}", file_path, e);
                crate::pnfs::Error::Io(e)
            })?;
        
        // Seek to requested offset
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| crate::pnfs::Error::Io(e))?;
        
        // Read data
        let mut buffer = vec![0u8; count as usize];
        let bytes_read = file.read(&mut buffer)
            .map_err(|e| crate::pnfs::Error::Io(e))?;
        
        buffer.truncate(bytes_read);
        
        // Check if we reached EOF
        let file_size = file.metadata()
            .map(|m| m.len())
            .unwrap_or(0);
        let eof = offset + (bytes_read as u64) >= file_size;
        
        debug!("DS READ: returned {} bytes, eof={}", bytes_read, eof);
        
        Ok(ReadResult {
            eof,
            data: buffer,
        })
    }

    /// Handle WRITE operation (RFC 8881 Section 18.32)
    /// 
    /// Writes bytes to a file at the specified offset.
    /// Supports different stability levels (unstable, data_sync, file_sync).
    pub async fn write(
        &self,
        filehandle: &[u8],
        offset: u64,
        data: &[u8],
        stable: WriteStable,
    ) -> Result<WriteResult> {
        debug!(
            "DS WRITE: fh={:?}, offset={}, len={}, stable={:?}",
            &filehandle[0..4.min(filehandle.len())],
            offset,
            data.len(),
            stable
        );

        let file_path = self.filehandle_to_path(filehandle)?;
        
        // Open file for writing (create if doesn't exist)
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&file_path)
            .map_err(|e| {
                warn!("Failed to open file for writing {:?}: {}", file_path, e);
                crate::pnfs::Error::Io(e)
            })?;
        
        // Seek to requested offset
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| crate::pnfs::Error::Io(e))?;
        
        // Write data
        file.write_all(data)
            .map_err(|e| crate::pnfs::Error::Io(e))?;
        
        // Sync based on stability level
        match stable {
            WriteStable::Unstable => {
                // No sync - data may be in cache
            }
            WriteStable::DataSync => {
                // Sync data only (not metadata)
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    // On Unix, sync_data() syncs data but not metadata
                    file.sync_data().map_err(|e| crate::pnfs::Error::Io(e))?;
                }
                #[cfg(not(unix))]
                {
                    file.sync_all().map_err(|e| crate::pnfs::Error::Io(e))?;
                }
            }
            WriteStable::FileSync => {
                // Sync data and metadata
                file.sync_all().map_err(|e| crate::pnfs::Error::Io(e))?;
            }
        }
        
        debug!("DS WRITE: wrote {} bytes", data.len());
        
        Ok(WriteResult {
            count: data.len() as u32,
            committed: stable,
            verifier: Self::generate_verifier(),
        })
    }

    /// Handle COMMIT operation (RFC 8881 Section 18.3)
    /// 
    /// Ensures previously written data is committed to stable storage.
    pub async fn commit(
        &self,
        filehandle: &[u8],
        offset: u64,
        count: u32,
    ) -> Result<CommitResult> {
        debug!(
            "DS COMMIT: fh={:?}, offset={}, count={}",
            &filehandle[0..4.min(filehandle.len())],
            offset,
            count
        );

        let file_path = self.filehandle_to_path(filehandle)?;
        
        // Open file and sync
        let file = File::open(&file_path)
            .map_err(|e| {
                warn!("Failed to open file for commit {:?}: {}", file_path, e);
                crate::pnfs::Error::Io(e)
            })?;
        
        // Sync all data to stable storage
        file.sync_all()
            .map_err(|e| crate::pnfs::Error::Io(e))?;
        
        debug!("DS COMMIT: synced to stable storage");
        
        Ok(CommitResult {
            verifier: Self::generate_verifier(),
        })
    }
    
    /// Map NFS filehandle to filesystem path
    /// 
    /// Resolves filehandle to local path.
    /// Supports both traditional path-based and pNFS file-ID based filehandles.
    fn filehandle_to_path(&self, filehandle: &[u8]) -> Result<PathBuf> {
        use crate::nfs::v4::filehandle_pnfs;
        
        if filehandle.is_empty() {
            return Err(crate::pnfs::Error::Config(
                "Invalid empty filehandle".to_string()
            ));
        }
        
        // Convert to Nfs4FileHandle
        let nfs_fh = Nfs4FileHandle {
            data: filehandle.to_vec(),
        };
        
        // Check if this is a pNFS filehandle (version 2, file-ID based)
        if filehandle_pnfs::is_pnfs_filehandle(&nfs_fh) {
            // pNFS filehandle: map (file_id, stripe_index) to local storage
            let ds_path = filehandle_pnfs::filehandle_to_ds_path(
                &nfs_fh,
                &self.base_path
            ).map_err(|e| crate::pnfs::Error::Config(format!("pNFS filehandle error: {}", e)))?;
            
            info!("📂 pNFS filehandle resolved to: {:?}", ds_path);
            return Ok(ds_path);
        }
        
        // Traditional filehandle: use FileHandleManager
        self.fh_manager
            .filehandle_to_path(&nfs_fh)
            .map_err(|e| crate::pnfs::Error::Config(format!("Invalid filehandle: {}", e)))
    }
    
    /// Get the file handle manager (for filehandle generation)
    pub fn file_handle_manager(&self) -> Arc<FileHandleManager> {
        Arc::clone(&self.fh_manager)
    }
    
    /// Generate a write verifier
    /// 
    /// The verifier is used to detect server reboots. If a client
    /// sees a different verifier, it knows the server restarted
    /// and unstable writes may have been lost.
    fn generate_verifier() -> [u8; 8] {
        // TODO: Use server boot time or instance ID
        // For now, return a fixed verifier
        [0u8; 8]
    }
}

impl Default for IoOperationHandler {
    fn default() -> Self {
        // Use a default path - should be overridden in production
        Self::new("/tmp/pnfs-data").expect("Failed to create default I/O handler")
    }
}

/// READ operation result
#[derive(Debug, Clone)]
pub struct ReadResult {
    /// End of file reached
    pub eof: bool,
    
    /// Data read
    pub data: Vec<u8>,
}

/// WRITE operation result
#[derive(Debug, Clone)]
pub struct WriteResult {
    /// Number of bytes written
    pub count: u32,
    
    /// Stability level achieved
    pub committed: WriteStable,
    
    /// Write verifier (for detecting server reboots)
    pub verifier: [u8; 8],
}

/// COMMIT operation result
#[derive(Debug, Clone)]
pub struct CommitResult {
    /// Write verifier
    pub verifier: [u8; 8],
}

/// Write stability level (RFC 8881 Section 18.32.3)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WriteStable {
    /// Unstable - data may be in cache
    Unstable = 0,
    
    /// Data sync - data written, metadata not necessarily
    DataSync = 1,
    
    /// File sync - data and metadata written
    FileSync = 2,
}


