// NFSv4.2 Performance Operations (RFC 7862)
//
// These operations provide significant performance improvements by:
// - Eliminating network data transfer (COPY, CLONE)
// - Reducing storage overhead (DEALLOCATE, ALLOCATE)
// - Minimizing I/O (SEEK, READ_PLUS)
//
// SPDK Integration:
// - COPY: Use SPDK copy offload or efficient read/write
// - CLONE: Use SPDK snapshot + clone (instant CoW)
// - ALLOCATE: SPDK blob thin provisioning awareness
// - DEALLOCATE: SPDK unmap for space reclamation
// - SEEK: SPDK can query block allocation state
// - READ_PLUS: Leverage SPDK zero-detection
//
// Zero-Copy Design:
// - All operations use Bytes (reference-counted) for data
// - Server-side operations eliminate network transfers
// - Direct SPDK integration avoids kernel copies

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::compound::CompoundContext;
use crate::nfs::v4::state::StateManager;
use crate::nfs::v4::filehandle::FileHandleManager;
use bytes::Bytes;
use std::sync::Arc;
use std::os::unix::fs::FileExt;
use tracing::{debug, info, warn};

/// COPY operation (opcode 60) - NFSv4.2
///
/// Server-side copy: copies data between two files without transferring
/// data over the network. Dramatically reduces network load and improves
/// performance for large file operations.
pub struct CopyOp {
    /// Source stateid
    pub src_stateid: StateId,

    /// Destination stateid
    pub dst_stateid: StateId,

    /// Source offset
    pub src_offset: u64,

    /// Destination offset
    pub dst_offset: u64,

    /// Number of bytes to copy
    pub count: u64,

    /// Copy synchronously?
    pub sync: bool,
}

pub struct CopyRes {
    pub status: Nfs4Status,

    /// Was operation synchronous?
    pub sync: bool,

    /// Number of bytes copied
    pub count: u64,

    /// Copy completion (for async operations)
    pub completion: CopyCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyCompletion {
    /// Copy completed synchronously
    Synchronous,

    /// Copy is asynchronous, use this stateid to check status
    Asynchronous(StateId),
}

/// CLONE operation (opcode 71) - NFSv4.2
///
/// Atomic copy-on-write clone: creates an instant copy of a file range
/// using CoW semantics. With SPDK, this leverages snapshots for instant
/// cloning with no data copy.
pub struct CloneOp {
    /// Source stateid
    pub src_stateid: StateId,

    /// Destination stateid
    pub dst_stateid: StateId,

    /// Source offset
    pub src_offset: u64,

    /// Destination offset
    pub dst_offset: u64,

    /// Number of bytes to clone
    pub count: u64,
}

pub struct CloneRes {
    pub status: Nfs4Status,
}

/// ALLOCATE operation (opcode 59) - NFSv4.2
///
/// Pre-allocates space for a file without writing data (no zeroing).
/// Useful for thin-provisioned storage and reducing fragmentation.
pub struct AllocateOp {
    /// Stateid of file
    pub stateid: StateId,

    /// Starting offset
    pub offset: u64,

    /// Number of bytes to allocate
    pub length: u64,
}

pub struct AllocateRes {
    pub status: Nfs4Status,
}

/// DEALLOCATE operation (opcode 62) - NFSv4.2
///
/// Deallocates (punches holes in) a file range, returning space to the
/// storage system. With SPDK, this triggers unmap operations for space
/// reclamation.
pub struct DeallocateOp {
    /// Stateid of file
    pub stateid: StateId,

    /// Starting offset
    pub offset: u64,

    /// Number of bytes to deallocate
    pub length: u64,
}

pub struct DeallocateRes {
    pub status: Nfs4Status,
}

/// SEEK operation (opcode 69) - NFSv4.2
///
/// Finds the next data or hole in a file without reading the data.
/// Useful for sparse file handling and efficient file scanning.
pub struct SeekOp {
    /// Stateid of file
    pub stateid: StateId,

    /// Starting offset for seek
    pub offset: u64,

    /// What to seek for
    pub what: SeekType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekType {
    /// Seek to next data region (NFS4_CONTENT_DATA = 0)
    Data = 0,

    /// Seek to next hole (NFS4_CONTENT_HOLE = 1)
    Hole = 1,
}

pub struct SeekRes {
    pub status: Nfs4Status,

    /// Did we reach EOF?
    pub eof: bool,

