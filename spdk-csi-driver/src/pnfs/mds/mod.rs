//! Metadata Server (MDS) Implementation
//!
//! The MDS handles all NFS metadata operations and serves layout information
//! to clients, telling them which data servers to use for I/O.
//!
//! # Responsibilities
//!
//! - Handle metadata operations: OPEN, CLOSE, GETATTR, SETATTR, etc.
//! - Serve layout information: LAYOUTGET, LAYOUTRETURN, LAYOUTCOMMIT
//! - Manage device registry: GETDEVICEINFO, GETDEVICELIST
//! - Track client state: sessions, stateids, leases
//! - Handle DS failures: layout recalls, failover
//!
//! # State Management
//!
//! The MDS maintains several types of state:
//!
//! - **Device Registry**: Available data servers and their endpoints
//! - **Layout State**: Active layouts issued to clients
//! - **Client State**: Sessions, stateids, leases (from base NFSv4)
//!
//! State can be persisted using different backends:
//! - In-memory (dev/testing)
//! - Kubernetes ConfigMap
//! - etcd (HA production)

/// Device registry management
pub mod device;

/// Layout generation and management
pub mod layout;

/// MDS server implementation
pub mod server;

/// pNFS-specific operations
pub mod operations;

/// Callback operations (CB_LAYOUTRECALL)
pub mod callback;

// Re-exports
pub use device::{DeviceRegistry, DeviceInfo, DeviceStatus};
pub use layout::{LayoutManager, IoMode, LayoutType};
pub use server::MetadataServer;
pub use operations::PnfsOperationHandler;


