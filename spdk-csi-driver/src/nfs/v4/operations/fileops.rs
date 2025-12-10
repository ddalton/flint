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
use crate::nfs::v4::compound::{CompoundContext, ChangeInfo};
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

/// RENAME operation (opcode 29)
///
/// Renames a file or directory from saved FH to current FH.
/// Requires: saved_fh (source parent), current_fh (dest parent)
pub struct RenameOp {
    pub oldname: String, // Name in saved filehandle directory
    pub newname: String, // Name in current filehandle directory
}

pub struct RenameRes {
    pub status: Nfs4Status,
    pub source_cinfo: Option<ChangeInfo>,
    pub target_cinfo: Option<ChangeInfo>,
}

/// LINK operation (opcode 11)
///
/// Creates a hard link to current FH in saved FH directory.
/// Requires: current_fh (existing file), saved_fh (target directory)
pub struct LinkOp {
    pub newname: String, // Name for the new link
}

pub struct LinkRes {
    pub status: Nfs4Status,
    pub change_info: Option<ChangeInfo>,
}

/// READLINK operation (opcode 27)
///
/// Reads the target of a symbolic link.
pub struct ReadLinkOp;

pub struct ReadLinkRes {
    pub status: Nfs4Status,
    pub link: Option<String>, // Link target path
}

/// PUTPUBFH operation (opcode 23)
///
/// Sets current filehandle to the public filehandle.
/// Note: Public FH is rarely used, defaults to root FH.
pub struct PutPubFhOp;

pub struct PutPubFhRes {
    pub status: Nfs4Status,
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

