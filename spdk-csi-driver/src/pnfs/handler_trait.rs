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
    /// Every pinned DS is healthy or the outage exceeded the ceiling:
    /// fail with NFS4ERR_IO so the client can recover.
    ///
    /// HISTORY (F66): "healthy fleet ⇒ FailFast" used to be the FIRST
    /// answer, on the theory that fallback I/O against a healthy fleet
    /// means a client trapped in its MDS-fallback loop. fsx refuted the
    /// theory: a truncate makes the Linux client RETURN its layouts
    /// (PNFS_LAYOUTRET_ON_SETATTR), and one writeback page queued in
    /// that window arrives as a straggler from a HEALTHY client — the
    /// repro shows LAYOUTGET succeeding 200 µs before the refused
    /// WRITE, and the NFS4ERR_IO surfaced as msync EIO in userspace.
    /// Healthy-fleet fallback now proxies (below); FailFast remains
    /// the floor for dead fleets past the ceiling and for proxy-less
    /// configurations.
    FailFast,
    /// F66: every pinned DS is reachable and advertises a DsControl
    /// listener — the dispatcher applies the I/O to the stripes
    /// through the MDS fallback proxy
    /// (docs/plans/mds-fallback-proxy-plan.md). On a transient proxy
    /// failure the dispatcher answers NFS4ERR_DELAY; a genuinely dead
    /// DS stops heartbeating and the next disposition takes the
    /// bounded Delay→FailFast ladder instead of this arm.
    Proxy,
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
    /// F68a: the MDS data-path meter, if this handler carries one.
    /// The dispatcher caches the Arc at construction and feeds it from
    /// the READ/WRITE fallback lanes and the layout ops.
    fn f68a_meter(&self) -> Option<std::sync::Arc<crate::pnfs::mds::f68a_meter::DataPathMeter>> {
        None
    }

    /// Handle LAYOUTGET operation (opcode 50)
    fn layoutget(&self, args: LayoutGetArgs) -> Result<LayoutGetResult, LayoutGetError>;
    
    /// Handle GETDEVICEINFO operation (opcode 47)
    fn getdeviceinfo(&self, args: GetDeviceInfoArgs) -> Result<GetDeviceInfoResult, GetDeviceInfoError>;
    
    /// Handle LAYOUTRETURN operation (opcode 51)
    /// Returns the TYPED error: the dispatcher has to distinguish
    /// BadStateId (benign to a client, and the normal answer after a
    /// server-side revoke) from a genuine fault. Stringifying it here
    /// forced a blanket SERVERFAULT at the call site (audit R3).
    fn layoutreturn(
        &self,
        args: LayoutReturnArgs,
    ) -> Result<(), crate::pnfs::mds::operations::LayoutReturnError>;
    
    /// Delegation grant rule 6 (docs/plans/nfs-delegations-design.md
    /// §4): the client ids holding a write-capable layout — iomode RW
    /// or ANY — on `file_key` (export-relative path). A READ delegation
    /// is refused while any client OTHER than the requester appears
    /// here: a layout holder writes straight to the data servers and
    /// the MDS never sees a byte of it, so no fence could recall the
    /// delegation in time.
    ///
    /// REQUIRED rather than defaulted on purpose. A default of "nobody"
    /// would be fail-open for any handler that forgot to override it —
    /// the silent shape this feature has been bitten by twice. Test
    /// handlers answer an empty Vec explicitly.
    fn write_layout_holders(&self, file_key: &str) -> Vec<u64>;

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

    /// F66: apply a fallback READ to the stripes and resolve holes
    /// against `stub_size`, the file size only the MDS's stub knows.
    /// Returns `(data, eof)` where data is `min(count, size - offset)`
    /// bytes with stripe holes zero-filled, and eof is exact. Only
    /// meaningful when [`fallback_io_disposition`] returned `Proxy`;
    /// the default refuses so non-MDS roles never accidentally serve.
    ///
    /// [`fallback_io_disposition`]: PnfsOperations::fallback_io_disposition
    async fn proxy_fallback_read(
        &self,
        _file_key: &str,
        _offset: u64,
        _count: u32,
        _stub_size: u64,
    ) -> Result<(Vec<u8>, bool), String> {
        Err("fallback proxy not supported by this handler".into())
    }

    /// F66: apply a fallback WRITE to the stripes. `Ok(())` means every
    /// chunk is DURABLE on its DS (the DS fdatasyncs before answering),
    /// which is what lets the dispatcher reply FILE_SYNC honestly. The
    /// dispatcher extends the stub size afterwards — the same-dispatch
    /// equivalent of what LAYOUTCOMMIT does for DS-path writes.
    async fn proxy_fallback_write(
        &self,
        _file_key: &str,
        _offset: u64,
        _data: bytes::Bytes,
    ) -> Result<(), String> {
        Err("fallback proxy not supported by this handler".into())
    }

    /// A file was REMOVEd through the MDS namespace: forget its pin so
    /// a future same-name file gets a fresh placement (and fresh
    /// file_id ⇒ fresh DS stripe paths), and enqueue best-effort DS
    /// stripe cleanup.
    /// `_file_id` is the removed stub's inode (captured BEFORE the
    /// unlink — the extent tables key on it, and it is unrecoverable
    /// after). 0 = unknown; a scsi reclaim then leaks to the sweep.
    fn note_remove(&self, _file_key: &str, _file_id: u64) {}

    /// A size-changing SETATTR (or OPEN with a size createattr) was
    /// applied to `file_key`'s MDS stub. For a striped file the stub
    /// holds only the size — the BYTES live in DS stripe files, and a
    /// truncate-down must cut them there too or a later extension
    /// re-exposes them (fsx-found: expected zeros, read stale bytes).
    /// Implementations push `set_len(new_size)` to every pinned DS
    /// before returning; on failure they park the file behind a
    /// LAYOUTGET/fallback gate until a background retry confirms.
    async fn note_truncate(&self, _file_key: &str, _new_size: u64, _file_id: u64) {}

    /// The deepest size change currently unconfirmed on this file's
    /// pinned DSes, if the truncate gate is armed.
    ///
    /// LAYOUTCOMMIT consults this before extending the MDS stub. A
    /// client returning a layout with uncommitted writes reports the
    /// last byte it wrote (RFC 8881 §18.42.1), and the recall a truncate
    /// now fires actively INVITES that LAYOUTCOMMIT — so without this
    /// the sequence "truncate to 0 → recall → client commits
    /// last_write_offset = 1 MiB-1" sets the stub back to 1 MiB while
    /// the DS stripes are being zeroed. The size attribute then lies and
    /// reads return a megabyte of zeros that `stat` calls data (audit
    /// C4).
    ///
    /// `None` means no cut is pending and the commit may extend freely.
    fn truncate_gate_ceiling(&self, _file_key: &str) -> Option<u64> {
        None
    }

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

    // ── pnfs-block (scsi layout) surface, design doc §5 ──────────────
    //
    // The dispatcher owns the scsi LAYOUTGET/COMMIT/RETURN logic (it is
    // the async context, and the extent ops are backend transactions);
    // the handler supplies the three sync lookups below. The files-path
    // methods above are NEVER called for a scsi-class file — the
    // dispatcher branches on `layout_class_for` first.

    /// Which layout class serves `file_key`'s volume — the per-volume
    /// dispatch key. Default File keeps every existing handler and
    /// test fake on the historical path.
    fn layout_class_for(&self, _file_key: &str) -> crate::pnfs::mds::layout::LayoutClass {
        crate::pnfs::mds::layout::LayoutClass::File
    }

    /// The durable state backend carrying the extent allocator, when
    /// this handler can serve block-class volumes. `None` (the
    /// default) makes every scsi LAYOUTGET answer LAYOUTUNAVAILABLE —
    /// a handler that cannot reach the allocator must not pretend.
    fn extent_backend(&self) -> Option<std::sync::Arc<dyn crate::state_backend::StateBackend>> {
        None
    }

    /// Resolve a scsi-class deviceid — which IS the volume's NGUID
    /// (one identity for GETDEVICEINFO, the namespace, and the
    /// reservation scope) — back to the volume name. Geometry-cache
    /// scan; `None` = unknown device.
    fn scsi_volume_for_deviceid(&self, _device_id: &[u8; 16]) -> Option<String> {
        None
    }

    /// Record that a session fetched a scsi volume's device and which
    /// notifications it accepts — the CB_NOTIFY_DEVICEID address book.
    ///
    /// Kept separately from layout state on purpose: the client that
    /// needs telling is often NOT a layout holder (in the expand case
    /// it had returned every layout and still served stale device
    /// geometry from its cache), so "who holds a layout" cannot answer
    /// "who believes something about this device".
    /// Keyed on the CLIENT id, not the session that fetched: the
    /// session does not survive an MDS restart (startup drops persisted
    /// sessions so the kernel re-CREATE_SESSIONs), the client id does,
    /// and the client's cached device survives both. Measured — before
    /// this was client-keyed and durable, an expand after a restart
    /// notified nobody and the application got EIO.
    fn note_scsi_device_fetch(&self, _volume: &str, _client_id: u64, _notify_mask: u32) {}

    /// Mint and register the recall handle for a scsi grant (the
    /// stateid CB_LAYOUTRECALL and LAYOUTRETURN address; the allocator's
    /// grant rows remain the authority on what the client holds).
    /// `None` = this handler has no layout state machine and must not
    /// grant.
    fn register_scsi_layout(
        &self,
        _owner: crate::pnfs::mds::layout::LayoutOwner,
        _filehandle: Vec<u8>,
        _file_key: &str,
        _iomode: crate::pnfs::mds::layout::IoMode,
    ) -> Option<[u8; 16]> {
        None
    }

    /// Remove a scsi layout by stateid, yielding `(client_id,
    /// file_ident)` so the caller can drop the allocator grant rows.
    /// `None` = unknown stateid (benign on the return path).
    fn take_scsi_layout(&self, _stateid: &[u8; 16]) -> Option<(u64, String)> {
        None
    }

    /// Ensure `client_id` (NVMe identity `host_nqn`) is admitted on
    /// `volume`'s block export — durable desired-state upsert plus a
    /// converge pass against spdk-tgt. Called by the dispatcher BEFORE
    /// the scsi grant transaction, so a failed admission leaves no
    /// grant behind (the client retries; nothing to roll back).
    ///
    /// Default `Ok(())`: a handler with no reconciler attached has
    /// nothing to converge — reachable only in unit tests and for
    /// legacy volumes, because CreateVolume refuses block-class
    /// provisions on an MDS without a configured block export.
    async fn admit_block_host(
        &self,
        _volume: &str,
        _client_id: u64,
        _host_nqn: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

