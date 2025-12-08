//! NLM (Network Lock Manager) Protocol Implementation
//!
//! Implements NFSv3 file locking using the NLM protocol (RFC 1813 Appendix I).
//! Program 100021, Versions 1-4.
//!
//! ## Performance
//! - Uses lock-free LockManager with DashMap
//! - Per-file lock storage (no global contention)
//! - Fast path for files with no locks

use super::lock_manager::{ClientId, Lock, LockManager, LockResult, LockType};
use super::rpc::{CallMessage, ReplyBuilder};
use super::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;
use std::sync::Arc;
use tracing::{debug, info, warn};

// NLM program and version
pub const NLM_PROGRAM: u32 = 100021;
pub const NLM_VERSION: u32 = 4;

// NLM procedures
const NLM_NULL: u32 = 0;
const NLM_TEST: u32 = 1;
const NLM_LOCK: u32 = 2;
const NLM_CANCEL: u32 = 3;
const NLM_UNLOCK: u32 = 4;
const NLM_GRANTED: u32 = 5;
const NLM_TEST_MSG: u32 = 6;
const NLM_LOCK_MSG: u32 = 7;
const NLM_CANCEL_MSG: u32 = 8;
const NLM_UNLOCK_MSG: u32 = 9;
const NLM_GRANTED_MSG: u32 = 10;
const NLM_TEST_RES: u32 = 11;
const NLM_LOCK_RES: u32 = 12;
const NLM_CANCEL_RES: u32 = 13;
const NLM_UNLOCK_RES: u32 = 14;
const NLM_GRANTED_RES: u32 = 15;
const NLM_SHARE: u32 = 20;
const NLM_UNSHARE: u32 = 21;
const NLM_NM_LOCK: u32 = 22;
const NLM_FREE_ALL: u32 = 23;

// NLM status codes
#[repr(u32)]
#[derive(Debug, Clone, Copy)]
enum NlmStat {
    Granted = 0,
    Denied = 1,
    DeniedNoLocks = 2,
    Blocked = 3,
    DeniedGracePeriod = 4,
    Deadlock = 5,
}

/// NLM service
pub struct NlmService {
    lock_manager: Arc<LockManager>,
}

impl NlmService {
    pub fn new() -> Self {
        Self {
            lock_manager: Arc::new(LockManager::new()),
        }
    }

    pub fn lock_manager(&self) -> Arc<LockManager> {
        self.lock_manager.clone()
    }

    /// Handle NLM RPC call
    pub async fn handle_call(&self, call: &CallMessage, dec: &mut XdrDecoder) -> Bytes {
        let proc_name = match call.procedure {
            NLM_NULL => "NLM_NULL",
            NLM_TEST => "NLM_TEST",
            NLM_LOCK => "NLM_LOCK",
            NLM_CANCEL => "NLM_CANCEL",
            NLM_UNLOCK => "NLM_UNLOCK",
            NLM_GRANTED => "NLM_GRANTED",
            NLM_TEST_MSG => "NLM_TEST_MSG",
            NLM_LOCK_MSG => "NLM_LOCK_MSG",
            NLM_CANCEL_MSG => "NLM_CANCEL_MSG",
            NLM_UNLOCK_MSG => "NLM_UNLOCK_MSG",
            NLM_GRANTED_MSG => "NLM_GRANTED_MSG",
            NLM_FREE_ALL => "NLM_FREE_ALL",
            _ => "UNKNOWN",
        };
        info!(">>> NLM procedure {} ({})", proc_name, call.procedure);

        match call.procedure {
            NLM_NULL => self.handle_null(call).await,
            NLM_TEST => self.handle_test(call, dec).await,
            NLM_LOCK => self.handle_lock(call, dec).await,
            NLM_UNLOCK => self.handle_unlock(call, dec).await,
            NLM_CANCEL => self.handle_cancel(call, dec).await,
            NLM_SHARE | NLM_UNSHARE => {
                warn!("NLM_SHARE/UNSHARE not implemented");
                ReplyBuilder::success(call.xid).finish()
            }
            NLM_FREE_ALL => self.handle_free_all(call, dec).await,
            _ => {
                warn!("Unknown NLM procedure: {}", call.procedure);
                ReplyBuilder::proc_unavail(call.xid)
            }
        }
    }

    async fn handle_null(&self, call: &CallMessage) -> Bytes {
        debug!("NLM_NULL");
        ReplyBuilder::success(call.xid).finish()
    }

