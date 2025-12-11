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
use crate::nfs::v4::compound::{CompoundContext, ChangeInfo, DirEntry as CompoundDirEntry};
use crate::nfs::v4::filehandle::FileHandleManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};
use bytes::{Bytes, BufMut, BytesMut};

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
    pub entries: Vec<CompoundDirEntry>,  // Use compound module's DirEntry (attrs: Bytes)
    pub eof: bool,
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

// NFSv4 Attribute IDs (FATTR4_*) - Per RFC 5661 Table 3
const FATTR4_SUPPORTED_ATTRS: u32 = 0;
const FATTR4_TYPE: u32 = 1;
const FATTR4_FH_EXPIRE_TYPE: u32 = 2;
const FATTR4_CHANGE: u32 = 3;
const FATTR4_SIZE: u32 = 4;
const FATTR4_LINK_SUPPORT: u32 = 5;
const FATTR4_SYMLINK_SUPPORT: u32 = 6;
const FATTR4_NAMED_ATTR: u32 = 7;
const FATTR4_FSID: u32 = 8;
const FATTR4_UNIQUE_HANDLES: u32 = 9;
const FATTR4_LEASE_TIME: u32 = 10;
const FATTR4_RDATTR_ERROR: u32 = 11;
const FATTR4_ACLSUPPORT: u32 = 12;
const FATTR4_ACL: u32 = 13;
const FATTR4_ARCHIVE: u32 = 14;
const FATTR4_CANSETTIME: u32 = 15;  // FIXED: was 35
const FATTR4_CASE_INSENSITIVE: u32 = 16;  // FIXED: was 39
const FATTR4_CASE_PRESERVING: u32 = 17;  // FIXED: was 40
const FATTR4_CHOWN_RESTRICTED: u32 = 18;
const FATTR4_FILEHANDLE: u32 = 19;
const FATTR4_FILEID: u32 = 20;
const FATTR4_FILES_AVAIL: u32 = 21;
const FATTR4_FILES_FREE: u32 = 22;
const FATTR4_FILES_TOTAL: u32 = 23;
const FATTR4_FS_LOCATIONS: u32 = 24;
const FATTR4_HIDDEN: u32 = 25;
const FATTR4_HOMOGENEOUS: u32 = 26;
const FATTR4_MAXFILESIZE: u32 = 27;  // FIXED: was 42
const FATTR4_MAXLINK: u32 = 28;  // FIXED: was 41
const FATTR4_MAXNAME: u32 = 29;  // FIXED: was 45
const FATTR4_MAXREAD: u32 = 30;  // FIXED: was 43
const FATTR4_MAXWRITE: u32 = 31;  // FIXED: was 44
const FATTR4_MIMETYPE: u32 = 32;
const FATTR4_MODE: u32 = 33;
const FATTR4_NO_TRUNC: u32 = 34;
const FATTR4_NUMLINKS: u32 = 35;  // FIXED: was 27
const FATTR4_OWNER: u32 = 36;
const FATTR4_OWNER_GROUP: u32 = 37;
const FATTR4_QUOTA_AVAIL_HARD: u32 = 38;
const FATTR4_QUOTA_AVAIL_SOFT: u32 = 39;
const FATTR4_QUOTA_USED: u32 = 40;
const FATTR4_RAWDEV: u32 = 41;  // ADDED: was missing
const FATTR4_SPACE_AVAIL: u32 = 42;  // FIXED: was 47
const FATTR4_SPACE_FREE: u32 = 43;  // FIXED: was 48
const FATTR4_SPACE_TOTAL: u32 = 44;  // FIXED: was 49
const FATTR4_SPACE_USED: u32 = 45;  // FIXED: was 50
const FATTR4_SYSTEM: u32 = 46;
const FATTR4_TIME_ACCESS: u32 = 47;  // FIXED: was 51
const FATTR4_TIME_ACCESS_SET: u32 = 48;
const FATTR4_TIME_BACKUP: u32 = 49;
const FATTR4_TIME_CREATE: u32 = 50;
const FATTR4_TIME_DELTA: u32 = 51;
const FATTR4_TIME_METADATA: u32 = 52;
const FATTR4_TIME_MODIFY: u32 = 53;
const FATTR4_TIME_MODIFY_SET: u32 = 54;
const FATTR4_MOUNTED_ON_FILEID: u32 = 55;
const FATTR4_SUPPATTR_EXCLCREAT: u32 = 75;

