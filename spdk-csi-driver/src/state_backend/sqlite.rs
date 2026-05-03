//! Single-file SQLite [`StateBackend`] — the impl that ships in
//! production. Phase B.2 of `pnfs-production-readiness.md`.
//!
//! Every record put through this backend is durable across MDS
//! restarts: a reconnecting client gets back its existing clientid
//! (no `STALE_CLIENTID`), its in-flight stateids still validate (no
//! `BAD_STATEID`), and its layouts come back too (no `STALE_DEVICEID`,
//! no fresh-LAYOUTGET storm). Atomicity is via the SQLite WAL +
//! synchronous=NORMAL combination — crash-safe at the transaction
//! level (a power loss may lose the last commit but the DB never
//! corrupts).
//!
//! ## Why SQLite (not etcd, not Kubernetes ConfigMap)
//!
//! - The MDS is a single process. Adding etcd is operational weight
//!   users won't want for a perf-tier storage system.
//! - SQLite gives crash-safe atomic writes for free, with no daemon.
//! - Forensics: a `.db` file is `sqlite3 state.db 'SELECT * FROM
//!   clients'` away from inspection. Hard to beat for debugging.
//!
//! ## Concurrency model
//!
//! `rusqlite::Connection` is `Send` but `!Sync`, so we hold one
//! behind `Arc<std::sync::Mutex>`. Each trait method does its work
//! inside `tokio::task::spawn_blocking`: the mutex guard is acquired
//! and released entirely on the blocking thread, never held across
//! `.await`. Hot-path reads in B.3 will go through the in-memory
//! manager caches; the trait only sees writes (and the once-per-
//! startup `list_*` calls), so a single connection is plenty.
//!
//! ## Type mapping
//!
//! - `u64` ↔ `INTEGER` via two's-complement reinterpret (`as i64`).
//!   Same on both directions; values that exceed `i64::MAX` round-trip
//!   correctly (the bit pattern is preserved).
//! - Fixed byte arrays (`[u8; 16]`, `[u8; 12]`) ↔ `BLOB` with a length
//!   check on read. A row with the wrong-sized blob is a serialization
//!   error, not a panic.
//! - Nested types (`Vec<LayoutSegmentRecord>`,
//!   `CachedCreateSessionResRecord`) ↔ JSON TEXT via `serde_json`.
//!   This is the right trade-off for B.2: the records are small (KBs
//!   not MBs), the schema is stable, and migrations are easier with
//!   JSON than with relational decomposition.
//! - Enums (`StateTypeRecord`, `IoModeRecord`) ↔ `INTEGER`.

use super::{
    CachedCreateSessionResRecord, ClientRecord, IoModeRecord, LayoutRecord, LayoutSegmentRecord,
    SessionRecord, StateBackend, StateBackendError, StateBackendResult, StateIdRecord,
    StateTypeRecord,
};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Schema version persisted in the `schema_version` table. Bump when
/// adding columns or tables; the open path runs supported migrations
/// and errors out on unsupported version drift (operator must move
/// the DB aside).
///
/// Version history:
///   1 → initial: clients, sessions, stateids, layouts,
///        instance_counter.
///   2 → add `server_identity` (singleton row with the persistent
///        per-deployment server id used by FileHandleManager so
///        cached FHs survive MDS restart).
const SCHEMA_VERSION: i64 = 2;

/// Single-file SQLite [`StateBackend`].
pub struct SqliteBackend {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteBackend {
    /// Open or create the DB at `path`, applying the schema if the
    /// file is empty. Caller (B.4) typically points this at a path
    /// like `/var/lib/flint-pnfs/state.db`.
    ///
    /// Sets `journal_mode=WAL` and `synchronous=NORMAL` — the right
    /// combination for crash-safety with reasonable throughput on the
    /// write path. FULL is paranoid; OFF is unsafe; NORMAL+WAL is what
    /// almost every embedded SQLite deployment uses.
    pub fn open<P: AsRef<Path>>(path: P) -> StateBackendResult<Self> {
        let conn = Connection::open(path).map_err(|e| {
            StateBackendError::Storage(format!("open: {}", e))
        })?;
        Self::init(conn)
    }

