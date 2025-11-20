//! Volume Snapshot Module
//! 
//! Provides CSI volume snapshot functionality using SPDK's bdev_lvol_snapshot.
//! This module is completely isolated and does not modify existing volume operations.
//!
//! # Architecture
//! 
//! - `snapshot_service`: Core SPDK snapshot operations
//! - `snapshot_models`: Data structures for snapshot operations
//! - `snapshot_routes`: HTTP endpoints for node agent
//! - `snapshot_csi`: CSI Controller RPC implementations
//!
//! # Usage
//!
//! The snapshot module is integrated via minimal changes to existing code:
//! - lib.rs: Exports this module
//! - node_agent.rs: Registers HTTP routes
//! - main.rs: Delegates CSI snapshot RPCs
//!

pub mod snapshot_service;
pub mod snapshot_models;
pub mod snapshot_routes;
pub mod snapshot_csi;

// Re-export commonly used types
pub use snapshot_service::SnapshotService;
pub use snapshot_models::{
    SnapshotInfo,
    CreateSnapshotRequest,
    CreateSnapshotResponse,
    DeleteSnapshotRequest,
    DeleteSnapshotResponse,
    CloneSnapshotRequest,
    CloneSnapshotResponse,
    ListSnapshotsResponse,
};
pub use snapshot_routes::register_snapshot_routes;
pub use snapshot_csi::SnapshotController;