/// Encode NFSv4 attributes based on requested bitmap
///
/// Returns (attribute_values, supported_bitmap) where:
/// - attribute_values: XDR-encoded attribute values in bitmap order
/// - supported_bitmap: Bitmap of attributes we actually encoded
fn encode_attributes(
    requested_bitmap: &[u32],
    metadata: &std::fs::Metadata,
    path: &Path,
) -> (Vec<u8>, Vec<u32>) {
    use std::collections::BTreeSet;
    
    // Parse bitmap to get list of requested attribute IDs in order
    let mut requested_attrs = BTreeSet::new();
    for (word_idx, &bitmap_word) in requested_bitmap.iter().enumerate() {
        debug!("  Bitmap word {}: 0x{:08x}", word_idx, bitmap_word);
        for bit in 0..32 {
            if (bitmap_word & (1 << bit)) != 0 {
                let attr_id = (word_idx * 32 + bit) as u32;
                debug!("    Bit {} set → Attribute {}", bit, attr_id);
                requested_attrs.insert(attr_id);
            }
        }
    }
    
    debug!("GETATTR: Requested attributes: {:?}", requested_attrs);
    
    // Encode attributes in order
    let mut attr_vals = BytesMut::new();
    let mut supported_attrs = BTreeSet::new();
    
    for attr_id in requested_attrs {
        let before_len = attr_vals.len();
        if encode_single_attribute(attr_id, metadata, path, &mut attr_vals) {
            let after_len = attr_vals.len();
            let bytes_added = after_len - before_len;
            debug!("  Encoded attr {}: {} bytes (total now: {})", attr_id, bytes_added, after_len);
            supported_attrs.insert(attr_id);
        } else {
            debug!("  Skipped attr {} (unsupported)", attr_id);
        }
    }
    
    // Convert supported attributes back to bitmap
    let mut supported_bitmap = vec![0u32; 3]; // Support up to 96 attributes
    for attr_id in supported_attrs {
        let word_idx = (attr_id / 32) as usize;
        let bit = attr_id % 32;
        if word_idx < supported_bitmap.len() {
            supported_bitmap[word_idx] |= 1 << bit;
        }
    }
    
    // Trim trailing zeros from bitmap
    while supported_bitmap.len() > 1 && supported_bitmap.last() == Some(&0) {
        supported_bitmap.pop();
    }
    
    (attr_vals.to_vec(), supported_bitmap)
}

/// Encode NFSv4 attributes for pseudo-root (RFC 7530 Section 7)
///
/// Returns (attribute_values, supported_bitmap) with synthetic values
fn encode_pseudo_root_attributes(
    requested_bitmap: &[u32],
    attrs: &crate::nfs::v4::pseudo::PseudoRootAttrs,
) -> (Vec<u8>, Vec<u32>) {
    use std::collections::BTreeSet;
    use crate::nfs::v4::pseudo::{PSEUDO_ROOT_FSID, PSEUDO_ROOT_FILEID};
    
    // Parse bitmap to get list of requested attribute IDs in order
    let mut requested_attrs = BTreeSet::new();
    for (word_idx, &bitmap_word) in requested_bitmap.iter().enumerate() {
        for bit in 0..32 {
            if (bitmap_word & (1 << bit)) != 0 {
                let attr_id = (word_idx * 32 + bit) as u32;
                requested_attrs.insert(attr_id);
            }
        }
    }
    
    debug!("PSEUDO-ROOT GETATTR: Requested attributes: {:?}", requested_attrs);
    
    // Encode attributes in order with SYNTHETIC values
    let mut attr_vals = BytesMut::new();
    let mut supported_attrs = BTreeSet::new();
    
    for attr_id in requested_attrs {
        let before_len = attr_vals.len();
        if encode_pseudo_root_attribute(attr_id, attrs, &mut attr_vals) {
            let after_len = attr_vals.len();
            let bytes_added = after_len - before_len;
            debug!("  Encoded pseudo-root attr {}: {} bytes", attr_id, bytes_added);
            supported_attrs.insert(attr_id);
        }
    }
    
    // Convert supported attributes back to bitmap
    let mut supported_bitmap = vec![0u32; 3];
    for attr_id in supported_attrs {
        let word_idx = (attr_id / 32) as usize;
        let bit = attr_id % 32;
        if word_idx < supported_bitmap.len() {
            supported_bitmap[word_idx] |= 1 << bit;
        }
    }
    
    // Trim trailing zeros from bitmap
    while supported_bitmap.len() > 1 && supported_bitmap.last() == Some(&0) {
        supported_bitmap.pop();
    }
    
    (attr_vals.to_vec(), supported_bitmap)
}

