// NFSv4 Basic File Operations
//
// This module implements core file operations for NFSv4:
// - File handle operations: PUTROOTFH, PUTFH, GETFH, SAVEFH, RESTOREFH
// - Navigation: LOOKUP, LOOKUPP
// - Attributes: GETATTR, SETATTR
// - Directory: READDIR
// - Access: ACCESS
//
// These operations work with the COMPOUND context's current/saved filehandles.

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::compound::CompoundContext;
use crate::nfs::v4::filehandle::FileHandleManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// PUTROOTFH operation (opcode 24)
///
/// Sets current filehandle to the root of the export.
pub struct PutRootFhOp;

pub struct PutRootFhRes {
    pub status: Nfs4Status,
}

/// PUTFH operation (opcode 22)
///
/// Sets current filehandle to the specified handle.
pub struct PutFhOp {
    pub filehandle: Nfs4FileHandle,
}

pub struct PutFhRes {
    pub status: Nfs4Status,
}

/// GETFH operation (opcode 10)
///
/// Returns the current filehandle.
pub struct GetFhOp;

pub struct GetFhRes {
    pub status: Nfs4Status,
    pub filehandle: Option<Nfs4FileHandle>,
}

/// SAVEFH operation (opcode 32)
///
/// Saves the current filehandle to saved filehandle.
pub struct SaveFhOp;

pub struct SaveFhRes {
    pub status: Nfs4Status,
}

/// RESTOREFH operation (opcode 30)
///
/// Restores saved filehandle to current filehandle.
pub struct RestoreFhOp;

pub struct RestoreFhRes {
    pub status: Nfs4Status,
}

/// LOOKUP operation (opcode 15)
///
/// Looks up a component in the current directory.
pub struct LookupOp {
    pub component: String,
}

pub struct LookupRes {
    pub status: Nfs4Status,
}

/// LOOKUPP operation (opcode 16)
///
/// Looks up parent directory.
pub struct LookupPOp;

pub struct LookupPRes {
    pub status: Nfs4Status,
}

/// GETATTR operation (opcode 9)
///
/// Gets attributes for current filehandle.
pub struct GetAttrOp {
    pub attr_request: Vec<u32>, // Bitmap of requested attributes
}

pub struct GetAttrRes {
    pub status: Nfs4Status,
    pub obj_attributes: Option<Fattr4>,
}

/// Fattr4 - NFSv4 file attributes
#[derive(Debug, Clone)]
pub struct Fattr4 {
    pub attrmask: Vec<u32>,
    pub attr_vals: Vec<u8>, // XDR-encoded attribute values
}

/// SETATTR operation (opcode 34)
///
/// Sets attributes for current filehandle.
pub struct SetAttrOp {
    pub stateid: StateId,
    pub obj_attributes: Fattr4,
}

pub struct SetAttrRes {
    pub status: Nfs4Status,
    pub attrsset: Vec<u32>, // Bitmap of attributes that were set
}

/// ACCESS operation (opcode 3)
///
/// Checks access permissions.
pub struct AccessOp {
    pub access: u32, // Bitmap of access to check
}

pub struct AccessRes {
    pub status: Nfs4Status,
    pub supported: u32, // Access bits supported
    pub access: u32,    // Access bits granted
}

/// Access bits (ACCESS4_*)
pub const ACCESS4_READ: u32 = 0x00000001;
pub const ACCESS4_LOOKUP: u32 = 0x00000002;
pub const ACCESS4_MODIFY: u32 = 0x00000004;
pub const ACCESS4_EXTEND: u32 = 0x00000008;
pub const ACCESS4_DELETE: u32 = 0x00000010;
pub const ACCESS4_EXECUTE: u32 = 0x00000020;

/// READDIR operation (opcode 26)
///
/// Reads directory entries.
pub struct ReadDirOp {
    pub cookie: u64,        // Position in directory
    pub cookieverf: u64,    // Cookie verifier
    pub dircount: u32,      // Max directory bytes
    pub maxcount: u32,      // Max response bytes
    pub attr_request: Vec<u32>, // Requested attributes for entries
}

pub struct ReadDirRes {
    pub status: Nfs4Status,
    pub cookieverf: u64,
    pub entries: Vec<DirEntry>,
    pub eof: bool,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub cookie: u64,
    pub name: String,
    pub attrs: Fattr4,
}

/// CREATE operation (opcode 6)
///
/// Creates a file or directory.
pub struct CreateOp {
    pub objtype: Nfs4FileType,
    pub objname: String,
    pub createattrs: Fattr4,
}

