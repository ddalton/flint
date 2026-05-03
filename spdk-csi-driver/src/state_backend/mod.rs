//! Pluggable persistence for NFSv4 / pNFS server state.
//!
//! Phase B of `docs/plans/pnfs-production-readiness.md`. Today every
//! piece of NFSv4 + pNFS state lives in `DashMap`s in process memory:
//! `ClientManager`, `SessionManager`, `StateIdManager`, `LayoutManager`.
//! On MDS restart the maps evaporate and active clients see
//! `STALE_CLIENTID` / `BAD_STATEID` / `STALE_DEVICEID` on their next op
//! — a long-running pNFS PVC effectively has its mount destroyed by a
//! pod roll. Unacceptable for any production deployment.
//!
//! This module introduces a [`StateBackend`] trait so the managers can
//! be backed by either:
//! * [`MemoryBackend`](memory::MemoryBackend) — DashMap-wrapping parity
//!   with today's behaviour (default for tests, dev work, anyone who
//!   doesn't care about restart survival), or
//! * `SqliteBackend` — durable single-file SQLite; ships in production.
//!   Lands in B.2.
//!
//! The records below are deliberately plain (`Vec<u8>`, `u64`, fixed-
//! size byte arrays) so they survive byte-for-byte across process
//! lifetimes. The boundary code in B.3 converts them to/from the
//! richer in-memory types (`Client`, `Session`, `StateEntry`,
//! `LayoutState`).
//!
//! Records intentionally NOT in the trait:
//! * Slot replay-cache contents — RFC 8881 §15.1.10.4 permits losing
//!   them on restart; clients re-issue.
//! * Per-connection state — TCP connections drop and re-establish
//!   regardless.
//! * In-flight RPC futures — they time out client-side and retry.

pub mod memory;
pub mod sqlite;

pub use memory::MemoryBackend;
pub use sqlite::SqliteBackend;

use std::future::Future;
use std::sync::Arc;

/// Fire-and-forget persistence from a sync mutation site.
///
/// Phase B.3's bridge between the existing sync manager APIs (which
/// the dispatcher calls in many places) and the async [`StateBackend`].
/// The pattern is: do the in-memory DashMap edit synchronously as
/// today (so callers see the new state immediately), then call this
/// helper to push the resulting record to the backend on a background
/// task.
///
/// **Acceptable lag bound:** ~1s in the steady state. RFC 8881
/// §15.1.10.4 lets clients retry uncached operations, so a crash
/// between in-memory mutation and persist completion loses at most
/// the last op (which the client redoes). The clientid / sessionid /
/// stateid / layout records that survived the previous successful
/// persist are what matter for restart survival.
///
/// **No-runtime fallback:** if called from a thread that's not inside
/// a tokio runtime (most `#[test]` sync tests), the persist is
/// silently skipped. Production always runs under tokio so this only
/// affects unit tests, where MemoryBackend's in-memory DashMap is
/// authoritative anyway.
pub fn spawn_persist<F, Fut>(label: &'static str, f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = StateBackendResult<()>> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = f().await {
                tracing::error!(target: "state_persist", label, error=%e, "persist failed");
            }
        });
    }
}

/// Convenience: build a default in-memory backend wrapped in `Arc<dyn
/// StateBackend>`. Used by tests and by production when the operator
/// hasn't configured durable persistence (`state.backend: memory`).
pub fn memory_backend() -> Arc<dyn StateBackend> {
    Arc::new(MemoryBackend::new())
}

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Errors a [`StateBackend`] can surface to the caller. Modeled on the
/// distinct failure modes SQLite will produce in B.2 — `MemoryBackend`
/// is infallible but uses the same shape so the boundary code doesn't
/// need a second error path.
#[derive(Debug, thiserror::Error)]
pub enum StateBackendError {
    /// Underlying storage hiccup (SQLite I/O, disk-full, locked-db, …).
    /// Carries the source error message; the in-memory backend never
    /// produces this variant.
    #[error("backend storage error: {0}")]
    Storage(String),

    /// A row decoded from the backend didn't round-trip cleanly. Most
    /// likely cause is a schema-version mismatch between the running
    /// MDS and the on-disk file — operator should `mv state.db
    /// state.db.bak` and restart, or run a migration. The in-memory
    /// backend never produces this variant.
    #[error("serialization error: {0}")]
    Serialization(String),
}

pub type StateBackendResult<T> = std::result::Result<T, StateBackendError>;