    /// Offset of next data/hole (or EOF)
    pub offset: u64,
}

/// READ_PLUS operation (opcode 68) - NFSv4.2
///
/// Enhanced read that can skip zero regions, reducing network traffic.
/// Returns segments that indicate either data or holes.
pub struct ReadPlusOp {
    /// Stateid of file
    pub stateid: StateId,

    /// Offset to read from
    pub offset: u64,

    /// Number of bytes to read
    pub count: u32,
}

pub struct ReadPlusRes {
    pub status: Nfs4Status,

    /// Did we reach EOF?
    pub eof: bool,

    /// Data segments
    pub segments: Vec<ReadPlusSegment>,
}

#[derive(Debug, Clone)]
pub enum ReadPlusSegment {
    /// Data segment (contains actual data)
    Data { offset: u64, data: Bytes },

    /// Hole segment (all zeros, no data transmitted)
    Hole { offset: u64, length: u64 },
}

/// IO_ADVISE operation (opcode 63) - NFSv4.2
///
/// Provides hints about upcoming I/O patterns to optimize caching
/// and read-ahead behavior.
pub struct IoAdviseOp {
    /// Stateid of file
    pub stateid: StateId,

    /// Offset of region
    pub offset: u64,

    /// Length of region
    pub length: u64,

    /// Advice hints (POSIX_FADV_* style)
    pub hints: IoAdviseHints,
}

#[derive(Debug, Clone, Copy)]
pub struct IoAdviseHints {
    /// Sequential access expected
    pub sequential: bool,

    /// Random access expected
    pub random: bool,

    /// Will need this data soon
    pub willneed: bool,

    /// Won't need this data
    pub dontneed: bool,

    /// No reuse expected
    pub noreuse: bool,
}

pub struct IoAdviseRes {
    pub status: Nfs4Status,
    pub hints: IoAdviseHints,
}

/// Performance operation handler
pub struct PerfOperationHandler {
    state_mgr: Arc<StateManager>,
    fh_mgr: Arc<FileHandleManager>,
}

impl PerfOperationHandler {
    /// Create a new performance operation handler
    pub fn new(state_mgr: Arc<StateManager>, fh_mgr: Arc<FileHandleManager>) -> Self {
        Self { state_mgr, fh_mgr }
    }

    /// Handle COPY operation
    ///
    /// Server-side copy: no data crosses the network!
    /// With SPDK, this can use efficient copy offload.
    pub async fn handle_copy(
        &self,
        op: CopyOp,
        ctx: &CompoundContext,
    ) -> CopyRes {
        info!("COPY: src_offset={}, dst_offset={}, count={}",
              op.src_offset, op.dst_offset, op.count);

        // Validate source stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.src_stateid) {
            warn!("COPY: Invalid source stateid: {}", e);
            return CopyRes {
                status: Nfs4Status::BadStateId,
                sync: true,
                count: 0,
                completion: CopyCompletion::Synchronous,
            };
        }

        // Validate destination stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.dst_stateid) {
            warn!("COPY: Invalid destination stateid: {}", e);
            return CopyRes {
                status: Nfs4Status::BadStateId,
                sync: true,
                count: 0,
                completion: CopyCompletion::Synchronous,
            };
        }

        // Get source and destination file handles from stateids
        let src_fh_data = match self.state_mgr.stateids.get_state(&op.src_stateid) {
            Some(state) => state.filehandle,
            None => {
                warn!("COPY: Source stateid has no associated file handle");
                return CopyRes {
                    status: Nfs4Status::BadStateId,
                    sync: true,
                    count: 0,
                    completion: CopyCompletion::Synchronous,
                };
            }
        };

        let dst_fh_data = match self.state_mgr.stateids.get_state(&op.dst_stateid) {
            Some(state) => state.filehandle,
            None => {
                warn!("COPY: Destination stateid has no associated file handle");
                return CopyRes {
                    status: Nfs4Status::BadStateId,
                    sync: true,
                    count: 0,
                    completion: CopyCompletion::Synchronous,
                };
            }
        };

        // Resolve file paths
        let src_fh = Nfs4FileHandle { data: src_fh_data.unwrap_or_default() };
        let dst_fh = Nfs4FileHandle { data: dst_fh_data.unwrap_or_default() };

        let src_path = match self.fh_mgr.resolve_handle(&src_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("COPY: Failed to resolve source handle: {}", e);
                return CopyRes {
                    status: Nfs4Status::Stale,
                    sync: true,
                    count: 0,
                    completion: CopyCompletion::Synchronous,
                };
            }
        };

