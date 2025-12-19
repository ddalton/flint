//! Flint NFSv4.2 Server
//!
//! A high-performance NFSv4.2 server implementation for serving SPDK-backed volumes
//! over NFS to provide ReadWriteMany (RWX) capability with concurrent I/O support.
//!
//! # Architecture
//!
//! This implementation follows a layered design:
//! - Transport layer: TCP handling (Tokio-based async I/O)
//! - RPC layer: Sun RPC message encoding/decoding
//! - Protocol layer: NFSv4.2 COMPOUND operations with CREATE, REMOVE, READ, WRITE, etc.
//! - State management: Lock-free session and lock state tracking
//! - Filesystem layer: VFS trait backed by local filesystem
//!
//! # Protocol References
//!
//! - [RFC 7862](https://datatracker.ietf.org/doc/html/rfc7862) - NFS Version 4.2
//! - [RFC 8881](https://datatracker.ietf.org/doc/html/rfc8881) - NFS Version 4.1
//! - [RFC 4506](https://datatracker.ietf.org/doc/html/rfc4506) - XDR
//! - [RFC 5531](https://datatracker.ietf.org/doc/html/rfc5531) - RPC
//!
//! # Design Principles
//!
//! - **Stateful**: NFSv4.2 uses sessions for concurrent operations
//! - **Async**: All I/O operations use Tokio for high concurrency
//! - **Lock-free**: State management uses DashMap for concurrent access
//! - **Complete**: Implements CREATE, REMOVE, and all operations for concurrent I/O

pub mod xdr;          // XDR encoding/decoding (shared with RPC)
pub mod rpc;          // RPC message handling (shared with NFSv4)
pub mod rpcsec_gss;   // RPCSEC_GSS authentication (Kerberos support)
pub mod server_v4;    // NFSv4.2 TCP server
pub mod v4;           // NFSv4.2 implementation (COMPLETE)

// Note: tests.rs contains old NFSv3 tests that are outdated
// TODO: Update or remove tests.rs
// #[cfg(test)]
// mod tests;

// Re-exports for convenience
pub use server_v4::{NfsServer, NfsConfig};

/// NFS server result type
pub type Result<T> = std::result::Result<T, Error>;

/// NFS server errors
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("XDR encoding error: {0}")]
    Xdr(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("NFS error: {0}")]
    Nfs(String),

    #[error("Invalid file handle")]
    InvalidFileHandle,

    #[error("File not found")]
    NotFound,

    #[error("Permission denied")]
    PermissionDenied,
}