    /// Open an in-memory DB. Useful for tests; the schema still gets
    /// applied so behaviour matches a real on-disk file. Production
    /// code should use `open` with a path.
    #[cfg(test)]
    pub fn open_in_memory() -> StateBackendResult<Self> {
        let conn = Connection::open_in_memory().map_err(|e| {
            StateBackendError::Storage(format!("open_in_memory: {}", e))
        })?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> StateBackendResult<Self> {
        // WAL + NORMAL is the standard durability/throughput point.
        // execute_batch instead of pragma_update so we get a single
        // round-trip for the small bundle.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
        .map_err(|e| StateBackendError::Storage(format!("pragmas: {}", e)))?;

        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| StateBackendError::Storage(format!("schema: {}", e)))?;

        // Schema version: insert if first run, else verify match. This
        // is the operator-visible canary for "the DB is from an older
        // build" — better to fail open than silently misread rows.
        let existing: Option<i64> = conn
            .query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| StateBackendError::Storage(format!("schema_version read: {}", e)))?;

        match existing {
            None => {
                conn.execute(
                    "INSERT INTO schema_version (id, version) VALUES (1, ?1)",
                    params![SCHEMA_VERSION],
                )
                .map_err(|e| {
                    StateBackendError::Storage(format!("schema_version insert: {}", e))
                })?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(1) if SCHEMA_VERSION == 2 => {
                // v1 → v2 migration: the new `server_identity` table
                // was added by the schema-batch above (CREATE TABLE
                // IF NOT EXISTS). Just bump the version row so future
                // opens don't keep re-running the migration. The first
                // call to `get_or_init_server_id` will populate the
                // singleton row with a fresh random id.
                conn.execute(
                    "UPDATE schema_version SET version = ?1 WHERE id = 1",
                    params![SCHEMA_VERSION],
                )
                .map_err(|e| {
                    StateBackendError::Storage(format!("schema_version migrate: {}", e))
                })?;
                tracing::info!(
                    "SqliteBackend: migrated schema 1 → 2 (added server_identity table)"
                );
            }
            Some(v) => {
                return Err(StateBackendError::Storage(format!(
                    "schema_version mismatch: db has {}, code expects {}; \
                     move the file aside or run a migration",
                    v, SCHEMA_VERSION
                )));
            }
        }

        // Ensure the singleton instance_counter row exists; INSERT OR
        // IGNORE so re-opens are a no-op. After this, the
        // increment/get paths assume row id=1 exists.
        conn.execute(
            "INSERT OR IGNORE INTO instance_counter (id, value) VALUES (1, 0)",
            [],
        )
        .map_err(|e| StateBackendError::Storage(format!("counter init: {}", e)))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Borrow the connection on a blocking thread. The closure runs
    /// inside `spawn_blocking` so it never blocks the tokio runtime
    /// thread; any rusqlite error is mapped to `Storage` for the
    /// caller.
    async fn with_conn<T, F>(&self, f: F) -> StateBackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| rusqlite::Error::InvalidQuery /* poisoned */)?;
            f(&guard)
        })
        .await
        .map_err(|e| StateBackendError::Storage(format!("join: {}", e)))?
        .map_err(|e| StateBackendError::Storage(format!("sqlite: {}", e)))
    }
}

// ── Type-mapping helpers ──────────────────────────────────────────────

/// Reinterpret a `u64` as `i64` for SQLite storage. The bit pattern
/// is preserved — values above `i64::MAX` come out negative on the
/// SQL side but `i64_to_u64` reverses the trick on read.
fn u64_to_i64(u: u64) -> i64 {
    u as i64
}

fn i64_to_u64(i: i64) -> u64 {
    i as u64
}

fn bool_to_i64(b: bool) -> i64 {
    if b { 1 } else { 0 }
}

fn i64_to_bool(i: i64) -> bool {
    i != 0
}

fn state_type_to_i64(t: StateTypeRecord) -> i64 {
    match t {
        StateTypeRecord::Open => 0,
        StateTypeRecord::Lock => 1,
        StateTypeRecord::Delegation => 2,
    }
}

fn i64_to_state_type(i: i64) -> StateBackendResult<StateTypeRecord> {
    match i {
        0 => Ok(StateTypeRecord::Open),
        1 => Ok(StateTypeRecord::Lock),
        2 => Ok(StateTypeRecord::Delegation),
        other => Err(StateBackendError::Serialization(format!(
            "unknown state_type discriminant: {}",
            other
        ))),
    }
}