// ── Record types ──────────────────────────────────────────────────────
//
// These mirror the in-memory state types, stripped to plain fields a
// SQLite row can hold. Naming follows the in-memory type with a
// `Record` suffix to make the boundary obvious. Any time you add a
// field to the in-memory type that needs to survive restart, also add
// it here AND bump `SCHEMA_VERSION` in the SQLite backend (B.2).

/// Persisted bits of a CREATE_SESSION response, returned byte-identical
/// on a CREATE_SESSION replay (RFC 8881 §15.1.10.4 / §18.36.4).
/// Mirrors `nfs::v4::state::client::CachedCreateSessionRes` but holds
/// the session id as a 16-byte array so this module doesn't pull in
/// the NFSv4 protocol module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedCreateSessionResRecord {
    pub session_id: [u8; 16],
    pub sequence: u32,
    pub flags: u32,
    pub fore_max_request_size: u32,
    pub fore_max_response_size: u32,
    pub fore_max_response_size_cached: u32,
    pub fore_max_operations: u32,
    pub fore_max_requests: u32,
}

/// One client established via EXCHANGE_ID. Restored on MDS restart so a
/// reconnecting client gets back its existing clientid (no
/// `STALE_CLIENTID`) and any in-flight CREATE_SESSION replay still
/// returns the original byte-identical fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRecord {
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub verifier: u64,
    pub server_owner: String,
    pub server_scope: Vec<u8>,
    pub sequence_id: u32,
    pub flags: u32,
    pub principal: Vec<u8>,
    pub confirmed: bool,
    pub last_cs_sequence: Option<u32>,
    pub cs_cached_res: Option<CachedCreateSessionResRecord>,
    pub initial_cs_sequence: u32,
}

/// One NFSv4.1 session. Slot replay state is deliberately not
/// persisted (see module docs); only the channel attributes and the
/// client/cb-program binding are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: [u8; 16],
    pub client_id: u64,
    pub sequence: u32,
    pub flags: u32,
    pub fore_chan_maxrequestsize: u32,
    pub fore_chan_maxresponsesize: u32,
    pub fore_chan_maxresponsesize_cached: u32,
    pub fore_chan_maxops: u32,
    pub fore_chan_maxrequests: u32,
    pub cb_program: u32,
}

/// Type tag mirroring `nfs::v4::state::stateid::StateType`. Held as
/// its own enum so this module doesn't depend on the NFSv4 layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateTypeRecord {
    Open,
    Lock,
    Delegation,
}

/// One stateid (OPEN / LOCK / DELEGATION). The `seqid` here is the
/// server's current value; a reconnecting client whose request carries
/// `seqid - 1` still validates under `validate_for_read`'s relaxation
/// (see `nfs/v4/state/stateid.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateIdRecord {
    pub other: [u8; 12],
    pub seqid: u32,
    pub state_type: StateTypeRecord,
    pub client_id: u64,
    pub filehandle: Option<Vec<u8>>,
    pub revoked: bool,
}

/// I/O mode tag mirroring `pnfs::mds::layout::IoMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoModeRecord {
    Read,
    ReadWrite,
    Any,
}

/// One stripe within a layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSegmentRecord {
    pub offset: u64,
    pub length: u64,
    pub iomode: IoModeRecord,
    pub device_id: String,
    pub stripe_index: u32,
    pub pattern_offset: u64,
}

/// One pNFS layout issued to a client. Restored on MDS restart so the
/// client doesn't see `BAD_STATEID` on its next LAYOUTRETURN /
/// LAYOUTCOMMIT. The owning client/session ids let CB_LAYOUTRECALL
/// route correctly after restart too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutRecord {
    pub stateid: [u8; 16],
    pub owner_client_id: u64,
    pub owner_session_id: [u8; 16],
    pub owner_fsid: u64,
    pub filehandle: Vec<u8>,
    pub segments: Vec<LayoutSegmentRecord>,
    pub iomode: IoModeRecord,
    pub return_on_close: bool,
}

// ── The trait ─────────────────────────────────────────────────────────

