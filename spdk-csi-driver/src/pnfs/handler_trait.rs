//! pNFS Handler Trait
//!
//! Defines the trait for pNFS operation handling that can be plugged into
//! the NFSv4 dispatcher without modifying core NFS logic.

use crate::pnfs::mds::operations::{
    LayoutGetArgs, LayoutGetResult, LayoutGetError,
    GetDeviceInfoArgs, GetDeviceInfoResult, GetDeviceInfoError,
    LayoutReturnArgs,
};

/// How the MDS should answer a READ/WRITE that reaches it for a
/// placement-pinned (striped) file — the kernel client's MDS-fallback
/// path. Serving the local file is never an option (it is a sparse
/// size-only stub; serving it returns silent zeros), so the choice is
/// between parking the client and failing it:
///
/// - `Delay` (NFS4ERR_DELAY) parks the client's fallback RPC in a
///   ~100 ms retry loop. Appropriate ONLY while a pinned DS is down
///   and recently so — the loop never re-drives the client's layout
///   path (kernel-verified, 6.1: `nfs4_read_done_cb` retries the
///   identical MDS READ forever), so DELAY past the DS's recovery is
///   a livelock: the looping task holds page locks, and every later
///   read of those pages on that node queues behind it.
/// - `FailFast` (NFS4ERR_IO) completes the fallback RPC with an
///   error. This is the ONLY thing that springs a trapped client:
///   pages unlock, the loop exits, and the application's retry
///   re-enters the client's pnfs path (its 120 s device/layout marks
///   self-expire) → fresh LAYOUTGET → good data from the DS.
///
/// See docs/pnfs-operator-runbook.md ("the DELAY livelock").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackIoDisposition {
    /// Not placement-pinned — the MDS holds the real bytes; serve.
    Serve,
    /// A pinned DS is down, outage still within the bounded window:
    /// park the client with NFS4ERR_DELAY and wait for DS recovery.
    Delay,
    /// Every pinned DS is healthy (client is stuck in its fallback
    /// trap) or the outage exceeded the ceiling: fail with
    /// NFS4ERR_IO so the client can recover.
    FailFast,
}

/// Trait for handling pNFS operations
///
/// This trait allows the NFSv4 dispatcher to optionally support pNFS
/// without creating a hard dependency on pNFS code.
///
/// (`async_trait` because `note_truncate` must complete a network
/// round-trip to the DS fleet before the dispatcher replies; the other
/// methods stay synchronous.)
#[tonic::async_trait]
pub trait PnfsOperations: Send + Sync {
    /// Handle LAYOUTGET operation (opcode 50)
    fn layoutget(&self, args: LayoutGetArgs) -> Result<LayoutGetResult, LayoutGetError>;
    
    /// Handle GETDEVICEINFO operation (opcode 47)
    fn getdeviceinfo(&self, args: GetDeviceInfoArgs) -> Result<GetDeviceInfoResult, GetDeviceInfoError>;
    
    /// Handle LAYOUTRETURN operation (opcode 51)
    fn layoutreturn(&self, args: LayoutReturnArgs) -> Result<(), String>;
    
    /// Handle LAYOUTCOMMIT operation (opcode 49)
    fn layoutcommit(&self) -> Result<(), String> {
        // Default implementation: not required for basic pNFS
        Ok(())
    }
    
    /// Handle GETDEVICELIST operation (opcode 48)
    fn getdevicelist(&self) -> Result<Vec<Vec<u8>>, String> {
        // Default implementation: return empty list
        Ok(Vec::new())
    }

    /// Whether `file_key` (export-relative path) is pNFS-managed —
    /// i.e. it has a pinned stripe placement, so its bytes live on the
    /// DS fleet and the MDS's local file is a sparse size-only stub.
    /// Serving the stub returns silent zeros (data corruption, found
    /// live on runn 2026-07-06 by the DS-outage read drill). Default
    /// `false` keeps non-striped files (never layouted) fully
    /// readable/writable through the MDS.
    fn is_pnfs_managed(&self, _file_key: &str) -> bool {
        false
    }

    /// How the dispatcher should answer a READ/WRITE through the MDS
    /// for `file_key` — see [`FallbackIoDisposition`]. The default
    /// mirrors the pre-bounded behavior (DELAY whenever pinned);
    /// implementations with a device registry should escalate to
    /// FailFast once the pinned DSes are healthy again or the outage
    /// exceeds the bounded window.
    fn fallback_io_disposition(&self, file_key: &str) -> FallbackIoDisposition {
        if self.is_pnfs_managed(file_key) {
            FallbackIoDisposition::Delay
        } else {
            FallbackIoDisposition::Serve
        }
    }

    /// A file was REMOVEd through the MDS namespace: forget its pin so
    /// a future same-name file gets a fresh placement (and fresh
    /// file_id ⇒ fresh DS stripe paths), and enqueue best-effort DS
    /// stripe cleanup.
    fn note_remove(&self, _file_key: &str) {}

    /// A size-changing SETATTR (or OPEN with a size createattr) was
    /// applied to `file_key`'s MDS stub. For a striped file the stub
    /// holds only the size — the BYTES live in DS stripe files, and a
    /// truncate-down must cut them there too or a later extension
    /// re-exposes them (fsx-found: expected zeros, read stale bytes).
    /// Implementations push `set_len(new_size)` to every pinned DS
    /// before returning; on failure they park the file behind a
    /// LAYOUTGET/fallback gate until a background retry confirms.
    async fn note_truncate(&self, _file_key: &str, _new_size: u64) {}

    /// Whether RENAME of `old_key` can preserve data. False only for
    /// legacy path-keyed pins (file_id 0): their DS stripes live at the
    /// old rebased path, so fresh readers of the new name would resolve
    /// to nothing — the op must be REFUSED (today it silently serves
    /// zeros). v2 pins and unpinned files rename freely.
    fn rename_preserves_data(&self, _old_key: &str) -> bool {
        true
    }

    /// A successful RENAME old→new through the MDS namespace: re-key
    /// the pin (v2 pins only — data follows the identity, not the
    /// path). If a pinned file was overwritten at `new_key`, its pin is
    /// forgotten and its stripes enqueued for cleanup.
    fn note_rename(&self, _old_key: &str, _new_key: &str) {}

    /// Whether LINK to a pinned file is supported. Pins are keyed by
    /// path, so a hard link would give the striped file a second name
    /// with NO pin — reads via the link would serve the sparse stub
    /// (silent zeros). Refused for pinned files until pins are keyed by
    /// identity end-to-end.
    fn link_allowed(&self, target_key: &str) -> bool {
        !self.is_pnfs_managed(target_key)
    }

    // NOTE: there is deliberately no `stripe_unit()` here. The stripe
    // unit is per-file (pinned on the placement at first LAYOUTGET and
    // carried on each `Layout`) — a global value is exactly the
    // fleet-change re-mapping bug Phase 0 of the durable-DS plan
    // removed.
}

