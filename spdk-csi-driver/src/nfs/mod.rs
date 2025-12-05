//! Flint NFSv3 Server
//!
//! A high-performance NFSv3 server implementation for serving SPDK-backed volumes
//! over NFS to provide ReadWriteMany (RWX) capability in Kubernetes.
//!
//! # Architecture
//!
//! This implementation follows a layered design:
//! - Transport layer: TCP/UDP handling (Tokio-based async I/O)
//! - RPC layer: Sun RPC message encoding/decoding
//! - Protocol layer: NFSv3 operations (LOOKUP, READ, WRITE, etc.)
//! - Filesystem layer: VFS trait backed by local filesystem
//!
//! # Protocol References
//!
//! - [RFC 1813](https://datatracker.ietf.org/doc/html/rfc1813) - NFS Version 3
//! - [RFC 4506](https://datatracker.ietf.org/doc/html/rfc4506) - XDR
//! - [RFC 5531](https://datatracker.ietf.org/doc/html/rfc5531) - RPC
//!
//! # Design Principles
//!
//! - **Stateless**: NFSv3 is stateless; no session management needed
//! - **Async**: All I/O operations use Tokio for high concurrency
//! - **Local**: Optimized for local filesystem access (SPDK volumes mounted locally)
//! - **Simple**: Only implements operations needed for Kubernetes RWX

pub mod xdr;          // XDR encoding/decoding
pub mod rpc;          // RPC message handling
pub mod protocol;     // NFSv3 protocol types
pub mod filehandle;   // File handle management
pub mod vfs;          // Filesystem backend
pub mod handlers;     // NFSv3 operation handlers
pub mod setattr;      // SETATTR handler
pub mod server;       // TCP/UDP server
pub mod portmap;      // Portmapper registration

#[cfg(test)]
mod tests;            // Integration tests

// Re-exports for convenience
pub use server::{NfsServer, NfsConfig};
pub use vfs::LocalFilesystem;
pub use protocol::{NFS3Status, FileHandle, FileAttr, FileType};

/// NFSv3 server result type
pub type Result<T> = std::result::Result<T, Error>;

/// NFSv3 server errors
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