/// Pluggable persistence for NFSv4 / pNFS server state.
///
/// All methods are async because the production impl ([`SqliteBackend`]
/// in B.2) does blocking disk I/O; the in-memory impl is trivially
/// async-compatible. Returning `Result` everywhere keeps the boundary
/// code (B.3) on a single error type even though `MemoryBackend` is
/// infallible in practice.
///
/// Idempotency contract: `put_*` is upsert (last-writer-wins on
/// matching primary key); `delete_*` on a non-existent key is `Ok(())`,
/// not an error — both backends rely on the upper layers' DashMap
/// semantics where double-removes are no-ops.
///
/// `load_all_*` exists for the boundary code to populate the
/// in-memory caches at startup. Hot-path reads go through the
/// in-memory cache; the trait is only consulted on writes and on
/// startup.
#[async_trait]
pub trait StateBackend: Send + Sync {
    // Clients
    async fn put_client(&self, c: &ClientRecord) -> StateBackendResult<()>;
    async fn get_client(&self, client_id: u64) -> StateBackendResult<Option<ClientRecord>>;
    async fn list_clients(&self) -> StateBackendResult<Vec<ClientRecord>>;
    async fn delete_client(&self, client_id: u64) -> StateBackendResult<()>;

    // Sessions
    async fn put_session(&self, s: &SessionRecord) -> StateBackendResult<()>;
    async fn get_session(&self, session_id: &[u8; 16]) -> StateBackendResult<Option<SessionRecord>>;
    async fn list_sessions(&self) -> StateBackendResult<Vec<SessionRecord>>;
    async fn delete_session(&self, session_id: &[u8; 16]) -> StateBackendResult<()>;

    // StateIds
    async fn put_stateid(&self, s: &StateIdRecord) -> StateBackendResult<()>;
    async fn get_stateid(&self, other: &[u8; 12]) -> StateBackendResult<Option<StateIdRecord>>;
    async fn list_stateids(&self) -> StateBackendResult<Vec<StateIdRecord>>;
    async fn delete_stateid(&self, other: &[u8; 12]) -> StateBackendResult<()>;

    // Layouts
    async fn put_layout(&self, l: &LayoutRecord) -> StateBackendResult<()>;
    async fn get_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<Option<LayoutRecord>>;
    async fn list_layouts(&self) -> StateBackendResult<Vec<LayoutRecord>>;
    async fn delete_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<()>;

    /// Atomically bump the persisted instance counter and return the
    /// new value. Called once at MDS start; the value is mixed into
    /// device-id prefixes so post-restart device ids never collide
    /// with pre-restart ones. Old client caches see `STALE_DEVICEID`
    /// and re-fetch — much better than silent identity collision.
    async fn increment_instance_counter(&self) -> StateBackendResult<u64>;

    /// Read the current persisted instance counter without mutating
    /// it. Mostly for diagnostics + tests.
    async fn get_instance_counter(&self) -> StateBackendResult<u64>;
}

#[cfg(test)]
mod tests {
    //! Trait-level tests — anything that should hold for *every*
    //! backend impl. Each backend module has its own tests for impl-
    //! specific behaviour (e.g. SqliteBackend's restart survival).
    use super::*;