fn iomode_to_i64(m: IoModeRecord) -> i64 {
    match m {
        IoModeRecord::Read => 0,
        IoModeRecord::ReadWrite => 1,
        IoModeRecord::Any => 2,
    }
}

fn i64_to_iomode(i: i64) -> StateBackendResult<IoModeRecord> {
    match i {
        0 => Ok(IoModeRecord::Read),
        1 => Ok(IoModeRecord::ReadWrite),
        2 => Ok(IoModeRecord::Any),
        other => Err(StateBackendError::Serialization(format!(
            "unknown iomode discriminant: {}",
            other
        ))),
    }
}

/// `[u8; N]` round-trip via BLOB with a length check on read. A row
/// with a mis-sized blob is a serialization error rather than a
/// silent truncation.
fn blob_to_array<const N: usize>(blob: Vec<u8>, field: &str) -> StateBackendResult<[u8; N]> {
    if blob.len() != N {
        return Err(StateBackendError::Serialization(format!(
            "{}: expected {} bytes, got {}",
            field,
            N,
            blob.len()
        )));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&blob);
    Ok(out)
}

// ── Trait impl ────────────────────────────────────────────────────────

#[async_trait]
impl StateBackend for SqliteBackend {
    async fn put_client(&self, c: &ClientRecord) -> StateBackendResult<()> {
        let c = c.clone();
        self.with_conn(move |conn| {
            // INSERT OR REPLACE = upsert, matches MemoryBackend semantics.
            // serde_json for the optional CachedCreateSessionResRecord —
            // small struct, stable schema, easier to migrate than a sub-
            // table with foreign keys.
            let cs_json = match &c.cs_cached_res {
                Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                })?),
                None => None,
            };
            conn.execute(
                "INSERT OR REPLACE INTO clients
                 (client_id, owner, verifier, server_owner, server_scope,
                  sequence_id, flags, principal, confirmed,
                  last_cs_sequence, cs_cached_res, initial_cs_sequence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    u64_to_i64(c.client_id),
                    c.owner,
                    u64_to_i64(c.verifier),
                    c.server_owner,
                    c.server_scope,
                    c.sequence_id as i64,
                    c.flags as i64,
                    c.principal,
                    bool_to_i64(c.confirmed),
                    c.last_cs_sequence.map(|v| v as i64),
                    cs_json,
                    c.initial_cs_sequence as i64,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_client(&self, client_id: u64) -> StateBackendResult<Option<ClientRecord>> {
        let id = u64_to_i64(client_id);
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT client_id, owner, verifier, server_owner, server_scope,
                            sequence_id, flags, principal, confirmed,
                            last_cs_sequence, cs_cached_res, initial_cs_sequence
                     FROM clients WHERE client_id = ?1",
                    params![id],
                    decode_client_row,
                )
                .optional()
            })
            .await?;
        row.transpose()
    }

    async fn list_clients(&self) -> StateBackendResult<Vec<ClientRecord>> {
        let rows = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT client_id, owner, verifier, server_owner, server_scope,
                            sequence_id, flags, principal, confirmed,
                            last_cs_sequence, cs_cached_res, initial_cs_sequence
                     FROM clients",
                )?;
                let rows: rusqlite::Result<Vec<_>> = stmt
                    .query_map([], decode_client_row)?
                    .collect();
                rows
            })
            .await?;
        rows.into_iter().collect()
    }

    async fn delete_client(&self, client_id: u64) -> StateBackendResult<()> {
        let id = u64_to_i64(client_id);
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM clients WHERE client_id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    async fn put_session(&self, s: &SessionRecord) -> StateBackendResult<()> {
        let s = s.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO sessions
                 (session_id, client_id, sequence, flags,
                  fore_chan_maxrequestsize, fore_chan_maxresponsesize,
                  fore_chan_maxresponsesize_cached, fore_chan_maxops,
                  fore_chan_maxrequests, cb_program)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    s.session_id.to_vec(),
                    u64_to_i64(s.client_id),
                    s.sequence as i64,
                    s.flags as i64,
                    s.fore_chan_maxrequestsize as i64,
                    s.fore_chan_maxresponsesize as i64,
                    s.fore_chan_maxresponsesize_cached as i64,
                    s.fore_chan_maxops as i64,
                    s.fore_chan_maxrequests as i64,
                    s.cb_program as i64,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_session(
        &self,
        session_id: &[u8; 16],
    ) -> StateBackendResult<Option<SessionRecord>> {
        let key = session_id.to_vec();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT session_id, client_id, sequence, flags,
                            fore_chan_maxrequestsize, fore_chan_maxresponsesize,
                            fore_chan_maxresponsesize_cached, fore_chan_maxops,
                            fore_chan_maxrequests, cb_program
                     FROM sessions WHERE session_id = ?1",
                    params![key],
                    decode_session_row,
                )
                .optional()
            })
            .await?;
        row.transpose()
    }

    async fn list_sessions(&self) -> StateBackendResult<Vec<SessionRecord>> {
        let rows = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT session_id, client_id, sequence, flags,
                            fore_chan_maxrequestsize, fore_chan_maxresponsesize,
                            fore_chan_maxresponsesize_cached, fore_chan_maxops,
                            fore_chan_maxrequests, cb_program
                     FROM sessions",
                )?;
                let rows: rusqlite::Result<Vec<_>> =
                    stmt.query_map([], decode_session_row)?.collect();
                rows
            })
            .await?;
        rows.into_iter().collect()
    }

    async fn delete_session(&self, session_id: &[u8; 16]) -> StateBackendResult<()> {
        let key = session_id.to_vec();
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM sessions WHERE session_id = ?1", params![key])?;
            Ok(())
        })
        .await
    }

    async fn put_stateid(&self, s: &StateIdRecord) -> StateBackendResult<()> {
        let s = s.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO stateids
                 (other, seqid, state_type, client_id, filehandle, revoked)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    s.other.to_vec(),
                    s.seqid as i64,
                    state_type_to_i64(s.state_type),
                    u64_to_i64(s.client_id),
                    s.filehandle,
                    bool_to_i64(s.revoked),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_stateid(&self, other: &[u8; 12]) -> StateBackendResult<Option<StateIdRecord>> {
        let key = other.to_vec();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT other, seqid, state_type, client_id, filehandle, revoked
                     FROM stateids WHERE other = ?1",
                    params![key],
                    decode_stateid_row,
                )
                .optional()
            })
            .await?;
        row.transpose()
    }

    async fn list_stateids(&self) -> StateBackendResult<Vec<StateIdRecord>> {
        let rows = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT other, seqid, state_type, client_id, filehandle, revoked
                     FROM stateids",
                )?;
                let rows: rusqlite::Result<Vec<_>> =
                    stmt.query_map([], decode_stateid_row)?.collect();
                rows
            })
            .await?;
        rows.into_iter().collect()
    }

    async fn delete_stateid(&self, other: &[u8; 12]) -> StateBackendResult<()> {
        let key = other.to_vec();
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM stateids WHERE other = ?1", params![key])?;
            Ok(())
        })
        .await
    }

    async fn put_layout(&self, l: &LayoutRecord) -> StateBackendResult<()> {
        let l = l.clone();
        self.with_conn(move |conn| {
            let segments_json = serde_json::to_string(&l.segments).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            })?;
            conn.execute(
                "INSERT OR REPLACE INTO layouts
                 (stateid, owner_client_id, owner_session_id, owner_fsid,
                  filehandle, segments, iomode, return_on_close)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    l.stateid.to_vec(),
                    u64_to_i64(l.owner_client_id),
                    l.owner_session_id.to_vec(),
                    u64_to_i64(l.owner_fsid),
                    l.filehandle,
                    segments_json,
                    iomode_to_i64(l.iomode),
                    bool_to_i64(l.return_on_close),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<Option<LayoutRecord>> {
        let key = stateid.to_vec();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT stateid, owner_client_id, owner_session_id, owner_fsid,
                            filehandle, segments, iomode, return_on_close
                     FROM layouts WHERE stateid = ?1",
                    params![key],
                    decode_layout_row,
                )
                .optional()
            })
            .await?;
        row.transpose()
    }

    async fn list_layouts(&self) -> StateBackendResult<Vec<LayoutRecord>> {
        let rows = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT stateid, owner_client_id, owner_session_id, owner_fsid,
                            filehandle, segments, iomode, return_on_close
                     FROM layouts",
                )?;
                let rows: rusqlite::Result<Vec<_>> =
                    stmt.query_map([], decode_layout_row)?.collect();
                rows
            })
            .await?;
        rows.into_iter().collect()
    }

    async fn delete_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<()> {
        let key = stateid.to_vec();
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM layouts WHERE stateid = ?1", params![key])?;
            Ok(())
        })
        .await
    }

    async fn increment_instance_counter(&self) -> StateBackendResult<u64> {
        // SQLite 3.35+ RETURNING gives us read-and-increment in a
        // single statement, so we don't need a BEGIN/COMMIT pair.
        // Two concurrent callers serialize on the connection mutex
        // so the values they observe are still distinct.
        let v: i64 = self
            .with_conn(|conn| {
                conn.query_row(
                    "UPDATE instance_counter SET value = value + 1 WHERE id = 1 RETURNING value",
                    [],
                    |r| r.get(0),
                )
            })
            .await?;
        Ok(i64_to_u64(v))
    }

    async fn get_instance_counter(&self) -> StateBackendResult<u64> {
        let v: i64 = self
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT value FROM instance_counter WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
            })
            .await?;
        Ok(i64_to_u64(v))
    }

    async fn get_or_init_server_id(&self) -> StateBackendResult<u64> {
        // INSERT-OR-IGNORE-then-SELECT pattern: if the singleton row
        // doesn't exist (first ever call), atomically inject one
        // with a random non-zero u64; otherwise the IGNORE branch
        // fires and the existing row's value is read on the SELECT.
        // Both happen on the same connection mutex so concurrent
        // first-callers serialise and observe the same value.
        let candidate = i64::from_ne_bytes((rand::random::<u64>() | 1).to_ne_bytes());
        let v: i64 = self
            .with_conn(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO server_identity (id, server_id) VALUES (1, ?1)",
                    params![candidate],
                )?;
                conn.query_row(
                    "SELECT server_id FROM server_identity WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
            })
            .await?;
        Ok(i64_to_u64(v))
    }
}

