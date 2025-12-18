//! pNFS (Parallel NFS) Support Module
//!
//! This module provides pNFS (NFSv4.1+) support for the Flint NFS server,
//! enabling separation of metadata operations from data operations for
//! high-performance parallel I/O.
//!
//! # Architecture
//!
//! pNFS introduces two server roles:
//!
//! - **Metadata Server (MDS)**: Handles all NFS metadata operations (OPEN, GETATTR, etc.)
//!   and serves layout information telling clients which data servers to use
//!
//! - **Data Server (DS)**: Handles only data I/O operations (READ, WRITE, COMMIT)
//!   for high-throughput parallel access
//!
//! # Design Principles
//!
//! - **Zero impact on existing code**: The standalone NFSv4.2 server continues
//!   to work unchanged. pNFS is entirely additive.
//!
//! - **Configuration-driven**: Enable/disable pNFS via configuration file,
//!   no code changes required
//!
//! - **Modular**: MDS and DS are separate modules that can be deployed
//!   independently
//!
//! # Module Structure
//!
//! ```text
//! pnfs/
//! ├── mod.rs              # This file - module exports
//! ├── config.rs           # Configuration parsing
//! ├── mds/                # Metadata Server implementation
//! │   ├── server.rs       # MDS main loop
//! │   ├── layout.rs       # Layout generation
//! │   ├── device.rs       # Device registry
//! │   └── operations/     # pNFS-specific operations
//! └── ds/                 # Data Server implementation
//!     ├── server.rs       # DS main loop
//!     ├── io.rs           # I/O operations
//!     └── registration.rs # MDS registration
//! ```
//!
//! # Usage
//!
//! ## Standalone Mode (default)
//!
//! ```rust,no_run
//! use spdk_csi_driver::nfs::{NfsServer, NfsConfig};
//!
//! # async fn example() -> std::io::Result<()> {
//! let config = NfsConfig::default();
//! let server = NfsServer::new(config)?;
//! server.serve().await
//! # }
//! ```
//!
//! ## Metadata Server Mode
//!
//! ```rust,no_run
//! use spdk_csi_driver::pnfs::{PnfsConfig, MetadataServer};
//!
//! # async fn example() -> std::io::Result<()> {
//! let config = PnfsConfig::from_file("/etc/flint/pnfs.yaml")?;
//! let mds = MetadataServer::new(config.mds.unwrap())?;
//! mds.serve().await
//! # }
//! ```
//!
//! ## Data Server Mode
//!
//! ```rust,no_run
//! use spdk_csi_driver::pnfs::{PnfsConfig, DataServer};
//!
//! # async fn example() -> std::io::Result<()> {
//! let config = PnfsConfig::from_file("/etc/flint/pnfs.yaml")?;
//! let ds = DataServer::new(config.ds.unwrap())?;
//! ds.serve().await
//! # }
//! ```
//!
//! # References
//!
//! - [RFC 5661](https://datatracker.ietf.org/doc/html/rfc5661) - NFSv4.1 (includes pNFS)
//! - [RFC 8881](https://datatracker.ietf.org/doc/html/rfc8881) - NFSv4.1 (updated)

// Configuration module
pub mod config;

// Protocol definitions and XDR encoding/decoding
pub mod protocol;

// COMPOUND context (filehandle tracking)
pub mod context;

// EXCHANGE_ID handler for pNFS role flags
pub mod exchange_id;

// COMPOUND wrapper for pNFS operations
pub mod compound_wrapper;

// gRPC control protocol (MDS-DS communication)
pub mod grpc;

// Metadata Server module
pub mod mds;

// Data Server module
pub mod ds;

// Re-exports for convenience
pub use config::{PnfsConfig, PnfsMode, MdsConfig, DsConfig};
pub use compound_wrapper::PnfsCompoundWrapper;

/// pNFS result type
pub type Result<T> = std::result::Result<T, Error>;

/// pNFS errors
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Device not found: {0}")]
    DeviceNotFound(String),

    #[error("Layout error: {0}")]
    Layout(String),

    #[error("Registration error: {0}")]
    Registration(String),

    #[error("State error: {0}")]
    State(String),
}


