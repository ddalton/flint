//! pNFS Handler Trait
//!
//! Defines the trait for pNFS operation handling that can be plugged into
//! the NFSv4 dispatcher without modifying core NFS logic.

use crate::pnfs::mds::operations::{
    LayoutGetArgs, LayoutGetResult, LayoutGetError,
    GetDeviceInfoArgs, GetDeviceInfoResult, GetDeviceInfoError,
    LayoutReturnArgs,
};

/// Trait for handling pNFS operations
///
/// This trait allows the NFSv4 dispatcher to optionally support pNFS
/// without creating a hard dependency on pNFS code.
pub trait PnfsOperations: Send + Sync {
    /// Handle LAYOUTGET operation (opcode 50)
    fn layoutget(&self, args: LayoutGetArgs) -> Result<LayoutGetResult, LayoutGetError>;
    
    /// Handle GETDEVICEINFO operation (opcode 47)
    fn getdeviceinfo(&self, args: GetDeviceInfoArgs) -> Result<GetDeviceInfoResult, GetDeviceInfoError>;
    
    /// Handle LAYOUTRETURN operation (opcode 51)
    fn layoutreturn(&self, args: LayoutReturnArgs) -> Result<(), String>;
    
    /// Handle LAYOUTCOMMIT operation (opcode 49)
    fn layoutcommit(&self) -> Result<(), String> {
        // Default implementation: not required for basic pNFS
        Ok(())
    }
    
    /// Handle GETDEVICELIST operation (opcode 48)
    fn getdevicelist(&self) -> Result<Vec<Vec<u8>>, String> {
        // Default implementation: return empty list
        Ok(Vec::new())
    }

    /// Whether `file_key` (export-relative path) is pNFS-managed —
    /// i.e. it has a pinned stripe placement, so its bytes live on the
    /// DS fleet and the MDS's local file is a sparse size-only stub.
    /// The dispatcher uses this to refuse READ/WRITE through the MDS
    /// with NFS4ERR_DELAY: the kernel client falls back to MDS I/O
    /// when a DS is unreachable, and serving the stub returns silent
    /// zeros (data corruption, found live on runn 2026-07-06 by the
    /// DS-outage read drill). Default `false` keeps non-striped files
    /// (never layouted) fully readable/writable through the MDS.
    fn is_pnfs_managed(&self, _file_key: &str) -> bool {
        false
    }

    // NOTE: there is deliberately no `stripe_unit()` here. The stripe
    // unit is per-file (pinned on the placement at first LAYOUTGET and
    // carried on each `Layout`) — a global value is exactly the
    // fleet-change re-mapping bug Phase 0 of the durable-DS plan
    // removed.
}

