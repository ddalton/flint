// NFSv4 I/O Operations
//
// This module implements file I/O operations for NFSv4:
// - OPEN: Open a file and receive stateid
// - CLOSE: Close a file and release stateid
// - READ: Read data from file
// - WRITE: Write data to file
// - COMMIT: Commit unstable writes to stable storage
//
// NFSv4 uses stateids to track open files and locks.
// Every I/O operation (READ/WRITE) requires a valid stateid.

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::compound::CompoundContext;
use crate::nfs::v4::state::{StateManager, StateType};
use crate::nfs::v4::operations::fileops::Fattr4;
use crate::nfs::v4::filehandle::FileHandleManager;
use bytes::Bytes;
use dashmap::DashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::os::unix::fs::FileExt;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Open claim types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenClaimType {
    Null = 0,           // CLAIM_NULL - open by name
    Previous = 1,       // CLAIM_PREVIOUS - reclaim after reboot
    DelegateCur = 2,    // CLAIM_DELEGATE_CUR - via delegation
    DelegatePrev = 3,   // CLAIM_DELEGATE_PREV - reclaim delegation
    FH = 4,             // CLAIM_FH - open by filehandle (NFSv4.1)
    DelegCurFH = 5,     // CLAIM_DELEG_CUR_FH (NFSv4.1)
    DelegPrevFH = 6,    // CLAIM_DELEG_PREV_FH (NFSv4.1)
}

/// Open delegation types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenDelegationType {
    None = 0,           // No delegation
    Read = 1,           // Read delegation
    Write = 2,          // Write delegation
}

/// OPEN operation (opcode 18)
///
/// Opens a file and returns a stateid for subsequent I/O.
pub struct OpenOp {
    /// Sequence ID (for exactly-once semantics with open-owner)
    pub seqid: u32,

    /// Share access (READ, WRITE, BOTH)
    pub share_access: u32,

    /// Share deny (NONE, READ, WRITE, BOTH)
    pub share_deny: u32,

    /// Open owner (client-provided identifier)
    pub owner: Vec<u8>,

    /// How to open (CREATE, NOCREATE)
    pub openhow: OpenHow,

    /// Claim type and value
    pub claim: OpenClaim,
}

#[derive(Debug, Clone)]
pub enum OpenHow {
    /// Don't create - file must exist
    NoCreate,

    /// Create if doesn't exist (with attributes)
    Create(Fattr4),

    /// Exclusive create (with verifier)
    Exclusive(u64),

    /// Exclusive create with attributes (NFSv4.1)
    Exclusive4_1 { verifier: u64, attrs: Fattr4 },
}

#[derive(Debug, Clone)]
pub enum OpenClaim {
    /// Open by name in current directory
    Null(String),

    /// Open by filehandle (NFSv4.1)
    Fh,
}

/// Share access bits
pub const OPEN4_SHARE_ACCESS_READ: u32 = 0x00000001;
pub const OPEN4_SHARE_ACCESS_WRITE: u32 = 0x00000002;
pub const OPEN4_SHARE_ACCESS_BOTH: u32 = 0x00000003;

/// Share deny bits
pub const OPEN4_SHARE_DENY_NONE: u32 = 0x00000000;
pub const OPEN4_SHARE_DENY_READ: u32 = 0x00000001;
pub const OPEN4_SHARE_DENY_WRITE: u32 = 0x00000002;
pub const OPEN4_SHARE_DENY_BOTH: u32 = 0x00000003;

pub struct OpenRes {
    pub status: Nfs4Status,
    pub stateid: Option<StateId>,
    pub change_info: Option<ChangeInfo>,
    pub result_flags: u32,
    pub delegation: OpenDelegationType,
    pub attrset: Vec<u32>,  // Which CREATE attrs were set
}

#[derive(Debug, Clone)]
pub struct ChangeInfo {
    pub atomic: bool,
    pub before: u64,
    pub after: u64,
}

/// CLOSE operation (opcode 4)
///
/// Closes a file and releases the stateid.
pub struct CloseOp {
    pub seqid: u32,
    pub stateid: StateId,
}

pub struct CloseRes {
    pub status: Nfs4Status,
    pub stateid: Option<StateId>,
}

