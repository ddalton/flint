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
}

