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
    let (set_mode, set_size) = decode_setattr_fields(dec);

    // Decode guard (optional time check)
    let _guard_check = match dec.decode_bool() {
        Ok(b) => b,
        Err(_) => return ReplyBuilder::garbage_args(call.xid),
    };

    debug!("SETATTR: mode={:?}, size={:?}", set_mode, set_size);

    // Apply the changes
    let result = async {
        match (set_mode, set_size) {
            (Some(mode), Some(size)) => {
                // Set both mode and size
                fs.setattr_mode(&file_handle, mode).await?;
                fs.setattr_size(&file_handle, size).await
            }
            (Some(mode), None) => {
                // Set mode only
                fs.setattr_mode(&file_handle, mode).await
            }
            (None, Some(size)) => {
                // Set size only (truncate/extend)
                fs.setattr_size(&file_handle, size).await
            }
            (None, None) => {
                // No changes requested
                Ok(())
            }
        }
    }
    .await;
    
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

/// Decode sattr3 and extract mode and size if present
fn decode_setattr_fields(dec: &mut XdrDecoder) -> (Option<u32>, Option<u64>) {
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

    // size (for truncate/ftruncate)
    let size = if dec.decode_bool().unwrap_or(false) {
        dec.decode_u64().ok()
    } else {
        None
    };

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

    (mode, size)
}