    /// Round-trip every record type through a backend, then compare.
    /// Generic over backend so SqliteBackend in B.2 can reuse this.
    pub(crate) async fn round_trip_all<B: StateBackend>(b: &B) {
        let cs = CachedCreateSessionResRecord {
            session_id: [9u8; 16],
            sequence: 7,
            flags: 0x101,
            fore_max_request_size: 4096,
            fore_max_response_size: 4096,
            fore_max_response_size_cached: 1024,
            fore_max_operations: 16,
            fore_max_requests: 8,
        };
        let client = ClientRecord {
            client_id: 42,
            owner: b"owner-bytes".to_vec(),
            verifier: 0xdead_beef,
            server_owner: "flint-pnfs".into(),
            server_scope: b"flint-pnfs-mds".to_vec(),
            sequence_id: 3,
            flags: 0x4000_0000,
            principal: b"alice@FLINT".to_vec(),
            confirmed: true,
            last_cs_sequence: Some(7),
            cs_cached_res: Some(cs.clone()),
            initial_cs_sequence: 1,
        };
        b.put_client(&client).await.unwrap();
        assert_eq!(b.get_client(42).await.unwrap(), Some(client.clone()));

        let session = SessionRecord {
            session_id: [9u8; 16],
            client_id: 42,
            sequence: 7,
            flags: 1,
            fore_chan_maxrequestsize: 4096,
            fore_chan_maxresponsesize: 4096,
            fore_chan_maxresponsesize_cached: 1024,
            fore_chan_maxops: 16,
            fore_chan_maxrequests: 8,
            cb_program: 0x4000_0001,
        };
        b.put_session(&session).await.unwrap();
        assert_eq!(b.get_session(&[9u8; 16]).await.unwrap(), Some(session.clone()));

        let stateid = StateIdRecord {
            other: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            seqid: 5,
            state_type: StateTypeRecord::Open,
            client_id: 42,
            filehandle: Some(b"/foo/bar".to_vec()),
            revoked: false,
        };
        b.put_stateid(&stateid).await.unwrap();
        assert_eq!(
            b.get_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap(),
            Some(stateid.clone())
        );

        let layout = LayoutRecord {
            stateid: [7u8; 16],
            owner_client_id: 42,
            owner_session_id: [9u8; 16],
            owner_fsid: 100,
            filehandle: vec![0xCA, 0xFE, 0xBA, 0xBE],
            segments: vec![
                LayoutSegmentRecord {
                    offset: 0,
                    length: 8 * 1024 * 1024,
                    iomode: IoModeRecord::ReadWrite,
                    device_id: "ds-1".into(),
                    stripe_index: 0,
                    pattern_offset: 0,
                },
                LayoutSegmentRecord {
                    offset: 8 * 1024 * 1024,
                    length: 8 * 1024 * 1024,
                    iomode: IoModeRecord::ReadWrite,
                    device_id: "ds-2".into(),
                    stripe_index: 1,
                    pattern_offset: 0,
                },
            ],
            iomode: IoModeRecord::ReadWrite,
            return_on_close: true,
        };
        b.put_layout(&layout).await.unwrap();
        assert_eq!(b.get_layout(&[7u8; 16]).await.unwrap(), Some(layout.clone()));

        // list_* surfaces what we put in. Use len-then-contains rather
        // than equality so the test is robust to backend ordering.
        assert_eq!(b.list_clients().await.unwrap().len(), 1);
        assert_eq!(b.list_sessions().await.unwrap().len(), 1);
        assert_eq!(b.list_stateids().await.unwrap().len(), 1);
        assert_eq!(b.list_layouts().await.unwrap().len(), 1);

        // Deletes are idempotent — second delete is Ok, not Err.
        b.delete_client(42).await.unwrap();
        b.delete_client(42).await.unwrap();
        assert!(b.get_client(42).await.unwrap().is_none());

        b.delete_session(&[9u8; 16]).await.unwrap();
        b.delete_session(&[9u8; 16]).await.unwrap();
        assert!(b.get_session(&[9u8; 16]).await.unwrap().is_none());

        b.delete_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap();
        b.delete_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap();
        assert!(b.get_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).await.unwrap().is_none());

        b.delete_layout(&[7u8; 16]).await.unwrap();
        b.delete_layout(&[7u8; 16]).await.unwrap();
        assert!(b.get_layout(&[7u8; 16]).await.unwrap().is_none());
    }

    /// Instance counter starts at 0, increments monotonically, and
    /// `get_instance_counter` reflects the latest value. Generic so
    /// SqliteBackend in B.2 can reuse — that impl additionally verifies
    /// the counter survives a backend re-open over the same file.
    pub(crate) async fn instance_counter_monotonic<B: StateBackend>(b: &B) {
        assert_eq!(b.get_instance_counter().await.unwrap(), 0);
        assert_eq!(b.increment_instance_counter().await.unwrap(), 1);
        assert_eq!(b.increment_instance_counter().await.unwrap(), 2);
        assert_eq!(b.get_instance_counter().await.unwrap(), 2);
    }

    /// Upserts overwrite. Important — the higher layer calls
    /// `put_client` after `mark_confirmed` to persist the bit flip,
    /// expecting the new record to replace the old.
    pub(crate) async fn put_overwrites<B: StateBackend>(b: &B) {
        let mut c = ClientRecord {
            client_id: 1,
            owner: b"o".to_vec(),
            verifier: 1,
            server_owner: "s".into(),
            server_scope: b"sc".to_vec(),
            sequence_id: 1,
            flags: 0,
            principal: b"p".to_vec(),
            confirmed: false,
            last_cs_sequence: None,
            cs_cached_res: None,
            initial_cs_sequence: 1,
        };
        b.put_client(&c).await.unwrap();
        c.confirmed = true;
        c.last_cs_sequence = Some(2);
        b.put_client(&c).await.unwrap();
        let got = b.get_client(1).await.unwrap().unwrap();
        assert!(got.confirmed);
        assert_eq!(got.last_cs_sequence, Some(2));
        assert_eq!(b.list_clients().await.unwrap().len(), 1, "upsert, not append");
    }
}
