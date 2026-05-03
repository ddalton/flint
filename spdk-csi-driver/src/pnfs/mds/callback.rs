//! pNFS Callback Operations
//!
//! Implements NFSv4.1 callback operations for pNFS, specifically CB_LAYOUTRECALL
//! which is used to recall layouts from clients when data servers fail or
//! layouts need to be revoked.
//!
//! # Protocol References
//! - RFC 8881 Section 20.5 - CB_LAYOUTRECALL operation
//! - RFC 8881 Section 12.5.5 - Layout Recall

use crate::nfs::v4::protocol::{Nfs4Status, SessionId};
use crate::pnfs::mds::layout::LayoutStateId;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Callback channel manager
///
/// Manages callback channels to clients for sending CB_LAYOUTRECALL
/// and other callback operations.
pub struct CallbackManager {
    /// Map of session ID to callback channel
    channels: Arc<RwLock<HashMap<SessionId, CallbackChannel>>>,
}

/// Callback channel to a specific client
struct CallbackChannel {
    callback_addr: Option<String>,  // Client callback address (from CREATE_SESSION)
    callback_prog: u32,              // Callback program number
}

impl CallbackManager {
    /// Create a new callback manager
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a callback channel for a session
    pub async fn register_channel(
        &self,
        session_id: SessionId,
        callback_addr: Option<String>,
        callback_prog: u32,
        _callback_sec: Vec<u32>,
    ) {
        let mut channels = self.channels.write().await;
        channels.insert(
            session_id,
            CallbackChannel {
                callback_addr: callback_addr.clone(),
                callback_prog,
            },
        );
        info!(
            "Registered callback channel for session {:?}, addr={:?}, prog={}",
            session_id, callback_addr, callback_prog
        );
    }

    /// Unregister a callback channel
    pub async fn unregister_channel(&self, session_id: &SessionId) {
        let mut channels = self.channels.write().await;
        channels.remove(session_id);
        info!("Unregistered callback channel for session {:?}", session_id);
    }

    /// Send CB_LAYOUTRECALL to a client
    ///
    /// # Arguments
    /// * `session_id` - Client session to recall from
    /// * `layout_stateid` - Layout to recall
    /// * `layout_type` - Type of layout being recalled
    /// * `iomode` - I/O mode to recall (READ, RW, or ANY)
    /// * `changed` - Whether layout has changed
    ///
    /// # Returns
    /// Ok(true) if recall was sent successfully, Ok(false) if client not found
    pub async fn send_layoutrecall(
        &self,
        session_id: &SessionId,
        layout_stateid: &LayoutStateId,
        layout_type: u32,
        iomode: u32,
        changed: bool,
    ) -> Result<bool, String> {
        let channels = self.channels.read().await;

        if let Some(channel) = channels.get(session_id) {
            info!(
                "📢 Sending CB_LAYOUTRECALL to session {:?}, stateid={:?}",
                session_id,
                &layout_stateid[0..4]
            );

            // Encode CB_COMPOUND with CB_LAYOUTRECALL
            let cb_compound = self.encode_cb_layoutrecall(
                session_id,
                layout_stateid,
                layout_type,
                iomode,
                changed,
            )?;

            debug!("CB_LAYOUTRECALL parameters:");
            debug!("  layout_type: {}", layout_type);
            debug!("  iomode: {}", iomode);
            debug!("  changed: {}", changed);
            debug!("  encoded size: {} bytes", cb_compound.len());

            // Send callback RPC
            if let Some(addr) = &channel.callback_addr {
                match self.send_callback_rpc(addr, channel.callback_prog, &cb_compound).await {
                    Ok(_) => {
                        info!("✅ CB_LAYOUTRECALL sent successfully to {}", addr);
                        Ok(true)
                    }
                    Err(e) => {
                        warn!("❌ Failed to send CB_LAYOUTRECALL to {}: {}", addr, e);
                        // Don't fail - client might be temporarily unreachable
                        // Server will retry or client will return layouts on next operation
                        Ok(false)
                    }
                }
            } else {
                warn!("⚠️ No callback address for session {:?}", session_id);
                Ok(false)
            }
        } else {
            warn!(
                "⚠️ No callback channel for session {:?}, cannot send CB_LAYOUTRECALL",
                session_id
            );
            Ok(false)
        }
    }