    async fn handle_test(&self, call: &CallMessage, dec: &mut XdrDecoder) -> Bytes {
        debug!("NLM_TEST");

        // Decode nlm4_testargs
        let (cookie, exclusive, file_id, owner, offset, length) = match Self::decode_lock_args(dec) {
            Ok(args) => args,
            Err(_) => return ReplyBuilder::garbage_args(call.xid),
        };

        let lock = Lock {
            owner,
            lock_type: if exclusive {
                LockType::Write
            } else {
                LockType::Read
            },
            offset,
            length,
        };

        let result = self.lock_manager.test_lock(file_id, &lock);

        // Build reply
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Encode cookie
        Self::encode_cookie(enc, &cookie);

        // Encode result
        match result {
            LockResult::Granted => {
                enc.encode_u32(NlmStat::Granted as u32);
            }
            LockResult::Denied | LockResult::AlreadyHeld => {
                enc.encode_u32(NlmStat::Denied as u32);
                // Encode holder (simplified - just indicate lock is held)
                enc.encode_bool(true); // exclusive
                enc.encode_u32(0); // svid
                Self::encode_netobj(enc, &[0u8; 4]); // oh (opaque handle)
                enc.encode_u64(offset);
                enc.encode_u64(length);
            }
        }

        reply.finish()
    }

    async fn handle_lock(&self, call: &CallMessage, dec: &mut XdrDecoder) -> Bytes {
        info!("    NLM_LOCK: Decoding lock arguments...");

        // Decode nlm4_lockargs
        let (cookie, block, exclusive, file_id, owner, offset, length) = match Self::decode_lock_args_with_block(dec) {
            Ok(args) => {
                info!("    NLM_LOCK: Decoded successfully");
                args
            }
            Err(_) => {
                warn!("    NLM_LOCK: Failed to decode arguments - returning GARBAGE_ARGS");
                return ReplyBuilder::garbage_args(call.xid);
            }
        };

        info!("    NLM_LOCK: file_id={}, owner={:?}, type={}, offset={}, length={}, block={}",
            file_id, owner, if exclusive { "WRITE" } else { "READ" }, offset, length, block);

        let lock = Lock {
            owner: owner.clone(),
            lock_type: if exclusive {
                LockType::Write
            } else {
                LockType::Read
            },
            offset,
            length,
        };

        let result = self.lock_manager.lock(file_id, lock);
        info!("    NLM_LOCK: Lock result = {:?}", result);

        // Build reply
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Encode cookie
        Self::encode_cookie(enc, &cookie);

        // Encode status
        let status = match result {
            LockResult::Granted => NlmStat::Granted,
            LockResult::Denied => NlmStat::Denied,
            LockResult::AlreadyHeld => NlmStat::Granted, // Already held by us
        };

        info!("    NLM_LOCK: Replying with status {:?}", status);
        enc.encode_u32(status as u32);

        let response = reply.finish();
        info!("<<< NLM_LOCK completed ({} bytes)", response.len());
        response
    }

    async fn handle_unlock(&self, call: &CallMessage, dec: &mut XdrDecoder) -> Bytes {
        info!("    NLM_UNLOCK: Decoding unlock arguments...");

        // Decode nlm4_unlockargs
        let (cookie, file_id, owner, offset, length) = match Self::decode_unlock_args(dec) {
            Ok(args) => {
                info!("    NLM_UNLOCK: Decoded successfully");
                args
            }
            Err(_) => {
                warn!("    NLM_UNLOCK: Failed to decode arguments - returning GARBAGE_ARGS");
                return ReplyBuilder::garbage_args(call.xid);
            }
        };

        info!("    NLM_UNLOCK: file_id={}, owner={:?}, offset={}, length={}",
            file_id, owner, offset, length);

        let success = self.lock_manager.unlock(file_id, &owner, offset, length);
        info!("    NLM_UNLOCK: Unlock success = {}", success);

        // Build reply
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Encode cookie
        Self::encode_cookie(enc, &cookie);

        // Status: always granted (unlock is idempotent)
        enc.encode_u32(NlmStat::Granted as u32);

        let response = reply.finish();
        info!("<<< NLM_UNLOCK completed");
        response
    }

    async fn handle_cancel(&self, call: &CallMessage, dec: &mut XdrDecoder) -> Bytes {
        info!("    NLM_CANCEL: Decoding cancel arguments...");

        // Decode same as unlock for simplicity
        let (cookie, file_id, owner, offset, length) = match Self::decode_unlock_args(dec) {
            Ok(args) => {
                info!("    NLM_CANCEL: Decoded successfully");
                args
            }
            Err(_) => {
                warn!("    NLM_CANCEL: Failed to decode arguments - returning GARBAGE_ARGS");
                return ReplyBuilder::garbage_args(call.xid);
            }
        };

        info!("    NLM_CANCEL: file_id={}, owner={:?}, offset={}, length={}",
            file_id, owner, offset, length);

        // Cancel is same as unlock for our simplified implementation
        let success = self.lock_manager.unlock(file_id, &owner, offset, length);
        info!("    NLM_CANCEL: Cancel success = {}", success);

        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        Self::encode_cookie(enc, &cookie);
        enc.encode_u32(NlmStat::Granted as u32);

        let response = reply.finish();
        info!("<<< NLM_CANCEL completed");
        response
    }

