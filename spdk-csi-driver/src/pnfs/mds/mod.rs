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

/// F67: durable file_id↔path binding stored on the stub (xattr)
pub mod stub_binding;

/// F68a: MDS data-path observability (the two silent fallback lanes).
pub mod f68a_meter;

/// pnfs-block (scsi) NVMe export reconciler — lvol, subsystem-per-volume,
/// pinned NGUID, grant-driven host admission (design doc §5).
pub mod block_export;

/// The MDS-as-NVMe-host reservation fence lane (RFC 9561 §2.2 preempt).
pub mod resv_fence;

// Re-exports
pub use device::{DeviceRegistry, DeviceInfo, DeviceStatus};
pub use layout::{LayoutManager, IoMode, LayoutType};
pub use server::MetadataServer;
pub use operations::PnfsOperationHandler;


