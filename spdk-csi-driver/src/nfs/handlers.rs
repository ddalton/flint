//! NFSv3 Procedure Handlers
//!
//! Implements all NFSv3 RPC procedures according to RFC 1813.
//! Each handler decodes request arguments, calls the VFS, and encodes the reply.

use super::protocol::{FileHandle, NFS3Status};
use super::rpc::{CallMessage, ReplyBuilder};
use super::vfs::LocalFilesystem;
use super::xdr::XdrDecoder;
use bytes::Bytes;
use std::sync::Arc;
use tracing::{debug, warn};

/// Handle NULL procedure (Procedure 0)
/// RFC 1813 Section 3.3.0
pub fn handle_null(call: &CallMessage) -> Bytes {
    debug!("NFS NULL");
    ReplyBuilder::success(call.xid).finish()
}

/// Handle GETATTR procedure (Procedure 1)
/// RFC 1813 Section 3.3.1
pub async fn handle_getattr(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS GETATTR");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Get attributes from filesystem
    match fs.getattr(&file_handle).await {
        Ok(attrs) => {
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Object attributes
            attrs.encode(enc);
            
            reply.finish()
        }
        Err(e) => {
            warn!("GETATTR failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle ACCESS procedure (Procedure 4)
/// RFC 1813 Section 3.3.4
pub async fn handle_access(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS ACCESS");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode access bits requested
    let access_requested = match dec.decode_u32() {
        Ok(a) => a,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("ACCESS: requested={:#x}", access_requested);
    
    // For simplicity, we grant all requested permissions
    // In a real implementation, we'd check actual file permissions
    let access_granted = access_requested;
    
    // Get attributes
    let attrs = fs.getattr(&file_handle).await.ok();
    
    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();
    
    // Status: NFS3_OK
    enc.encode_u32(NFS3Status::Ok as u32);
    
    // Object attributes (optional but we provide if available)
    if let Some(attr) = attrs {
        enc.encode_bool(true);
        attr.encode(enc);
    } else {
        enc.encode_bool(false);
    }
    
    // Access rights granted
    enc.encode_u32(access_granted);
    
    reply.finish()
}

/// Handle LOOKUP procedure (Procedure 3)
/// RFC 1813 Section 3.3.3
pub async fn handle_lookup(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS LOOKUP");
    
    // Decode directory file handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode filename
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(e) => {
            warn!("Failed to decode filename: {}", e);
            return ReplyBuilder::garbage_args(call.xid);
        }
    };
    
    debug!("LOOKUP: name={}", name);
    
    // Lookup in filesystem
    match fs.lookup(&dir_handle, &name).await {
        Ok((file_handle, attrs)) => {
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // File handle
            file_handle.encode(enc);
            
            // Object attributes (optional but we always provide)
            enc.encode_bool(true); // obj_attributes_follow
            attrs.encode(enc);
            
            // Directory attributes (optional, we skip for simplicity)
            enc.encode_bool(false); // dir_attributes_follow
            
            reply.finish()
        }
        Err(e) => {
            warn!("LOOKUP failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle READ procedure (Procedure 6)
/// RFC 1813 Section 3.3.6
pub async fn handle_read(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS READ");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode offset and count
    let offset = match dec.decode_u64() {
        Ok(o) => o,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    let count = match dec.decode_u32() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("READ: offset={}, count={}", offset, count);
    
    // Read from filesystem
    match fs.read(&file_handle, offset, count).await {
        Ok(data) => {
            let eof = data.len() < count as usize;
            
            // Get updated attributes
            let attrs = fs.getattr(&file_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // File attributes (optional but we provide if available)
            if let Some(attr) = attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            // Count of bytes read
            enc.encode_u32(data.len() as u32);
            
            // EOF flag
            enc.encode_bool(eof);
            
            // Data
            enc.encode_opaque(&data);
            
            reply.finish()
        }
        Err(e) => {
            warn!("READ failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle WRITE procedure (Procedure 7)
/// RFC 1813 Section 3.3.7
pub async fn handle_write(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS WRITE");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode offset
    let offset = match dec.decode_u64() {
        Ok(o) => o,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode count
    let count = match dec.decode_u32() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode stable (how to commit data)
    let _stable = match dec.decode_u32() {
        Ok(s) => s,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode data
    let data = match dec.decode_opaque() {
        Ok(d) => d,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("WRITE: offset={}, count={}, actual={}", offset, count, data.len());
    
    // Write to filesystem
    match fs.write(&file_handle, offset, &data).await {
        Ok(written) => {
            // Get updated attributes
            let attrs = fs.getattr(&file_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // File attributes before operation (we skip)
            enc.encode_bool(false);
            
            // File attributes after operation
            if let Some(attr) = attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            // Count of bytes written
            enc.encode_u32(written);
            
            // How data was committed (FILE_SYNC = 2, data is stable)
            enc.encode_u32(2);
            
            // Write verifier (for detecting server reboots)
            enc.encode_u64(0);
            
            reply.finish()
        }
        Err(e) => {
            warn!("WRITE failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle CREATE procedure (Procedure 8)
/// RFC 1813 Section 3.3.8
pub async fn handle_create(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS CREATE");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode filename
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode create mode (UNCHECKED=0, GUARDED=1, EXCLUSIVE=2)
    let create_mode = match dec.decode_u32() {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to decode create mode: {}", e);
            return ReplyBuilder::garbage_args(call.xid);
        }
    };
    
    // Decode sattr3 (set attributes) - RFC 1813 Section 3.3.8
    // For UNCHECKED and GUARDED modes, we need to decode the sattr3 structure
    let file_mode = if create_mode == 2 {
        // EXCLUSIVE mode: has createverf3 instead of sattr3
        match dec.decode_u64() {
            Ok(_) => 0o644u32,
            Err(e) => {
                warn!("Failed to decode createverf3: {}", e);
                return ReplyBuilder::garbage_args(call.xid);
            }
        }
    } else {
        // UNCHECKED or GUARDED: decode sattr3
        match decode_sattr3(dec) {
            Ok(mode) => mode,
            Err(e) => {
                warn!("Failed to decode sattr3: {}", e);
                return ReplyBuilder::garbage_args(call.xid);
            }
        }
    };
    
    debug!("CREATE: name={}, mode={:#o}, create_mode={}", name, file_mode, create_mode);
    
    // Create file
    match fs.create(&dir_handle, &name, file_mode).await {
        Ok((file_handle, attrs)) => {
            debug!("CREATE succeeded: {}", name);
            
            // Get directory attributes after
            let dir_attrs = fs.getattr(&dir_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // post_op_fh3 (file handle)
            enc.encode_bool(true); // handle_follows
            file_handle.encode(enc);
            
            // post_op_attr (object attributes)
            enc.encode_bool(true); // obj_attributes_follow
            attrs.encode(enc);
            
            // dir_wcc (wcc_data for parent directory):
            enc.encode_bool(false); // pre_op_attr (skip)
            if let Some(attr) = dir_attrs {
                enc.encode_bool(true); // post_op_attr
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("CREATE failed: {}", e);
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            // post_op_fh3 (no handle on failure)
            enc.encode_bool(false);
            
            // post_op_attr (no attributes on failure)
            enc.encode_bool(false);
            
            // dir_wcc (skip both)
            enc.encode_bool(false);
            enc.encode_bool(false);
            
            reply.finish()
        }
    }
}

/// Handle MKDIR procedure (Procedure 9)
/// RFC 1813 Section 3.3.9
pub async fn handle_mkdir(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS MKDIR");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode directory name
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode sattr3 (set attributes)
    let dir_mode = match decode_sattr3(dec) {
        Ok(mode) => mode,
        Err(e) => {
            warn!("Failed to decode sattr3: {}", e);
            return ReplyBuilder::garbage_args(call.xid);
        }
    };
    
    debug!("MKDIR: name={}, mode={:#o}", name, dir_mode);
    
    // Create directory
    match fs.mkdir(&dir_handle, &name, dir_mode).await {
        Ok((file_handle, attrs)) => {
            debug!("MKDIR succeeded: {}", name);
            
            // Get parent directory attributes after
            let parent_attrs = fs.getattr(&dir_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // post_op_fh3 (new directory handle)
            enc.encode_bool(true); // handle_follows
            file_handle.encode(enc);
            
            // post_op_attr (new directory attributes)
            enc.encode_bool(true); // obj_attributes_follow
            attrs.encode(enc);
            
            // dir_wcc (parent directory wcc_data):
            enc.encode_bool(false); // pre_op_attr (skip)
            if let Some(attr) = parent_attrs {
                enc.encode_bool(true); // post_op_attr
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("MKDIR failed: {}", e);
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            // post_op_fh3 (no handle on failure)
            enc.encode_bool(false);
            
            // post_op_attr (no attributes on failure)
            enc.encode_bool(false);
            
            // dir_wcc (skip both)
            enc.encode_bool(false);
            enc.encode_bool(false);
            
            reply.finish()
        }
    }
}

/// Handle REMOVE procedure (Procedure 12)
/// RFC 1813 Section 3.3.12
pub async fn handle_remove(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS REMOVE");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode filename
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("REMOVE: name={}", name);
    
    // Remove file
    match fs.remove(&dir_handle, &name).await {
        Ok(()) => {
            debug!("REMOVE succeeded for: {}", name);
            
            // Get directory attributes after for post_op_attr
            let dir_attrs_after = fs.getattr(&dir_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // wcc_data structure:
            // - pre_op_attr before (optional - we skip for simplicity)
            enc.encode_bool(false);
            
            // - post_op_attr after (optional - full attributes)
            if let Some(attr) = dir_attrs_after {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("REMOVE failed for {}: {} (kind: {:?})", name, e, e.kind());
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            // wcc_data (we skip both)
            enc.encode_bool(false); // pre_op_attr
            enc.encode_bool(false); // post_op_attr
            
            reply.finish()
        }
    }
}

/// Handle RMDIR procedure (Procedure 13)
/// RFC 1813 Section 3.3.13
pub async fn handle_rmdir(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS RMDIR");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode directory name
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("RMDIR: name={}", name);
    
    // Remove directory
    match fs.rmdir(&dir_handle, &name).await {
        Ok(()) => {
            debug!("RMDIR succeeded for: {}", name);
            
            // Get directory attributes after
            let dir_attrs = fs.getattr(&dir_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // wcc_data for directory:
            // - pre_op_attr (we skip)
            enc.encode_bool(false);
            
            // - post_op_attr (provide if available)
            if let Some(attr) = dir_attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("RMDIR failed: {}", e);
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            // wcc_data (skip both)
            enc.encode_bool(false);
            enc.encode_bool(false);
            
            reply.finish()
        }
    }
}

/// Handle READDIR procedure (Procedure 16)
/// RFC 1813 Section 3.3.16
pub async fn handle_readdir(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS READDIR");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode cookie (resume point)
    let cookie = match dec.decode_u64() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode cookie verifier (we ignore for simplicity)
    let _cookieverf = match dec.decode_u64() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode count (max bytes to return)
    let count = match dec.decode_u32() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("READDIR: cookie={}, count={}", cookie, count);
    
    // Read directory
    match fs.readdir(&dir_handle, cookie, count).await {
        Ok(entries) => {
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Directory attributes (optional, we skip)
            enc.encode_bool(false);
            
            // Cookie verifier
            enc.encode_u64(0);
            
            // Encode entries
            for entry in &entries {
                enc.encode_bool(true); // value_follows
                enc.encode_u64(entry.fileid);
                enc.encode_string(&entry.name);
                enc.encode_u64(entry.cookie);
            }
            enc.encode_bool(false); // no more entries
            
            // EOF (true if all entries returned)
            enc.encode_bool(true);
            
            reply.finish()
        }
        Err(e) => {
            warn!("READDIR failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle READDIRPLUS procedure (Procedure 17)
/// RFC 1813 Section 3.3.17 - Like READDIR but also returns file handles and attributes
pub async fn handle_readdirplus(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS READDIRPLUS");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode cookie (resume point)
    let cookie = match dec.decode_u64() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode cookie verifier (we ignore for simplicity)
    let _cookieverf = match dec.decode_u64() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode dircount (max directory bytes)
    let _dircount = match dec.decode_u32() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode maxcount (max response bytes including attributes)
    let maxcount = match dec.decode_u32() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("READDIRPLUS: cookie={}, maxcount={}", cookie, maxcount);
    
    // Read directory
    match fs.readdir(&dir_handle, cookie, maxcount).await {
        Ok(entries) => {
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Directory attributes (optional, we skip)
            enc.encode_bool(false);
            
            // Cookie verifier
            enc.encode_u64(0);
            
            // Encode entries with file handles and attributes
            for entry in &entries {
                enc.encode_bool(true); // value_follows
                
                // File ID
                enc.encode_u64(entry.fileid);
                
                // Name
                enc.encode_string(&entry.name);
                
                // Cookie
                enc.encode_u64(entry.cookie);
                
                // Name attributes (optional - we provide them)
                if let Some(ref attr) = entry.attr {
                    enc.encode_bool(true);
                    attr.encode(enc);
                } else {
                    enc.encode_bool(false);
                }
                
                // Name handle (optional - we provide it)
                // Look up the child to get its file handle
                match fs.lookup(&dir_handle, &entry.name).await {
                    Ok((child_fh, _)) => {
                        debug!("READDIRPLUS: Got handle for {}", entry.name);
                        enc.encode_bool(true);
                        child_fh.encode(enc);
                    }
                    Err(e) => {
                        warn!("READDIRPLUS: Failed to get handle for {}: {}", entry.name, e);
                        enc.encode_bool(false);
                    }
                }
            }
            enc.encode_bool(false); // no more entries
            
            // EOF (true if all entries returned)
            enc.encode_bool(true);
            
            reply.finish()
        }
        Err(e) => {
            warn!("READDIRPLUS failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle FSSTAT procedure (Procedure 18)
/// RFC 1813 Section 3.3.18
pub async fn handle_fsstat(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS FSSTAT");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Get filesystem stats
    match fs.statfs(&file_handle).await {
        Ok(stat) => {
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Object attributes (optional, we skip)
            enc.encode_bool(false);
            
            // Filesystem statistics
            stat.encode(enc);
            
            reply.finish()
        }
        Err(e) => {
            warn!("FSSTAT failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle FSINFO procedure (Procedure 19)
/// RFC 1813 Section 3.3.19
pub async fn handle_fsinfo(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS FSINFO");
    
    // Decode file handle (not actually used, but required by protocol)
    let _file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    let info = fs.fsinfo();
    
    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();
    
    // Status: NFS3_OK
    enc.encode_u32(NFS3Status::Ok as u32);
    
    // Object attributes (optional, we skip)
    enc.encode_bool(false);
    
    // Filesystem info
    info.encode(enc);
    
    reply.finish()
}

/// Handle READLINK procedure (Procedure 5)
/// RFC 1813 Section 3.3.5
pub async fn handle_readlink(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS READLINK");
    
    // Decode symlink file handle
    let symlink_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Read the symlink target
    match fs.readlink(&symlink_handle).await {
        Ok(target) => {
            let attrs = fs.getattr(&symlink_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Symlink attributes (optional)
            if let Some(attr) = attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            // Symlink target path
            enc.encode_string(&target);
            
            reply.finish()
        }
        Err(e) => {
            warn!("READLINK failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle SYMLINK procedure (Procedure 10)
/// RFC 1813 Section 3.3.10
pub async fn handle_symlink(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS SYMLINK");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode symlink name
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode sattr3 (we ignore but must decode)
    if let Err(e) = decode_sattr3(dec) {
        warn!("Failed to decode sattr3: {}", e);
        return ReplyBuilder::garbage_args(call.xid);
    }
    
    // Decode target path
    let target = match dec.decode_string() {
        Ok(t) => t,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("SYMLINK: name={}, target={}", name, target);
    
    // Create symlink
    match fs.symlink(&dir_handle, &name, &target).await {
        Ok((file_handle, attrs)) => {
            debug!("SYMLINK succeeded: {} -> {}", name, target);
            
            // Get directory attributes after
            let dir_attrs = fs.getattr(&dir_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // post_op_fh3
            enc.encode_bool(true);
            file_handle.encode(enc);
            
            // post_op_attr
            enc.encode_bool(true);
            attrs.encode(enc);
            
            // dir_wcc:
            enc.encode_bool(false); // pre_op_attr
            if let Some(attr) = dir_attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("SYMLINK failed: {}", e);
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            enc.encode_bool(false); // post_op_fh3
            enc.encode_bool(false); // post_op_attr
            enc.encode_bool(false); // dir_wcc pre
            enc.encode_bool(false); // dir_wcc post
            
            reply.finish()
        }
    }
}

/// Handle MKNOD procedure (Procedure 11)
/// RFC 1813 Section 3.3.11 - Create special files (FIFO, socket)
pub async fn handle_mknod(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS MKNOD");
    
    // Decode directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode name
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode type (what kind of special file)
    let ftype = match dec.decode_u32() {
        Ok(t) => t,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode sattr3
    if let Err(e) = decode_sattr3(dec) {
        warn!("Failed to decode sattr3: {}", e);
        return ReplyBuilder::garbage_args(call.xid);
    }
    
    // For device files, decode device specifier (major, minor)
    if ftype == 3 || ftype == 4 {
        // Block or char device - decode but we don't support creating these
        let _ = dec.decode_u32(); // major
        let _ = dec.decode_u32(); // minor
        
        warn!("MKNOD: Device file creation not supported");
        return error_reply(call.xid, NFS3Status::NotSupported);
    }
    
    debug!("MKNOD: name={}, type={}", name, ftype);
    
    // Create the special file
    match fs.mknod(&dir_handle, &name, ftype).await {
        Ok((file_handle, attrs)) => {
            debug!("MKNOD succeeded: {} (type {})", name, ftype);
            
            // Get directory attributes after
            let dir_attrs = fs.getattr(&dir_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // post_op_fh3
            enc.encode_bool(true);
            file_handle.encode(enc);
            
            // post_op_attr
            enc.encode_bool(true);
            attrs.encode(enc);
            
            // dir_wcc:
            enc.encode_bool(false); // pre_op_attr
            if let Some(attr) = dir_attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("MKNOD failed: {}", e);
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            enc.encode_bool(false); // post_op_fh3
            enc.encode_bool(false); // post_op_attr
            enc.encode_bool(false); // dir_wcc pre
            enc.encode_bool(false); // dir_wcc post
            
            reply.finish()
        }
    }
}

/// Handle LINK procedure (Procedure 15)
/// RFC 1813 Section 3.3.15
pub async fn handle_link(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS LINK");
    
    // Decode file handle (file to link from)
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode link directory handle
    let dir_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode link name
    let name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("LINK: name={}", name);
    
    // Create hard link
    match fs.link(&file_handle, &dir_handle, &name).await {
        Ok(attrs) => {
            debug!("LINK succeeded: {}", name);
            
            // Get directory attributes after
            let dir_attrs = fs.getattr(&dir_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // File attributes (post_op_attr - updated link count)
            enc.encode_bool(true);
            attrs.encode(enc);
            
            // linkdir_wcc (wcc_data):
            enc.encode_bool(false); // pre_op_attr
            if let Some(attr) = dir_attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("LINK failed: {}", e);
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            // File attributes (skip)
            enc.encode_bool(false);
            
            // linkdir_wcc (skip both)
            enc.encode_bool(false);
            enc.encode_bool(false);
            
            reply.finish()
        }
    }
}

/// Handle RENAME procedure (Procedure 14)
/// RFC 1813 Section 3.3.14
pub async fn handle_rename(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS RENAME");
    
    // Decode from directory handle
    let from_dir = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode from_dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode from name
    let from_name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Decode to directory handle
    let to_dir = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode to_dir handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode to name
    let to_name = match dec.decode_string() {
        Ok(n) => n,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("RENAME: {} -> {}", from_name, to_name);
    
    // Rename
    match fs.rename(&from_dir, &from_name, &to_dir, &to_name).await {
        Ok(()) => {
            debug!("RENAME succeeded: {} -> {}", from_name, to_name);
            
            // Get directory attributes after
            let from_dir_attrs = fs.getattr(&from_dir).await.ok();
            let to_dir_attrs = fs.getattr(&to_dir).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // fromdir_wcc (wcc_data):
            enc.encode_bool(false); // pre_op_attr
            if let Some(attr) = from_dir_attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            // todir_wcc (wcc_data):
            enc.encode_bool(false); // pre_op_attr
            if let Some(attr) = to_dir_attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("RENAME failed: {}", e);
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            enc.encode_u32(NFS3Status::from_io_error(&e) as u32);
            
            // fromdir_wcc (skip both)
            enc.encode_bool(false);
            enc.encode_bool(false);
            
            // todir_wcc (skip both)
            enc.encode_bool(false);
            enc.encode_bool(false);
            
            reply.finish()
        }
    }
}

/// Handle COMMIT procedure (Procedure 21)
/// RFC 1813 Section 3.3.21
pub async fn handle_commit(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS COMMIT");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Decode offset and count (we ignore - commit entire file)
    let _offset = match dec.decode_u64() {
        Ok(o) => o,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    let _count = match dec.decode_u32() {
        Ok(c) => c,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // Commit (sync to disk)
    match fs.commit(&file_handle).await {
        Ok(()) => {
            let attrs = fs.getattr(&file_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // File attributes before (we skip)
            enc.encode_bool(false);
            
            // File attributes after
            if let Some(attr) = attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            // Write verifier (for detecting server reboots)
            enc.encode_u64(0);
            
            reply.finish()
        }
        Err(e) => {
            warn!("COMMIT failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
        }
    }
}

/// Handle PATHCONF procedure (Procedure 20)
/// RFC 1813 Section 3.3.20 - Retrieve POSIX information
pub async fn handle_pathconf(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS PATHCONF");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            return error_reply(call.xid, NFS3Status::BadHandle);
        }
    };
    
    // Get attributes
    let attrs = fs.getattr(&file_handle).await.ok();
    
    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();
    
    // Status: NFS3_OK
    enc.encode_u32(NFS3Status::Ok as u32);
    
    // Object attributes (optional)
    if let Some(attr) = attrs {
        enc.encode_bool(true);
        attr.encode(enc);
    } else {
        enc.encode_bool(false);
    }
    
    // PATHCONF result (POSIX pathconf values)
    enc.encode_u32(255);        // linkmax: max hard links
    enc.encode_u32(255);        // name_max: max filename length
    enc.encode_bool(false);     // no_trunc: don't truncate long names
    enc.encode_bool(false);     // chown_restricted: chown is restricted
    enc.encode_bool(true);      // case_insensitive: filenames case insensitive (false for Linux)
    enc.encode_bool(true);      // case_preserving: preserve case in names
    
    reply.finish()
}

/// Helper to create an error reply
fn error_reply(xid: u32, status: NFS3Status) -> Bytes {
    let mut reply = ReplyBuilder::success(xid);
    let enc = reply.encoder();
    enc.encode_u32(status as u32);
    reply.finish()
}

/// Decode sattr3 structure (set attributes) - RFC 1813 Section 1.3.3
/// For simplicity, we decode but ignore most fields and just extract mode if present
fn decode_sattr3(dec: &mut XdrDecoder) -> Result<u32, String> {
    // mode: optional<u32>
    let mode = if dec.decode_bool()? {
        Some(dec.decode_u32()?)
    } else {
        None
    };
    
    // uid: optional<u32>
    if dec.decode_bool()? {
        let _ = dec.decode_u32()?;
    }
    
    // gid: optional<u32>
    if dec.decode_bool()? {
        let _ = dec.decode_u32()?;
    }
    
    // size: optional<u64>
    if dec.decode_bool()? {
        let _ = dec.decode_u64()?;
    }
    
    // atime: optional<time_how>
    let atime_how = dec.decode_u32()?;
    if atime_how == 2 {
        // SET_TO_CLIENT_TIME
        let _ = dec.decode_u32()?; // seconds
        let _ = dec.decode_u32()?; // nanoseconds
    }
    
    // mtime: optional<time_how>
    let mtime_how = dec.decode_u32()?;
    if mtime_how == 2 {
        // SET_TO_CLIENT_TIME
        let _ = dec.decode_u32()?; // seconds
        let _ = dec.decode_u32()?; // nanoseconds
    }
    
    Ok(mode.unwrap_or(0o644))
}

