//! Local Filesystem Backend for NFS Server
//!
//! Serves files from a locally mounted directory (typically a ublk-mounted SPDK volume)
//! over NFS. This enables ReadWriteMany (RWX) access to Flint volumes.

use super::filehandle::HandleCache;
use super::protocol::{FileAttr, FileHandle, FsInfo, FsStat};
use bytes::Bytes;
use std::io;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Directory entry
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub fileid: u64,
    pub name: String,
    pub cookie: u64,
    pub attr: Option<FileAttr>,
}

/// Local filesystem backend for NFS server
///
/// Serves files from a locally mounted directory. In Flint's architecture, this
/// is typically a ublk device mounted at /var/lib/flint/mounts/vol-{id}
pub struct LocalFilesystem {
    /// Root export path (e.g., /var/lib/flint/mounts/vol-123)
    root: PathBuf,
    
    /// File handle cache
    handle_cache: HandleCache,
}

impl LocalFilesystem {
    /// Create a new local filesystem backend
    pub fn new(root: PathBuf) -> io::Result<Self> {
        // Ensure the root exists and is a directory
        let metadata = std::fs::metadata(&root)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Export path must be a directory",
            ));
        }
        
        let handle_cache = HandleCache::new(root.clone());
        
        Ok(Self {
            root,
            handle_cache,
        })
    }
    
    /// Get the root file handle
    pub fn root_handle(&self) -> io::Result<FileHandle> {
        self.handle_cache.root_handle()
    }
    
    /// Resolve a file handle to a path
    fn resolve(&self, fh: &FileHandle) -> io::Result<PathBuf> {
        self.handle_cache.resolve(fh)
    }
    
    /// Get file attributes
    pub async fn getattr(&self, fh: &FileHandle) -> io::Result<FileAttr> {
        let path = self.resolve(fh)?;
        let metadata = fs::metadata(&path).await?;
        
        use std::os::unix::fs::MetadataExt;
        let fileid = metadata.ino();
        
        Ok(FileAttr::from_metadata(&metadata.into(), fileid))
    }
    
    /// Look up a file/directory by name within a parent directory
    pub async fn lookup(&self, dir_fh: &FileHandle, name: &str) -> io::Result<(FileHandle, FileAttr)> {
        // Special handling for "." and ".."
        let fh = if name == "." {
            dir_fh.clone()
        } else if name == ".." {
            // For simplicity, treat ".." as current directory if at root
            // In a full implementation, we'd track parent directories
            dir_fh.clone()
        } else {
            self.handle_cache.lookup_child(dir_fh, name)?
        };
        
        let attr = self.getattr(&fh).await?;
        Ok((fh, attr))
    }
    
    /// Read data from a file
    pub async fn read(&self, fh: &FileHandle, offset: u64, count: u32) -> io::Result<Bytes> {
        let path = self.resolve(fh)?;
        let mut file = fs::File::open(&path).await?;
        
        // Seek to offset
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        
        // Read up to count bytes
        let mut buffer = vec![0u8; count as usize];
        let n = file.read(&mut buffer).await?;
        
        buffer.truncate(n);
        Ok(Bytes::from(buffer))
    }
    
    /// Write data to a file
    pub async fn write(&self, fh: &FileHandle, offset: u64, data: &[u8]) -> io::Result<u32> {
        let path = self.resolve(fh)?;
        
        // Open file for writing
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await?;
        
        // Seek to offset
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        
        // Write data
        file.write_all(data).await?;
        
        // Sync to disk (NFSv3 expects data to be stable)
        file.sync_all().await?;
        
        Ok(data.len() as u32)
    }
    
    /// Create a new regular file
    pub async fn create(&self, dir_fh: &FileHandle, name: &str, mode: u32) -> io::Result<(FileHandle, FileAttr)> {
        let dir_path = self.resolve(dir_fh)?;
        let file_path = dir_path.join(name);
        
        // Validate path is within export
        if !file_path.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Path traversal attempt",
            ));
        }
        
        // Create the file
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&file_path)
            .await?;
        
        drop(file);
        
        // Get file handle and attributes
        let fh = self.handle_cache.lookup_child(dir_fh, name)?;
        let attr = self.getattr(&fh).await?;
        
        Ok((fh, attr))
    }
    
    /// Remove a file
    pub async fn remove(&self, dir_fh: &FileHandle, name: &str) -> io::Result<()> {
        let dir_path = self.resolve(dir_fh)?;
        let file_path = dir_path.join(name);
        
        // Validate path
        if !file_path.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Path traversal attempt",
            ));
        }
        
        fs::remove_file(&file_path).await
    }
    
    /// Create a directory
    pub async fn mkdir(&self, dir_fh: &FileHandle, name: &str, mode: u32) -> io::Result<(FileHandle, FileAttr)> {
        let dir_path = self.resolve(dir_fh)?;
        let new_dir_path = dir_path.join(name);
        
        // Validate path
        if !new_dir_path.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Path traversal attempt",
            ));
        }
        
        // Create directory
        fs::create_dir(&new_dir_path).await?;
        
        // Set permissions (Unix-specific)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(&new_dir_path, perms)?;
        }
        
        // Get file handle and attributes
        let fh = self.handle_cache.lookup_child(dir_fh, name)?;
        let attr = self.getattr(&fh).await?;
        
        Ok((fh, attr))
    }
    
    /// Remove a directory
    pub async fn rmdir(&self, dir_fh: &FileHandle, name: &str) -> io::Result<()> {
        let dir_path = self.resolve(dir_fh)?;
        let target_path = dir_path.join(name);
        
        // Validate path
        if !target_path.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Path traversal attempt",
            ));
        }
        
        fs::remove_dir(&target_path).await
    }
    
    /// Read directory entries
    pub async fn readdir(&self, dir_fh: &FileHandle, cookie: u64, _count: u32) -> io::Result<Vec<DirEntry>> {
        let dir_path = self.resolve(dir_fh)?;
        
        let mut entries = Vec::new();
        let mut read_dir = fs::read_dir(&dir_path).await?;
        
        let mut current_cookie = 0u64;
        
        while let Some(entry) = read_dir.next_entry().await? {
            current_cookie += 1;
            
            // Skip entries before the requested cookie
            if current_cookie <= cookie {
                continue;
            }
            
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata().await?;
            
            use std::os::unix::fs::MetadataExt;
            let fileid = metadata.ino();
            
            entries.push(DirEntry {
                fileid,
                name,
                cookie: current_cookie,
                attr: Some(FileAttr::from_metadata(&metadata.into(), fileid)),
            });
        }
        
        Ok(entries)
    }
    
    /// Get filesystem statistics
    pub async fn statfs(&self, _fh: &FileHandle) -> io::Result<FsStat> {
        // Get filesystem statistics using statvfs
        let stat = nix::sys::statvfs::statvfs(&self.root)?;
        
        let block_size = stat.block_size() as u64;
        let total_blocks = stat.blocks() as u64;
        let free_blocks = stat.blocks_free() as u64;
        let avail_blocks = stat.blocks_available() as u64;
        
        Ok(FsStat {
            tbytes: total_blocks * block_size,
            fbytes: free_blocks * block_size,
            abytes: avail_blocks * block_size,
            tfiles: stat.files() as u64,
            ffiles: stat.files_free() as u64,
            afiles: stat.files_available() as u64,
            invarsec: 0, // Filesystem doesn't change unexpectedly
        })
    }
    
    /// Get filesystem info/capabilities
    pub fn fsinfo(&self) -> FsInfo {
        FsInfo::default_config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::protocol::FileType;
    
    #[tokio::test]
    async fn test_create_and_read() {
        let tmpdir = tempfile::tempdir().unwrap();
        let vfs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
        let root_fh = vfs.root_handle().unwrap();
        
        // Create a file
        let (file_fh, _attr) = vfs.create(&root_fh, "test.txt", 0o644).await.unwrap();
        
        // Write some data
        let data = b"Hello, NFS!";
        let written = vfs.write(&file_fh, 0, data).await.unwrap();
        assert_eq!(written, data.len() as u32);
        
        // Read it back
        let read_data = vfs.read(&file_fh, 0, 100).await.unwrap();
        assert_eq!(read_data.as_ref(), data);
    }
    
    #[tokio::test]
    async fn test_mkdir_and_lookup() {
        let tmpdir = tempfile::tempdir().unwrap();
        let vfs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
        let root_fh = vfs.root_handle().unwrap();
        
        // Create a directory
        let (_dir_fh, attr) = vfs.mkdir(&root_fh, "mydir", 0o755).await.unwrap();
        assert_eq!(attr.file_type, FileType::Directory);
        
        // Look it up
        let (lookup_fh, lookup_attr) = vfs.lookup(&root_fh, "mydir").await.unwrap();
        assert_eq!(lookup_attr.file_type, FileType::Directory);
    }
    
    #[tokio::test]
    async fn test_readdir() {
        let tmpdir = tempfile::tempdir().unwrap();
        let vfs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
        let root_fh = vfs.root_handle().unwrap();
        
        // Create some files
        vfs.create(&root_fh, "file1.txt", 0o644).await.unwrap();
        vfs.create(&root_fh, "file2.txt", 0o644).await.unwrap();
        vfs.mkdir(&root_fh, "dir1", 0o755).await.unwrap();
        
        // Read directory
        let entries = vfs.readdir(&root_fh, 0, 8192).await.unwrap();
        assert_eq!(entries.len(), 3);
        
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"file1.txt".to_string()));
        assert!(names.contains(&"file2.txt".to_string()));
        assert!(names.contains(&"dir1".to_string()));
    }
}

