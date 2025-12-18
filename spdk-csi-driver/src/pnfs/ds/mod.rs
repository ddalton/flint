//! Data Server (DS) Implementation
//!
//! The DS handles only data I/O operations (READ, WRITE, COMMIT) for
//! high-throughput parallel access.
//!
//! # Responsibilities
//!
//! - Handle data operations: READ, WRITE, COMMIT
//! - Register with MDS at startup
//! - Send periodic heartbeats to MDS
//! - Serve direct I/O from SPDK bdevs
//!
//! # Design
//!
//! The DS is intentionally minimal - it does NOT:
//! - Handle OPEN/CLOSE (MDS does this)
//! - Track file metadata (MDS does this)
//! - Manage layouts (MDS does this)
//! - Handle locking (MDS does this)
//!
//! This allows the DS to be optimized purely for I/O performance.

/// Data server I/O operations
pub mod io;

/// MDS registration
pub mod registration;

/// DS server implementation
pub mod server;

// Re-exports
pub use server::DataServer;