// ── Row decoders ──────────────────────────────────────────────────────
//
// Pulled out as free functions so they're reusable between `query_row`
// (single) and `query_map` (list). Each maps a rusqlite Row to either
// a record or a `Result<Record, StateBackendError>` depending on
// whether downstream serialization can fail.

fn decode_client_row(r: &rusqlite::Row) -> rusqlite::Result<StateBackendResult<ClientRecord>> {
    let client_id: i64 = r.get(0)?;
    let owner: Vec<u8> = r.get(1)?;
    let verifier: i64 = r.get(2)?;
    let server_owner: String = r.get(3)?;
    let server_scope: Vec<u8> = r.get(4)?;
    let sequence_id: i64 = r.get(5)?;
    let flags: i64 = r.get(6)?;
    let principal: Vec<u8> = r.get(7)?;
    let confirmed: i64 = r.get(8)?;
    let last_cs_sequence: Option<i64> = r.get(9)?;
    let cs_json: Option<String> = r.get(10)?;
    let initial_cs_sequence: i64 = r.get(11)?;

    Ok((|| -> StateBackendResult<ClientRecord> {
        let cs_cached_res = match cs_json {
            Some(j) => Some(serde_json::from_str::<CachedCreateSessionResRecord>(&j).map_err(
                |e| StateBackendError::Serialization(format!("cs_cached_res: {}", e)),
            )?),
            None => None,
        };
        Ok(ClientRecord {
            client_id: i64_to_u64(client_id),
            owner,
            verifier: i64_to_u64(verifier),
            server_owner,
            server_scope,
            sequence_id: sequence_id as u32,
            flags: flags as u32,
            principal,
            confirmed: i64_to_bool(confirmed),
            last_cs_sequence: last_cs_sequence.map(|v| v as u32),
            cs_cached_res,
            initial_cs_sequence: initial_cs_sequence as u32,
        })
    })())
}