    /// Encode CB_COMPOUND with a CB_SEQUENCE + CB_LAYOUTRECALL pair.
    ///
    /// Args (no RPC envelope) — Phase A.3 will wrap this in
    /// `cb_compound::encode_cb_call(xid, cb_program, …)` and push the
    /// result onto a back-channel writer. Keeping the args step
    /// separate keeps the encoder side fully decoupled from the
    /// connection lifetime.
    fn encode_cb_layoutrecall(
        &self,
        session_id: &SessionId,
        layout_stateid: &LayoutStateId,
        layout_type: u32,
        iomode: u32,
        changed: bool,
    ) -> Result<Bytes, String> {
        use crate::nfs::v4::cb_compound::{CbCompoundCall, CbOp, LayoutRecall};
        use crate::nfs::v4::protocol::StateId;

        // The `LayoutStateId` alias is a 16-byte blob (seqid:4 +
        // other:12, big-endian). Crack it back into the typed
        // `StateId` the CB encoder takes — the wire layout is
        // identical.
        let stateid = StateId {
            seqid: u32::from_be_bytes([
                layout_stateid[0],
                layout_stateid[1],
                layout_stateid[2],
                layout_stateid[3],
            ]),
            other: {
                let mut o = [0u8; 12];
                o.copy_from_slice(&layout_stateid[4..16]);
                o
            },
        };

        let call = CbCompoundCall {
            tag: String::new(),
            minorversion: 1,
            callback_ident: 0,
            ops: vec![
                CbOp::Sequence {
                    sessionid: *session_id,
                    // Slot 0, seqid 1 — Phase A.3 will track per-
                    // session back-channel slot state. The first
                    // ever call uses slot=0, seqid=1 (matches what
                    // the forward-channel SEQUENCE handler expects).
                    sequenceid: 1,
                    slotid: 0,
                    highest_slotid: 0,
                    cachethis: false,
                },
                CbOp::LayoutRecall {
                    layout_type,
                    iomode,
                    changed,
                    recall: LayoutRecall::File {
                        // Empty filehandle = "any layout for this
                        // session." Linux's client treats it as a
                        // session-wide return. When we recall a
                        // specific file we'll plumb the FH through.
                        fh: Vec::new(),
                        offset: 0,
                        length: u64::MAX,
                        stateid,
                    },
                },
            ],
        };

        Ok(call.encode())
    }

    /// Send callback RPC to client
    async fn send_callback_rpc(
        &self,
        addr: &str,
        prog: u32,
        compound: &Bytes,
    ) -> Result<(), String> {
        // For now, this is a stub. In production, this would:
        // 1. Parse addr to get IP:port
        // 2. Establish TCP connection
        // 3. Send RPC with proper RPC header
        // 4. Wait for response
        // 5. Parse response status

        info!("Would send callback RPC to {} prog={}", addr, prog);
        info!("Payload size: {} bytes", compound.len());

        // TODO: Implement actual RPC transport
        // This requires:
        // - TCP connection pool
        // - RPC header encoding (XID, prog, vers, proc)
        // - Response handling
        // - Timeout and retry logic

        Ok(())
    }

    /// Send CB_LAYOUTRECALL to all clients with layouts on a specific device
    ///
    /// This is used when a data server fails and all layouts using that
    /// device need to be recalled.
    pub async fn recall_layouts_for_device(
        &self,
        device_id: &str,
        layout_stateids: &[LayoutStateId],
    ) -> usize {
        info!(
            "📢 Recalling {} layouts for failed device: {}",
            layout_stateids.len(),
            device_id
        );

        let mut recalled_count = 0;

        for stateid in layout_stateids {
            // TODO: Map stateid to session_id
            // For now, broadcast to all sessions
            
            let channels = self.channels.read().await;
            for (session_id, _channel) in channels.iter() {
                match self.send_layoutrecall(
                    session_id,
                    stateid,
                    1,  // LAYOUT4_NFSV4_1_FILES
                    3,  // LAYOUTIOMODE4_ANY
                    true,
                ).await {
                    Ok(true) => recalled_count += 1,
                    Ok(false) => {}
                    Err(e) => {
                        warn!("Failed to send CB_LAYOUTRECALL: {}", e);
                    }
                }
            }
        }

        info!("✅ Sent {} CB_LAYOUTRECALL messages", recalled_count);
        recalled_count
    }
}

impl Default for CallbackManager {
    fn default() -> Self {
        Self::new()
    }
}

/// CB_LAYOUTRECALL arguments (RFC 8881 Section 20.5.1)
#[derive(Debug, Clone)]
pub struct CbLayoutRecallArgs {
    pub layout_type: u32,
    pub iomode: u32,
    pub changed: bool,
    pub recall: LayoutRecall,
}

/// Layout recall type
#[derive(Debug, Clone)]
pub enum LayoutRecall {
    /// Recall specific layout
    File {
        fh: Vec<u8>,
        offset: u64,
        length: u64,
        stateid: LayoutStateId,
    },
    /// Recall all layouts for filesystem
    Fsid {
        fsid: u64,
    },
    /// Recall all layouts
    All,
}

/// CB_LAYOUTRECALL result (RFC 8881 Section 20.5.2)
#[derive(Debug, Clone)]
pub struct CbLayoutRecallResult {
    pub status: Nfs4Status,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_callback_manager() {
        let manager = CallbackManager::new();
        let session_id = SessionId([1u8; 16]);

        // Register channel
        manager.register_channel(
            session_id,
            Some("10.0.0.1:2049".to_string()),
            0x40000000,  // NFSv4 callback program
            vec![1],     // AUTH_SYS
        ).await;

        // Send recall (will encode but not actually send since no real channel)
        let stateid = [2u8; 16];
        let result = manager.send_layoutrecall(&session_id, &stateid, 1, 3, true).await;
        assert!(result.is_ok());

        // Unregister
        manager.unregister_channel(&session_id).await;
    }
}

