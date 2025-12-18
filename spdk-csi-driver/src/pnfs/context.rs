//! pNFS COMPOUND Context
//!
//! Tracks state during COMPOUND operation processing, including
//! current filehandle, saved filehandle, and session information.

use crate::nfs::v4::protocol::{Nfs4FileHandle, SessionId};

/// COMPOUND execution context
///
/// Maintains state across operations within a single COMPOUND request.
/// This is essential for operations like LAYOUTGET which need the current
/// filehandle set by a previous PUTFH operation.
#[derive(Debug, Clone, Default)]
pub struct CompoundContext {
    /// Current filehandle (set by PUTFH, PUTROOTFH, etc.)
    pub current_fh: Option<Nfs4FileHandle>,
    
    /// Saved filehandle (set by SAVEFH, restored by RESTOREFH)
    pub saved_fh: Option<Nfs4FileHandle>,
    
    /// Current session (set by SEQUENCE operation)
    pub session_id: Option<SessionId>,
    
    /// Sequence ID (from SEQUENCE operation)
    pub sequence_id: Option<u32>,
}

impl CompoundContext {
    /// Create a new empty context
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the current filehandle
    pub fn set_current_fh(&mut self, fh: Nfs4FileHandle) {
        self.current_fh = Some(fh);
    }

    /// Get the current filehandle
    pub fn current_fh(&self) -> Option<&Nfs4FileHandle> {
        self.current_fh.as_ref()
    }

    /// Save the current filehandle
    pub fn save_fh(&mut self) -> Result<(), String> {
        if let Some(ref fh) = self.current_fh {
            self.saved_fh = Some(fh.clone());
            Ok(())
        } else {
            Err("No current filehandle to save".to_string())
        }
    }

    /// Restore the saved filehandle
    pub fn restore_fh(&mut self) -> Result<(), String> {
        if let Some(ref fh) = self.saved_fh {
            self.current_fh = Some(fh.clone());
            Ok(())
        } else {
            Err("No saved filehandle to restore".to_string())
        }
    }

    /// Set the session ID
    pub fn set_session(&mut self, session_id: SessionId, sequence_id: u32) {
        self.session_id = Some(session_id);
        self.sequence_id = Some(sequence_id);
    }

    /// Get the session ID
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_filehandle() {
        let mut ctx = CompoundContext::new();
        assert!(ctx.current_fh().is_none());

        let fh = Nfs4FileHandle {
            data: vec![1, 2, 3, 4],
        };

        ctx.set_current_fh(fh.clone());
        assert!(ctx.current_fh().is_some());
        assert_eq!(ctx.current_fh().unwrap().data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_context_save_restore() {
        let mut ctx = CompoundContext::new();

        let fh1 = Nfs4FileHandle { data: vec![1, 2, 3] };
        let fh2 = Nfs4FileHandle { data: vec![4, 5, 6] };

        ctx.set_current_fh(fh1.clone());
        ctx.save_fh().unwrap();

        ctx.set_current_fh(fh2.clone());
        assert_eq!(ctx.current_fh().unwrap().data, vec![4, 5, 6]);

        ctx.restore_fh().unwrap();
        assert_eq!(ctx.current_fh().unwrap().data, vec![1, 2, 3]);
    }
}