fn decode_session_row(r: &rusqlite::Row) -> rusqlite::Result<StateBackendResult<SessionRecord>> {
    let session_id: Vec<u8> = r.get(0)?;
    let client_id: i64 = r.get(1)?;
    let sequence: i64 = r.get(2)?;
    let flags: i64 = r.get(3)?;
    let fmrs: i64 = r.get(4)?;
    let fmrespsize: i64 = r.get(5)?;
    let fmrespcached: i64 = r.get(6)?;
    let fmops: i64 = r.get(7)?;
    let fmreqs: i64 = r.get(8)?;
    let cb_program: i64 = r.get(9)?;

    Ok((|| -> StateBackendResult<SessionRecord> {
        Ok(SessionRecord {
            session_id: blob_to_array::<16>(session_id, "session_id")?,
            client_id: i64_to_u64(client_id),
            sequence: sequence as u32,
            flags: flags as u32,
            fore_chan_maxrequestsize: fmrs as u32,
            fore_chan_maxresponsesize: fmrespsize as u32,
            fore_chan_maxresponsesize_cached: fmrespcached as u32,
            fore_chan_maxops: fmops as u32,
            fore_chan_maxrequests: fmreqs as u32,
            cb_program: cb_program as u32,
        })
    })())
}

fn decode_stateid_row(r: &rusqlite::Row) -> rusqlite::Result<StateBackendResult<StateIdRecord>> {
    let other: Vec<u8> = r.get(0)?;
    let seqid: i64 = r.get(1)?;
    let state_type: i64 = r.get(2)?;
    let client_id: i64 = r.get(3)?;
    let filehandle: Option<Vec<u8>> = r.get(4)?;
    let revoked: i64 = r.get(5)?;

    Ok((|| -> StateBackendResult<StateIdRecord> {
        Ok(StateIdRecord {
            other: blob_to_array::<12>(other, "stateid.other")?,
            seqid: seqid as u32,
            state_type: i64_to_state_type(state_type)?,
            client_id: i64_to_u64(client_id),
            filehandle,
            revoked: i64_to_bool(revoked),
        })
    })())
}