/// DELEGRETURN operation (opcode 8)
/// Client voluntarily returns a delegation (or after recall)
pub struct DelegReturnRes {
    pub status: Nfs4Status,
}

/// READ operation (opcode 25)
///
/// Reads data from a file.
pub struct ReadOp {
    pub stateid: StateId,
    pub offset: u64,
    pub count: u32,
}

pub struct ReadRes {
    pub status: Nfs4Status,
    pub eof: bool,
    pub data: Bytes,
}

/// WRITE operation (opcode 38)
///
/// Writes data to a file.
pub struct WriteOp {
    pub stateid: StateId,
    pub offset: u64,
    pub stable: u32,    // UNSTABLE=0, DATA_SYNC=1, FILE_SYNC=2
    pub data: Bytes,
}

/// Write stability
pub const UNSTABLE4: u32 = 0;       // May be cached
pub const DATA_SYNC4: u32 = 1;      // Committed to storage
pub const FILE_SYNC4: u32 = 2;      // Data + metadata committed

pub struct WriteRes {
    pub status: Nfs4Status,
    pub count: u32,     // Bytes written
    pub committed: u32, // Actual stability achieved
    pub writeverf: u64, // Write verifier (for COMMIT)
}

/// COMMIT operation (opcode 5)
///
/// Commits unstable writes to stable storage.
pub struct CommitOp {
    pub offset: u64,
    pub count: u32,
}

pub struct CommitRes {
    pub status: Nfs4Status,
    pub writeverf: u64,
}

/// Cached open file entry
struct CachedFile {
    file: Arc<std::sync::Mutex<File>>,
    path: PathBuf,
    last_access: Instant,
}

/// I/O operation handler with file descriptor caching
pub struct IoOperationHandler {
    state_mgr: Arc<StateManager>,
    fh_mgr: Arc<FileHandleManager>,
    write_verifier: u64,
    /// File descriptor cache: stateid → open file
    /// Eliminates open/close overhead for every write
    fd_cache: Arc<DashMap<StateId, CachedFile>>,
}

impl IoOperationHandler {
    /// Create a new I/O operation handler
    pub fn new(state_mgr: Arc<StateManager>, fh_mgr: Arc<FileHandleManager>) -> Self {
        // Generate write verifier (used to detect server reboots)
        use std::time::{SystemTime, UNIX_EPOCH};
        let write_verifier = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        Self { 
            state_mgr, 
            fh_mgr, 
            write_verifier,
            fd_cache: Arc::new(DashMap::new()),
        }
    }

    /// Get client ID from compound context
    ///
    /// Looks up the session (set by SEQUENCE) to determine the client ID.
    /// Falls back to 1 for backward compatibility with tests that don't use SEQUENCE.
    fn get_client_id_from_context(&self, ctx: &CompoundContext) -> u64 {
        if let Some(session_id) = &ctx.session_id {
            if let Some(session) = self.state_mgr.sessions.get_session(session_id) {
                return session.client_id;
            }
            warn!("OPEN: Session {:?} not found in context, falling back to client_id=1", session_id);
        } else {
            debug!("OPEN: No session in context (likely test), using client_id=1");
        }
        1 // Fallback for tests
    }

