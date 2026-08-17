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
use std::path::Path;
use tracing::{debug, warn};

/// Resolve a wire `count` against the source file, or reject the request.
///
/// RFC 7862 §15.13.3 (CLONE) and §15.2.3 (COPY): a count of 0 means "to
/// the end of the source file", not "zero bytes".
///
/// This exists because `handle_clone` used to hold TWO readings of the
/// same request. Its `(0,0,0)` branch treated count==0 as "replace the
/// whole destination file"; its range branch treated the identical
/// count==0 as "to source EOF, leave the destination's tail alone". One
/// request, one function, two meanings. There is now one path and one
/// reading, and it lives here so COPY can adopt it in the conformance
/// pass without the two drifting apart again.
///
/// Rejecting `src_offset > src_size` is not defensive politeness: the
/// old range branch computed `metadata()?.len() - src_offset` on u64,
/// the workspace `Cargo.toml` has no `[profile]` section, so that
/// subtraction WRAPPED in release builds and yielded a ~16-exabyte
/// length. A `saturating_sub` would have hidden it rather than fixed it.
/// RFC 7862 §15.2.3 (COPY) and §15.13.3 (CLONE) carry the identical
/// sentence, which is why one helper serves both:
///
/// > If the source offset or the source offset plus count is greater than
/// > the size of the source file, the operation MUST fail with
/// > NFS4ERR_INVAL.
fn resolve_range_len(src_size: u64, src_offset: u64, count: u64) -> std::io::Result<u64> {
    let inval = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg);

    if src_offset > src_size {
        return Err(inval(format!(
            "source offset {} is past end of source file ({} bytes)",
            src_offset, src_size
        )));
    }
    let len = if count == 0 {
        src_size - src_offset
    } else {
        count
    };
    // checked_add, not `src_offset + len`: both come off the wire as u64
    // and their sum is exactly the quantity the RFC asks us to compare.
    match src_offset.checked_add(len) {
        Some(end) if end <= src_size => Ok(len),
        _ => Err(inval(format!(
            "source range [{}, {}+{}) extends past end of source file ({} bytes)",
            src_offset, src_offset, len, src_size
        ))),
    }
}

/// Whether two open files are the same filesystem object.
///
/// Compares `(dev, ino)` rather than paths because the filehandle layer
/// follows a rename-alias table: one inode is legitimately reachable
/// through different handle bytes and different path strings, so a path
/// comparison would miss the case the RFC is about.
#[cfg(unix)]
fn same_file(a: &std::fs::File, b: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (x, y) = (a.metadata()?, b.metadata()?);
    Ok(x.dev() == y.dev() && x.ino() == y.ino())
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::File, _b: &std::fs::File) -> std::io::Result<bool> {
    Ok(false)
}

/// Bump the change counter for an already-open file.
///
/// Takes the fd rather than a path (the `bump_path` form) so a rename
/// racing the operation cannot steer the bump onto a different inode.
#[cfg(unix)]
fn bump_change_counter(f: &std::fs::File) {
    use std::os::unix::fs::MetadataExt;
    if let Ok(md) = f.metadata() {
        crate::nfs::v4::change_counter::bump(
            md.dev(),
            md.ino(),
            crate::nfs::v4::change_counter::ctime_ns(&md),
        );
    }
}

#[cfg(not(unix))]
fn bump_change_counter(_f: &std::fs::File) {}

/// Copy a byte range with positioned I/O. Returns bytes actually copied,
/// which is short only when the source ends early.
fn copy_range(
    src_file: &std::fs::File,
    dst_file: &std::fs::File,
    src_offset: u64,
    dst_offset: u64,
    len: u64,
) -> std::io::Result<u64> {
    const CHUNK: usize = 1024 * 1024;
    let mut buffer = vec![0u8; (len.min(CHUNK as u64)) as usize];
    let mut done = 0u64;

    while done < len {
        let want = (len - done).min(buffer.len() as u64) as usize;
        let got = src_file.read_at(&mut buffer[..want], src_offset + done)?;
        if got == 0 {
            break; // source ended early
        }
        dst_file.write_at(&buffer[..got], dst_offset + done)?;
        done += got as u64;
    }

    Ok(done)
}

/// Try a copy-on-write clone of a byte range via the Linux FICLONERANGE
/// ioctl (XFS with reflink, Btrfs, OCFS2).
///
/// NON-DESTRUCTIVE ON FAILURE, and that is the entire reason this is
/// FICLONERANGE and not FICLONE. The FICLONE path this replaces opened
/// the destination `.truncate(true)` BEFORE issuing the ioctl, so on any
/// filesystem without reflink support — and `mkfs.ext4` is a shipped
/// option (main.rs) — every whole-file CLONE emptied the destination and
/// then rebuilt it non-atomically. When the rebuild then failed (ENOSPC,
/// EACCES) the client was told CLONE had FAILED and the destination was
/// already gone: an error naming the opposite of what happened.
///
/// FICLONERANGE writes nothing on failure and grows the destination only
/// when the cloned range reaches past its end — which is what a byte-range
/// CLONE is supposed to do.
///
/// Err (EOPNOTSUPP off-reflink, EINVAL when the range is not block
/// aligned) means "fall back to a read/write loop", not "the file is
/// damaged".
#[cfg(target_os = "linux")]
fn try_reflink_range(
    src_file: &std::fs::File,
    dst_file: &std::fs::File,
    src_offset: u64,
    len: u64,
    dst_offset: u64,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // struct file_clone_range {
    //     __s64 src_fd; __u64 src_offset; __u64 src_length; __u64 dest_offset;
    // };
    #[repr(C)]
    struct FileCloneRange {
        src_fd: i64,
        src_offset: u64,
        src_length: u64,
        dest_offset: u64,
    }

    // linux/fs.h: FICLONERANGE = _IOW(0x94, 13, struct file_clone_range)
    //   _IOC_WRITE<<30 | sizeof(32)<<16 | 0x94<<8 | 13
    // = 0x40000000 | 0x00200000 | 0x9400 | 0x0D
    const FICLONERANGE: nix::libc::Ioctl = 0x4020_940D;

    let arg = FileCloneRange {
        src_fd: src_file.as_raw_fd() as i64,
        src_offset,
        src_length: len,
        dest_offset: dst_offset,
    };

    let rc = unsafe {
        nix::libc::ioctl(
            dst_file.as_raw_fd(),
            FICLONERANGE,
            &arg as *const FileCloneRange,
        )
    };

    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn try_reflink_range(
    _src_file: &std::fs::File,
    _dst_file: &std::fs::File,
    _src_offset: u64,
    _len: u64,
    _dst_offset: u64,
) -> std::io::Result<()> {
    // Reflink is Linux-only. Note this arm touches NEITHER file, so the
    // fallback below sees an untouched destination — same contract as the
    // Linux failure path.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Reflink cloning only supported on Linux",
    ))
}

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

/// What a fallocate-backed op does to the range (platform-neutral so
/// non-Linux dev builds compile; translated to FallocateFlags on Linux).
#[derive(Debug, Clone, Copy)]
enum AllocMode {
    /// ALLOCATE: reserve blocks and extend size (posix_fallocate).
    Allocate,
    /// DEALLOCATE: punch a hole, keep size.
    PunchHole,
}

/// Performance operation handler
pub struct PerfOperationHandler {
    state_mgr: Arc<StateManager>,
    fh_mgr: Arc<FileHandleManager>,
    /// Present only in the MDS role. See `is_striped`.
    pnfs_handler: Option<Arc<dyn crate::pnfs::PnfsOperations>>,
}

impl PerfOperationHandler {
    /// Create a new performance operation handler (standalone / DS role:
    /// no pNFS, every file is served locally).
    pub fn new(state_mgr: Arc<StateManager>, fh_mgr: Arc<FileHandleManager>) -> Self {
        Self::new_with_pnfs(state_mgr, fh_mgr, None)
    }

    /// Create a handler that knows about striped files.
    pub fn new_with_pnfs(
        state_mgr: Arc<StateManager>,
        fh_mgr: Arc<FileHandleManager>,
        pnfs_handler: Option<Arc<dyn crate::pnfs::PnfsOperations>>,
    ) -> Self {
        Self { state_mgr, fh_mgr, pnfs_handler }
    }