    async fn handle_free_all(&self, call: &CallMessage, dec: &mut XdrDecoder) -> Bytes {
        debug!("NLM_FREE_ALL");

        // Decode client name (simplified)
        let client_name = match Self::decode_string(dec) {
            Ok(name) => name,
            Err(_) => return ReplyBuilder::garbage_args(call.xid),
        };

        let _state = dec.decode_u32().unwrap_or(0);

        // Create client ID from name
        let client = ClientId {
            addr: client_name,
            pid: 0, // Unknown
        };

        info!("NLM_FREE_ALL for client: {:?}", client);
        self.lock_manager.unlock_all_for_client(&client);

        ReplyBuilder::success(call.xid).finish()
    }

    // Helper functions for encoding/decoding
    fn decode_lock_args(dec: &mut XdrDecoder) -> Result<(Vec<u8>, bool, u64, ClientId, u64, u64), ()> {
        let cookie = Self::decode_cookie(dec)?;
        let _block = dec.decode_bool().map_err(|_| ())?;
        let exclusive = dec.decode_bool().map_err(|_| ())?;
        let (file_id, owner, offset, length) = Self::decode_lock_info(dec)?;
        Ok((cookie, exclusive, file_id, owner, offset, length))
    }

    fn decode_lock_args_with_block(dec: &mut XdrDecoder) -> Result<(Vec<u8>, bool, bool, u64, ClientId, u64, u64), ()> {
        let cookie = Self::decode_cookie(dec)?;
        let block = dec.decode_bool().map_err(|_| ())?;
        let exclusive = dec.decode_bool().map_err(|_| ())?;
        let (file_id, owner, offset, length) = Self::decode_lock_info(dec)?;
        Ok((cookie, block, exclusive, file_id, owner, offset, length))
    }

    fn decode_unlock_args(dec: &mut XdrDecoder) -> Result<(Vec<u8>, u64, ClientId, u64, u64), ()> {
        let cookie = Self::decode_cookie(dec)?;
        let (file_id, owner, offset, length) = Self::decode_lock_info(dec)?;
        Ok((cookie, file_id, owner, offset, length))
    }

    fn decode_lock_info(dec: &mut XdrDecoder) -> Result<(u64, ClientId, u64, u64), ()> {
        // Decode caller_name
        let caller_name = Self::decode_string(dec)?;

        // Decode fh (file handle) - simplified: extract inode
        let fh_data = Self::decode_netobj(dec)?;
        let file_id = if fh_data.len() >= 8 {
            u64::from_le_bytes([
                fh_data[0], fh_data[1], fh_data[2], fh_data[3],
                fh_data[4], fh_data[5], fh_data[6], fh_data[7],
            ])
        } else {
            0
        };

        // Decode oh (owner handle) - used as pid
        let oh_data = Self::decode_netobj(dec)?;
        let pid = if oh_data.len() >= 4 {
            u32::from_le_bytes([oh_data[0], oh_data[1], oh_data[2], oh_data[3]])
        } else {
            0
        };

        // Decode svid (server ID)
        let _svid = dec.decode_u32().map_err(|_| ())?;

        // Decode offset and length
        let offset = dec.decode_u64().map_err(|_| ())?;
        let length = dec.decode_u64().map_err(|_| ())?;

        let owner = ClientId {
            addr: caller_name,
            pid,
        };

        Ok((file_id, owner, offset, length))
    }

    fn decode_cookie(dec: &mut XdrDecoder) -> Result<Vec<u8>, ()> {
        Self::decode_netobj(dec)
    }

    fn decode_netobj(dec: &mut XdrDecoder) -> Result<Vec<u8>, ()> {
        // Use XDR's decode_opaque which handles length and padding automatically
        dec.decode_opaque().map(|b| b.to_vec()).map_err(|_| ())
    }

    fn decode_string(dec: &mut XdrDecoder) -> Result<String, ()> {
        let data = Self::decode_netobj(dec)?;
        String::from_utf8(data).map_err(|_| ())
    }

    fn encode_cookie(enc: &mut XdrEncoder, cookie: &[u8]) {
        Self::encode_netobj(enc, cookie);
    }

    fn encode_netobj(enc: &mut XdrEncoder, data: &[u8]) {
        // Use XDR's encode_opaque which handles length and padding automatically
        enc.encode_opaque(data);
    }
}