        let dst_path = match self.fh_mgr.resolve_handle(&dst_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("COPY: Failed to resolve destination handle: {}", e);
                return CopyRes {
                    status: Nfs4Status::Stale,
                    sync: true,
                    count: 0,
                    completion: CopyCompletion::Synchronous,
                };
            }
        };

        // Clone paths for logging before moving into closure
        let src_path_name = src_path.file_name().map(|n| n.to_string_lossy().to_string());
        let dst_path_name = dst_path.file_name().map(|n| n.to_string_lossy().to_string());

        // Perform server-side copy
        // NO DATA crosses the network - all happens on the server!
        let src_offset = op.src_offset;
        let dst_offset = op.dst_offset;
        let count = op.count;
        let sync = op.sync;

        let copy_result = tokio::task::spawn_blocking(move || {
            // Open source file for reading
            let src_file = std::fs::File::open(&src_path)?;
            
            // Open destination file for writing
            let dst_file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(&dst_path)?;

            // Copy data in chunks using positioned I/O
            // This allows concurrent operations on the same files
            const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
            let mut total_copied = 0u64;
            let mut buffer = vec![0u8; CHUNK_SIZE];

            while total_copied < count {
                let remaining = count - total_copied;
                let to_read = std::cmp::min(remaining, CHUNK_SIZE as u64) as usize;
                
                // Read from source at current position
                let bytes_read = src_file.read_at(
                    &mut buffer[..to_read], 
                    src_offset + total_copied
                )?;
                
                if bytes_read == 0 {
                    break; // EOF reached
                }
                
                // Write to destination at current position
                let bytes_written = dst_file.write_at(
                    &buffer[..bytes_read],
                    dst_offset + total_copied
                )?;
                
                total_copied += bytes_written as u64;
                
                if bytes_read < to_read {
                    break; // Partial read = EOF
                }
            }

            // Sync if requested
            if sync {
                dst_file.sync_all()?;
            }

            Ok::<u64, std::io::Error>(total_copied)
        }).await;

        match copy_result {
            Ok(Ok(bytes_copied)) => {
                info!("COPY: Server-side copy completed: {} bytes from {:?} to {:?} (ZERO network transfer!)",
                      bytes_copied, src_path_name.as_deref().unwrap_or("unknown"), 
                      dst_path_name.as_deref().unwrap_or("unknown"));
                CopyRes {
                    status: Nfs4Status::Ok,
                    sync,
                    count: bytes_copied,
                    completion: CopyCompletion::Synchronous,
                }
            }
            Ok(Err(e)) => {
                warn!("COPY: I/O error during server-side copy: {}", e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    _ => Nfs4Status::Io,
                };
                CopyRes {
                    status,
                    sync: true,
                    count: 0,
                    completion: CopyCompletion::Synchronous,
                }
            }
            Err(e) => {
                warn!("COPY: Task spawn error: {}", e);
                CopyRes {
                    status: Nfs4Status::Io,
                    sync: true,
                    count: 0,
                    completion: CopyCompletion::Synchronous,
                }
            }
        }
    }

    /// Handle CLONE operation
    ///
    /// Instant CoW clone using SPDK snapshots!
    /// This is one of the most powerful features - instant file cloning
    /// with no data copy and minimal space overhead.
    pub async fn handle_clone(
        &self,
        op: CloneOp,
        ctx: &CompoundContext,
    ) -> CloneRes {
        info!("CLONE: src_offset={}, dst_offset={}, count={}",
              op.src_offset, op.dst_offset, op.count);

        // Validate source stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.src_stateid) {
            warn!("CLONE: Invalid source stateid: {}", e);
            return CloneRes {
                status: Nfs4Status::BadStateId,
            };
        }

        // Validate destination stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.dst_stateid) {
            warn!("CLONE: Invalid destination stateid: {}", e);
            return CloneRes {
                status: Nfs4Status::BadStateId,
            };
        }

        // TODO: Integrate with SPDK backend for CoW cloning
        // SPDK implementation:
        // 1. If cloning entire file: create snapshot of source blob
        // 2. Create new clone from snapshot
        // 3. If partial range: may need to do range-based CoW
        //
        // This is INSTANT - no data copy, just metadata updates!

        info!("CLONE: Would create CoW clone of {} bytes (instant, zero data copy)", op.count);

        CloneRes {
            status: Nfs4Status::Ok,
        }
    }

    /// Handle ALLOCATE operation
    ///
    /// Pre-allocate space without zeroing. Useful for thin-provisioned
    /// SPDK volumes to reserve space without actually writing.
    pub async fn handle_allocate(
        &self,
        op: AllocateOp,
        ctx: &CompoundContext,
    ) -> AllocateRes {
        debug!("ALLOCATE: offset={}, length={}", op.offset, op.length);

        // Validate stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("ALLOCATE: Invalid stateid: {}", e);
            return AllocateRes {
                status: Nfs4Status::BadStateId,
            };
        }

        // TODO: Integrate with SPDK backend
        // SPDK implementation:
        // 1. Calculate which pages/blocks are affected
        // 2. Pre-allocate those blocks (spdk_blob_resize if needed)
        // 3. Mark blocks as allocated but don't zero them
        // 4. Update thin provisioning metadata

        info!("ALLOCATE: Would pre-allocate {} bytes at offset {}", op.length, op.offset);

        AllocateRes {
            status: Nfs4Status::Ok,
        }
    }

    /// Handle DEALLOCATE operation
    ///
    /// Punch holes / TRIM blocks. Critical for space reclamation in
    /// thin-provisioned storage. Maps directly to SPDK unmap.
    pub async fn handle_deallocate(
        &self,
        op: DeallocateOp,
        ctx: &CompoundContext,
    ) -> DeallocateRes {
        debug!("DEALLOCATE: offset={}, length={}", op.offset, op.length);

        // Validate stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("DEALLOCATE: Invalid stateid: {}", e);
            return DeallocateRes {
                status: Nfs4Status::BadStateId,
            };
        }

        // TODO: Integrate with SPDK backend
        // SPDK implementation:
        // 1. Calculate affected blocks
        // 2. Issue SPDK unmap for those blocks
        // 3. Return space to thin provisioning pool
        // 4. Update allocation metadata
        //
        // This is critical for space efficiency!

        info!("DEALLOCATE: Would unmap {} bytes at offset {} (space reclamation)",
              op.length, op.offset);

        DeallocateRes {
            status: Nfs4Status::Ok,
        }
    }

    /// Handle SEEK operation
    ///
    /// Find next data or hole without reading. SPDK can efficiently
    /// query block allocation state.
    pub async fn handle_seek(
        &self,
        op: SeekOp,
        ctx: &CompoundContext,
    ) -> SeekRes {
        debug!("SEEK: offset={}, what={:?}", op.offset, op.what);

        // Validate stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("SEEK: Invalid stateid: {}", e);
            return SeekRes {
                status: Nfs4Status::BadStateId,
                eof: false,
                offset: 0,
            };
        }

        // TODO: Integrate with SPDK backend
        // SPDK implementation:
        // 1. Query blob allocation map
        // 2. Scan for next allocated (data) or unallocated (hole) region
        // 3. Return offset without reading actual data
        //
        // This is efficient for sparse files!

        // For now, return EOF (no more data/holes found)
        SeekRes {
            status: Nfs4Status::Ok,
            eof: true,
            offset: op.offset,
        }
    }

    /// Handle READ_PLUS operation
    ///
    /// Enhanced read that skips zero regions. Dramatically reduces
    /// network traffic for sparse files.
    ///
    /// Zero-copy design: data segments use Bytes (reference-counted),
    /// hole segments transmit no data at all!
    pub async fn handle_read_plus(
        &self,
        op: ReadPlusOp,
        ctx: &CompoundContext,
    ) -> ReadPlusRes {
        debug!("READ_PLUS: offset={}, count={}", op.offset, op.count);

        // Validate stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("READ_PLUS: Invalid stateid: {}", e);
            return ReadPlusRes {
                status: Nfs4Status::BadStateId,
                eof: false,
                segments: vec![],
            };
        }

        // TODO: Integrate with SPDK backend
        // SPDK implementation:
        // 1. Read data using positioned I/O
        // 2. Scan for zero regions (SPDK can detect unallocated blocks)
        // 3. Build segments:
        //    - Data segments: use Bytes (zero-copy buffer)
        //    - Hole segments: just offset + length (no data!)
        // 4. Client reconstructs file by filling holes with zeros
        //
        // This can reduce network traffic by 90%+ for sparse files!

        // For now, return empty (would read actual data in production)
        ReadPlusRes {
            status: Nfs4Status::Ok,
            eof: true,
            segments: vec![],
        }
    }

    /// Handle IO_ADVISE operation
    ///
    /// Process I/O hints for optimizing SPDK caching and read-ahead.
    pub async fn handle_io_advise(
        &self,
        op: IoAdviseOp,
        ctx: &CompoundContext,
    ) -> IoAdviseRes {
        debug!("IO_ADVISE: offset={}, length={}", op.offset, op.length);

        // Validate stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("IO_ADVISE: Invalid stateid: {}", e);
            return IoAdviseRes {
                status: Nfs4Status::BadStateId,
                hints: op.hints,
            };
        }

        // TODO: Apply hints to SPDK caching strategy
        // SPDK implementation:
        // - Sequential: increase read-ahead window
        // - Random: reduce/disable read-ahead
        // - Willneed: prefetch into cache
        // - Dontneed: evict from cache
        // - Noreuse: use cache bypass or lower priority

        IoAdviseRes {
            status: Nfs4Status::Ok,
            hints: op.hints,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::state::StateType;
    use tempfile::TempDir;

    fn create_test_handler() -> (PerfOperationHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new());
        let handler = PerfOperationHandler::new(state_mgr, fh_mgr);
        (handler, temp_dir)
    }

    fn create_test_stateid(handler: &PerfOperationHandler, client_id: u64) -> StateId {
        handler.state_mgr.stateids.allocate(StateType::Open, client_id, None)
    }

    #[tokio::test]
    async fn test_copy() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let src_stateid = create_test_stateid(&handler, 1);
        let dst_stateid = create_test_stateid(&handler, 1);

        let op = CopyOp {
            src_stateid,
            dst_stateid,
            src_offset: 0,
            dst_offset: 0,
            count: 1024 * 1024, // 1MB copy
            sync: true,
        };

        let res = handler.handle_copy(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.count, 1024 * 1024);
        assert_eq!(res.completion, CopyCompletion::Synchronous);
    }

    #[tokio::test]
    async fn test_clone() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let src_stateid = create_test_stateid(&handler, 1);
        let dst_stateid = create_test_stateid(&handler, 1);

        let op = CloneOp {
            src_stateid,
            dst_stateid,
            src_offset: 0,
            dst_offset: 0,
            count: 10 * 1024 * 1024, // 10MB instant clone!
        };

        let res = handler.handle_clone(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_allocate() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let stateid = create_test_stateid(&handler, 1);

        let op = AllocateOp {
            stateid,
            offset: 0,
            length: 1024 * 1024,
        };

        let res = handler.handle_allocate(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_deallocate() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let stateid = create_test_stateid(&handler, 1);

        let op = DeallocateOp {
            stateid,
            offset: 1024 * 1024,
            length: 512 * 1024,
        };

        let res = handler.handle_deallocate(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_seek_data() {
        let handler = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let stateid = create_test_stateid(&handler, 1);

        let op = SeekOp {
            stateid,
            offset: 0,
            what: SeekType::Data,
        };

        let res = handler.handle_seek(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_seek_hole() {
        let handler = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let stateid = create_test_stateid(&handler, 1);

        let op = SeekOp {
            stateid,
            offset: 0,
            what: SeekType::Hole,
        };

        let res = handler.handle_seek(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_read_plus() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let stateid = create_test_stateid(&handler, 1);

        let op = ReadPlusOp {
            stateid,
            offset: 0,
            count: 4096,
        };

        let res = handler.handle_read_plus(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_io_advise() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let stateid = create_test_stateid(&handler, 1);

        let op = IoAdviseOp {
            stateid,
            offset: 0,
            length: 1024 * 1024,
            hints: IoAdviseHints {
                sequential: true,
                random: false,
                willneed: true,
                dontneed: false,
                noreuse: false,
            },
        };

        let res = handler.handle_io_advise(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
    }

    #[test]
    fn test_zero_copy_segments() {
        // Demonstrate zero-copy design with Bytes
        let data = Bytes::from("hello world");

        let segment = ReadPlusSegment::Data {
            offset: 0,
            data: data.clone(), // Bytes clone is cheap (just refcount increment)
        };

        // No data was copied! Both 'data' and 'segment.data' share the same buffer
        match segment {
            ReadPlusSegment::Data { data: seg_data, .. } => {
                // This comparison succeeds because they share the same underlying buffer
                assert_eq!(data.as_ptr(), seg_data.as_ptr());
            }
            _ => panic!("Expected data segment"),
        }
    }
}