/// Encode attributes for an export entry in pseudo-root READDIR
///
/// Returns synthetic attributes for the export entry (not the pseudo-root itself).
/// These are placeholder values since the actual export attributes will be
/// retrieved via LOOKUP + GETATTR.
fn encode_export_entry_attributes(_name: &str) -> (Vec<u8>, Vec<u32>) {
    let mut buf = BytesMut::new();
    
    // For READDIR entries, return minimal attributes
    // Client will do LOOKUP + GETATTR to get full attributes
    
    // TYPE (attribute 1): Directory
    buf.put_u32(2); // NF4DIR
    
    // FILEID (attribute 20): Synthetic ID for export entry
    buf.put_u64(2); // Use ID 2 (pseudo-root is 1)
    
    // Only claim to support these two attributes
    // Bitmap with TYPE (1) and FILEID (20) set
    // Word 0: bit 1 and bit 20 = 0x00100002
    let bitmap = vec![0x00100002];
    
    (buf.to_vec(), bitmap)
}

/// Encode a single pseudo-root attribute
fn encode_pseudo_root_attribute(
    attr_id: u32,
    attrs: &crate::nfs::v4::pseudo::PseudoRootAttrs,
    buf: &mut BytesMut,
) -> bool {
    use crate::nfs::v4::pseudo::{PSEUDO_ROOT_FSID, PSEUDO_ROOT_FILEID};
    
    match attr_id {
        FATTR4_TYPE => {
            buf.put_u32(2); // NF4DIR - directory
            true
        }
        FATTR4_FSID => {
            // Pseudo-filesystem FSID: {0, 0}
            buf.put_u64(PSEUDO_ROOT_FSID.0);
            buf.put_u64(PSEUDO_ROOT_FSID.1);
            true
        }
        FATTR4_FILEID => {
            // Pseudo-root file ID: 1
            buf.put_u64(PSEUDO_ROOT_FILEID);
            true
        }
        FATTR4_MOUNTED_ON_FILEID => {
            // Same as FILEID for pseudo-root
            buf.put_u64(PSEUDO_ROOT_FILEID);
            true
        }
        FATTR4_SIZE => {
            buf.put_u64(attrs.size); // Synthetic size (4096)
            true
        }
        FATTR4_NUMLINKS => {
            buf.put_u32(attrs.nlink); // 2 + number of exports
            true
        }
        FATTR4_MODE => {
            buf.put_u32(0o755); // rwxr-xr-x
            true
        }
        FATTR4_CHANGE => {
            buf.put_u64(attrs.create_time);
            true
        }
        FATTR4_TIME_ACCESS | FATTR4_TIME_METADATA | FATTR4_TIME_MODIFY => {
            // All times = pseudo-root creation time
            buf.put_i64(attrs.create_time as i64); // seconds
            buf.put_u32(0); // nanoseconds
            true
        }
        FATTR4_OWNER => {
            // "root"
            buf.put_u32(4);
            buf.put_slice(b"root");
            true
        }
        FATTR4_OWNER_GROUP => {
            // "root"
            buf.put_u32(4);
            buf.put_slice(b"root");
            true
        }
        FATTR4_RAWDEV => {
            // Raw device specdata4 (major, minor) - pseudo-root is not a device
            buf.put_u32(0); // major
            buf.put_u32(0); // minor
            true
        }
        FATTR4_SPACE_USED => {
            // Space used by pseudo-root (minimal)
            buf.put_u64(4096); // One block
            true
        }
        FATTR4_SPACE_AVAIL | FATTR4_SPACE_FREE | FATTR4_SPACE_TOTAL => {
            // Pseudo-filesystem has "infinite" space (return large value)
            buf.put_u64(u64::MAX / 2); // Very large but not overflow
            true
        }
        FATTR4_SUPPORTED_ATTRS => {
            // Return bitmap of attributes we support
            let supported: u64 = (1u64 << FATTR4_TYPE)
                | (1u64 << FATTR4_SIZE)
                | (1u64 << FATTR4_CHANGE)
                | (1u64 << FATTR4_FSID)
                | (1u64 << FATTR4_FILEID)
                | (1u64 << FATTR4_MODE)
                | (1u64 << FATTR4_NUMLINKS)
                | (1u64 << FATTR4_OWNER)
                | (1u64 << FATTR4_OWNER_GROUP)
                | (1u64 << FATTR4_RAWDEV)
                | (1u64 << FATTR4_SPACE_AVAIL)
                | (1u64 << FATTR4_SPACE_FREE)
                | (1u64 << FATTR4_SPACE_TOTAL)
                | (1u64 << FATTR4_SPACE_USED)
                | (1u64 << FATTR4_TIME_ACCESS)
                | (1u64 << FATTR4_TIME_MODIFY)
                | (1u64 << FATTR4_TIME_METADATA)
                | (1u64 << FATTR4_MOUNTED_ON_FILEID);
            
            // Encode as bitmap4
            buf.put_u32(2); // array length
            buf.put_u32((supported >> 32) as u32);
            buf.put_u32(supported as u32);
            true
        }
        _ => {
            // Attribute not supported for pseudo-root
            debug!("  Pseudo-root attr {} not supported", attr_id);
            false
        }
    }
}