    /// Whether `path` names a pNFS-managed (placement-pinned) file — one
    /// whose bytes live on the DS fleet and whose MDS-local file is a
    /// sparse, size-only stub.
    ///
    /// COPY and CLONE are guarded HERE rather than in the dispatcher, and
    /// that is forced, not stylistic. Under RFC 7862 §15.2 the COPY source
    /// is SAVED_FH and the destination is CURRENT_FH; these two handlers
    /// resolve both ends from their own stateids and never read the
    /// compound context at all. A dispatcher-side guard keyed on the
    /// CURRENT filehandle — which is how ALLOCATE/DEALLOCATE/SEEK are
    /// guarded, correctly, because those DO read current_fh — structurally
    /// cannot see a COPY's source, for any client, conforming or not.
    ///
    /// Keyed PER FILE (`is_pnfs_managed`), not per role
    /// (`pnfs_handler.is_some()`): an MDS deliberately keeps files that
    /// were never layouted fully readable and writable, a decision spelled
    /// out in both `pnfs/handler_trait.rs` and the READ/WRITE guard in
    /// `dispatcher.rs`. Reversing it here alone would make COPY stricter
    /// than WRITE on the same file.
    fn is_striped(&self, path: &Path) -> bool {
        let Some(pnfs) = &self.pnfs_handler else {
            return false;
        };
        let export = self.fh_mgr.get_export_path();
        let key = path
            .strip_prefix(export)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        !key.is_empty() && pnfs.is_pnfs_managed(&key)
    }

    /// Handle COPY operation
    ///
    /// Server-side copy: no data crosses the network!
    /// With SPDK, this can use efficient copy offload.
    pub async fn handle_copy(
        &self,
        op: CopyOp,
        _ctx: &CompoundContext,
    ) -> CopyRes {
        debug!("COPY: src_offset={}, dst_offset={}, count={}",
              op.src_offset, op.dst_offset, op.count);

        // Validate source stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.src_stateid) {
            warn!("COPY: Invalid source stateid: {}", e);
            return CopyRes {
                status: Nfs4Status::BadStateId,
                sync: true,
                count: 0,
                completion: CopyCompletion::Synchronous,
            };
        }

        // Validate destination stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.dst_stateid) {
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

        // Refuse either end being a striped file. Copying FROM one reads
        // the sparse stub and silently produces a destination full of
        // zeros; copying TO one writes bytes the DSes will never serve.
        // Both report success, which is the F15 failure class exactly.
        for (role, path) in [("source", &src_path), ("destination", &dst_path)] {
            if self.is_striped(path) {
                warn!(
                    "⛔ COPY {} '{}' is a striped file — its bytes live on the DSes and the MDS file is a sparse stub. NFS4ERR_NOTSUPP so the client falls back to read/write",
                    role,
                    path.display()
                );
                return CopyRes {
                    status: Nfs4Status::NotSupp,
                    sync: true,
                    count: 0,
                    completion: CopyCompletion::Synchronous,
                };
            }
        }

        // Clone paths for logging before moving into closure
        let src_path_name = src_path.file_name().map(|n| n.to_string_lossy().to_string());
        let dst_path_name = dst_path.file_name().map(|n| n.to_string_lossy().to_string());

        // Perform server-side copy
        // NO DATA crosses the network - all happens on the server!
        let src_offset = op.src_offset;
        let dst_offset = op.dst_offset;
        let count = op.count;
        // `op.sync` (ca_synchronous) is deliberately NOT read. It is the
        // client's REQUEST, and this server has exactly one behaviour:
        // copy synchronously, fsync, reply. Letting it steer the fsync is
        // what made wr_committed = FILE_SYNC4 a lie for async requests.