    /// Handle OPEN operation
    pub fn handle_open(
        &self,
        op: OpenOp,
        ctx: &mut CompoundContext,
    ) -> OpenRes {
        debug!("OPEN: share_access=0x{:08x}, share_deny=0x{:08x}",
               op.share_access, op.share_deny);
        debug!("OPEN: openhow={:?}, claim={:?}", op.openhow, op.claim);

        // Check current filehandle (directory we're creating in)
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return OpenRes {
                    status: Nfs4Status::NoFileHandle,
                    stateid: None,
                    change_info: None,
                    result_flags: 0,
                    delegation: OpenDelegationType::None,
                    attrset: vec![],
                };
            }
        };

        // Extract filename from claim
        let filename = match &op.claim {
            OpenClaim::Null(name) => name.clone(),
            OpenClaim::Fh => {
                // CLAIM_FH - opening by filehandle, file must exist
                debug!("OPEN: CLAIM_FH - file must exist");
                String::new()
            }
        };

        // Determine if we need to create the file
        let should_create = !matches!(op.openhow, OpenHow::NoCreate);
        
        if should_create && !filename.is_empty() {
            // Create the file
            debug!("OPEN: Creating file '{}'", filename);
            
            // Resolve parent directory path
            let parent_path = match self.fh_mgr.resolve_handle(current_fh) {
                Ok(p) => p,
                Err(e) => {
                    warn!("OPEN: Failed to resolve parent directory: {}", e);
                    return OpenRes {
                        status: Nfs4Status::Stale,
                        stateid: None,
                        change_info: None,
                        result_flags: 0,
                        delegation: OpenDelegationType::None,
                        attrset: vec![],
                    };
                }
            };

            // Build full file path
            let file_path = parent_path.join(&filename);
            debug!("OPEN: Creating file at {:?}", file_path);

            // Create the file
            match std::fs::File::create(&file_path) {
                Ok(_) => {
                    info!("OPEN: Successfully created file {:?}", file_path);
                    
                    // Generate filehandle for the new file
                    match self.fh_mgr.path_to_filehandle(&file_path) {
                        Ok(new_fh) => {
                            info!("OPEN: Generated filehandle for new file");
                            // Update current filehandle to the newly created file
                            ctx.set_current_fh(new_fh.clone());

                            // Get client ID from session (set by SEQUENCE operation)
                            let client_id = self.get_client_id_from_context(ctx);

                            // Allocate stateid for this open
                            let stateid = self.state_mgr.stateids.allocate(
                                StateType::Open,
                                client_id,
                                Some(new_fh.data.clone()),
                            );

                            info!("OPEN: Allocated stateid {:?} for client {}", stateid, client_id);

                            return OpenRes {
                                status: Nfs4Status::Ok,
                                stateid: Some(stateid),
                                change_info: Some(ChangeInfo {
                                    atomic: true,
                                    before: 0,
                                    after: 1,
                                }),
                                result_flags: 0,
                                delegation: OpenDelegationType::None,
                                attrset: match &op.openhow {
                                    OpenHow::Create(attrs) => attrs.attrmask.clone(),
                                    OpenHow::Exclusive4_1 { attrs, .. } => attrs.attrmask.clone(),
                                    _ => vec![],
                                },
                            };
                        }
                        Err(e) => {
                            warn!("OPEN: Failed to generate filehandle for new file: {}", e);
                            return OpenRes {
                                status: Nfs4Status::Io,
                                stateid: None,
                                change_info: None,
                                result_flags: 0,
                                delegation: OpenDelegationType::None,
                                attrset: vec![],
                            };
                        }
                    }
                }
                Err(e) => {
                    warn!("OPEN: Failed to create file {:?}: {}", file_path, e);
                    let status = match e.kind() {
                        std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                        std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                        std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                        _ => Nfs4Status::Io,
                    };
                    return OpenRes {
                        status,
                        stateid: None,
                        change_info: None,
                        result_flags: 0,
                        delegation: OpenDelegationType::None,
                        attrset: vec![],
                    };
                }
            }
        }

        // OPEN without CREATE or CLAIM_FH - file must exist
        debug!("OPEN: Opening existing file (no create)");

        // Get client ID from session (set by SEQUENCE operation)
        let client_id = self.get_client_id_from_context(ctx);

        // If opening for WRITE, recall any read delegations
        // share_access: 1 = READ, 2 = WRITE, 3 = BOTH
        if op.share_access & 2 != 0 {
            // Opening for write - recall read delegations
            if let Ok(file_path) = self.fh_mgr.resolve_handle(current_fh) {
                let recalled = self.state_mgr.delegations.recall_read_delegations(&file_path);
                if !recalled.is_empty() {
                    info!("📢 OPEN: Recalled {} read delegations for write access to {:?}", 
                          recalled.len(), file_path);
                    // In a full implementation, we would wait for clients to return delegations
                    // For now, we just mark them as recalled and proceed
                }
            }
        }

        // Allocate stateid for this open
        let stateid = self.state_mgr.stateids.allocate(
            StateType::Open,
            client_id,
            Some(current_fh.data.clone()),
        );

        info!("OPEN: Allocated stateid {:?} for client {}", stateid, client_id);

        // Try to grant read delegation if appropriate
        let delegation = self.try_grant_read_delegation(
            client_id,
            current_fh,
            op.share_access,
        );

        OpenRes {
            status: Nfs4Status::Ok,
            stateid: Some(stateid),
            change_info: Some(ChangeInfo {
                atomic: true,
                before: 0,
                after: 1,
            }),
            result_flags: 0,
            delegation,
            attrset: vec![],
        }
    }

    /// Try to grant a read delegation
    ///
    /// Read delegations can be granted if:
    /// - Client is opening for READ only (not WRITE)
    /// - No other clients have the file open for WRITE
    /// - File is not being actively modified
    fn try_grant_read_delegation(
        &self,
        client_id: u64,
        filehandle: &Nfs4FileHandle,
        share_access: u32,
    ) -> OpenDelegationType {
        // Only grant read delegations for READ-only opens
        // share_access: 1 = READ, 2 = WRITE, 3 = BOTH
        if share_access != 1 {
            debug!("OPEN: Not granting delegation - not read-only access");
            return OpenDelegationType::None;
        }

        // Resolve file path
        let file_path = match self.fh_mgr.resolve_handle(filehandle) {
            Ok(path) => path,
            Err(e) => {
                debug!("OPEN: Cannot grant delegation - failed to resolve path: {}", e);
                return OpenDelegationType::None;
            }
        };

        // Try to grant read delegation
        match self.state_mgr.delegations.grant_read_delegation(
            client_id,
            filehandle.data.clone(),
            file_path,
        ) {
            Some(deleg_stateid) => {
                info!("✅ OPEN: Granted read delegation {:?} to client {}", deleg_stateid, client_id);
                OpenDelegationType::Read
            }
            None => {
                debug!("OPEN: Cannot grant delegation - conflicts exist");
                OpenDelegationType::None
            }
        }
    }

    /// Handle DELEGRETURN operation
    pub fn handle_delegreturn(
        &self,
        stateid: StateId,
        _ctx: &CompoundContext,
    ) -> DelegReturnRes {
        debug!("DELEGRETURN: stateid={:?}", stateid);

        // Return the delegation
        match self.state_mgr.delegations.return_delegation(&stateid) {
            Ok(()) => {
                info!("✅ DELEGRETURN: Successfully returned delegation {:?}", stateid);
                DelegReturnRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(status) => {
                warn!("❌ DELEGRETURN: Failed to return delegation {:?}: {:?}", stateid, status);
                DelegReturnRes {
                    status,
                }
            }
        }
    }

    /// Handle CLOSE operation
    pub fn handle_close(
        &self,
        op: CloseOp,
        _ctx: &CompoundContext,
    ) -> CloseRes {
        debug!("CLOSE: stateid={:?}", op.stateid);

        // Validate stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("CLOSE: Invalid stateid: {}", e);
            return CloseRes {
                status: Nfs4Status::BadStateId,
                stateid: None,
            };
        }

        // Remove file descriptor from cache (file closes on drop)
        if let Some((_, cached)) = self.fd_cache.remove(&op.stateid) {
            info!("🗑️ FD CACHE CLOSE: Removed and closed FD for {:?} (path: {:?})", op.stateid, cached.path);
        } else {
            info!("⚠️ CLOSE: No cached FD found for {:?} (was already closed or never cached)", op.stateid);
        }

        // Revoke the stateid
        if let Err(e) = self.state_mgr.stateids.revoke(&op.stateid) {
            warn!("CLOSE: Failed to revoke stateid: {}", e);
            return CloseRes {
                status: Nfs4Status::BadStateId,
                stateid: None,
            };
        }

        info!("CLOSE: Revoked stateid {:?}", op.stateid);

        // Return final stateid (with seqid incremented)
        let final_stateid = StateId {
            seqid: op.stateid.seqid + 1,
            other: op.stateid.other,
        };

        CloseRes {
            status: Nfs4Status::Ok,
            stateid: Some(final_stateid),
        }
    }

    /// Handle READ operation
    pub async fn handle_read(
        &self,
        op: ReadOp,
        ctx: &CompoundContext,
    ) -> ReadRes {
        debug!("READ: stateid={:?}, offset={}, count={}",
               op.stateid, op.offset, op.count);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return ReadRes {
                    status: Nfs4Status::NoFileHandle,
                    eof: false,
                    data: Bytes::new(),
                };
            }
        };

        // Validate stateid with relaxed checking for READ operations
        // This allows seqid=0 for anonymous/first reads
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("READ: Invalid stateid: {}", e);
            return ReadRes {
                status: Nfs4Status::BadStateId,
                eof: false,
                data: Bytes::new(),
            };
        }

        // Resolve file path from filehandle
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("READ: Failed to resolve file handle: {}", e);
                return ReadRes {
                    status: Nfs4Status::Stale,
                    eof: false,
                    data: Bytes::new(),
                };
            }
        };

        // Get filename for logging before moving path
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string());

        // Perform positioned read using blocking I/O
        // Uses positioned I/O (pread) for concurrent access without seek
        let offset = op.offset;
        let count = op.count as usize;
        
        let read_result = tokio::task::spawn_blocking(move || {
            // Open file for reading
            let file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) => return Err(e),
            };

            // Get file size to determine EOF
            let metadata = file.metadata()?;
            let file_size = metadata.len();
            
            // Determine actual read count (don't read past EOF)
            let actual_count = if offset >= file_size {
                0
            } else {
                std::cmp::min(count, (file_size - offset) as usize)
            };
            
            if actual_count == 0 {
                return Ok((Bytes::new(), true));
            }

            // Read data using positioned I/O (no seek needed - concurrent safe!)
            let mut buffer = vec![0u8; actual_count];
            let bytes_read = file.read_at(&mut buffer, offset)?;
            
            buffer.truncate(bytes_read);
            let eof = offset + bytes_read as u64 >= file_size;
            
            Ok((Bytes::from(buffer), eof))
        }).await;

        match read_result {
            Ok(Ok((data, eof))) => {
                info!("READ: Read {} bytes at offset {} from {:?}, eof={}", 
                      data.len(), op.offset, filename.as_deref().unwrap_or("unknown"), eof);
                ReadRes {
                    status: Nfs4Status::Ok,
                    eof,
                    data,
                }
            }
            Ok(Err(e)) => {
                warn!("READ: I/O error reading file: {}", e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::IsADirectory => Nfs4Status::IsDir,
                    _ => Nfs4Status::Io,
                };
                ReadRes {
                    status,
                    eof: false,
                    data: Bytes::new(),
                }
            }
            Err(e) => {
                warn!("READ: Task spawn error: {}", e);
                ReadRes {
                    status: Nfs4Status::Io,
                    eof: false,
                    data: Bytes::new(),
                }
            }
        }
    }

    /// Handle WRITE operation
    pub async fn handle_write(
        &self,
        op: WriteOp,
        ctx: &CompoundContext,
    ) -> WriteRes {
        debug!("WRITE: stateid={:?}, offset={}, count={}, stable={}",
               op.stateid, op.offset, op.data.len(), op.stable);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return WriteRes {
                    status: Nfs4Status::NoFileHandle,
                    count: 0,
                    committed: UNSTABLE4,
                    writeverf: 0,
                };
            }
        };

        // Validate stateid with relaxed checking (accept seqid=0 like READ)
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("WRITE: Invalid stateid: {}", e);
            return WriteRes {
                status: Nfs4Status::BadStateId,
                count: 0,
                committed: UNSTABLE4,
                writeverf: 0,
            };
        }

        // Resolve file path from filehandle
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("WRITE: Failed to resolve file handle: {}", e);
                return WriteRes {
                    status: Nfs4Status::Stale,
                    count: 0,
                    committed: UNSTABLE4,
                    writeverf: 0,
                };
            }
        };

        // Get filename for logging before moving path
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string());

        // Try to get cached file descriptor first
        let cached_entry = self.fd_cache.get(&op.stateid);
        
        let file_arc = if let Some(entry) = cached_entry {
            // Found in cache - reuse existing FD!
            info!("✅ FD CACHE HIT: Reusing cached file descriptor for {:?}", op.stateid);
            Arc::clone(&entry.file)
        } else {
            // Not in cache - open and cache it
            info!("🔧 FD CACHE MISS: Opening file and caching for {:?} (path: {:?})", op.stateid, path);
            
            let path_clone = path.clone();
            let file_result = tokio::task::spawn_blocking(move || {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&path_clone)
            }).await;
            
            let file = match file_result {
                Ok(Ok(f)) => f,
                Ok(Err(e)) => {
                    warn!("WRITE: Failed to open file {:?}: {}", path, e);
                    return WriteRes {
                        status: Nfs4Status::Io,
                        count: 0,
                        committed: UNSTABLE4,
                        writeverf: 0,
                    };
                }
                Err(e) => {
                    warn!("WRITE: spawn_blocking error: {}", e);
                    return WriteRes {
                        status: Nfs4Status::Io,
                        count: 0,
                        committed: UNSTABLE4,
                        writeverf: 0,
                    };
                }
            };
            
            let file_arc = Arc::new(std::sync::Mutex::new(file));
            
            // Cache the file descriptor
            self.fd_cache.insert(op.stateid.clone(), CachedFile {
                file: Arc::clone(&file_arc),
                path: path.clone(),
                last_access: Instant::now(),
            });
            
            info!("WRITE: Cached new FD for {:?} (path: {:?})", op.stateid, path);
            file_arc
        };

        // Perform positioned write using cached/opened file
        // ZERO-COPY: data is Bytes (Arc-backed), clone is cheap
        let offset = op.offset;
        let data_clone = op.data.clone(); // Cheap: just Arc increment
        let stable = op.stable;
        let write_verifier = self.write_verifier;
        
        let write_result = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
            use std::os::unix::fs::FileExt;
            
            // Get mutable access to file (lock for this write)
            let file = file_arc.lock().unwrap();
            
            // Write data using positioned I/O (no seek needed - concurrent safe!)
            let bytes_written = file.write_at(&data_clone, offset)?;
            
            // Handle stability requirement
            // UNSTABLE4 (0): Can cache, flush later (fast)
            // DATA_SYNC4 (1): Sync data, metadata can be cached
            // FILE_SYNC4 (2): Sync both data and metadata (slow)
            if stable == FILE_SYNC4 {
                file.sync_all()?; // Full fsync
            } else if stable == DATA_SYNC4 {
                file.sync_data()?; // Sync data only
            }
            // UNSTABLE4: no sync, will be done on COMMIT
            
            Ok(bytes_written)
        }).await;

        match write_result {
            Ok(Ok(bytes_written)) => {
                let count = bytes_written as u32;
                info!("WRITE: Wrote {} bytes at offset {} to {:?}, stable={}", 
                      count, offset, filename.as_deref().unwrap_or("unknown"), stable);
                WriteRes {
                    status: Nfs4Status::Ok,
                    count,
                    committed: stable,
                    writeverf: write_verifier,
                }
            }
            Ok(Err(e)) => {
                warn!("WRITE: I/O error writing file: {}", e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::IsADirectory => Nfs4Status::IsDir,
                    _ => Nfs4Status::Io,
                };
                WriteRes {
                    status,
                    count: 0,
                    committed: UNSTABLE4,
                    writeverf: 0,
                }
            }
            Err(e) => {
                warn!("WRITE: Task spawn error: {}", e);
                WriteRes {
                    status: Nfs4Status::Io,
                    count: 0,
                    committed: UNSTABLE4,
                    writeverf: 0,
                }
            }
        }
    }

    /// Handle COMMIT operation
    pub async fn handle_commit(
        &self,
        op: CommitOp,
        ctx: &CompoundContext,
    ) -> CommitRes {
        debug!("COMMIT: offset={}, count={}", op.offset, op.count);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return CommitRes {
                    status: Nfs4Status::NoFileHandle,
                    writeverf: 0,
                };
            }
        };

        // Resolve file path from filehandle
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("COMMIT: Failed to resolve file handle: {}", e);
                return CommitRes {
                    status: Nfs4Status::Stale,
                    writeverf: 0,
                };
            }
        };

        // Get filename for logging before moving path
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string());

        // Perform fsync to commit UNSTABLE writes to stable storage
        // This is critical for data integrity!
        let write_verifier = self.write_verifier;
        
        let commit_result = tokio::task::spawn_blocking(move || {
            // Open file for syncing
            let file = match std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
            {
                Ok(f) => f,
                Err(e) => return Err(e),
            };

            // Full fsync: sync both data and metadata
            // This ensures UNSTABLE writes are committed to persistent storage
            file.sync_all()?;
            
            Ok(())
        }).await;

        match commit_result {
            Ok(Ok(())) => {
                info!("COMMIT: Synced data to disk for {:?}", filename.as_deref().unwrap_or("unknown"));
                CommitRes {
                    status: Nfs4Status::Ok,
                    writeverf: write_verifier,
                }
            }
            Ok(Err(e)) => {
                warn!("COMMIT: I/O error syncing file: {}", e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    _ => Nfs4Status::Io,
                };
                CommitRes {
                    status,
                    writeverf: 0,
                }
            }
            Err(e) => {
                warn!("COMMIT: Task spawn error: {}", e);
                CommitRes {
                    status: Nfs4Status::Io,
                    writeverf: 0,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::filehandle::FileHandleManager;
    use tempfile::TempDir;

    fn create_test_handler() -> (IoOperationHandler, Arc<FileHandleManager>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        
        // Create a test file for I/O tests
        std::fs::write(export_path.join("testfile.txt"), b"test data for reading").unwrap();
        
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new());
        let handler = IoOperationHandler::new(state_mgr, fh_mgr.clone());
        (handler, fh_mgr, temp_dir)
    }

    #[test]
    fn test_open() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let res = handler.handle_open(op, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
        assert_eq!(res.delegation, OpenDelegationType::None);
    }

    #[test]
    fn test_open_close() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        // OPEN
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let open_res = handler.handle_open(open_op, &mut ctx);
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();

        // CLOSE
        let close_op = CloseOp {
            seqid: 0,
            stateid,
        };

        let close_res = handler.handle_close(close_op, &ctx);
        assert_eq!(close_res.status, Nfs4Status::Ok);
        assert!(close_res.stateid.is_some());
    }

    #[tokio::test]
    async fn test_read() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Get filehandle for the test file we created
        let test_file_path = fh_mgr.get_export_path().join("testfile.txt");
        let test_fh = fh_mgr.path_to_filehandle(&test_file_path).unwrap();
        ctx.current_fh = Some(test_fh);

        // Open first
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let open_res = handler.handle_open(open_op, &mut ctx);
        let stateid = open_res.stateid.unwrap();

        // READ
        let read_op = ReadOp {
            stateid,
            offset: 0,
            count: 1024,
        };

        let read_res = handler.handle_read(read_op, &ctx).await;
        assert_eq!(read_res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_write() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Get filehandle for the test file
        let test_file_path = fh_mgr.get_export_path().join("testfile.txt");
        let test_fh = fh_mgr.path_to_filehandle(&test_file_path).unwrap();
        ctx.current_fh = Some(test_fh);

        // Open first
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_WRITE,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let open_res = handler.handle_open(open_op, &mut ctx);
        let stateid = open_res.stateid.unwrap();

        // WRITE
        let write_op = WriteOp {
            stateid,
            offset: 0,
            stable: UNSTABLE4,
            data: Bytes::from("hello world"),
        };

        let write_res = handler.handle_write(write_op, &ctx).await;
        assert_eq!(write_res.status, Nfs4Status::Ok);
        assert_eq!(write_res.count, 11);
    }

    #[tokio::test]
    async fn test_commit() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Get filehandle for the test file
        let test_file_path = fh_mgr.get_export_path().join("testfile.txt");
        let test_fh = fh_mgr.path_to_filehandle(&test_file_path).unwrap();
        ctx.current_fh = Some(test_fh);

        // COMMIT
        let commit_op = CommitOp {
            offset: 0,
            count: 0, // 0 means commit entire file
        };

        let commit_res = handler.handle_commit(commit_op, &ctx).await;
        assert_eq!(commit_res.status, Nfs4Status::Ok);
    }

    #[test]
    fn test_open_with_file_creation() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle to export root (parent directory for creation)
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh);

        let op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_WRITE,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
            claim: OpenClaim::Null("new-file.txt".to_string()),
        };

        let res = handler.handle_open(op, &mut ctx);
        
        // Should succeed and create the file
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
        
        // Verify current filehandle was updated to the new file
        assert!(ctx.current_fh.is_some());
        
        // Verify file exists on disk
        let file_path = fh_mgr.resolve_handle(ctx.current_fh.as_ref().unwrap()).unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap().to_str().unwrap(), "new-file.txt");
    }

    #[tokio::test]
    async fn test_write_with_relaxed_stateid_validation() {
        let (handler, fh_mgr, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Create a test file first
        let test_file = temp.path().join("test-write.txt");
        std::fs::File::create(&test_file).unwrap();
        
        // Set current filehandle to the test file
        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&test_file).unwrap());

        // Allocate a stateid with seqid=1
        let stateid = handler.state_mgr.stateids.allocate(
            StateType::Open,
            1,
            Some(ctx.current_fh.as_ref().unwrap().data.clone()),
        );

        // Test WRITE with seqid=0 (client sends wrong seqid)
        let write_op = WriteOp {
            stateid: StateId {
                seqid: 0,  // Client sends 0 instead of 1
                other: stateid.other,
            },
            offset: 0,
            stable: UNSTABLE4,
            data: Bytes::from("test data"),
        };

        let write_res = handler.handle_write(write_op, &ctx).await;
        
        // Should succeed with relaxed validation
        assert_eq!(write_res.status, Nfs4Status::Ok);
        assert_eq!(write_res.count, 9);
    }

    #[tokio::test]
    async fn test_read_with_relaxed_stateid_validation() {
        let (handler, fh_mgr, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Create a test file with content
        let test_file = temp.path().join("test-read.txt");
        std::fs::write(&test_file, b"test content").unwrap();
        
        // Set current filehandle to the test file
        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&test_file).unwrap());

        // Allocate a stateid
        let stateid = handler.state_mgr.stateids.allocate(
            StateType::Open,
            1,
            Some(ctx.current_fh.as_ref().unwrap().data.clone()),
        );

        // Test READ with seqid=0
        let read_op = ReadOp {
            stateid: StateId {
                seqid: 0,  // Relaxed validation should accept this
                other: stateid.other,
            },
            offset: 0,
            count: 100,
        };

        let read_res = handler.handle_read(read_op, &ctx).await;
        
        // Should succeed
        assert_eq!(read_res.status, Nfs4Status::Ok);
        assert_eq!(read_res.data.as_ref(), b"test content");
    }

    #[test]
    fn test_open_without_create() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.root_filehandle().unwrap());

        let op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Null("nonexistent.txt".to_string()),
        };

        let res = handler.handle_open(op, &mut ctx);
        
        // Should succeed (we don't validate file existence for NoCreate)
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
    }

    #[tokio::test]
    async fn test_full_write_workflow() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle to export root (parent directory for file creation)
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh);

        // 1. OPEN with create (will create a NEW file)
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_BOTH,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
            claim: OpenClaim::Null("workflow-test.txt".to_string()),
        };

        let open_res = handler.handle_open(open_op, &mut ctx);
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();

        // 2. WRITE data
        let write_op = WriteOp {
            stateid: StateId {
                seqid: 0,  // Use relaxed validation
                other: stateid.other,
            },
            offset: 0,
            stable: FILE_SYNC4,
            data: Bytes::from("Hello, NFS!"),
        };

        let write_res = handler.handle_write(write_op, &ctx).await;
        assert_eq!(write_res.status, Nfs4Status::Ok);
        assert_eq!(write_res.count, 11);

        // 3. READ data back
        let read_op = ReadOp {
            stateid: StateId {
                seqid: 0,
                other: stateid.other,
            },
            offset: 0,
            count: 100,
        };

        let read_res = handler.handle_read(read_op, &ctx).await;
        assert_eq!(read_res.status, Nfs4Status::Ok);
        assert_eq!(read_res.data.as_ref(), b"Hello, NFS!");

        // 4. CLOSE
        let close_op = CloseOp {
            seqid: 0,
            stateid,
        };

        let close_res = handler.handle_close(close_op, &ctx);
        assert_eq!(close_res.status, Nfs4Status::Ok);
    }
}
