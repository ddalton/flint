//! SETATTR implementation - Change file attributes
//!
//! Separate module for SETATTR to keep handlers.rs cleaner

use super::protocol::{FileHandle, NFS3Status};
use super::rpc::{CallMessage, ReplyBuilder};
use super::vfs::LocalFilesystem;
use super::xdr::XdrDecoder;
use bytes::Bytes;
use std::sync::Arc;
use tracing::{debug, warn};

/// Handle SETATTR procedure (Procedure 2)
/// RFC 1813 Section 3.3.2
pub async fn handle_setattr(
    fs: Arc<LocalFilesystem>,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    debug!("NFS SETATTR");
    
    // Decode file handle
    let file_handle = match FileHandle::decode(dec) {
        Ok(fh) => fh,
        Err(e) => {
            warn!("Failed to decode file handle: {}", e);
            let mut reply = ReplyBuilder::success(call.xid);
            reply.encoder().encode_u32(NFS3Status::BadHandle as u32);
            return reply.finish();
        }
    };
    
    // Decode sattr3 (attributes to set)
    let set_mode = decode_setattr_fields(dec);
    
    // Decode guard (optional time check)
    let _guard_check = match dec.decode_bool() {
        Ok(b) => b,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };
    
    debug!("SETATTR: mode={:?}", set_mode);
    
    // Apply the changes
    let result = if let Some(mode) = set_mode {
        fs.setattr_mode(&file_handle, mode).await
    } else {
        Ok(()) // No changes requested
    };
    
    match result {
        Ok(()) => {
            // Get updated attributes
            let attrs = fs.getattr(&file_handle).await.ok();
            
            let mut reply = ReplyBuilder::success(call.xid);
            let enc = reply.encoder();
            
            // Status: NFS3_OK
            enc.encode_u32(NFS3Status::Ok as u32);
            
            // Object attributes before (we skip)
            enc.encode_bool(false);
            
            // Object attributes after
            if let Some(attr) = attrs {
                enc.encode_bool(true);
                attr.encode(enc);
            } else {
                enc.encode_bool(false);
            }
            
            reply.finish()
        }
        Err(e) => {
            warn!("SETATTR failed: {}", e);
            let mut reply = ReplyBuilder::success(call.xid);
            reply.encoder().encode_u32(NFS3Status::from_io_error(&e) as u32);
            reply.finish()
        }
    }
}

/// Decode sattr3 and extract mode if present
fn decode_setattr_fields(dec: &mut XdrDecoder) -> Option<u32> {
    // mode
    let mode = if dec.decode_bool().unwrap_or(false) {
        dec.decode_u32().ok()
    } else {
        None
    };
    
    // uid
    if dec.decode_bool().unwrap_or(false) {
        let _ = dec.decode_u32();
    }
    
    // gid
    if dec.decode_bool().unwrap_or(false) {
        let _ = dec.decode_u32();
    }
    
    // size
    if dec.decode_bool().unwrap_or(false) {
        let _ = dec.decode_u64();
    }
    
    // atime
    let atime_how = dec.decode_u32().unwrap_or(0);
    if atime_how == 2 {
        let _ = dec.decode_u32();
        let _ = dec.decode_u32();
    }
    
    // mtime
    let mtime_how = dec.decode_u32().unwrap_or(0);
    if mtime_how == 2 {
        let _ = dec.decode_u32();
        let _ = dec.decode_u32();
    }
    
    mode
}