pub struct CreateRes {
    pub status: Nfs4Status,
    pub change_info: Option<ChangeInfo>,
    pub attrset: Vec<u32>, // Which attributes were set
}

#[derive(Debug, Clone)]
pub struct ChangeInfo {
    pub atomic: bool,
    pub before: u64, // Change attribute before operation
    pub after: u64,  // Change attribute after operation
}

/// REMOVE operation (opcode 28)
///
/// Removes a file or directory.
pub struct RemoveOp {
    pub target: String, // Name of file/directory to remove
}

pub struct RemoveRes {
    pub status: Nfs4Status,
    pub change_info: Option<ChangeInfo>,
}

/// File operation handler
pub struct FileOperationHandler {
    fh_mgr: Arc<FileHandleManager>,
}

impl FileOperationHandler {
    /// Create a new file operation handler
    pub fn new(fh_mgr: Arc<FileHandleManager>) -> Self {
        Self { fh_mgr }
    }

    /// Handle PUTROOTFH operation
    pub fn handle_putrootfh(
        &self,
        _op: PutRootFhOp,
        ctx: &mut CompoundContext,
    ) -> PutRootFhRes {
        debug!("PUTROOTFH");

        // Get root filehandle
        match self.fh_mgr.get_root_fh() {
            Ok(fh) => {
                ctx.current_fh = Some(fh);
                PutRootFhRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("PUTROOTFH failed: {}", e);
                PutRootFhRes {
                    status: Nfs4Status::Resource,
                }
            }
        }
    }

    /// Handle PUTFH operation
    pub fn handle_putfh(
        &self,
        op: PutFhOp,
        ctx: &mut CompoundContext,
    ) -> PutFhRes {
        debug!("PUTFH");

        // Validate filehandle
        match self.fh_mgr.validate_handle(&op.filehandle) {
            Ok(_) => {
                ctx.current_fh = Some(op.filehandle);
                PutFhRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("PUTFH validation failed: {}", e);
                PutFhRes {
                    status: Nfs4Status::BadHandle,
                }
            }
        }
    }

    /// Handle GETFH operation
    pub fn handle_getfh(
        &self,
        _op: GetFhOp,
        ctx: &CompoundContext,
    ) -> GetFhRes {
        debug!("GETFH");

        if let Some(ref fh) = ctx.current_fh {
            GetFhRes {
                status: Nfs4Status::Ok,
                filehandle: Some(fh.clone()),
            }
        } else {
            GetFhRes {
                status: Nfs4Status::NoFileHandle,
                filehandle: None,
            }
        }
    }

    /// Handle SAVEFH operation
    pub fn handle_savefh(
        &self,
        _op: SaveFhOp,
        ctx: &mut CompoundContext,
    ) -> SaveFhRes {
        debug!("SAVEFH");

        if let Some(ref fh) = ctx.current_fh {
            ctx.saved_fh = Some(fh.clone());
            SaveFhRes {
                status: Nfs4Status::Ok,
            }
        } else {
            SaveFhRes {
                status: Nfs4Status::NoFileHandle,
            }
        }
    }

    /// Handle RESTOREFH operation
    pub fn handle_restorefh(
        &self,
        _op: RestoreFhOp,
        ctx: &mut CompoundContext,
    ) -> RestoreFhRes {
        debug!("RESTOREFH");

        if let Some(ref fh) = ctx.saved_fh {
            ctx.current_fh = Some(fh.clone());
            RestoreFhRes {
                status: Nfs4Status::Ok,
            }
        } else {
            RestoreFhRes {
                status: Nfs4Status::RestoReFh,
            }
        }
    }

