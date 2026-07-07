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
use dashmap::DashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Upper bound on cached DS file descriptors. Large datasets (e.g. the
/// 16k-file dataloader benchmark) would otherwise exhaust the process
/// fd limit. Eviction is arbitrary-entry — an evicted-but-hot file just
/// pays one reopen, which is the pre-cache behavior for every op.
const FD_CACHE_CAP: usize = 512;

/// I/O at or below this size runs directly on the async worker — a
/// page-cache pread/pwrite at this scale is a few µs, cheaper than
/// ANY handoff. `spawn_blocking` costs a cross-thread send+wake per
/// op (measured −19% of 4k randread throughput) and even
/// `block_in_place` migrates the worker's queue per call (measured
/// −10%); see pnfs-performance-plan.md Phase 2. Larger transfers use
/// `block_in_place` so a slow/cold read can't stall other tasks
/// queued on this worker.
const INLINE_IO_MAX: usize = 64 * 1024;

/// Run a filesystem op of `len` bytes with the cheapest safe
/// blocking strategy (see [`INLINE_IO_MAX`]). ms-scale ops (fsync)
/// must NOT come through here — they go to `spawn_blocking` so they
/// can't hijack async workers under pipelined dispatch.
fn fast_blocking<T>(len: usize, f: impl FnOnce() -> T) -> T {
    if len <= INLINE_IO_MAX {
        return f();
    }
    match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        // current_thread runtime (unit tests): run inline.
        _ => f(),
    }
}