fn decode_layout_row(r: &rusqlite::Row) -> rusqlite::Result<StateBackendResult<LayoutRecord>> {
    let stateid: Vec<u8> = r.get(0)?;
    let owner_client_id: i64 = r.get(1)?;
    let owner_session_id: Vec<u8> = r.get(2)?;
    let owner_fsid: i64 = r.get(3)?;
    let filehandle: Vec<u8> = r.get(4)?;
    let segments_json: String = r.get(5)?;
    let iomode: i64 = r.get(6)?;
    let return_on_close: i64 = r.get(7)?;

    Ok((|| -> StateBackendResult<LayoutRecord> {
        let segments: Vec<LayoutSegmentRecord> = serde_json::from_str(&segments_json)
            .map_err(|e| StateBackendError::Serialization(format!("segments: {}", e)))?;
        Ok(LayoutRecord {
            stateid: blob_to_array::<16>(stateid, "layout.stateid")?,
            owner_client_id: i64_to_u64(owner_client_id),
            owner_session_id: blob_to_array::<16>(owner_session_id, "owner_session_id")?,
            owner_fsid: i64_to_u64(owner_fsid),
            filehandle,
            segments,
            iomode: i64_to_iomode(iomode)?,
            return_on_close: i64_to_bool(return_on_close),
        })
    })())
}