    /// Handle LOOKUP operation
    pub async fn handle_lookup(
        &self,
        op: LookupOp,
        ctx: &mut CompoundContext,
    ) -> LookupRes {
        debug!("LOOKUP: component={}", op.component);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return LookupRes {
                    status: Nfs4Status::NoFileHandle,
                };
            }
        };

        // Resolve current path
        let current_path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(path) => path,
            Err(e) => {
                warn!("LOOKUP: Failed to resolve handle: {}", e);
                return LookupRes {
                    status: Nfs4Status::Stale,
                };
            }
        };

        // Build target path
        let target_path = current_path.join(&op.component);

        // TODO: Check if path exists via filesystem
        // For now, assume all lookups succeed if path can be constructed

        // Generate filehandle for target
        match self.fh_mgr.get_or_create_handle(&target_path) {
            Ok(fh) => {
                ctx.current_fh = Some(fh);
                LookupRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("LOOKUP: Failed to create handle: {}", e);
                LookupRes {
                    status: Nfs4Status::Resource,
                }
            }
        }
    }

    /// Handle LOOKUPP operation
    pub async fn handle_lookupp(
        &self,
        _op: LookupPOp,
        ctx: &mut CompoundContext,
    ) -> LookupPRes {
        debug!("LOOKUPP");

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return LookupPRes {
                    status: Nfs4Status::NoFileHandle,
                };
            }
        };

        // Resolve current path
        let current_path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(path) => path,
            Err(e) => {
                warn!("LOOKUPP: Failed to resolve handle: {}", e);
                return LookupPRes {
                    status: Nfs4Status::Stale,
                };
            }
        };

        // Get parent
        let parent_path = match current_path.parent() {
            Some(p) => p,
            None => {
                // Already at root
                return LookupPRes {
                    status: Nfs4Status::NoEnt,
                };
            }
        };

        // Generate filehandle for parent
        match self.fh_mgr.get_or_create_handle(parent_path) {
            Ok(fh) => {
                ctx.current_fh = Some(fh);
                LookupPRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("LOOKUPP: Failed to create handle: {}", e);
                LookupPRes {
                    status: Nfs4Status::Resource,
                }
            }
        }
    }

    /// Handle ACCESS operation
    pub async fn handle_access(
        &self,
        op: AccessOp,
        ctx: &CompoundContext,
    ) -> AccessRes {
        debug!("ACCESS: access=0x{:08x}", op.access);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return AccessRes {
                    status: Nfs4Status::NoFileHandle,
                    supported: 0,
                    access: 0,
                };
            }
        };

        // For now, grant all requested access
        // TODO: Implement proper permission checking
        let supported = ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY |
                       ACCESS4_EXTEND | ACCESS4_DELETE | ACCESS4_EXECUTE;

        AccessRes {
            status: Nfs4Status::Ok,
            supported,
            access: op.access & supported,
        }
    }

    /// Handle GETATTR operation
    pub async fn handle_getattr(
        &self,
        op: GetAttrOp,
        ctx: &CompoundContext,
    ) -> GetAttrRes {
        debug!("GETATTR: attrs={:?}", op.attr_request);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return GetAttrRes {
                    status: Nfs4Status::NoFileHandle,
                    obj_attributes: None,
                };
            }
        };

        // Resolve path
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("GETATTR: Failed to resolve handle: {}", e);
                return GetAttrRes {
                    status: Nfs4Status::Stale,
                    obj_attributes: None,
                };
            }
        };

        // TODO: Get actual attributes via filesystem
        // For now, return empty attributes

        let fattr = Fattr4 {
            attrmask: op.attr_request.clone(),
            attr_vals: vec![], // TODO: Encode actual attributes
        };

        GetAttrRes {
            status: Nfs4Status::Ok,
            obj_attributes: Some(fattr),
        }
    }

    /// Handle SETATTR operation
    pub async fn handle_setattr(
        &self,
        op: SetAttrOp,
        ctx: &CompoundContext,
    ) -> SetAttrRes {
        debug!("SETATTR");

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return SetAttrRes {
                    status: Nfs4Status::NoFileHandle,
                    attrsset: vec![],
                };
            }
        };

        // TODO: Implement actual SETATTR via VFS
        // For now, return success with empty attrsset

        SetAttrRes {
            status: Nfs4Status::Ok,
            attrsset: op.obj_attributes.attrmask,
        }
    }

    /// Handle READDIR operation
    pub async fn handle_readdir(
        &self,
        op: ReadDirOp,
        ctx: &CompoundContext,
    ) -> ReadDirRes {
        debug!("READDIR: cookie={}, maxcount={}", op.cookie, op.maxcount);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return ReadDirRes {
                    status: Nfs4Status::NoFileHandle,
                    cookieverf: 0,
                    entries: vec![],
                    eof: true,
                };
            }
        };

        // TODO: Read actual directory entries via filesystem
        // For now, return empty directory

        ReadDirRes {
            status: Nfs4Status::Ok,
            cookieverf: 0,
            entries: vec![],
            eof: true,
        }
    }

    /// Handle CREATE operation
    pub async fn handle_create(
        &self,
        op: CreateOp,
        ctx: &mut CompoundContext,
    ) -> CreateRes {
        debug!("CREATE: type={:?}, name={}", op.objtype, op.objname);

        // Check current filehandle (parent directory)
        let parent_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return CreateRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                    attrset: vec![],
                };
            }
        };

        // Resolve parent directory path
        let parent_path = match self.fh_mgr.resolve_handle(parent_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("CREATE: Failed to resolve parent handle: {}", e);
                return CreateRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                    attrset: vec![],
                };
            }
        };

        // Build full path for new object
        let obj_path = parent_path.join(&op.objname);

        // Create the object based on type
        let create_result = match op.objtype {
            Nfs4FileType::Regular => {
                // Create regular file
                tokio::fs::File::create(&obj_path).await.map(|_| ())
            }
            Nfs4FileType::Directory => {
                // Create directory
                tokio::fs::create_dir(&obj_path).await
            }
            _ => {
                // Other types not supported yet
                return CreateRes {
                    status: Nfs4Status::BadType,
                    change_info: None,
                    attrset: vec![],
                };
            }
        };

        match create_result {
            Ok(_) => {
                // Generate filehandle for new object
                match self.fh_mgr.get_or_create_handle(&obj_path) {
                    Ok(new_fh) => {
                        // Set new filehandle as current
                        ctx.current_fh = Some(new_fh);

                        CreateRes {
                            status: Nfs4Status::Ok,
                            change_info: Some(ChangeInfo {
                                atomic: true,
                                before: 0, // TODO: Actual change attr
                                after: 1,
                            }),
                            attrset: op.createattrs.attrmask,
                        }
                    }
                    Err(e) => {
                        warn!("CREATE: Failed to generate handle: {}", e);
                        CreateRes {
                            status: Nfs4Status::Io,
                            change_info: None,
                            attrset: vec![],
                        }
                    }
                }
            }
            Err(e) => {
                warn!("CREATE: Failed to create {}: {}", op.objname, e);
                let status = match e.kind() {
                    std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    _ => Nfs4Status::Io,
                };
                CreateRes {
                    status,
                    change_info: None,
                    attrset: vec![],
                }
            }
        }
    }

    /// Handle REMOVE operation
    pub async fn handle_remove(
        &self,
        op: RemoveOp,
        ctx: &CompoundContext,
    ) -> RemoveRes {
        debug!("REMOVE: target={}", op.target);

        // Check current filehandle (parent directory)
        let parent_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return RemoveRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                };
            }
        };

        // Resolve parent directory path
        let parent_path = match self.fh_mgr.resolve_handle(parent_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("REMOVE: Failed to resolve parent handle: {}", e);
                return RemoveRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                };
            }
        };

        // Build full path for target
        let target_path = parent_path.join(&op.target);

        // Check if target is a directory or file
        match tokio::fs::metadata(&target_path).await {
            Ok(metadata) => {
                let result = if metadata.is_dir() {
                    tokio::fs::remove_dir(&target_path).await
                } else {
                    tokio::fs::remove_file(&target_path).await
                };

                match result {
                    Ok(_) => {
                        RemoveRes {
                            status: Nfs4Status::Ok,
                            change_info: Some(ChangeInfo {
                                atomic: true,
                                before: 1, // TODO: Actual change attr
                                after: 2,
                            }),
                        }
                    }
                    Err(e) => {
                        warn!("REMOVE: Failed to remove {}: {}", op.target, e);
                        let status = match e.kind() {
                            std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                            std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                            _ => Nfs4Status::Io,
                        };
                        RemoveRes {
                            status,
                            change_info: None,
                        }
                    }
                }
            }
            Err(e) => {
                warn!("REMOVE: Failed to stat {}: {}", op.target, e);
                RemoveRes {
                    status: Nfs4Status::NoEnt,
                    change_info: None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_handler() -> (FileOperationHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let handler = FileOperationHandler::new(fh_mgr);
        (handler, temp_dir)
    }

    #[test]
    fn test_putrootfh() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let res = handler.handle_putrootfh(PutRootFhOp, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(ctx.current_fh.is_some());
    }

    #[test]
    fn test_getfh() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // No current FH
        let res = handler.handle_getfh(GetFhOp, &ctx);
        assert_eq!(res.status, Nfs4Status::NoFileHandle);

        // Set root FH
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);

        // Get FH
        let res = handler.handle_getfh(GetFhOp, &ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.filehandle.is_some());
    }

    #[test]
    fn test_savefh_restorefh() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set root FH
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);
        let root_fh = ctx.current_fh.clone();

        // Save FH
        let res = handler.handle_savefh(SaveFhOp, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(ctx.saved_fh, root_fh);

        // Clear current FH
        ctx.current_fh = None;

        // Restore FH
        let res = handler.handle_restorefh(RestoreFhOp, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(ctx.current_fh, root_fh);
    }

    #[tokio::test]
    async fn test_access() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set root FH
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);

        // Check access
        let op = AccessOp {
            access: ACCESS4_READ | ACCESS4_LOOKUP,
        };

        let res = handler.handle_access(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_ne!(res.access, 0);
    }
}