        // Get file metadata from filesystem
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                warn!("GETATTR: Failed to get metadata for {:?}: {}", path, e);
                return GetAttrRes {
                    status: if e.kind() == std::io::ErrorKind::NotFound {
                        Nfs4Status::NoEnt
                    } else {
                        Nfs4Status::Io
                    },
                    obj_attributes: None,
                };
            }
        };

        // Encode actual file attributes
        // This is a simplified implementation - proper NFSv4 attribute encoding
        // would use XDR and handle all possible attributes per RFC 7530/7862
        use bytes::{BufMut, BytesMut};
        let mut attr_buf = BytesMut::new();
        
        // For each requested attribute, encode its value
        // Common attributes: type, size, mode, nlink, uid, gid, times
        // Simplified encoding - just putting basic values
        
        // Size (attribute 0)
        attr_buf.put_u64(metadata.len());
        
        // File type (simplified)
        let file_type = if metadata.is_dir() { 2u32 } else { 1u32 };
        attr_buf.put_u32(file_type);
        
        // Mode/permissions (ONLY permission bits, not file type)
        // NFSv4 expects just the permission bits since type is encoded separately
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode();
            // Mask off file type bits (S_IFMT), keep only permission bits
            let permissions = mode & 0o7777;  // Keep only permission bits
            attr_buf.put_u32(permissions);
        }
        #[cfg(not(unix))]
        {
            // Default permissions for non-Unix systems
            let mode = if metadata.is_dir() { 0o755u32 } else { 0o644u32 };
            attr_buf.put_u32(mode);
        }
        
        // Timestamps (modified time)
        if let Ok(modified) = metadata.modified() {
            if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                attr_buf.put_u64(duration.as_secs()); // seconds
                attr_buf.put_u32(duration.subsec_nanos()); // nanoseconds
            }
        }

        let fattr = Fattr4 {
            attrmask: op.attr_request.clone(),
            attr_vals: attr_buf.to_vec(),
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

        // Resolve path
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("SETATTR: Failed to resolve handle: {}", e);
                return SetAttrRes {
                    status: Nfs4Status::Stale,
                    attrsset: vec![],
                };
            }
        };

        // Verify file exists
        if !path.exists() {
            return SetAttrRes {
                status: Nfs4Status::NoEnt,
                attrsset: vec![],
            };
        }

        // Set file attributes
        // This is a simplified implementation - proper NFSv4 would decode
        // XDR-encoded attributes and set each requested attribute
        // For now, we handle common operations like setting permissions
        
        let mut attrs_set = vec![];
        let mut errors = vec![];

        // Try to set permissions if specified
        // In a full implementation, we would decode attr_vals to get the actual values
        // For now, if any attributes are requested, we try to set basic permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            
            // If attribute values are provided, try to parse permissions
            if !op.obj_attributes.attr_vals.is_empty() && op.obj_attributes.attr_vals.len() >= 4 {
                // Try to read mode from attributes (simplified)
                // In real implementation, properly decode XDR
                let mode_bytes = &op.obj_attributes.attr_vals[..std::cmp::min(4, op.obj_attributes.attr_vals.len())];
                if mode_bytes.len() == 4 {
                    let mode = u32::from_be_bytes([mode_bytes[0], mode_bytes[1], mode_bytes[2], mode_bytes[3]]);
                    
                    let permissions = std::fs::Permissions::from_mode(mode);
                    match std::fs::set_permissions(&path, permissions) {
                        Ok(_) => {
                            debug!("SETATTR: Set permissions {:o} on {:?}", mode, path);
                            attrs_set.extend_from_slice(&op.obj_attributes.attrmask);
                        }
                        Err(e) => {
                            warn!("SETATTR: Failed to set permissions on {:?}: {}", path, e);
                            errors.push(e);
                        }
                    }
                }
            }
        }

        // Return status based on whether we successfully set any attributes
        if errors.is_empty() {
            SetAttrRes {
                status: Nfs4Status::Ok,
                attrsset: op.obj_attributes.attrmask,
            }
        } else {
            // Partial success or failure
            SetAttrRes {
                status: if attrs_set.is_empty() {
                    Nfs4Status::Inval // No attributes could be set
                } else {
                    Nfs4Status::Ok // Some attributes were set
                },
                attrsset: attrs_set,
            }
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
                                before: 0,
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
                                before: 1,
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

    /// Handle RENAME operation (RFC 7862 Section 15.9)
    ///
    /// Renames a file or directory from source to destination.
    /// Requires: saved_fh (source parent), current_fh (dest parent)
    pub async fn handle_rename(
        &self,
        op: RenameOp,
        ctx: &CompoundContext,
    ) -> RenameRes {
        debug!("RENAME: {} -> {}", op.oldname, op.newname);

        // Check saved filehandle (source parent directory)
        let source_parent_fh = match &ctx.saved_fh {
            Some(fh) => fh,
            None => {
                return RenameRes {
                    status: Nfs4Status::NoFileHandle,
                    source_cinfo: None,
                    target_cinfo: None,
                };
            }
        };

        // Check current filehandle (dest parent directory)
        let dest_parent_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return RenameRes {
                    status: Nfs4Status::NoFileHandle,
                    source_cinfo: None,
                    target_cinfo: None,
                };
            }
        };

        // Resolve source parent directory path
        let source_parent_path = match self.fh_mgr.resolve_handle(source_parent_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("RENAME: Failed to resolve source parent handle: {}", e);
                return RenameRes {
                    status: Nfs4Status::Stale,
                    source_cinfo: None,
                    target_cinfo: None,
                };
            }
        };

        // Resolve dest parent directory path
        let dest_parent_path = match self.fh_mgr.resolve_handle(dest_parent_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("RENAME: Failed to resolve dest parent handle: {}", e);
                return RenameRes {
                    status: Nfs4Status::Stale,
                    source_cinfo: None,
                    target_cinfo: None,
                };
            }
        };

        // Build full paths
        let source_path = source_parent_path.join(&op.oldname);
        let dest_path = dest_parent_path.join(&op.newname);

        // Perform the rename operation
        match tokio::fs::rename(&source_path, &dest_path).await {
            Ok(_) => {
                info!("RENAME: Successfully renamed {:?} to {:?}", source_path, dest_path);
                RenameRes {
                    status: Nfs4Status::Ok,
                    source_cinfo: Some(ChangeInfo {
                        atomic: true,
                        before: 1,
                        after: 2,
                    }),
                    target_cinfo: Some(ChangeInfo {
                        atomic: true,
                        before: 1,
                        after: 2,
                    }),
                }
            }
            Err(e) => {
                warn!("RENAME: Failed to rename {:?} to {:?}: {}", source_path, dest_path, e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                    _ => Nfs4Status::Io,
                };
                RenameRes {
                    status,
                    source_cinfo: None,
                    target_cinfo: None,
                }
            }
        }
    }

    /// Handle LINK operation (RFC 7862 Section 15.4)
    ///
    /// Creates a hard link to current FH in saved FH directory.
    /// Requires: current_fh (existing file), saved_fh (target directory)
    pub async fn handle_link(
        &self,
        op: LinkOp,
        ctx: &CompoundContext,
    ) -> LinkRes {
        debug!("LINK: new name={}", op.newname);

        // Check current filehandle (existing file to link to)
        let file_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return LinkRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                };
            }
        };

        // Check saved filehandle (target directory for new link)
        let target_dir_fh = match &ctx.saved_fh {
            Some(fh) => fh,
            None => {
                return LinkRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                };
            }
        };

        // Resolve existing file path
        let file_path = match self.fh_mgr.resolve_handle(file_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("LINK: Failed to resolve file handle: {}", e);
                return LinkRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                };
            }
        };

        // Resolve target directory path
        let target_dir_path = match self.fh_mgr.resolve_handle(target_dir_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("LINK: Failed to resolve target dir handle: {}", e);
                return LinkRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                };
            }
        };

        // Build path for new link
        let link_path = target_dir_path.join(&op.newname);

        // Create hard link
        match tokio::fs::hard_link(&file_path, &link_path).await {
            Ok(_) => {
                info!("LINK: Successfully created hard link {:?} -> {:?}", link_path, file_path);
                LinkRes {
                    status: Nfs4Status::Ok,
                    change_info: Some(ChangeInfo {
                        atomic: true,
                        before: 1,
                        after: 2,
                    }),
                }
            }
            Err(e) => {
                warn!("LINK: Failed to create hard link {:?} -> {:?}: {}", link_path, file_path, e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                    std::io::ErrorKind::InvalidInput => Nfs4Status::NotDir, // Source is directory
                    _ => Nfs4Status::Io,
                };
                LinkRes {
                    status,
                    change_info: None,
                }
            }
        }
    }

    /// Handle READLINK operation (RFC 7862 Section 15.8)
    ///
    /// Reads the target of a symbolic link.
    pub async fn handle_readlink(
        &self,
        _op: ReadLinkOp,
        ctx: &CompoundContext,
    ) -> ReadLinkRes {
        debug!("READLINK");

        // Check current filehandle
        let link_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return ReadLinkRes {
                    status: Nfs4Status::NoFileHandle,
                    link: None,
                };
            }
        };

        // Resolve symlink path
        let link_path = match self.fh_mgr.resolve_handle(link_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("READLINK: Failed to resolve handle: {}", e);
                return ReadLinkRes {
                    status: Nfs4Status::Stale,
                    link: None,
                };
            }
        };

        // Read the symbolic link
        match tokio::fs::read_link(&link_path).await {
            Ok(target) => {
                let target_str = target.to_string_lossy().to_string();
                info!("READLINK: {:?} -> {}", link_path, target_str);
                ReadLinkRes {
                    status: Nfs4Status::Ok,
                    link: Some(target_str),
                }
            }
            Err(e) => {
                warn!("READLINK: Failed to read symlink {:?}: {}", link_path, e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::InvalidInput => Nfs4Status::Inval, // Not a symlink
                    _ => Nfs4Status::Io,
                };
                ReadLinkRes {
                    status,
                    link: None,
                }
            }
        }
    }

    /// Handle PUTPUBFH operation (RFC 7862 Section 15.7)
    ///
    /// Sets current filehandle to the public filehandle.
    /// In most implementations, public FH is the same as root FH.
    pub fn handle_putpubfh(
        &self,
        _op: PutPubFhOp,
        ctx: &mut CompoundContext,
    ) -> PutPubFhRes {
        debug!("PUTPUBFH (using root FH as public FH)");

        // In most NFSv4 implementations, the public filehandle is the same as root
        // RFC 7862 Section 15.7: Public FH is rarely used in NFSv4
        match self.fh_mgr.get_root_fh() {
            Ok(fh) => {
                ctx.current_fh = Some(fh);
                PutPubFhRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("PUTPUBFH failed: {}", e);
                PutPubFhRes {
                    status: Nfs4Status::Resource,
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
