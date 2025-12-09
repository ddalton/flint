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
use bytes::Bytes;
use std::sync::Arc;
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

    // TODO: Add other claim types as needed
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

/// I/O operation handler
pub struct IoOperationHandler {
    state_mgr: Arc<StateManager>,
}

impl IoOperationHandler {
    /// Create a new I/O operation handler
    pub fn new(state_mgr: Arc<StateManager>) -> Self {
        Self { state_mgr }
    }

    /// Handle OPEN operation
    pub fn handle_open(
        &self,
        op: OpenOp,
        ctx: &mut CompoundContext,
    ) -> OpenRes {
        debug!("OPEN: share_access=0x{:08x}, share_deny=0x{:08x}",
               op.share_access, op.share_deny);

        // Check current filehandle
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

        // TODO: Determine client ID from context
        let client_id = 1; // Placeholder

        // Allocate stateid for this open
        let stateid = self.state_mgr.stateids.allocate(
            StateType::Open,
            client_id,
            Some(current_fh.data.clone()),
        );

        info!("OPEN: Allocated stateid {:?} for client {}", stateid, client_id);

        OpenRes {
            status: Nfs4Status::Ok,
            stateid: Some(stateid),
            change_info: Some(ChangeInfo {
                atomic: true,
                before: 0,
                after: 1,
            }),
            result_flags: 0,
            delegation: OpenDelegationType::None,
            attrset: vec![],
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

        // Validate stateid (unless special stateid)
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("READ: Invalid stateid: {}", e);
            return ReadRes {
                status: Nfs4Status::BadStateId,
                eof: false,
                data: Bytes::new(),
            };
        }

        // TODO: Perform actual read via filesystem
        // For now, return empty data

        ReadRes {
            status: Nfs4Status::Ok,
            eof: true,
            data: Bytes::new(),
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

        // Validate stateid
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("WRITE: Invalid stateid: {}", e);
            return WriteRes {
                status: Nfs4Status::BadStateId,
                count: 0,
                committed: UNSTABLE4,
                writeverf: 0,
            };
        }

        // TODO: Perform actual write via filesystem
        // For now, claim we wrote all bytes

        let count = op.data.len() as u32;

        info!("WRITE: Wrote {} bytes at offset {}", count, op.offset);

        WriteRes {
            status: Nfs4Status::Ok,
            count,
            committed: op.stable,
            writeverf: 1, // TODO: Generate proper write verifier
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

        // TODO: Perform actual fsync/commit via filesystem
        // For now, claim success

        CommitRes {
            status: Nfs4Status::Ok,
            writeverf: 1, // Should match WRITE writeverf
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
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new());
        let handler = IoOperationHandler::new(state_mgr);
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

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

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

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

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

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        // COMMIT
        let commit_op = CommitOp {
            offset: 0,
            count: 0, // 0 means commit entire file
        };

        let commit_res = handler.handle_commit(commit_op, &ctx).await;
        assert_eq!(commit_res.status, Nfs4Status::Ok);
    }
}