/// Cached open fd for one DS-side file, keyed by raw filehandle bytes.
///
/// `Arc<File>` with no mutex: every operation is positioned I/O
/// (`read_at`/`write_all_at`) or fsync, all of which take `&File` and
/// are safe to issue concurrently on one fd. Safe to key by
/// filehandle because the DS namespace is append-only from the DS's
/// point of view — it serves no REMOVE/RENAME, so a filehandle's
/// path→inode mapping never changes under a live fd.
struct CachedFd {
    file: Arc<File>,
    /// Whether the fd was opened with write access. A READ-populated
    /// read-only fd is upgraded (reopened rw) on the first WRITE.
    writable: bool,
}

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

    /// fd cache: filehandle bytes → open fd. Hits skip both the
    /// filehandle→path resolution and the open(2) that previously ran
    /// on every READ/WRITE/COMMIT.
    fd_cache: DashMap<Vec<u8>, CachedFd>,
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

        Ok(Self { base_path, fh_manager, fd_cache: DashMap::new() })
    }

    /// Look up a cached fd for this filehandle.
    fn cached_fd(&self, filehandle: &[u8]) -> Option<(Arc<File>, bool)> {
        self.fd_cache
            .get(filehandle)
            .map(|e| (Arc::clone(&e.file), e.writable))
    }

    /// Insert (or replace) a cached fd, evicting an arbitrary entry if
    /// the cache is full. In-flight ops hold their own `Arc<File>`
    /// clone, so eviction never closes an fd out from under an op.
    fn insert_fd(&self, filehandle: &[u8], file: Arc<File>, writable: bool) {
        if self.fd_cache.len() >= FD_CACHE_CAP && !self.fd_cache.contains_key(filehandle) {
            // Bind the victim key in its own statement: an `if let`
            // scrutinee would keep the iter shard guard alive across
            // the remove() and deadlock the shard.
            let victim = self.fd_cache.iter().next().map(|e| e.key().clone());
            if let Some(victim) = victim {
                self.fd_cache.remove(&victim);
            }
        }
        self.fd_cache
            .insert(filehandle.to_vec(), CachedFd { file, writable });
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

        let file = match self.cached_fd(filehandle) {
            Some((file, _)) => file,
            None => {
                let file_path = self.filehandle_to_path(filehandle)?;
                // Prefer a read+write fd so a later WRITE to the same
                // file reuses this entry; fall back to read-only.
                let (file, writable) = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&file_path)
                {
                    Ok(f) => (f, true),
                    Err(_) => match File::open(&file_path) {
                        Ok(f) => (f, false),
                        // A stripe file that doesn't exist is a HOLE,
                        // not an error: in the sparse layout a slot's
                        // file only appears on first write, but the
                        // client legitimately reads here whenever the
                        // LOGICAL size covers the range (truncate-up,
                        // fresh sparse files). Answer exactly like a
                        // read past a short stripe file's EOF — zero
                        // bytes + eof, which the client zero-fills.
                        // Returning EIO instead poisoned the file's
                        // layout for 120 s of MDS-fallback errors
                        // (fsstress-found: 13 poisonings in 300 ops).
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            debug!(
                                "DS READ: stripe file {:?} absent — hole, replying eof",
                                file_path
                            );
                            return Ok(ReadResult { eof: true, data: Vec::new() });
                        }
                        Err(e) => {
                            warn!("Failed to open file {:?}: {}", file_path, e);
                            return Err(crate::pnfs::Error::Io(e));
                        }
                    },
                };
                let file = Arc::new(file);
                self.insert_fd(filehandle, Arc::clone(&file), writable);
                file
            }
        };

        // Positioned read via block_in_place: pread(2) is
        // concurrency-safe on a shared fd (no seek pointer), so the
        // cached fd needs no mutex, and the scheduler migrates this
        // worker's queue for the syscall's duration — no per-op
        // cross-thread handoff.
        let (buffer, eof) = fast_blocking(count as usize, || -> std::io::Result<(Vec<u8>, bool)> {
            let mut buffer = vec![0u8; count as usize];
            let bytes_read = file.read_at(&mut buffer, offset)?;
            buffer.truncate(bytes_read);

            // Check if we reached EOF
            let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let eof = offset + (bytes_read as u64) >= file_size;
            Ok((buffer, eof))
        })
        .map_err(crate::pnfs::Error::Io)?;

        debug!("DS READ: returned {} bytes, eof={}", buffer.len(), eof);

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
        data: bytes::Bytes,
        stable: WriteStable,
    ) -> Result<WriteResult> {
        debug!(
            "DS WRITE: fh={:?}, offset={}, len={}, stable={:?}",
            &filehandle[0..4.min(filehandle.len())],
            offset,
            data.len(),
            stable
        );

        let file = match self.cached_fd(filehandle) {
            // Only a writable cached fd will do; a READ-populated
            // read-only entry is upgraded by falling through.
            Some((file, true)) => file,
            _ => {
                let file_path = self.filehandle_to_path(filehandle)?;
                // Rebased paths preserve the MDS directory structure,
                // so the parent tree may not exist yet on this DS.
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(crate::pnfs::Error::Io)?;
                }
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&file_path)
                    .map_err(|e| {
                        warn!("Failed to open file for writing {:?}: {}", file_path, e);
                        crate::pnfs::Error::Io(e)
                    })?;
                let file = Arc::new(file);
                self.insert_fd(filehandle, Arc::clone(&file), true);
                file
            }
        };

        // pwrite(2) at an explicit offset is safe to issue
        // concurrently on the shared cached fd. UNSTABLE writes are a
        // page-cache memcpy (µs) — run them via block_in_place with
        // no cross-thread handoff. Sync variants pay an fsync
        // (ms-scale), so pwrite+fsync go to the blocking pool where
        // they can't hijack async workers under pipelined dispatch.
        // `data` is `Bytes`, so moving it is a refcount bump.
        let wrote = data.len() as u32;
        match stable {
            WriteStable::Unstable => {
                fast_blocking(data.len(), || file.write_all_at(&data, offset))
                    .map_err(crate::pnfs::Error::Io)?;
            }
            WriteStable::DataSync | WriteStable::FileSync => {
                tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    file.write_all_at(&data, offset)?;
                    match stable {
                        WriteStable::DataSync => {
                            // Sync data only (not metadata)
                            #[cfg(unix)]
                            {
                                file.sync_data()?;
                            }
                            #[cfg(not(unix))]
                            {
                                file.sync_all()?;
                            }
                        }
                        _ => {
                            // FILE_SYNC: sync data and metadata
                            file.sync_all()?;
                        }
                    }
                    Ok(())
                })
                .await
                .map_err(|e| crate::pnfs::Error::Config(format!("blocking write task: {}", e)))?
                .map_err(crate::pnfs::Error::Io)?;
            }
        }

        debug!("DS WRITE: wrote {} bytes", wrote);

        Ok(WriteResult {
            count: wrote,
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

        // Reuse the WRITE-side cached fd; fsync doesn't need write
        // access, so any cached entry works. Cold path opens fresh
        // without caching (a commit-only file sees no further I/O).
        let file = match self.cached_fd(filehandle) {
            Some((file, _)) => file,
            None => {
                let file_path = self.filehandle_to_path(filehandle)?;
                match File::open(&file_path) {
                    Ok(f) => Arc::new(f),
                    // No stripe file = nothing was ever written to
                    // this slot = nothing to commit. Same hole
                    // semantics as READ.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        debug!(
                            "DS COMMIT: stripe file {:?} absent — nothing to commit",
                            file_path
                        );
                        return Ok(CommitResult { verifier: Self::generate_verifier() });
                    }
                    Err(e) => {
                        warn!("Failed to open file for commit {:?}: {}", file_path, e);
                        return Err(crate::pnfs::Error::Io(e));
                    }
                }
            }
        };

        // Sync all data to stable storage — on the blocking pool,
        // since fsync can take milliseconds under load.
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(|e| crate::pnfs::Error::Config(format!("blocking commit task: {}", e)))?
            .map_err(crate::pnfs::Error::Io)?;

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
            
            debug!("📂 pNFS filehandle resolved to: {:?}", ds_path);
            return Ok(ds_path);
        }
        
        // Traditional filehandle: try strict (own-instance) resolution
        // first, then fall back to lenient cross-instance parsing for
        // MDS-issued filehandles.
        //
        // In the pNFS file-layout data path, the *Metadata Server*
        // mints the filehandle the client uses against this DS. The
        // MDS bakes its own `instance_id` into the FH and hashes
        // `path || mds_instance_id` — the DS can't reproduce either,
        // so strict validation will always reject MDS-issued FHs.
        //
        // The trust model here is: the MDS is the layout authority.
        // The DS extracts the path bytes from the MDS-issued FH and
        // rebases the FULL path under its own `base_path`, preserving
        // the directory structure. Rebasing by basename (the previous
        // scheme) made files with equal basenames in different
        // directories silently share one backing file — found as data
        // corruption by the ADR 0004 cross-host bench. The DS stores
        // its slice of each file at the rebased path and pwrites at
        // the kernel-supplied offset; bytes the kernel routes to
        // other DSes become sparse holes.
        match self.fh_manager.filehandle_to_path(&nfs_fh) {
            Ok(path) => Ok(path),
            Err(_) => {
                let mds_path = FileHandleManager::parse_path_lenient(&nfs_fh)
                    .map_err(|e| crate::pnfs::Error::Config(
                        format!("Invalid MDS-issued filehandle: {}", e)
                    ))?;
                // Strip the leading '/' so join() nests instead of
                // replacing; guard against traversal components (the
                // MDS never mints them, but the FH is client-supplied
                // bytes on the wire).
                if mds_path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                    return Err(crate::pnfs::Error::Config(
                        "MDS-issued filehandle path contains '..'".to_string(),
                    ));
                }
                let rel = mds_path.strip_prefix("/").unwrap_or(&mds_path);
                let local = self.base_path.join(rel);
                debug!("DS: MDS FH {:?} → local {:?}", mds_path, local);
                Ok(local)
            }
        }
    }
    
    /// Get the file handle manager (for filehandle generation)
    pub fn file_handle_manager(&self) -> Arc<FileHandleManager> {
        Arc::clone(&self.fh_manager)
    }
    
    /// Generate a write verifier
    ///
    /// The verifier detects server restarts: RFC 8881 §18.32 requires
    /// it to change whenever the server may have lost UNSTABLE writes,
    /// so the client compares it on every WRITE/COMMIT reply and
    /// retransmits uncommitted data when it changes. A fixed value
    /// (the previous implementation) meant a DS crash between an
    /// UNSTABLE write and its COMMIT silently lost the data.
    fn generate_verifier() -> [u8; 8] {
        static BOOT_VERIFIER: std::sync::OnceLock<[u8; 8]> = std::sync::OnceLock::new();
        *BOOT_VERIFIER.get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
                ^ (std::process::id() as u64).rotate_left(32);
            nanos.to_be_bytes()
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn handler() -> (IoOperationHandler, TempDir) {
        let dir = TempDir::new().unwrap();
        let h = IoOperationHandler::new(dir.path()).unwrap();
        (h, dir)
    }

    fn fh_for(h: &IoOperationHandler, name: &str, dir: &TempDir) -> Vec<u8> {
        let path = dir.path().join(name);
        if !path.exists() {
            // path_to_filehandle canonicalizes, so the file must exist
            std::fs::File::create(&path).unwrap();
        }
        h.file_handle_manager()
            .path_to_filehandle(&path)
            .unwrap()
            .data
    }

    #[tokio::test]
    async fn write_read_commit_share_cached_fd() {
        let (h, dir) = handler();
        let fh = fh_for(&h, "f1", &dir);

        let w = h.write(&fh, 3, bytes::Bytes::from_static(b"hello"), WriteStable::Unstable).await.unwrap();
        assert_eq!(w.count, 5);
        assert_eq!(h.fd_cache.len(), 1);

        let r = h.read(&fh, 3, 5).await.unwrap();
        assert_eq!(&r.data, b"hello");
        assert!(r.eof);
        assert_eq!(h.fd_cache.len(), 1, "READ must hit the WRITE-cached fd");

        h.commit(&fh, 0, 0).await.unwrap();
        assert_eq!(h.fd_cache.len(), 1, "COMMIT must hit the cached fd");
    }

    #[tokio::test]
    async fn read_populated_entry_upgrades_on_write() {
        let (h, dir) = handler();
        std::fs::write(dir.path().join("ro"), b"data").unwrap();
        let fh = fh_for(&h, "ro", &dir);

        let r = h.read(&fh, 0, 4).await.unwrap();
        assert_eq!(&r.data, b"data");
        assert_eq!(h.fd_cache.len(), 1);

        // WRITE must succeed whether READ cached the fd rw or ro.
        let w = h.write(&fh, 4, bytes::Bytes::from_static(b"more"), WriteStable::FileSync).await.unwrap();
        assert_eq!(w.count, 4);
        let r = h.read(&fh, 0, 8).await.unwrap();
        assert_eq!(&r.data, b"datamore");
    }

    #[tokio::test]
    async fn read_and_commit_of_absent_stripe_file_are_holes_not_errors() {
        let (h, _dir) = handler();
        // v2 identity FH for a stripe file that was never written on
        // this DS. READ must answer zero bytes + eof (the client
        // zero-fills the hole); EIO here poisons the client's layout
        // for 120 s. COMMIT must be a no-op success.
        let fh = crate::nfs::v4::filehandle_pnfs::generate_pnfs_filehandle_from_id(
            0xABCD, 0xDEAD_BEEF_u64, 0,
        );
        let r = h.read(&fh.data, 0, 4096).await.expect("hole read must succeed");
        assert!(r.data.is_empty());
        assert!(r.eof);
        h.commit(&fh.data, 0, 0).await.expect("hole commit must succeed");
    }

    /// Build an MDS-issued filehandle (foreign instance id + garbage
    /// hash so strict resolution fails and the lenient rebase path
    /// runs — exactly what a DS sees on the wire).
    fn mds_fh(path: &str) -> Vec<u8> {
        let mut d = vec![1u8];
        d.extend_from_slice(&0xDEAD_BEEF_u64.to_be_bytes());
        d.extend_from_slice(&[0u8; 32]);
        d.extend_from_slice(&(path.len() as u16).to_be_bytes());
        d.extend_from_slice(path.as_bytes());
        d
    }

    /// ADR 0004 P1 regression: files with equal basenames in different
    /// directories must not share a backing file on the DS.
    #[tokio::test]
    async fn same_basename_different_dirs_do_not_collide() {
        let (h, _dir) = handler();
        let fh_a = mds_fh("/exports/dirA/data.bin");
        let fh_b = mds_fh("/exports/dirB/data.bin");

        h.write(&fh_a, 0, bytes::Bytes::from_static(b"AAAA"), WriteStable::FileSync)
            .await
            .unwrap();
        h.write(&fh_b, 0, bytes::Bytes::from_static(b"BBBB"), WriteStable::FileSync)
            .await
            .unwrap();

        let ra = h.read(&fh_a, 0, 4).await.unwrap();
        let rb = h.read(&fh_b, 0, 4).await.unwrap();
        assert_eq!(&ra.data, b"AAAA", "dirA content clobbered by dirB write");
        assert_eq!(&rb.data, b"BBBB");
    }

    /// The FH path is client-supplied wire bytes: '..' must not
    /// escape the DS data dir.
    #[tokio::test]
    async fn traversal_filehandle_rejected() {
        let (h, _dir) = handler();
        let fh = mds_fh("/exports/../../etc/passwd");
        assert!(h.read(&fh, 0, 4).await.is_err());
        assert!(h
            .write(&fh, 0, bytes::Bytes::from_static(b"x"), WriteStable::Unstable)
            .await
            .is_err());
    }

    /// RFC 8881 §18.32: the verifier must be stable within one server
    /// instance and must not be a constant across restarts (a fixed
    /// zero value silently loses UNSTABLE writes on DS crash).
    #[test]
    fn write_verifier_is_boot_derived() {
        let a = IoOperationHandler::generate_verifier();
        let b = IoOperationHandler::generate_verifier();
        assert_eq!(a, b, "verifier must be stable within a process");
        assert_ne!(a, [0u8; 8], "verifier must not be the fixed zero value");
    }

    #[tokio::test]
    async fn fd_cache_stays_bounded() {
        let (h, dir) = handler();
        for i in 0..(FD_CACHE_CAP + 8) {
            let fh = fh_for(&h, &format!("f{}", i), &dir);
            h.write(&fh, 0, bytes::Bytes::from_static(b"x"), WriteStable::Unstable).await.unwrap();
        }
        assert!(
            h.fd_cache.len() <= FD_CACHE_CAP,
            "cache len {} exceeds cap {}",
            h.fd_cache.len(),
            FD_CACHE_CAP
        );
    }
}


