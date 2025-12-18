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
    session_id: SessionId,
    // TODO: Add actual callback connection (TCP to client)
}

impl CallbackManager {
    /// Create a new callback manager
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a callback channel for a session
    pub async fn register_channel(&self, session_id: SessionId) {
        let mut channels = self.channels.write().await;
        channels.insert(
            session_id,
            CallbackChannel { session_id },
        );
        info!("Registered callback channel for session {:?}", session_id);
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
        
        if let Some(_channel) = channels.get(session_id) {
            info!(
                "📢 Sending CB_LAYOUTRECALL to session {:?}, stateid={:?}",
                session_id,
                &layout_stateid[0..4]
            );

            // TODO: Implement actual callback RPC
            // For now, just log that we would send it
            debug!("CB_LAYOUTRECALL parameters:");
            debug!("  layout_type: {}", layout_type);
            debug!("  iomode: {}", iomode);
            debug!("  changed: {}", changed);

            // In production, this would:
            // 1. Establish callback connection to client
            // 2. Send CB_COMPOUND with CB_LAYOUTRECALL
            // 3. Wait for client response
            // 4. Handle errors (client unreachable, etc.)

            Ok(true)
        } else {
            warn!(
                "⚠️ No callback channel for session {:?}, cannot send CB_LAYOUTRECALL",
                session_id
            );
            Ok(false)
        }
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
        manager.register_channel(session_id).await;

        // Send recall (will log but not actually send since no real channel)
        let stateid = [2u8; 16];
        let result = manager.send_layoutrecall(&session_id, &stateid, 1, 3, true).await;
        assert!(result.is_ok());

        // Unregister
        manager.unregister_channel(&session_id).await;
    }
}