        let copy_result = tokio::task::spawn_blocking(move || {
            // Open source file for reading
            let src_file = std::fs::File::open(&src_path)?;

            // Open destination file for writing. truncate(false) is
            // explicit, not incidental: COPY writes a byte RANGE and must
            // leave everything outside it — including the destination's
            // length — alone.
            let dst_file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&dst_path)?;

            // RFC 7862 §15.2.3: "SAVED_FH and CURRENT_FH must be different
            // files. If SAVED_FH and CURRENT_FH refer to the same file, the
            // operation MUST fail with NFS4ERR_INVAL." No overlap
            // qualifier — COPY is stricter than CLONE here.
            //
            // It is also the corruption case: the chunk loop is a memcpy
            // where a same-file copy would need a memmove, so with
            // src=0/dst=512K the first 1 MiB chunk overwrites source bytes
            // the second chunk has yet to read.
            if same_file(&src_file, &dst_file)? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "COPY source and destination are the same file",
                ));
            }

            // A4 write gate on the DESTINATION, held across the copy
            // loop, the sync, and the capture note. Excluded maps to
            // WouldBlock → NFS4ERR_DELAY in the caller.
            let _gate = crate::tier::gate::enter_file(&dst_file).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "tier: COPY destination excluded (evicting/hydrating)",
                )
            })?;

            // Step 10: BOTH ends consult the eviction marker — an
            // evicted source would copy stub bytes as if they were
            // data; an evicted destination is the C6 zero-publish
            // shape. DELAY until hydration (step 11).
            if crate::tier::evict::file_is_evicted(&src_file)
                || crate::tier::evict::file_is_evicted(&dst_file)
            {
                crate::tier::meter::bump(crate::tier::meter::Counter::EvictedOpDelays);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "tier: COPY endpoint evicted (awaiting hydration)",
                ));
            }

            // Resolves ca_count == 0 to "through EOF" and enforces the
            // source-range rule. Because the range is now known to lie
            // inside the source, the reads below cannot come up short, so
            // wr_count always equals the requested count.
            let src_size = src_file.metadata()?.len();
            let count = resolve_range_len(src_size, src_offset, count)?;

            // A10 admission with the RESOLVED length (ca_count == 0
            // means through-EOF, unknowable before this point).
            if crate::tier::space::admit_bytes(&dst_path, count).is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "tier: PVC headroom-minus-reserve exhausted",
                ));
            }

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
                    // The source shrank under us (the range was validated
                    // against its size a moment ago). Report the short
                    // count honestly rather than claiming the full copy.
                    break;
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

            // Unconditional, and NOT `if sync`. The reply hardcodes
            // wr_committed = FILE_SYNC4, so the durability claim was true
            // only when the client happened to ask for a synchronous copy.
            // Syncing always makes the claim true by construction, with no
            // new reply fields and no dependence on ca_synchronous — whose
            // value on the wire has never been measured (see
            // docs/plans/v42-copy-sparse-hardening.md, Phase 2).
            dst_file.sync_all()?;

            // COPY mutates the destination and must advance its change
            // attribute. `current()` returns max(stored, ctime floor), so
            // the omission was usually masked — except when a prior bump
            // on the same inode landed inside the same clock tick, which
            // pinned the reported value until some unrelated op moved it.
            bump_change_counter(&dst_file);
            // A2 dirty capture: the destination range now holds copied
            // bytes (short copies note the honest count).
            crate::tier::capture::note_file(
                &dst_file,
                crate::tier::capture::Mutation::Write {
                    offset: dst_offset,
                    len: total_copied,
                },
            );

            Ok::<u64, std::io::Error>(total_copied)
        }).await;

        match copy_result {
            Ok(Ok(bytes_copied)) => {
                debug!("COPY: Server-side copy completed: {} bytes from {:?} to {:?} (ZERO network transfer!)",
                      bytes_copied, src_path_name.as_deref().unwrap_or("unknown"), 
                      dst_path_name.as_deref().unwrap_or("unknown"));
                CopyRes {
                    status: Nfs4Status::Ok,
                    // TRUE unconditionally, because it is TRUE: the copy
                    // completed inside this call. This field used to echo
                    // the client's REQUEST (`op.sync`), which put two
                    // adjacent fields of one reply in contradiction —
                    // wr_callback_id is encoded as an empty array, meaning
                    // "nothing to wait for", while cr_synchronous=false
                    // claims an async copy the client should await. flint
                    // emits no CB_OFFLOAD and dispatches neither
                    // OFFLOAD_STATUS nor OFFLOAD_CANCEL, so there has never
                    // been an asynchronous copy to describe.
                    sync: true,
                    count: bytes_copied,
                    completion: CopyCompletion::Synchronous,
                }
            }
            Ok(Err(e)) => {
                warn!("COPY: I/O error during server-side copy: {}", e);
                // A10: errno first — a REAL ENOSPC/EDQUOT from the
                // copy loop must not collapse into EIO.
                let status = super::errno_status(&e).unwrap_or(match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    // Same-file and out-of-range rejections (RFC 7862
                    // §15.2.3), both raised above as InvalidInput.
                    std::io::ErrorKind::InvalidInput => Nfs4Status::Inval,
                    std::io::ErrorKind::StorageFull => Nfs4Status::NoSpc,
                    // A4 gate refusal: destination mid-evict/hydrate.
                    std::io::ErrorKind::WouldBlock => Nfs4Status::Delay,
                    _ => Nfs4Status::Io,
                });
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
        _ctx: &CompoundContext,
    ) -> CloneRes {
        debug!("CLONE: src_offset={}, dst_offset={}, count={}",
              op.src_offset, op.dst_offset, op.count);

        // Validate source stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.src_stateid) {
            warn!("CLONE: Invalid source stateid: {}", e);
            return CloneRes {
                status: Nfs4Status::BadStateId,
            };
        }

        // Validate destination stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.dst_stateid) {
            warn!("CLONE: Invalid destination stateid: {}", e);
            return CloneRes {
                status: Nfs4Status::BadStateId,
            };
        }

        // Get source and destination file handles from stateids
        let src_fh_data = match self.state_mgr.stateids.get_state(&op.src_stateid) {
            Some(state) => state.filehandle,
            None => {
                warn!("CLONE: Source stateid has no associated file handle");
                return CloneRes {
                    status: Nfs4Status::BadStateId,
                };
            }
        };

        let dst_fh_data = match self.state_mgr.stateids.get_state(&op.dst_stateid) {
            Some(state) => state.filehandle,
            None => {
                warn!("CLONE: Destination stateid has no associated file handle");
                return CloneRes {
                    status: Nfs4Status::BadStateId,
                };
            }
        };

        // Resolve file paths
        let src_fh = Nfs4FileHandle { data: src_fh_data.unwrap_or_default() };
        let dst_fh = Nfs4FileHandle { data: dst_fh_data.unwrap_or_default() };

        let src_path = match self.fh_mgr.resolve_handle(&src_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("CLONE: Failed to resolve source handle: {}", e);
                return CloneRes {
                    status: Nfs4Status::Stale,
                };
            }
        };

        let dst_path = match self.fh_mgr.resolve_handle(&dst_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("CLONE: Failed to resolve destination handle: {}", e);
                return CloneRes {
                    status: Nfs4Status::Stale,
                };
            }
        };

        // Same reasoning as COPY's guard above.
        for (role, path) in [("source", &src_path), ("destination", &dst_path)] {
            if self.is_striped(path) {
                warn!(
                    "⛔ CLONE {} '{}' is a striped file — its bytes live on the DSes and the MDS file is a sparse stub. NFS4ERR_NOTSUPP so the client falls back to read/write",
                    role,
                    path.display()
                );
                return CloneRes {
                    status: Nfs4Status::NotSupp,
                };
            }
        }

        let src_offset = op.src_offset;
        let dst_offset = op.dst_offset;
        let count = op.count;

        // ONE path for every range, including the whole-file case.
        //
        // What was here before: a `(0,0,0)` special case that called
        // FICLONE (destructive, see try_reflink_range) and, on failure,
        // std::fs::copy. std::fs::copy is whole-file — it truncates the
        // destination to the source's length and carries the source's
        // PERMISSION BITS across. Neither belongs to any NFS operation:
        // CLONE copies a byte range and says nothing about mode, owner,
        // or the bytes past the range.
        let clone_result = tokio::task::spawn_blocking(move || {
            let src_file = std::fs::File::open(&src_path)?;
            // truncate(false) is the whole fix stated as code: the old
            // whole-file path opened this destination with truncate(true)
            // BEFORE it knew whether the clone could succeed.
            let dst_file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&dst_path)?;

            let src_size = src_file.metadata()?.len();
            let len = resolve_range_len(src_size, src_offset, count)?;

            // RFC 7862 §15.13.3: "If SAVED_FH and CURRENT_FH refer to the
            // same file and the source and target ranges overlap, the
            // operation MUST fail with NFS4ERR_INVAL."
            //
            // Note this is WEAKER than COPY's rule, which forbids the same
            // file outright with no overlap qualifier. Two ops, two
            // sentences, deliberately not unified.
            if same_file(&src_file, &dst_file)?
                && src_offset < dst_offset.saturating_add(len)
                && dst_offset < src_offset.saturating_add(len)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "CLONE source and target ranges overlap within one file",
                ));
            }

            if len == 0 {
                return Ok::<u64, std::io::Error>(0);
            }

            // A10 admission with the RESOLVED length. The reflink
            // branch shares blocks and may need far less — refusing it
            // near-full is the deliberate over-refusal margin, not a
            // bug: an unsharing rewrite of a "free" clone later is
            // exactly the allocation the reserve exists to keep room
            // for.
            if crate::tier::space::admit_bytes(&dst_path, len).is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "tier: PVC headroom-minus-reserve exhausted",
                ));
            }

            // A4 write gate on the destination, spanning both the
            // reflink and copy-fallback branches with their capture
            // notes. Excluded maps to WouldBlock → NFS4ERR_DELAY.
            let _gate = crate::tier::gate::enter_file(&dst_file).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "tier: CLONE destination excluded (evicting/hydrating)",
                )
            })?;

            // Step 10: both ends consult the eviction marker (same
            // rationale as COPY).
            if crate::tier::evict::file_is_evicted(&src_file)
                || crate::tier::evict::file_is_evicted(&dst_file)
            {
                crate::tier::meter::bump(crate::tier::meter::Counter::EvictedOpDelays);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "tier: CLONE endpoint evicted (awaiting hydration)",
                ));
            }

            // Fast path. Failure here is free — nothing has been written.
            match try_reflink_range(&src_file, &dst_file, src_offset, len, dst_offset) {
                Ok(()) => {
                    debug!("CLONE: reflinked {} bytes (CoW, zero-copy)", len);
                    bump_change_counter(&dst_file);
                    // A2 dirty capture (reflink branch).
                    crate::tier::capture::note_file(
                        &dst_file,
                        crate::tier::capture::Mutation::Write { offset: dst_offset, len },
                    );
                    return Ok(len);
                }
                Err(e) => {
                    debug!("CLONE: reflink unavailable ({e}), copying the range instead");
                }
            }

            let copied = copy_range(&src_file, &dst_file, src_offset, dst_offset, len)?;
            bump_change_counter(&dst_file);
            // A2 dirty capture (copy fallback branch).
            crate::tier::capture::note_file(
                &dst_file,
                crate::tier::capture::Mutation::Write { offset: dst_offset, len: copied },
            );
            debug!(
                "CLONE: copied {} of {} bytes from offset {} to offset {}",
                copied, len, src_offset, dst_offset
            );
            Ok(copied)
        })
        .await;

        match clone_result {
            Ok(Ok(_bytes)) => {
                debug!("CLONE: Successfully cloned file");
                CloneRes {
                    status: Nfs4Status::Ok,
                }
            }
            Ok(Err(e)) => {
                warn!("CLONE: I/O error: {}", e);
                // A10: errno first — a REAL ENOSPC must not read as EIO.
                let status = super::errno_status(&e).unwrap_or(match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    // Out-of-range source and overlapping same-file ranges
                    // (RFC 7862 §15.13.3), both raised as InvalidInput.
                    std::io::ErrorKind::InvalidInput => Nfs4Status::Inval,
                    std::io::ErrorKind::StorageFull => Nfs4Status::NoSpc,
                    // A4 gate refusal: destination mid-evict/hydrate.
                    std::io::ErrorKind::WouldBlock => Nfs4Status::Delay,
                    _ => Nfs4Status::Io,
                });
                CloneRes {
                    status,
                }
            }
            Err(e) => {
                warn!("CLONE: Task spawn error: {}", e);
                CloneRes {
                    status: Nfs4Status::Io,
                }
            }
        }
    }

    /// Handle ALLOCATE operation
    ///
    /// Pre-allocate space without zeroing. Useful for thin-provisioned
    /// SPDK volumes to reserve space without actually writing.
    /// F15: this handler MUST really allocate. It shipped as a fake-OK
    /// stub, and the consequence was silent data corruption for any
    /// application that trusts posix_fallocate: PG16's bulk relation
    /// extend fallocates, the client extended its cached i_size on our
    /// fake OK, the backing file stayed at its old size, and the next
    /// server-refreshed size check collapsed the file back — postgres
    /// died with "unexpected data beyond EOF" on every pgbench load
    /// (phase-3 harness, runw). fallocate(mode=0) on the backing file is
    /// exactly posix_fallocate semantics: allocate and extend size.
    pub async fn handle_allocate(
        &self,
        op: AllocateOp,
        ctx: &CompoundContext,
    ) -> AllocateRes {
        debug!("ALLOCATE: offset={}, length={}", op.offset, op.length);

        // Validate stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("ALLOCATE: Invalid stateid: {}", e);
            return AllocateRes {
                status: Nfs4Status::BadStateId,
            };
        }

        let status = self
            .fallocate_current_fh(ctx, op.offset, op.length, AllocMode::Allocate)
            .await;
        AllocateRes { status }
    }

    /// Resolve the current filehandle and fallocate the backing file with
    /// `flags`. Shared by ALLOCATE (empty flags = allocate + extend size)
    /// and DEALLOCATE (PUNCH_HOLE|KEEP_SIZE). Bumps the change counter on
    /// success — allocation changes observable state (size / content).
    async fn fallocate_current_fh(
        &self,
        ctx: &CompoundContext,
        offset: u64,
        length: u64,
        mode: AllocMode,
    ) -> Nfs4Status {
        let Some(fh) = &ctx.current_fh else {
            return Nfs4Status::NoFileHandle;
        };
        let path = match self.fh_mgr.resolve_handle(fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("ALLOCATE/DEALLOCATE: unresolvable filehandle: {}", e);
                return Nfs4Status::Stale;
            }
        };
        if length == 0 {
            // RFC 7862: zero-length range is INVAL.
            return Nfs4Status::Inval;
        }
        // offset and length arrive as wire u64 and are cast to off_t (i64)
        // below. Anything above i64::MAX becomes NEGATIVE under that cast,
        // which fallocate would interpret as a range this server never
        // meant to name. Reject before the cast, not after.
        if offset > i64::MAX as u64
            || length > i64::MAX as u64
            || offset.checked_add(length).is_none_or(|end| end > i64::MAX as u64)
        {
            warn!(
                "ALLOCATE/DEALLOCATE: range [{}, +{}) is not representable as off_t → INVAL",
                offset, length
            );
            return Nfs4Status::Inval;
        }
        // A10 admission (Allocate only — a punch-hole FREES space and
        // must never be refused for fullness).
        if matches!(mode, AllocMode::Allocate)
            && crate::tier::space::admit_bytes(&path, length).is_err()
        {
            warn!("ALLOCATE: refused NOSPC — PVC headroom-minus-reserve exhausted");
            return Nfs4Status::NoSpc;
        }
        let p = path.clone();
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd;
                let flags = match mode {
                    AllocMode::Allocate => nix::fcntl::FallocateFlags::empty(),
                    AllocMode::PunchHole => {
                        nix::fcntl::FallocateFlags::FALLOC_FL_PUNCH_HOLE
                            | nix::fcntl::FallocateFlags::FALLOC_FL_KEEP_SIZE
                    }
                };
                let file = std::fs::OpenOptions::new().write(true).open(&p)?;
                // A4 write gate, spanning the fallocate AND its capture
                // note (which therefore lives in this closure, not at
                // the async caller). Excluded → WouldBlock → DELAY.
                let _gate = crate::tier::gate::enter_file(&file).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "tier: file excluded (evicting/hydrating)",
                    )
                })?;
                // Step 10: ALLOCATE/DEALLOCATE against the evicted
                // stub is not an operation on the data. DELAY.
                if crate::tier::evict::file_is_evicted(&file) {
                    crate::tier::meter::bump(crate::tier::meter::Counter::EvictedOpDelays);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "tier: file evicted (awaiting hydration)",
                    ));
                }
                nix::fcntl::fallocate(
                    file.as_raw_fd(),
                    flags,
                    offset as nix::libc::off_t,
                    length as nix::libc::off_t,
                )
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                // A2 dirty capture: ALLOCATE materializes defined bytes
                // (zeros in holes, size extension); DEALLOCATE punches
                // zeros in place. Both dirty the range; the Zero hint
                // distinguishes them for future flush optimization.
                crate::tier::capture::note_path(
                    &p,
                    match mode {
                        AllocMode::Allocate => crate::tier::capture::Mutation::Write {
                            offset,
                            len: length,
                        },
                        AllocMode::PunchHole => crate::tier::capture::Mutation::Zero {
                            offset,
                            len: length,
                        },
                    },
                );
                Ok(())
            }
            #[cfg(not(target_os = "linux"))]
            {
                // Non-Linux dev hosts: no fallocate — refuse honestly so
                // the client falls back to writing (never a fake OK).
                let _ = (&p, mode, offset, length);
                Err(std::io::Error::from_raw_os_error(nix::libc::EOPNOTSUPP))
            }
        })
        .await;
        match res {
            Ok(Ok(())) => {
                crate::nfs::v4::change_counter::bump_path(&path);
                Nfs4Status::Ok
            }
            // A4 gate refusal: the file is mid-evict/hydrate.
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => Nfs4Status::Delay,
            Ok(Err(e)) => {
                warn!("ALLOCATE/DEALLOCATE on {:?} failed: {}", path, e);
                match e.raw_os_error() {
                    Some(nix::libc::ENOSPC) => Nfs4Status::NoSpc,
                    Some(nix::libc::EISDIR) => Nfs4Status::IsDir,
                    Some(nix::libc::ENOENT) => Nfs4Status::NoEnt,
                    Some(nix::libc::EACCES) => Nfs4Status::Access,
                    Some(nix::libc::EOPNOTSUPP) => Nfs4Status::NotSupp,
                    _ => Nfs4Status::Io,
                }
            }
            Err(e) => {
                warn!("ALLOCATE/DEALLOCATE task join error: {}", e);
                Nfs4Status::Io
            }
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

        // Validate stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("DEALLOCATE: Invalid stateid: {}", e);
            return DeallocateRes {
                status: Nfs4Status::BadStateId,
            };
        }

        // F15: a fake-OK here means an unpunched hole — the client
        // believes the range now reads as zeros while the old data is
        // still there. Punch a real hole in the backing file.
        let status = self
            .fallocate_current_fh(ctx, op.offset, op.length, AllocMode::PunchHole)
            .await;
        DeallocateRes { status }
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

        // Validate stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("SEEK: Invalid stateid: {}", e);
            return SeekRes {
                status: Nfs4Status::BadStateId,
                eof: false,
                offset: 0,
            };
        }

        // F15 audit: the stub always answered "EOF at your offset",
        // which corrupts sparse-aware readers (cp --sparse, tar) by
        // truncating their view of the file. Real lseek on the backing
        // file: SEEK_DATA/SEEK_HOLE map 1:1; ENXIO = past-EOF → eof
        // with the file size per RFC 7862 §15.11.
        let fail = |status| SeekRes { status, eof: false, offset: 0 };
        let Some(fh) = &ctx.current_fh else {
            return fail(Nfs4Status::NoFileHandle);
        };
        let path = match self.fh_mgr.resolve_handle(fh) {
            Ok(p) => p,
            Err(_) => return fail(Nfs4Status::Stale),
        };
        let what = op.what;
        let start = op.offset;
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<(bool, u64)> {
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd;
                let file = std::fs::File::open(&path)?;
                let size = file.metadata()?.len();

                // RFC 7862 §15.11.3: "If the sa_offset is beyond the end of
                // the file, then SEEK MUST return NFS4ERR_NXIO."
                //
                // This has to be decided BEFORE lseek, because Linux
                // returns ENXIO for two different questions — "you are past
                // EOF" and "there is no more data before EOF" — and the RFC
                // gives them opposite answers. The old code collapsed both
                // into Ok(eof, size), so a genuinely out-of-range SEEK was
                // reported as success.
                if start > size {
                    return Err(std::io::Error::from_raw_os_error(nix::libc::ENXIO));
                }

                // A wire u64 above i64::MAX becomes a NEGATIVE off_t under
                // the cast below, which lseek would read as a rewind.
                if start > i64::MAX as u64 {
                    return Err(std::io::Error::from_raw_os_error(nix::libc::EINVAL));
                }

                let whence = match what {
                    SeekType::Data => nix::unistd::Whence::SeekData,
                    SeekType::Hole => nix::unistd::Whence::SeekHole,
                };
                match nix::unistd::lseek(file.as_raw_fd(), start as nix::libc::off_t, whence) {
                    // sr_eof is NOT "the operation finished". RFC 7862
                    // §15.11.3's own worked example is a dense file where
                    // {SEEK 0 CONTENT_HOLE} must answer {eof=1, offset=X}
                    // with X the file size, because "all files MUST have a
                    // virtual hole at the end of the file". eof was
                    // hardcoded false here, so it could never be true on a
                    // successful lseek and that example answered wrongly.
                    Ok(off) => {
                        let off = off as u64;
                        Ok((off >= size, off))
                    }
                    // Not past EOF (checked above), so this is the other
                    // question: no content of that type before EOF. §15.11.3
                    // — "If the server cannot find a corresponding sa_what,
                    // then the status will still be NFS4_OK, but sr_eof
                    // would be TRUE."
                    Err(nix::errno::Errno::ENXIO) => Ok((true, size)),
                    Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (&path, what, start);
                Err(std::io::Error::from_raw_os_error(nix::libc::EOPNOTSUPP))
            }
        })
        .await;
        match res {
            Ok(Ok((eof, offset))) => SeekRes { status: Nfs4Status::Ok, eof, offset },
            Ok(Err(e)) => {
                warn!("SEEK failed: {}", e);
                fail(match e.raw_os_error() {
                    Some(nix::libc::ENOENT) => Nfs4Status::NoEnt,
                    Some(nix::libc::EISDIR) => Nfs4Status::IsDir,
                    Some(nix::libc::EOPNOTSUPP) => Nfs4Status::NotSupp,
                    // RFC 7862 §15.11.3. Nfs4Status::NxIo has existed in
                    // protocol.rs since the beginning and had zero uses:
                    // this is the only operation that can produce it.
                    Some(nix::libc::ENXIO) => Nfs4Status::NxIo,
                    Some(nix::libc::EINVAL) => Nfs4Status::Inval,
                    _ => Nfs4Status::Io,
                })
            }
            Err(e) => {
                warn!("SEEK task join error: {}", e);
                fail(Nfs4Status::Io)
            }
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
        _ctx: &CompoundContext,
    ) -> ReadPlusRes {
        debug!("READ_PLUS: offset={}, count={}", op.offset, op.count);

        // Validate stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("READ_PLUS: Invalid stateid: {}", e);
            return ReadPlusRes {
                status: Nfs4Status::BadStateId,
                eof: false,
                segments: vec![],
            };
        }

        // F15 audit: the stub answered Ok+eof+no-segments — a claim that
        // EVERY file is empty. Any client that trusted it would read
        // zero bytes from real data. Until a real sparse-aware
        // implementation exists, refuse honestly: NOTSUPP makes the
        // kernel client fall back to plain READ (observed live — READ
        // is what the 4.2 client uses against this server).
        ReadPlusRes {
            status: Nfs4Status::NotSupp,
            eof: false,
            segments: vec![],
        }
    }

    /// Handle IO_ADVISE operation
    ///
    /// Process I/O hints for optimizing SPDK caching and read-ahead.
    pub async fn handle_io_advise(
        &self,
        op: IoAdviseOp,
        _ctx: &CompoundContext,
    ) -> IoAdviseRes {
        debug!("IO_ADVISE: offset={}, length={}", op.offset, op.length);

        // Validate stateid with relaxed checking (accept seqid=0)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
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

    /// A2 census (design review C5): COPY must note its destination
    /// range in the tier capture log.
    #[tokio::test]
    async fn copy_notes_tier_capture() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");
        let src_len = std::fs::metadata(&src_path).unwrap().len();
        // ext4 REUSES inode numbers: a dead test file's capture residue
        // can alias onto this identity (safe in production — pessimal
        // upload — but this test asserts EXACT capture state). Clear it.
        {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::metadata(&dst_path).unwrap();
            crate::tier::capture::forget(m.dev(), m.ino());
        }
        let res = handler
            .handle_copy(
                CopyOp {
                    src_stateid: open_stateid_for(&handler, &src_path),
                    dst_stateid: open_stateid_for(&handler, &dst_path),
                    src_offset: 0,
                    dst_offset: 512,
                    count: src_len,
                    sync: true,
                },
                &ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);
        let md = std::fs::metadata(&dst_path).unwrap();
        let cap = crate::tier::capture::snapshot(md.dev(), md.ino())
            .expect("COPY must note the tier capture (C5)");
        assert_eq!(cap.intervals, vec![(512, 512 + src_len)]);
    }

    /// A2 census (design review C5): CLONE must note its destination
    /// range — via whichever branch ran (reflink on reflink-capable
    /// filesystems, the copy fallback elsewhere, macOS included).
    #[tokio::test]
    async fn clone_notes_tier_capture() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");
        // ext4 REUSES inode numbers: a dead test file's capture residue
        // can alias onto this identity (safe in production — pessimal
        // upload — but this test asserts EXACT capture state). Clear it.
        {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::metadata(&dst_path).unwrap();
            crate::tier::capture::forget(m.dev(), m.ino());
        }
        let res = handler
            .handle_clone(
                CloneOp {
                    src_stateid: open_stateid_for(&handler, &src_path),
                    dst_stateid: open_stateid_for(&handler, &dst_path),
                    src_offset: 0,
                    dst_offset: 8,
                    count: 16,
                },
                &ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);
        let md = std::fs::metadata(&dst_path).unwrap();
        let cap = crate::tier::capture::snapshot(md.dev(), md.ino())
            .expect("CLONE must note the tier capture (C5)");
        assert_eq!(cap.intervals, vec![(8, 24)]);
    }

    /// A2 census (design review C5): ALLOCATE and DEALLOCATE must note
    /// their ranges. Linux-only like the ops themselves — the macOS
    /// suite is NOT the suite for these lanes (cross-build memory).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn allocate_and_deallocate_note_tier_capture() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let path = temp.path().join("source.txt");
        ctx.current_fh = Some(handler.fh_mgr.get_or_create_handle(&path).unwrap());
        // ext4 REUSES inode numbers: a dead test file's capture residue
        // can alias onto this identity (safe in production — pessimal
        // upload — but this test asserts EXACT capture state). Clear it.
        {
            let m = std::fs::metadata(&path).unwrap();
            crate::tier::capture::forget(m.dev(), m.ino());
        }
        let stateid = create_test_stateid(&handler, 1);
        let res = handler
            .handle_allocate(
                AllocateOp { stateid: stateid.clone(), offset: 0, length: 65536 },
                &ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);
        let md = std::fs::metadata(&path).unwrap();
        let cap = crate::tier::capture::snapshot(md.dev(), md.ino())
            .expect("ALLOCATE must note the tier capture (C5)");
        assert_eq!(cap.intervals, vec![(0, 65536)]);

        let res = handler
            .handle_deallocate(
                DeallocateOp { stateid, offset: 4096, length: 8192 },
                &ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);
        let cap = crate::tier::capture::snapshot(md.dev(), md.ino()).unwrap();
        // Already inside [0, 65536) — the union is unchanged, which is
        // itself the point: the range stays dirty.
        assert_eq!(cap.intervals, vec![(0, 65536)]);
    }

    fn create_test_handler() -> (PerfOperationHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        
        // Create test files for COPY/CLONE tests
        std::fs::write(export_path.join("source.txt"), b"source file data for copy/clone tests").unwrap();
        std::fs::write(export_path.join("dest.txt"), b"destination file").unwrap();
        
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = PerfOperationHandler::new(state_mgr, fh_mgr);
        (handler, temp_dir)
    }

    fn create_test_stateid(handler: &PerfOperationHandler, client_id: u64) -> StateId {
        handler.state_mgr.stateids.allocate(StateType::Open, client_id, None)
    }

    /// A pNFS handler that reports a fixed set of export-relative keys as
    /// placement-pinned. Only `is_pnfs_managed` matters to COPY/CLONE.
    struct PinnedKeys(std::collections::HashSet<String>);

    #[tonic::async_trait]
    impl crate::pnfs::PnfsOperations for PinnedKeys {
        fn layoutget(
            &self,
            _args: crate::pnfs::mds::operations::LayoutGetArgs,
        ) -> Result<
            crate::pnfs::mds::operations::LayoutGetResult,
            crate::pnfs::mds::operations::LayoutGetError,
        > {
            Err(crate::pnfs::mds::operations::LayoutGetError::LayoutUnavailable)
        }
        fn getdeviceinfo(
            &self,
            _args: crate::pnfs::mds::operations::GetDeviceInfoArgs,
        ) -> Result<
            crate::pnfs::mds::operations::GetDeviceInfoResult,
            crate::pnfs::mds::operations::GetDeviceInfoError,
        > {
            Err(crate::pnfs::mds::operations::GetDeviceInfoError::NoEnt)
        }
        fn layoutreturn(
            &self,
            _args: crate::pnfs::mds::operations::LayoutReturnArgs,
        ) -> Result<(), crate::pnfs::mds::operations::LayoutReturnError> {
            Ok(())
        }
        fn is_pnfs_managed(&self, file_key: &str) -> bool {
            self.0.contains(file_key)
        }
    }

    /// An MDS-role handler in whose export `pinned` are striped files.
    fn create_test_handler_pnfs(pinned: &[&str]) -> (PerfOperationHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        std::fs::write(export_path.join("source.txt"), b"source file data for copy/clone tests")
            .unwrap();
        std::fs::write(export_path.join("dest.txt"), b"destination file").unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let pnfs: Arc<dyn crate::pnfs::PnfsOperations> =
            Arc::new(PinnedKeys(pinned.iter().map(|s| s.to_string()).collect()));
        let handler = PerfOperationHandler::new_with_pnfs(state_mgr, fh_mgr, Some(pnfs));
        (handler, temp_dir)
    }

    /// Allocate an Open stateid bound to `path`'s filehandle.
    ///
    /// Every guard test needs a REAL stateid: all five perfops arms
    /// validate the stateid before touching a file, so a dummy one yields
    /// BadStateId both before and after the guard exists, and an
    /// `assert_ne!(status, Ok)` would pass either way.
    fn open_stateid_for(handler: &PerfOperationHandler, path: &Path) -> StateId {
        let fh = handler.fh_mgr.path_to_filehandle(path).unwrap();
        handler
            .state_mgr
            .stateids
            .allocate(StateType::Open, 1, Some(fh.data.clone()))
    }

    /// T2 — a COPY whose SOURCE is striped must refuse, and must not
    /// materialise a destination.
    ///
    /// The length assertion is the load-bearing half. Reading a striped
    /// file through the MDS returns the sparse stub's zeros, so a COPY
    /// that refused only *after* running would still report NOTSUPP while
    /// having written a megabyte of real zeros over the destination — and
    /// a status-only test would call that a pass.
    #[tokio::test]
    async fn copy_from_a_striped_file_is_refused_before_anything_is_written() {
        let (handler, _t) = create_test_handler_pnfs(&["source.txt"]);
        let ctx = CompoundContext::new(2);

        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");
        std::fs::write(&dst_path, b"").unwrap();

        let op = CopyOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: 1024 * 1024,
            sync: true,
        };

        let res = handler.handle_copy(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::NotSupp);
        assert_eq!(res.count, 0);
        assert_eq!(
            std::fs::metadata(&dst_path).unwrap().len(),
            0,
            "a refused COPY must not have written to the destination"
        );
    }

    /// The other end: a COPY whose DESTINATION is striped writes bytes the
    /// DSes will never serve. A guard that checked only the source would
    /// pass every other test in this file.
    #[tokio::test]
    async fn copy_to_a_striped_file_is_refused() {
        let (handler, _t) = create_test_handler_pnfs(&["dest.txt"]);
        let ctx = CompoundContext::new(2);
        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");

        let op = CopyOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: 16,
            sync: true,
        };
        assert_eq!(handler.handle_copy(op, &ctx).await.status, Nfs4Status::NotSupp);
    }

    /// T3 — THE TRAP ARM: the test that decides where the guard lives.
    ///
    /// The source is striped; `ctx.current_fh` names an unrelated,
    /// UNPINNED file. RFC 7862 §15.2 puts COPY's source in SAVED_FH, so a
    /// guard implemented in the dispatcher on `pnfs_current_fh_key` would
    /// consult the wrong file here and let the copy through — while every
    /// other COPY test in this file stayed green.
    ///
    /// Note the context is deliberately *hostile*, not merely absent: this
    /// also fails a guard that reads `current_fh` as a fallback.
    #[tokio::test]
    async fn the_copy_guard_does_not_read_the_current_filehandle() {
        let (handler, _t) = create_test_handler_pnfs(&["source.txt"]);
        let export = handler.fh_mgr.get_export_path().to_path_buf();
        let src_path = export.join("source.txt");
        let dst_path = export.join("dest.txt");

        let bystander = export.join("unpinned-bystander.txt");
        std::fs::write(&bystander, b"not striped, not involved").unwrap();

        let mut ctx = CompoundContext::new(2);
        ctx.current_fh = Some(handler.fh_mgr.path_to_filehandle(&bystander).unwrap());
        ctx.saved_fh = Some(handler.fh_mgr.path_to_filehandle(&src_path).unwrap());

        let op = CopyOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: 16,
            sync: true,
        };
        assert_eq!(
            handler.handle_copy(op, &ctx).await.status,
            Nfs4Status::NotSupp,
            "the COPY guard must key on the stateid-resolved source, not on current_fh"
        );
    }

    /// A file that was never layouted stays fully copyable on an MDS.
    /// Without this, a guard that simply refused COPY whenever a pNFS
    /// handler is present would pass every test above.
    #[tokio::test]
    async fn copy_between_unpinned_files_still_works_on_an_mds() {
        let (handler, _t) = create_test_handler_pnfs(&["something-else.txt"]);
        let ctx = CompoundContext::new(2);
        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");
        let src_len = std::fs::metadata(&src_path).unwrap().len();

        let op = CopyOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: src_len,
            sync: true,
        };
        let res = handler.handle_copy(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.count, src_len);
    }

    /// CLONE gets the same two-ended guard.
    #[tokio::test]
    async fn clone_refuses_either_end_being_striped() {
        for pinned in [&["source.txt"][..], &["dest.txt"][..]] {
            let (handler, _t) = create_test_handler_pnfs(pinned);
            let ctx = CompoundContext::new(2);
            let src_path = handler.fh_mgr.get_export_path().join("source.txt");
            let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");

            let op = CloneOp {
                src_stateid: open_stateid_for(&handler, &src_path),
                dst_stateid: open_stateid_for(&handler, &dst_path),
                src_offset: 0,
                dst_offset: 0,
                count: 0,
            };
            assert_eq!(
                handler.handle_clone(op, &ctx).await.status,
                Nfs4Status::NotSupp,
                "CLONE must refuse when {:?} is striped",
                pinned
            );
        }
    }

    /// T7 — THE DATA-DESTRUCTION REGRESSION. Fails against the old code
    /// with no mutation applied; that is the point of it.
    ///
    /// The old whole-file path opened the destination `.truncate(true)`
    /// BEFORE the FICLONE ioctl, so on any filesystem without reflink
    /// support the destination was emptied and then rebuilt by
    /// `std::fs::copy`. Here the rebuild is made to fail (the destination
    /// is read-only), which is the ENOSPC/EACCES shape: the client is told
    /// CLONE failed, and under the old code the file was already gone.
    ///
    /// No content-equality test on the SUCCESS path can see this. The
    /// fault injection is the instrument.
    #[tokio::test]
    async fn a_failed_clone_leaves_the_destination_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let (handler, _t) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let export = handler.fh_mgr.get_export_path().to_path_buf();
        let src_path = export.join("source.txt");
        let dst_path = export.join("dest.txt");

        let pre_fill: Vec<u8> = (0..4096u32).map(|i| (i % 251 + 1) as u8).collect();
        std::fs::write(&dst_path, &pre_fill).unwrap();

        // Make the destination unwritable so both the reflink attempt and
        // the byte-range fallback fail.
        let mut perms = std::fs::metadata(&dst_path).unwrap().permissions();
        perms.set_mode(0o400);
        std::fs::set_permissions(&dst_path, perms).unwrap();

        let op = CloneOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: 0,
        };
        let res = handler.handle_clone(op, &ctx).await;
        assert_ne!(res.status, Nfs4Status::Ok, "the clone was supposed to fail");

        let mut perms = std::fs::metadata(&dst_path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&dst_path, perms).unwrap();

        assert_eq!(
            std::fs::read(&dst_path).unwrap(),
            pre_fill,
            "a CLONE that reports failure must not have altered the destination"
        );
    }

    /// T8 — a whole-file CLONE is a byte-range clone of [0, src_len), not
    /// a file replacement. It must not shorten a longer destination and
    /// must not carry the source's permission bits.
    ///
    /// Restoring the old `(0,0,0) => std::fs::copy` path fails this on
    /// both counts. A test asserting only `dst[..src_len] == src` passes
    /// against that bug.
    #[tokio::test]
    async fn a_whole_file_clone_preserves_the_destination_tail_and_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (handler, _t) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let export = handler.fh_mgr.get_export_path().to_path_buf();
        let src_path = export.join("source.txt");
        let dst_path = export.join("dest.txt");

        let src_bytes = std::fs::read(&src_path).unwrap();
        let tail = b"TAIL-THAT-MUST-SURVIVE".to_vec();
        let mut dst_bytes = vec![b'x'; src_bytes.len()];
        dst_bytes.extend_from_slice(&tail);
        std::fs::write(&dst_path, &dst_bytes).unwrap();

        std::fs::set_permissions(&src_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&dst_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let op = CloneOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: 0,
        };
        assert_eq!(handler.handle_clone(op, &ctx).await.status, Nfs4Status::Ok);

        let after = std::fs::read(&dst_path).unwrap();
        assert_eq!(&after[..src_bytes.len()], &src_bytes[..], "cloned range");
        assert_eq!(
            after.len(),
            dst_bytes.len(),
            "a byte-range CLONE must not truncate the destination"
        );
        assert_eq!(&after[src_bytes.len()..], &tail[..], "destination tail");
        assert_eq!(
            std::fs::metadata(&dst_path).unwrap().permissions().mode() & 0o777,
            0o600,
            "CLONE must not carry the source's permission bits"
        );
    }

    /// A CLONE starting past the source's EOF is NFS4ERR_INVAL, not a
    /// wrapped length.
    ///
    /// The old code computed `len() - src_offset` on u64. The workspace
    /// has no `[profile]` section, so in release that wrapped to ~16 EiB
    /// and the loop ran until the source read returned 0.
    #[tokio::test]
    async fn a_clone_starting_past_eof_is_invalid_not_a_wrapped_length() {
        let (handler, _t) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let export = handler.fh_mgr.get_export_path().to_path_buf();
        let src_path = export.join("source.txt");
        let dst_path = export.join("dest.txt");
        let src_len = std::fs::metadata(&src_path).unwrap().len();

        let op = CloneOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: src_len + 1,
            dst_offset: 0,
            count: 0, // "to source EOF" — the arm that used to underflow
        };
        assert_eq!(handler.handle_clone(op, &ctx).await.status, Nfs4Status::Inval);
    }

    /// The paired success case. Without it, the INVAL test above is
    /// satisfied by a handler that refuses every CLONE.
    #[tokio::test]
    async fn a_clone_of_a_range_inside_the_source_copies_exactly_that_range() {
        let (handler, _t) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let export = handler.fh_mgr.get_export_path().to_path_buf();
        let src_path = export.join("source.txt");
        let dst_path = export.join("dest.txt");

        std::fs::write(&src_path, b"0123456789abcdef").unwrap();
        std::fs::write(&dst_path, vec![b'.'; 32]).unwrap();

        let op = CloneOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 4,
            dst_offset: 8,
            count: 6,
        };
        assert_eq!(handler.handle_clone(op, &ctx).await.status, Nfs4Status::Ok);

        let after = std::fs::read(&dst_path).unwrap();
        assert_eq!(&after[8..14], b"456789", "the cloned range");
        assert_eq!(&after[..8], &[b'.'; 8], "bytes before the range");
        assert_eq!(&after[14..], &[b'.'; 18], "bytes after the range");
    }

    /// `resolve_range_len` is the single reading of `count == 0` that
    /// replaced two contradictory ones. Pinned directly because it is now
    /// the only thing standing between a wire u64 and a length.
    #[test]
    fn resolve_range_len_reads_zero_as_to_eof_and_rejects_past_eof() {
        assert_eq!(resolve_range_len(100, 0, 0).unwrap(), 100);
        assert_eq!(resolve_range_len(100, 40, 0).unwrap(), 60);
        assert_eq!(resolve_range_len(100, 40, 10).unwrap(), 10);
        // Exactly at EOF is a legal empty range, not an error.
        assert_eq!(resolve_range_len(100, 100, 0).unwrap(), 0);
        // One past is where the old subtraction wrapped.
        assert_eq!(
            resolve_range_len(100, 101, 0).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            resolve_range_len(0, u64::MAX, 0).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    // Both of these tests used to request 1024 bytes from a 37-byte
    // source and assert Ok, which pinned the very bug RFC 7862 §15.2.3
    // and §15.13.3 name ("the source offset plus count is greater than
    // the size of the source file ... MUST fail with NFS4ERR_INVAL").
    // They also set current_fh = SOURCE and saved_fh = DESTINATION,
    // backwards from the RFC, and passed only because neither handler
    // reads the context — which is exactly the template that would lead
    // someone to build a guard around the wrong filehandle. Both are
    // corrected here, with post-conditions that a stubbed-out copy loop
    // cannot satisfy.

    #[tokio::test]
    async fn test_copy() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(2);

        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");

        let src_fh = handler.fh_mgr.path_to_filehandle(&src_path).unwrap();
        let dst_fh = handler.fh_mgr.path_to_filehandle(&dst_path).unwrap();

        // RFC 7862 §15.2: SAVED_FH is the source, CURRENT_FH the target.
        ctx.saved_fh = Some(src_fh.clone());
        ctx.current_fh = Some(dst_fh.clone());

        let src_bytes = std::fs::read(&src_path).unwrap();
        let src_len = src_bytes.len() as u64;

        let op = CopyOp {
            src_stateid: handler.state_mgr.stateids.allocate(
                StateType::Open, 1, Some(src_fh.data.clone())),
            dst_stateid: handler.state_mgr.stateids.allocate(
                StateType::Open, 1, Some(dst_fh.data.clone())),
            src_offset: 0,
            dst_offset: 0,
            count: src_len,
            // Request an ASYNC copy and expect the reply to say
            // synchronous: the field reports what the server did.
            sync: false,
        };

        let res = handler.handle_copy(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.count, src_len, "wr_count must be the full range");
        assert!(res.sync, "cr_synchronous states what the server did, not what was asked");
        assert_eq!(
            &std::fs::read(&dst_path).unwrap()[..src_len as usize],
            &src_bytes[..],
            "the destination must actually hold the source bytes"
        );
    }

    /// A COPY may not name one file twice — RFC 7862 §15.2.3: "SAVED_FH
    /// and CURRENT_FH must be different files."
    ///
    /// It is also the corruption case: the chunk loop is a memcpy where a
    /// same-file copy needs a memmove, so an overlapping request would
    /// shred the file and report Ok. The byte assertion is what stops a
    /// handler that returns INVAL only *after* doing the damage.
    #[tokio::test]
    async fn copy_within_one_file_is_refused_and_changes_nothing() {
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let path = handler.fh_mgr.get_export_path().join("selfcopy.bin");
        let original: Vec<u8> = (0..(3 * 1024 * 1024u32)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &original).unwrap();

        let op = CopyOp {
            src_stateid: open_stateid_for(&handler, &path),
            dst_stateid: open_stateid_for(&handler, &path),
            src_offset: 0,
            dst_offset: 512 * 1024,
            count: 2 * 1024 * 1024,
            sync: true,
        };

        let res = handler.handle_copy(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Inval);
        assert_eq!(res.count, 0);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "a refused same-file COPY must not have moved a byte"
        );
    }

    /// CLONE's same-file rule is WEAKER than COPY's: RFC 7862 §15.13.3
    /// forbids it only when the ranges overlap. Both arms asserted so the
    /// two rules cannot be quietly unified in either direction.
    #[tokio::test]
    async fn clone_within_one_file_is_refused_only_when_ranges_overlap() {
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let path = handler.fh_mgr.get_export_path().join("selfclone.bin");
        std::fs::write(&path, vec![b'a'; 4096]).unwrap();

        let mk = |src_offset, dst_offset, count| CloneOp {
            src_stateid: open_stateid_for(&handler, &path),
            dst_stateid: open_stateid_for(&handler, &path),
            src_offset,
            dst_offset,
            count,
        };

        // [0,1024) into [512,1536) — overlapping.
        assert_eq!(
            handler.handle_clone(mk(0, 512, 1024), &ctx).await.status,
            Nfs4Status::Inval
        );
        // [0,1024) into [2048,3072) — same file, disjoint: legal.
        assert_eq!(
            handler.handle_clone(mk(0, 2048, 1024), &ctx).await.status,
            Nfs4Status::Ok
        );
    }

    #[tokio::test]
    async fn test_clone() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(2);

        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");

        let src_fh = handler.fh_mgr.path_to_filehandle(&src_path).unwrap();
        let dst_fh = handler.fh_mgr.path_to_filehandle(&dst_path).unwrap();

        // RFC 7862 §15.13: SAVED_FH is the source, CURRENT_FH the target.
        ctx.saved_fh = Some(src_fh.clone());
        ctx.current_fh = Some(dst_fh.clone());

        let src_bytes = std::fs::read(&src_path).unwrap();

        let op = CloneOp {
            src_stateid: handler.state_mgr.stateids.allocate(
                StateType::Open, 1, Some(src_fh.data.clone())),
            dst_stateid: handler.state_mgr.stateids.allocate(
                StateType::Open, 1, Some(dst_fh.data.clone())),
            src_offset: 0,
            dst_offset: 0,
            count: src_bytes.len() as u64,
        };

        let res = handler.handle_clone(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(
            &std::fs::read(&dst_path).unwrap()[..src_bytes.len()],
            &src_bytes[..]
        );
    }

    /// A COPY range that runs past the end of the source is INVAL, not a
    /// short copy reported as success.
    #[tokio::test]
    async fn a_copy_past_the_end_of_the_source_is_invalid() {
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");
        let src_len = std::fs::metadata(&src_path).unwrap().len();

        let op = CopyOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: src_len + 1,
            sync: true,
        };
        assert_eq!(handler.handle_copy(op, &ctx).await.status, Nfs4Status::Inval);
    }

    /// SEEK's three RFC 7862 §15.11.3 rules, which the old code got wrong
    /// in three different ways.
    ///
    /// Linux-gated: `lseek(SEEK_DATA/SEEK_HOLE)` exists nowhere else, and
    /// every arm here would answer NOTSUPP on darwin — passing or failing
    /// for reasons that have nothing to do with the code under test.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn seek_reports_eof_and_distinguishes_past_eof_from_no_more_data() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(2);
        let path = handler.fh_mgr.get_export_path().join("sparse.bin");

        // A LEADING hole, so SEEK_DATA(0) has a non-zero answer — on a fully
        // dense file `SEEK_DATA(0) == 0` is also what a hardcoded
        // `Ok((false, start))` returns, and the test could not tell them apart.
        //
        // AND A DENSE TAIL, which assertion 2 below depends on. This used to
        // write 4 bytes at 64 KiB and leave the rest of the set_len() range a
        // hole, which made assertion 2 simply wrong: with a real hole running
        // to EOF, `SEEK_HOLE(size-1)` correctly answers `size-1` — the offset
        // is ALREADY inside a hole — not `size`. Verified against the kernel
        // on ext4: sparse tail -> 131071, dense tail -> 131072. The "virtual
        // hole at the end of the file" that RFC 7862 §15.11.3 guarantees is
        // only observable when the last byte is DATA.
        //
        // The mistake survived because this whole test is
        // #[cfg(target_os = "linux")] and never compiled (a FnOnce closure
        // called three times), so its premise was never once executed.
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(128 * 1024).unwrap();
            f.write_at(&vec![b'x'; 64 * 1024], 64 * 1024).unwrap();
            f.sync_all().unwrap();
        }
        let size = std::fs::metadata(&path).unwrap().len();
        ctx.current_fh = Some(handler.fh_mgr.path_to_filehandle(&path).unwrap());
        let stateid = open_stateid_for(&handler, &path);

        // Capture BORROWS, not the values. `async move` moves whatever the
        // closure captured, so capturing `handler`/`ctx` by value made this
        // closure FnOnce — and it is called three times below, which does
        // not compile. It went unnoticed because the whole test is
        // #[cfg(target_os = "linux")] and the suite is normally run on
        // macOS, where it is compiled out entirely. `&T` is Copy, so the
        // async block can move the references as many times as it likes.
        let handler_ref = &handler;
        let ctx_ref = &ctx;
        let seek = |what, offset| {
            let sid = stateid.clone();
            async move {
                handler_ref.handle_seek(SeekOp { stateid: sid, offset, what }, ctx_ref).await
            }
        };

        // 1. Real data found before EOF → eof FALSE, at the leading hole's end.
        let r = seek(SeekType::Data, 0).await;
        assert_eq!(r.status, Nfs4Status::Ok);
        assert!(!r.eof, "data exists before EOF, so sr_eof must be FALSE");
        assert!(r.offset > 0 && r.offset <= 64 * 1024, "offset={}", r.offset);

        // 2. "All files MUST have a virtual hole at the end of the file."
        //    Asking for a HOLE at the last byte lands at EOF → eof TRUE.
        //    This is the case a hardcoded `eof = false` could never answer.
        let r = seek(SeekType::Hole, size - 1).await;
        assert_eq!(r.status, Nfs4Status::Ok);
        assert!(r.eof, "a hole search reaching EOF must set sr_eof");
        assert_eq!(r.offset, size);

        // 3. "If the sa_offset is beyond the end of the file, then SEEK
        //    MUST return NFS4ERR_NXIO." The old code answered Ok(eof, size)
        //    here — success for an out-of-range request.
        let r = seek(SeekType::Data, size + 1).await;
        assert_eq!(r.status, Nfs4Status::NxIo, "past EOF is NXIO, not OK");
    }

    /// A range that cannot be represented as `off_t` is rejected BEFORE
    /// the cast, not handed to fallocate as a negative offset.
    ///
    /// Runs on every platform: the guard sits ahead of the Linux-only
    /// block, so it is one of the few space-op assertions that is not
    /// vacuous on darwin. `Inval` — not the `NotSupp` a non-Linux host
    /// answers for a well-formed request — is what makes that meaningful.
    #[tokio::test]
    async fn an_allocate_range_outside_off_t_is_rejected_before_the_cast() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(2);
        let path = handler.fh_mgr.get_export_path().join("alloc.bin");
        std::fs::write(&path, b"x").unwrap();
        ctx.current_fh = Some(handler.fh_mgr.path_to_filehandle(&path).unwrap());
        let stateid = open_stateid_for(&handler, &path);

        let over = i64::MAX as u64 + 1;
        for (offset, length) in [(over, 4096u64), (0, over), (i64::MAX as u64 - 1, 4096)] {
            let res = handler
                .handle_allocate(AllocateOp { stateid: stateid.clone(), offset, length }, &ctx)
                .await;
            assert_eq!(
                res.status,
                Nfs4Status::Inval,
                "offset={offset} length={length} must be refused as unrepresentable"
            );
        }
    }

    /// COPY must advance the destination's change attribute.
    ///
    /// The stored counter is seeded far beyond any real ctime and the
    /// floor is held fixed, so the assertion cannot be satisfied by the
    /// clock ticking during the test — which is how the naive version
    /// ("write, copy, assert it moved") passes against a server that
    /// never bumps at all.
    #[tokio::test]
    async fn copy_advances_the_destination_change_attribute() {
        use std::os::unix::fs::MetadataExt;
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(2);
        let src_path = handler.fh_mgr.get_export_path().join("source.txt");
        let dst_path = handler.fh_mgr.get_export_path().join("dest.txt");

        let md = std::fs::metadata(&dst_path).unwrap();
        let (dev, ino) = (md.dev(), md.ino());
        let far_future = crate::nfs::v4::change_counter::ctime_ns(&md) + 1_000_000_000_000;
        crate::nfs::v4::change_counter::bump(dev, ino, far_future);
        let before = crate::nfs::v4::change_counter::current(dev, ino, far_future);

        let src_len = std::fs::metadata(&src_path).unwrap().len();
        let op = CopyOp {
            src_stateid: open_stateid_for(&handler, &src_path),
            dst_stateid: open_stateid_for(&handler, &dst_path),
            src_offset: 0,
            dst_offset: 0,
            count: src_len,
            sync: true,
        };
        assert_eq!(handler.handle_copy(op, &ctx).await.status, Nfs4Status::Ok);

        let after = crate::nfs::v4::change_counter::current(dev, ino, far_future);
        assert!(
            after > before,
            "COPY must bump the change counter (before={before}, after={after})"
        );
    }

    #[tokio::test]
    async fn test_allocate_without_fh_refuses() {
        // The F15 stub said Ok while allocating nothing. The contract now:
        // no current filehandle → NoFileHandle, never a fake success.
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(0);
        let stateid = create_test_stateid(&handler, 1);
        let op = AllocateOp { stateid, offset: 0, length: 1024 * 1024 };
        let res = handler.handle_allocate(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::NoFileHandle);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_allocate_really_extends() {
        let (handler, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let path = temp.path().join("source.txt");
        ctx.current_fh = Some(handler.fh_mgr.get_or_create_handle(&path).unwrap());
        let stateid = create_test_stateid(&handler, 1);
        let op = AllocateOp { stateid, offset: 0, length: 1024 * 1024 };
        let res = handler.handle_allocate(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        // posix_fallocate semantics: the file size is now >= the range end.
        assert!(std::fs::metadata(&path).unwrap().len() >= 1024 * 1024);
    }

    #[tokio::test]
    async fn test_deallocate_without_fh_refuses() {
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(0);
        let stateid = create_test_stateid(&handler, 1);
        let op = DeallocateOp { stateid, offset: 1024 * 1024, length: 512 * 1024 };
        let res = handler.handle_deallocate(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::NoFileHandle);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_deallocate_keeps_size() {
        let (handler, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let path = temp.path().join("source.txt");
        let orig = std::fs::metadata(&path).unwrap().len();
        ctx.current_fh = Some(handler.fh_mgr.get_or_create_handle(&path).unwrap());
        let stateid = create_test_stateid(&handler, 1);
        let op = DeallocateOp { stateid, offset: 0, length: 4096 };
        let res = handler.handle_deallocate(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), orig, "PUNCH_HOLE must keep size");
    }

    #[tokio::test]
    async fn test_seek_without_fh_refuses() {
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(0);
        let stateid = create_test_stateid(&handler, 1);
        let op = SeekOp { stateid, offset: 0, what: SeekType::Data };
        let res = handler.handle_seek(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::NoFileHandle);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_seek_data_and_hole_real() {
        let (handler, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let path = temp.path().join("source.txt");
        let len = std::fs::metadata(&path).unwrap().len();
        ctx.current_fh = Some(handler.fh_mgr.get_or_create_handle(&path).unwrap());
        let res = handler
            .handle_seek(
                SeekOp { stateid: create_test_stateid(&handler, 1), offset: 0, what: SeekType::Data },
                &ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(res.offset, 0, "data starts at 0 in a dense file");
        let res = handler
            .handle_seek(
                SeekOp { stateid: create_test_stateid(&handler, 1), offset: 0, what: SeekType::Hole },
                &ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.offset >= len, "implicit hole at EOF");
    }

    #[tokio::test]
    async fn test_read_plus_is_notsupp() {
        // The stub claimed Ok+eof+no-segments — "every file is empty".
        // Until a real sparse-aware implementation exists the honest
        // answer is NOTSUPP (the kernel client falls back to READ).
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(0);
        let stateid = create_test_stateid(&handler, 1);
        let op = ReadPlusOp { stateid, offset: 0, count: 4096 };
        let res = handler.handle_read_plus(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::NotSupp);
    }

    #[tokio::test]
    async fn test_io_advise() {
        let (handler, _temp) = create_test_handler();
        let ctx = CompoundContext::new(0);

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
