// NFSv4.2 Server Implementation
//
// This module implements NFSv4.2 (RFC 7862) with NFSv4.1 (RFC 8881) foundation.
// Target: Production-ready NFS server for SPDK CSI driver with RWX support.
//
// Architecture:
// - Single COMPOUND-based protocol (no separate MOUNT/NLM/NSM)
// - Session-based state management (NFSv4.1)
// - Performance operations (NFSv4.2): COPY, CLONE, ALLOCATE, DEALLOCATE, SEEK, READ_PLUS
// - Integrated locking (LOCK/LOCKT/LOCKU)
//
// Implementation Status: Phases 1-3 COMPLETE
// Phase 1: NFSv4.1 Foundation ✅
// Phase 2: NFSv4.2 Performance Operations ✅
// Phase 3: Locking & COMPOUND Dispatcher ✅

pub mod protocol;
pub mod xdr;              // NFSv4 XDR encoding/decoding - DONE
pub mod compound;         // COMPOUND operation framework - DONE
pub mod filehandle;       // NFSv4 file handle management - DONE
pub mod filehandle_pnfs;  // pNFS file-ID based filehandles (RFC 8435) - NEW
pub mod pseudo;           // Pseudo-filesystem (RFC 7530 Section 7) - NEW
pub mod state;            // State management (stateids, sessions, leases) - DONE
pub mod operations;       // NFSv4 operations - DONE
pub mod dispatcher;       // COMPOUND dispatcher - DONE
pub mod back_channel;     // Per-connection writer for v4.1 callbacks (CB_LAYOUTRECALL etc.)
pub mod cb_compound;      // CB_COMPOUND XDR + ONC RPC framing for the v4.1 callback channel

pub use protocol::*;
pub use xdr::{Nfs4XdrEncoder, Nfs4XdrDecoder, AttrEncoder, AttrDecoder};
pub use compound::{
    CompoundRequest, CompoundResponse, CompoundContext,
    Operation, OperationResult, ChannelAttrs,
};

pub use dispatcher::{CompoundDispatcher, ServerStats};

// Re-export key types
pub use protocol::{
    Nfs4Status,
    StateId,
    Nfs4FileHandle,
    SessionId,
    ClientId,
};
