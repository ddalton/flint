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
    
    // Decode create mode
    let _mode = match dec.decode_u32() {
        Ok(m) => m,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    // For UNCHECKED and GUARDED modes, decode sattr3 (set attributes)
    // For simplicity, we just skip the attributes and use default mode
    let file_mode = 0o644u32;
    
    debug!("CREATE: name={}", name);
    
    // Create file
    match fs.create(&dir_handle, &name, file_mode).await {
        Ok((file_handle, attrs)) => {
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // File handle
            enc.encode_bool(true); // handle_follows
            file_handle.encode(enc);
            
            // Object attributes
            enc.encode_bool(true); // obj_attributes_follow
            attrs.encode(enc);
            
            // Directory attributes (we skip)
            enc.encode_bool(false);
            
            reply.finish()
        }
        Err(e) => {
            warn!("CREATE failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
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
    
    // Decode attributes (we skip and use default mode)
    let dir_mode = 0o755u32;
    
    debug!("MKDIR: name={}", name);
    
    // Create directory
    match fs.mkdir(&dir_handle, &name, dir_mode).await {
        Ok((file_handle, attrs)) => {
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // File handle
            enc.encode_bool(true); // handle_follows
            file_handle.encode(enc);
            
            // Object attributes
            enc.encode_bool(true); // obj_attributes_follow
            attrs.encode(enc);
            
            // Directory attributes (we skip)
            enc.encode_bool(false);
            
            reply.finish()
        }
        Err(e) => {
            warn!("MKDIR failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
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
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Directory attributes (we skip)
            enc.encode_bool(false);
            
            reply.finish()
        }
        Err(e) => {
            warn!("REMOVE failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
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
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Directory attributes (we skip)
            enc.encode_bool(false);
            
            reply.finish()
        }
        Err(e) => {
            warn!("RMDIR failed: {}", e);
            error_reply(call.xid, NFS3Status::from_io_error(&e))
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

/// Helper to create an error reply
fn error_reply(xid: u32, status: NFS3Status) -> Bytes {
    let mut reply = ReplyBuilder::success(xid);
    let enc = reply.encoder();
    enc.encode_u32(status as u32);
    reply.finish()
}