// ── Schema ────────────────────────────────────────────────────────────

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS clients (
    client_id INTEGER PRIMARY KEY,
    owner BLOB NOT NULL,
    verifier INTEGER NOT NULL,
    server_owner TEXT NOT NULL,
    server_scope BLOB NOT NULL,
    sequence_id INTEGER NOT NULL,
    flags INTEGER NOT NULL,
    principal BLOB NOT NULL,
    confirmed INTEGER NOT NULL,
    last_cs_sequence INTEGER,
    cs_cached_res TEXT,
    initial_cs_sequence INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    session_id BLOB PRIMARY KEY,
    client_id INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    flags INTEGER NOT NULL,
    fore_chan_maxrequestsize INTEGER NOT NULL,
    fore_chan_maxresponsesize INTEGER NOT NULL,
    fore_chan_maxresponsesize_cached INTEGER NOT NULL,
    fore_chan_maxops INTEGER NOT NULL,
    fore_chan_maxrequests INTEGER NOT NULL,
    cb_program INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS stateids (
    other BLOB PRIMARY KEY,
    seqid INTEGER NOT NULL,
    state_type INTEGER NOT NULL,
    client_id INTEGER NOT NULL,
    filehandle BLOB,
    revoked INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS layouts (
    stateid BLOB PRIMARY KEY,
    owner_client_id INTEGER NOT NULL,
    owner_session_id BLOB NOT NULL,
    owner_fsid INTEGER NOT NULL,
    filehandle BLOB NOT NULL,
    segments TEXT NOT NULL,
    iomode INTEGER NOT NULL,
    return_on_close INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS instance_counter (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL
);

-- Schema v2: persistent per-deployment server identifier. Generated
-- once on first start (random non-zero u64); reused for the lifetime
-- of the state.db. FileHandleManager stamps this into every NFSv4
-- file handle so cached FHs survive MDS restart.
CREATE TABLE IF NOT EXISTS server_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    server_id INTEGER NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::tests::{
        instance_counter_monotonic, put_overwrites, round_trip_all,
        server_id_stable_and_nonzero,
    };

    #[tokio::test]
    async fn sqlite_round_trip_all_records() {
        let b = SqliteBackend::open_in_memory().unwrap();
        round_trip_all(&b).await;
    }

    #[tokio::test]
    async fn sqlite_instance_counter_monotonic() {
        let b = SqliteBackend::open_in_memory().unwrap();
        instance_counter_monotonic(&b).await;
    }

    #[tokio::test]
    async fn sqlite_put_is_upsert() {
        let b = SqliteBackend::open_in_memory().unwrap();
        put_overwrites(&b).await;
    }

    #[tokio::test]
    async fn sqlite_server_id_stable_within_lifetime() {
        let b = SqliteBackend::open_in_memory().unwrap();
        server_id_stable_and_nonzero(&b).await;
    }

    /// **The whole point of the FH-stability follow-up.** Generate
    /// a server id, drop the backend (closing the file), reopen
    /// over the same path, observe the same id. This is what makes
    /// `FileHandleManager::instance_id` survive an MDS pod roll —
    /// every NFSv4 file handle stamped with this id remains valid
    /// after restart, so the kernel's cached FHs don't error out
    /// with `NFS4ERR_BADHANDLE`.
    #[tokio::test]
    async fn sqlite_server_id_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        let id_before = {
            let b = SqliteBackend::open(&path).unwrap();
            b.get_or_init_server_id().await.unwrap()
        };
        assert_ne!(id_before, 0);

        // Reopen over the same file. New connection, new mutex, same
        // row in `server_identity`.
        let b2 = SqliteBackend::open(&path).unwrap();
        let id_after = b2.get_or_init_server_id().await.unwrap();
        assert_eq!(
            id_before, id_after,
            "server_id must survive a backend reopen — that's the point of B follow-up"
        );

        // Multiple subsequent calls also stay stable.
        assert_eq!(b2.get_or_init_server_id().await.unwrap(), id_before);
    }

    /// Concurrent first-callers race on the singleton row. SQLite +
    /// the connection mutex serialise the INSERT-OR-IGNORE so all
    /// callers observe one unique value. Catches the obvious bug
    /// where two threads each `INSERT` and end up reading different
    /// rows.
    #[tokio::test]
    async fn sqlite_server_id_atomic_under_concurrency() {
        let b = Arc::new(SqliteBackend::open_in_memory().unwrap());
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                b.get_or_init_server_id().await.unwrap()
            }));
        }
        let mut seen = std::collections::HashSet::new();
        for t in tasks {
            seen.insert(t.await.unwrap());
        }
        assert_eq!(seen.len(), 1, "all callers must observe one stable id");
    }

    /// v1 → v2 migration: an on-disk DB written by an older build
    /// (before the FH-stability follow-up) opens cleanly, gets the
    /// `server_identity` table created, and a freshly-generated id
    /// becomes the new stable value.
    #[tokio::test]
    async fn sqlite_v1_to_v2_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // Forge a v1 DB: open at v2, then downgrade the version
        // marker. (No real v1 build in tree; this is the closest
        // approximation.) Deliberately do not pre-create the
        // server_identity table — the migration must do that.
        {
            let _ = SqliteBackend::open(&path).unwrap();
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE schema_version SET version = 1 WHERE id = 1",
                [],
            )
            .unwrap();
            conn.execute("DROP TABLE server_identity", []).unwrap();
        }
        // Re-open: should run the v1→v2 migration, not error out.
        let b = SqliteBackend::open(&path).unwrap();
        let id = b.get_or_init_server_id().await.unwrap();
        assert_ne!(id, 0);
        // Subsequent re-opens see the same id.
        drop(b);
        let b2 = SqliteBackend::open(&path).unwrap();
        assert_eq!(b2.get_or_init_server_id().await.unwrap(), id);
    }

    /// **The whole point of B.2.** Write records, drop the backend
    /// (closing the file), reopen over the same path, verify every
    /// record is still there byte-identically — including the
    /// instance counter and the JSON-encoded nested types.
    #[tokio::test]
    async fn sqlite_state_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        // Phase 1: write a representative blob of every record kind,
        // then drop the backend.
        {
            let b = SqliteBackend::open(&path).unwrap();
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
                owner: b"owner".to_vec(),
                verifier: 0xdead_beef_0000_0000, // > i32::MAX, exercises u64 round-trip
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
            b.put_session(&SessionRecord {
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
            })
            .await
            .unwrap();
            b.put_stateid(&StateIdRecord {
                other: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                seqid: 5,
                state_type: StateTypeRecord::Open,
                client_id: 42,
                filehandle: Some(b"/foo/bar".to_vec()),
                revoked: false,
            })
            .await
            .unwrap();
            b.put_layout(&LayoutRecord {
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
            })
            .await
            .unwrap();
            assert_eq!(b.increment_instance_counter().await.unwrap(), 1);
            assert_eq!(b.increment_instance_counter().await.unwrap(), 2);
            // Drop closes the connection; WAL is checkpointed on close.
        }

        // Phase 2: reopen the same file, verify every record is back.
        // This is what the production restart path does in B.4.
        let b = SqliteBackend::open(&path).unwrap();
        let client = b.get_client(42).await.unwrap().unwrap();
        assert_eq!(client.verifier, 0xdead_beef_0000_0000);
        assert!(client.confirmed);
        assert_eq!(client.last_cs_sequence, Some(7));
        assert_eq!(
            client.cs_cached_res.as_ref().unwrap().fore_max_response_size_cached,
            1024
        );

        let session = b.get_session(&[9u8; 16]).await.unwrap().unwrap();
        assert_eq!(session.cb_program, 0x4000_0001);

        let stateid = b
            .get_stateid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stateid.state_type, StateTypeRecord::Open);
        assert_eq!(stateid.filehandle.as_deref(), Some(b"/foo/bar".as_slice()));

        let layout = b.get_layout(&[7u8; 16]).await.unwrap().unwrap();
        assert_eq!(layout.segments.len(), 2);
        assert_eq!(layout.segments[1].device_id, "ds-2");
        assert_eq!(layout.segments[1].length, 8 * 1024 * 1024);

        // Counter survived too. Next increment continues from 2 → 3,
        // not 0 → 1; this is the bedrock of B.4's "device IDs prefixed
        // with instance counter, never collide with pre-restart ids."
        assert_eq!(b.get_instance_counter().await.unwrap(), 2);
        assert_eq!(b.increment_instance_counter().await.unwrap(), 3);
    }

    /// Unsupported schema_version refuses to open. Operator-visible
    /// canary for "newer-build DB hit an older MDS" or "DB from
    /// the future". Supported migrations (e.g. v1→v2) succeed; only
    /// values outside the supported range error out.
    #[tokio::test]
    async fn sqlite_schema_version_mismatch_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // Create the DB normally, then forge a future version that
        // current code can't downgrade from.
        {
            let _ = SqliteBackend::open(&path).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE schema_version SET version = ?1 WHERE id = 1",
                params![SCHEMA_VERSION + 50],
            )
            .unwrap();
        }
        let err = SqliteBackend::open(&path)
            .err()
            .expect("open must reject unsupported schema version");
        match err {
            StateBackendError::Storage(msg) => {
                assert!(msg.contains("schema_version mismatch"), "got: {}", msg);
            }
            other => panic!("expected Storage variant, got {:?}", other),
        }
    }

    /// Concurrent counter increments produce distinct values. SQLite
    /// + the connection mutex serializes; we just sanity-check no
    /// dupes / no skips. Mirrors `MemoryBackend`'s atomic test.
    #[tokio::test]
    async fn sqlite_instance_counter_atomic() {
        let b = Arc::new(SqliteBackend::open_in_memory().unwrap());
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                b.increment_instance_counter().await.unwrap()
            }));
        }
        let mut seen: Vec<u64> = Vec::new();
        for t in tasks {
            seen.push(t.await.unwrap());
        }
        seen.sort();
        assert_eq!(seen, (1..=16).collect::<Vec<u64>>());
        assert_eq!(b.get_instance_counter().await.unwrap(), 16);
    }
}