/// Encode a single attribute value
///
/// Returns true if attribute was encoded, false if unsupported
fn encode_single_attribute(
    attr_id: u32,
    metadata: &std::fs::Metadata,
    path: &Path,
    buf: &mut BytesMut,
) -> bool {
    match attr_id {
        FATTR4_SUPPORTED_ATTRS => {
            // Return bitmap of attributes we support (RFC 5661 compliant)
            let supported: u64 = (1u64 << FATTR4_TYPE)
                | (1u64 << FATTR4_SIZE)
                | (1u64 << FATTR4_CHANGE)
                | (1u64 << FATTR4_FSID)
                | (1u64 << FATTR4_ACLSUPPORT)
                | (1u64 << FATTR4_CANSETTIME)
                | (1u64 << FATTR4_CASE_INSENSITIVE)
                | (1u64 << FATTR4_CASE_PRESERVING)
                | (1u64 << FATTR4_FILEID)
                | (1u64 << FATTR4_MAXLINK)
                | (1u64 << FATTR4_MAXNAME)
                | (1u64 << FATTR4_MODE)
                | (1u64 << FATTR4_NUMLINKS)
                | (1u64 << FATTR4_OWNER)
                | (1u64 << FATTR4_OWNER_GROUP)
                | (1u64 << FATTR4_RAWDEV)
                | (1u64 << FATTR4_SPACE_AVAIL)
                | (1u64 << FATTR4_SPACE_FREE)
                | (1u64 << FATTR4_SPACE_TOTAL)
                | (1u64 << FATTR4_SPACE_USED)
                | (1u64 << FATTR4_TIME_ACCESS)
                | (1u64 << FATTR4_TIME_MODIFY)
                | (1u64 << FATTR4_TIME_METADATA)
                | (1u64 << FATTR4_MOUNTED_ON_FILEID);
            
            // Encode as bitmap4 (variable-length array per RFC 5661)
            // bitmap4 = array_length + words
            buf.put_u32(2); // array length (2 words for attrs 0-63)
            buf.put_u32((supported >> 32) as u32); // word 0 (attrs 32-63)
            buf.put_u32(supported as u32); // word 1 (attrs 0-31)
            true
        }
        
        FATTR4_TYPE => {
            // File type: 1=regular, 2=directory, 3=block, 4=char, 5=symlink, 6=socket, 7=fifo
            let ftype = if metadata.is_dir() { 
                2u32  // NF4DIR
            } else if metadata.is_symlink() {
                5u32  // NF4LNK
            } else { 
                1u32  // NF4REG
            };
            debug!("  Encoding TYPE: value={} (is_dir={}, is_symlink={})", 
                   ftype, metadata.is_dir(), metadata.is_symlink());
            buf.put_u32(ftype);
            true
        }
        
        FATTR4_FH_EXPIRE_TYPE => {
            // FH_PERSISTENT (0) = filehandles never expire
            buf.put_u32(0);
            true
        }
        
        FATTR4_CHANGE => {
            // Change attribute - use modification time as change ID
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let change_id = metadata.mtime() as u64;
                buf.put_u64(change_id);
            }
            #[cfg(not(unix))]
            {
                buf.put_u64(0);
            }
            true
        }
        
        FATTR4_SIZE => {
            buf.put_u64(metadata.len());
            true
        }
        
        FATTR4_LINK_SUPPORT => {
            // TRUE = hard links supported
            buf.put_u32(1);
            true
        }
        
        FATTR4_SYMLINK_SUPPORT => {
            // TRUE = symbolic links supported
            buf.put_u32(1);
            true
        }
        
        FATTR4_NAMED_ATTR => {
            // FALSE = no named attributes
            buf.put_u32(0);
            true
        }
        
        FATTR4_FSID => {
            // Filesystem ID - major and minor (8 bytes each)
            buf.put_u64(0); // major
            buf.put_u64(1); // minor
            true
        }
        
        FATTR4_UNIQUE_HANDLES => {
            // TRUE = filehandles are unique within filesystem
            buf.put_u32(1);
            true
        }
        
        FATTR4_LEASE_TIME => {
            // Lease time in seconds (90 seconds is standard)
            buf.put_u32(90);
            true
        }
        
        FATTR4_RDATTR_ERROR => {
            // No error reading attributes
            buf.put_u32(0); // NFS4_OK
            true
        }
        
        FATTR4_ACLSUPPORT => {
            // ACL support flags
            // ACL4_SUPPORT_ALLOW_ACL = 0x00000001
            // ACL4_SUPPORT_DENY_ACL = 0x00000002
            buf.put_u32(0x00000003); // Support both ALLOW and DENY ACLs
            true
        }
        
        FATTR4_ACL => {
            // Return empty ACL (no ACL set)
            buf.put_u32(0); // 0 ACE entries
            true
        }
        
        FATTR4_FILEID => {
            // File ID (inode number)
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                buf.put_u64(metadata.ino());
            }
            #[cfg(not(unix))]
            {
                // On non-Unix, use a hash of the path
                use std::collections::hash_map::DefaultHasher;
                use std::hash::{Hash, Hasher};
                let mut hasher = DefaultHasher::new();
                path.hash(&mut hasher);
                buf.put_u64(hasher.finish());
            }
            true
        }
        
        FATTR4_FILES_AVAIL | FATTR4_FILES_FREE | FATTR4_FILES_TOTAL => {
            // Total file/inode counts
            buf.put_u64(1_000_000); // Reasonable default
            true
        }
        
        FATTR4_MAXFILESIZE => {
            // Maximum file size (1TB)
            buf.put_u64(1024 * 1024 * 1024 * 1024);
            true
        }
        
        FATTR4_MAXREAD | FATTR4_MAXWRITE => {
            // Maximum read/write size (1MB)
            buf.put_u64(1024 * 1024);
            true
        }
        
        FATTR4_MAXLINK => {
            // Maximum number of hard links
            buf.put_u32(65535);
            true
        }
        
        FATTR4_MAXNAME => {
            // Maximum filename length
            buf.put_u32(255);
            true
        }
        
        FATTR4_CANSETTIME => {
            // TRUE = server can set time fields
            buf.put_u32(1);
            true
        }
        
        FATTR4_CASE_INSENSITIVE => {
            // FALSE = filesystem is case-sensitive
            buf.put_u32(0);
            true
        }
        
        FATTR4_CASE_PRESERVING => {
            // TRUE = filesystem preserves case
            buf.put_u32(1);
            true
        }
        
        FATTR4_ARCHIVE => {
            // Archive bit (not used on Unix)
            buf.put_u32(0);
            true
        }
        
        FATTR4_MODE => {
            // File mode/permissions (mask out file type bits, keep only permission bits)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = metadata.permissions().mode() & 0o7777;
                buf.put_u32(mode);
            }
            #[cfg(not(unix))]
            {
                // Default: rwxr-xr-x for dirs, rw-r--r-- for files
                let mode = if metadata.is_dir() { 0o755 } else { 0o644 };
                buf.put_u32(mode);
            }
            true
        }
        
        FATTR4_NUMLINKS => {
            // Number of hard links
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                buf.put_u32(metadata.nlink() as u32);
            }
            #[cfg(not(unix))]
            {
                buf.put_u32(1);
            }
            true
        }
        
        FATTR4_OWNER => {
            // Owner (user ID as string)
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let owner = format!("{}", metadata.uid());
                let owner_bytes = owner.as_bytes();
                buf.put_u32(owner_bytes.len() as u32);
                buf.put_slice(owner_bytes);
                // XDR padding to 4-byte boundary
                let padding = (4 - (owner_bytes.len() % 4)) % 4;
                for _ in 0..padding {
                    buf.put_u8(0);
                }
            }
            #[cfg(not(unix))]
            {
                let owner = b"nobody";
                buf.put_u32(owner.len() as u32);
                buf.put_slice(owner);
                // XDR padding
                buf.put_u16(0);
            }
            true
        }
        
        FATTR4_OWNER_GROUP => {
            // Owner group (group ID as string)
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let group = format!("{}", metadata.gid());
                let group_bytes = group.as_bytes();
                buf.put_u32(group_bytes.len() as u32);
                buf.put_slice(group_bytes);
                // XDR padding to 4-byte boundary
                let padding = (4 - (group_bytes.len() % 4)) % 4;
                for _ in 0..padding {
                    buf.put_u8(0);
                }
            }
            #[cfg(not(unix))]
            {
                let group = b"nogroup";
                buf.put_u32(group.len() as u32);
                buf.put_slice(group);
                // XDR padding
                buf.put_u8(0);
            }
            true
        }
        
        FATTR4_RAWDEV => {
            // Raw device (specdata4: major + minor device numbers)
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let rdev = metadata.rdev();
                // Extract major/minor from rdev
                let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
                let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
                buf.put_u32(major as u32);
                buf.put_u32(minor as u32);
            }
            #[cfg(not(unix))]
            {
                // Not a device
                buf.put_u32(0);
                buf.put_u32(0);
            }
            true
        }
        
        FATTR4_SPACE_AVAIL | FATTR4_SPACE_FREE => {
            // Available/free space (100GB)
            buf.put_u64(100 * 1024 * 1024 * 1024);
            true
        }
        
        FATTR4_SPACE_TOTAL => {
            // Total space (1TB)
            buf.put_u64(1024 * 1024 * 1024 * 1024);
            true
        }
        
        FATTR4_SPACE_USED => {
            // Space used by file (actual size)
            buf.put_u64(metadata.len());
            true
        }
        
        FATTR4_TIME_ACCESS | FATTR4_TIME_MODIFY | FATTR4_TIME_METADATA => {
            // Time values (NFSv4 nfstime4 format: seconds + nanoseconds)
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let (secs, nsecs) = match attr_id {
                    FATTR4_TIME_ACCESS => (metadata.atime(), metadata.atime_nsec()),
                    FATTR4_TIME_MODIFY => (metadata.mtime(), metadata.mtime_nsec()),
                    FATTR4_TIME_METADATA => (metadata.ctime(), metadata.ctime_nsec()),
                    _ => (0, 0),
                };
                debug!("  Encoding time attr {}: secs={}, nsecs={}", attr_id, secs, nsecs);
                buf.put_i64(secs);
                buf.put_u32(nsecs as u32);
            }
            #[cfg(not(unix))]
            {
                // Use modified time for all
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                        buf.put_i64(duration.as_secs() as i64);
                        buf.put_u32(duration.subsec_nanos());
                    } else {
                        buf.put_i64(0);
                        buf.put_u32(0);
                    }
                } else {
                    buf.put_i64(0);
                    buf.put_u32(0);
                }
            }
            true
        }
        
        FATTR4_MOUNTED_ON_FILEID => {
            // For non-mount-points, same as FILEID
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                buf.put_u64(metadata.ino());
            }
            #[cfg(not(unix))]
            {
                use std::collections::hash_map::DefaultHasher;
                use std::hash::{Hash, Hasher};
                let mut hasher = DefaultHasher::new();
                path.hash(&mut hasher);
                buf.put_u64(hasher.finish());
            }
            true
        }
        
        FATTR4_SUPPATTR_EXCLCREAT => {
            // Bitmap of attributes supported for exclusive create (RFC 5661 §5.8.1.14)
            // Return bitmap of settable attributes during exclusive create
            let supported: u64 = (1u64 << FATTR4_MODE)
                | (1u64 << FATTR4_OWNER)
                | (1u64 << FATTR4_OWNER_GROUP)
                | (1u64 << FATTR4_SIZE)
                | (1u64 << FATTR4_TIME_ACCESS_SET)
                | (1u64 << FATTR4_TIME_MODIFY_SET);
            
            // Encode as bitmap4 (array of u32)
            // For attributes 0-63, we need 2 words
            buf.put_u32(2); // bitmap array length
            buf.put_u32((supported >> 32) as u32); // word 0
            buf.put_u32(supported as u32); // word 1
            true
        }
        
        _ => {
            // Unsupported attribute
            debug!("GETATTR: Unsupported attribute {}", attr_id);
            false
        }
    }
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

        // Handle LOOKUP from pseudo-root (RFC 7530 Section 7)
        if self.fh_mgr.is_pseudo_root(current_fh) {
            info!("🔍 LOOKUP from PSEUDO-ROOT: component='{}'", op.component);
            
            // Lookup export by name
            if let Some(export) = self.fh_mgr.lookup_export(&op.component) {
                info!("✅ Found export '{}' → path {:?}", export.name, export.path);
                
                // Verify the export path exists
                match tokio::fs::metadata(&export.path).await {
                    Ok(metadata) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            info!("   Export metadata: is_dir={}, mode={:o}", 
                                  metadata.is_dir(), metadata.mode());
                        }
                        #[cfg(not(unix))]
                        {
                            info!("   Export metadata: is_dir={}", metadata.is_dir());
                        }
                    }
                    Err(e) => {
                        warn!("   Export path does not exist: {}", e);
                        return LookupRes {
                            status: Nfs4Status::NoEnt,
                        };
                    }
                }
                
                // Create filehandle for the export's actual path
                match self.fh_mgr.get_or_create_handle(&export.path) {
                    Ok(fh) => {
                        info!("   Created filehandle: {} bytes", fh.data.len());
                        ctx.current_fh = Some(fh);
                        return LookupRes {
                            status: Nfs4Status::Ok,
                        };
                    }
                    Err(e) => {
                        warn!("LOOKUP: Failed to create handle for export: {}", e);
                        return LookupRes {
                            status: Nfs4Status::Resource,
                        };
                    }
                }
            } else {
                warn!("❌ Export '{}' not found in pseudo-filesystem", op.component);
                let available = self.fh_mgr.get_pseudo_fs().list_exports();
                warn!("   Available exports: {:?}", available);
                return LookupRes {
                    status: Nfs4Status::NoEnt,
                };
            }
        }

        // Regular filesystem LOOKUP
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

        // Check if the target path exists
        let metadata = match tokio::fs::metadata(&target_path).await {
            Ok(m) => m,
            Err(e) => {
                debug!("LOOKUP: Path {:?} does not exist: {}", target_path, e);
                return LookupRes {
                    status: if e.kind() == std::io::ErrorKind::NotFound {
                        Nfs4Status::NoEnt
                    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                        Nfs4Status::Access
                    } else {
                        Nfs4Status::Io
                    },
                };
            }
        };

        debug!("LOOKUP: Found {:?} (is_dir={}, is_file={})", 
               target_path, metadata.is_dir(), metadata.is_file());

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

        // Cannot go up from pseudo-root (RFC 7530 Section 7)
        if self.fh_mgr.is_pseudo_root(current_fh) {
            debug!("LOOKUPP: Cannot go above pseudo-root");
            return LookupPRes {
                status: Nfs4Status::NoEnt,
            };
        }

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
            Some(p) => p.to_path_buf(),
            None => {
                // Already at root
                return LookupPRes {
                    status: Nfs4Status::NoEnt,
                };
            }
        };

        // Check if we're trying to go above the export root
        // Compare with the export root from the file handle manager
        let export_root = self.fh_mgr.get_export_path();
        if !parent_path.starts_with(export_root) {
            debug!("LOOKUPP: Attempt to go above export root (current={:?}, parent={:?}, export={:?})",
                   current_path, parent_path, export_root);
            return LookupPRes {
                status: Nfs4Status::NoEnt,
            };
        }

        // Check if the parent path exists
        let metadata = match tokio::fs::metadata(&parent_path).await {
            Ok(m) => m,
            Err(e) => {
                debug!("LOOKUPP: Parent path {:?} does not exist: {}", parent_path, e);
                return LookupPRes {
                    status: if e.kind() == std::io::ErrorKind::NotFound {
                        Nfs4Status::NoEnt
                    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                        Nfs4Status::Access
                    } else {
                        Nfs4Status::Io
                    },
                };
            }
        };

        // Verify it's a directory
        if !metadata.is_dir() {
            warn!("LOOKUPP: Parent path {:?} is not a directory", parent_path);
            return LookupPRes {
                status: Nfs4Status::NotDir,
            };
        }

        debug!("LOOKUPP: Moving from {:?} to parent {:?}", current_path, parent_path);

        // Generate filehandle for parent
        match self.fh_mgr.get_or_create_handle(&parent_path) {
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

        // Pseudo-root is always accessible for READ and LOOKUP
        if self.fh_mgr.is_pseudo_root(current_fh) {
            debug!("ACCESS on PSEUDO-ROOT - granting READ and LOOKUP");
            let supported = ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_EXECUTE;
            return AccessRes {
                status: Nfs4Status::Ok,
                supported,
                access: op.access & supported,
            };
        }

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

        // Check if this is the pseudo-root (RFC 7530 Section 7)
        if self.fh_mgr.is_pseudo_root(current_fh) {
            debug!("📂 GETATTR for PSEUDO-ROOT (synthetic attributes)");
            return self.handle_pseudo_root_getattr(op).await;
        }

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
        
        debug!("📂 GETATTR for path: {:?}", path);

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
        
        // Debug log metadata values
        debug!("📊 Metadata for {:?}:", path);
        debug!("   is_dir: {}, is_file: {}, is_symlink: {}", 
               metadata.is_dir(), metadata.is_file(), metadata.is_symlink());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            debug!("   size: {}, ino: {}, mode: {:o}", 
                   metadata.len(), metadata.ino(), metadata.mode());
            debug!("   mtime: {}, atime: {}, ctime: {}", 
                   metadata.mtime(), metadata.atime(), metadata.ctime());
        }

    // Encode requested attributes per RFC 7530/7862
    // Attributes must be encoded in bitmap order
    let (attr_vals, supported_bitmap) = encode_attributes(&op.attr_request, &metadata, &path);
    
    let fattr = Fattr4 {
        attrmask: supported_bitmap.clone(),
        attr_vals: attr_vals.clone(),
    };

    debug!("GETATTR: Returning {} bytes of attributes", fattr.attr_vals.len());
    
    // Detailed hex dump for debugging
    debug!("GETATTR: Supported bitmap: {:?}", supported_bitmap);
    if attr_vals.len() <= 256 {
        // Hex dump in 16-byte rows
        for (i, chunk) in attr_vals.chunks(16).enumerate() {
            let hex_str: String = chunk.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
            debug!("  Attr vals [{:04x}]: {}", i * 16, hex_str);
        }
    }

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

        // Handle READDIR on pseudo-root - list exports
        if self.fh_mgr.is_pseudo_root(current_fh) {
            debug!("READDIR on PSEUDO-ROOT - listing exports");
            
            let export_names = self.fh_mgr.get_pseudo_fs().list_exports();
            debug!("Found {} exports: {:?}", export_names.len(), export_names);
            
            // Create directory entries for exports
            let mut entries = vec![];
            for (i, name) in export_names.iter().enumerate() {
                if op.cookie > 0 && (i as u64) < op.cookie {
                    continue; // Skip entries before cookie
                }
                
                // Create minimal attributes for export entry
                let (attr_vals, supported_bitmap) = encode_export_entry_attributes(name);
                
                // Pre-encode Fattr4 into Bytes for compound module
                let mut fattr_buf = BytesMut::new();
                
                // Encode bitmap
                fattr_buf.put_u32(supported_bitmap.len() as u32);
                for word in &supported_bitmap {
                    fattr_buf.put_u32(*word);
                }
                
                // Encode attr_vals as opaque
                fattr_buf.put_u32(attr_vals.len() as u32);
                fattr_buf.put_slice(&attr_vals);
                let padding = (4 - (attr_vals.len() % 4)) % 4;
                for _ in 0..padding {
                    fattr_buf.put_u8(0);
                }
                
                entries.push(CompoundDirEntry {
                    cookie: (i + 1) as u64,
                    name: name.clone(),
                    attrs: fattr_buf.freeze(),
                });
            }
            
            debug!("READDIR returning {} export entries", entries.len());
            
            return ReadDirRes {
                status: Nfs4Status::Ok,
                cookieverf: 1, // Simple verifier
                entries,
                eof: true,
            };
        }

        // TODO: Read actual directory entries via filesystem
        // For now, return empty directory for regular directories

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
    
    /// Handle GETATTR for pseudo-root (RFC 7530 Section 7)
    ///
    /// Returns synthetic attributes for the virtual root filesystem.
    async fn handle_pseudo_root_getattr(&self, op: GetAttrOp) -> GetAttrRes {
        use crate::nfs::v4::pseudo::{PSEUDO_ROOT_FSID, PSEUDO_ROOT_FILEID};
        
        let pseudo_fs = self.fh_mgr.get_pseudo_fs();
        let attrs = pseudo_fs.get_pseudo_root_attrs();
        
        // Encode attributes with synthetic values
        let (attr_vals, supported_bitmap) = encode_pseudo_root_attributes(
            &op.attr_request,
            &attrs,
        );
        
        let fattr = Fattr4 {
            attrmask: supported_bitmap.clone(),
            attr_vals: attr_vals.clone(),
        };
        
        debug!("PSEUDO-ROOT GETATTR: Returning {} bytes of synthetic attributes", fattr.attr_vals.len());
        debug!("   FSID: {:?}", PSEUDO_ROOT_FSID);
        debug!("   FILEID: {}", PSEUDO_ROOT_FILEID);
        
        GetAttrRes {
            status: Nfs4Status::Ok,
            obj_attributes: Some(fattr),
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
