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
//! ## Concurrency model (F27 redesign, 2026-07-19)
//!
//! One dedicated writer THREAD owns the `Connection`; everything else
//! talks to it through an ordered channel. This replaced the original
//! `Arc<Mutex<Connection>>` + per-op `spawn_blocking` scheme, which
//! had three compounding problems under load: every row paid its own
//! serialized fsync (`synchronous=FULL` in production), every queued
//! persist parked a tokio blocking-pool thread while waiting on the
//! mutex, and the spawned persist tasks raced each other so a
//! put/delete pair for the same key could apply in reverse order and
//! resurrect the deleted row (the F27 ordering bug).
//!
//! The writer applies requests in channel order with **per-key
//! coalescing and group commit**: it drains whatever accumulated
//! while the previous transaction was committing, keeps only the
//! latest op per (table, key), applies the batch in one transaction,
//! then acks. Sequential callers see one commit per op (same latency
//! as before); concurrent bursts batch automatically, so throughput
//! scales with commit rate × batch size instead of being fsync-bound
//! per row. Reads and read-modify-write ops (`with_conn`) act as
//! barriers: the pending batch is flushed first, so read-your-writes
//! holds for every earlier call on this backend.
//!
//! Shutdown: dropping the backend closes the channel; the writer
//! flushes the remaining batch and exits, and `Drop` joins it. Ops
//! whose `enqueue_write` happened before the drop are therefore
//! committed (bounded loss only on abrupt process kill, where the
//! window is one in-flight batch — a few ms — strictly tighter than
//! the old unbounded spawn backlog).
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
    CachedCreateSessionResRecord, ClientRecord, FhMappingRecord, IoModeRecord, LayoutRecord,
    LayoutSegmentRecord, LockRecord, PlacementRecord, SessionRecord, StateBackend,
    StateBackendError, StateBackendResult, StateIdRecord, StateTypeRecord, WriteOp, WriteOpKey,
};
use async_trait::async_trait;
use crossbeam::channel::{unbounded, Receiver, Sender, TryRecvError};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::oneshot;

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
///   3 → add `clients.reclaim_complete` so a post-restart MDS knows
///        which clients have already done RECLAIM_COMPLETE — without
///        this, a second RECLAIM_COMPLETE would silently succeed
///        instead of returning `NFS4ERR_COMPLETE_ALREADY`.
///   4 → add `locks` (byte-range lock table). Lock STATEIDS already
///        persisted (v1 `stateids` rows with state_type=Lock) but the
///        lock substance was memory-only, so after a restart the
///        stateid validated while mutual exclusion was silently gone.
///        New table only — handled by the schema-batch's CREATE TABLE
///        IF NOT EXISTS, like v1 → v2.
///   5 → add `file_placement` (per-file stripe placement, durable-DS
///        plan Phase 0). Layout grants pin each file's ordered DS
///        list + stripe size here; without it the stripe map is
///        recomputed from the live device registry and silently
///        re-maps existing data when the fleet changes. New table
///        only — handled by the schema-batch's CREATE TABLE IF NOT
///        EXISTS.
const SCHEMA_VERSION: i64 = 6;

/// One request to the writer thread.
enum Req {
    /// A coalescable point write. `None` ack = enqueue_write
    /// (fire-and-forget, errors logged by the writer); `Some` ack =
    /// an awaited trait method, resolved when the op's batch commits.
    Write(WriteOp, Option<oneshot::Sender<StateBackendResult<()>>>),
    /// Flush the pending batch, then run this closure on the
    /// connection (reads, counter read-modify-writes). The closure
    /// carries its own response channel.
    Barrier(Box<dyn FnOnce(&mut Connection) + Send>),
}

/// Max ops per transaction — bounds transaction size under a burst;
/// the writer flushes and keeps draining when a batch fills.
const MAX_BATCH: usize = 1024;

/// Single-file SQLite [`StateBackend`]. See the module docs for the
/// writer-thread concurrency model.
pub struct SqliteBackend {
    /// `Some` until Drop. Dropping the sender is the writer's
    /// shutdown signal, so Drop takes it before joining.
    tx: Option<Sender<Req>>,
    writer: Option<std::thread::JoinHandle<()>>,
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
        Self::init(conn, false)
    }

    /// Like `open`, but with `synchronous=FULL`: every commit fsyncs the
    /// WAL before returning. Required when the DB lives on a volume whose
    /// backing device can be torn away without a clean unmount — the
    /// standalone NFS server's export-volume state DB is unstaged via a
    /// bounded umount that falls back to LAZY on a busy mount, after
    /// which the raid is deleted and any commit still in the page cache
    /// is gone (observed live, 2026-06-12). NORMAL's "durable at the
    /// checkpoint" promise assumes the filesystem outlives the process;
    /// here it may not. Group commit amortizes the per-commit fsync
    /// across every op in the batch, so FULL stays affordable under an
    /// OPEN/CLOSE storm.
    pub fn open_durable<P: AsRef<Path>>(path: P) -> StateBackendResult<Self> {
        let conn = Connection::open(path).map_err(|e| {
            StateBackendError::Storage(format!("open: {}", e))
        })?;
        Self::init(conn, true)
    }

    /// Open an in-memory DB. Useful for tests; the schema still gets
    /// applied so behaviour matches a real on-disk file. Production
    /// code should use `open` with a path.
    #[cfg(test)]
    pub fn open_in_memory() -> StateBackendResult<Self> {
        let conn = Connection::open_in_memory().map_err(|e| {
            StateBackendError::Storage(format!("open_in_memory: {}", e))
        })?;
        Self::init(conn, false)
    }

    fn init(conn: Connection, durable: bool) -> StateBackendResult<Self> {
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
            Some(prev) if prev >= 1 && prev < SCHEMA_VERSION => {
                // Stepwise migration. Each step is idempotent against
                // the running schema-batch (CREATE TABLE IF NOT
                // EXISTS already created any net-new tables); the
                // ALTER steps need explicit handling because SQLite
                // doesn't have IF NOT EXISTS for columns.
                //
                //  v1 → v2: server_identity table (handled by the
                //           schema-batch's IF NOT EXISTS).
                //  v2 → v3: clients.reclaim_complete column. ALTER
                //           with NOT NULL DEFAULT 0 so existing rows
                //           default to "haven't done RECLAIM_COMPLETE"
                //           — matches the conservative interpretation
                //           (a pre-v3 client gets to do RECLAIM_COMPLETE
                //           one more time post-upgrade, which is a
                //           harmless no-op).
                if prev < 2 {
                    tracing::info!("SqliteBackend: migrating schema → 2 (server_identity)");
                }
                if prev < 3 {
                    // Idempotent ALTER TABLE: SQLite has no
                    // "ADD COLUMN IF NOT EXISTS", so we ask
                    // pragma_table_info whether the column is already
                    // there. Idempotency matters because an
                    // interrupted migration could leave the column
                    // present but `schema_version` still at 2.
                    let has_col: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM pragma_table_info('clients') WHERE name = 'reclaim_complete'",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| {
                        StateBackendError::Storage(format!("migrate v→3 (probe column): {}", e))
                    })?;
                    if has_col == 0 {
                        conn.execute(
                            "ALTER TABLE clients ADD COLUMN reclaim_complete INTEGER NOT NULL DEFAULT 0",
                            [],
                        )
                        .map_err(|e| {
                            StateBackendError::Storage(format!("migrate v→3 (alter clients): {}", e))
                        })?;
                    }
                    tracing::info!("SqliteBackend: migrating schema → 3 (clients.reclaim_complete)");
                }
                if prev < 4 {
                    // locks table: created by the schema-batch above.
                    tracing::info!("SqliteBackend: migrating schema → 4 (locks table)");
                }
                if prev < 5 {
                    // file_placement table: created by the schema-batch above.
                    tracing::info!("SqliteBackend: migrating schema → 5 (file_placement table)");
                }
                if prev < 6 {
                    // file_placement.file_id column. DEFAULT 0 = the
                    // legacy path-keyed sentinel, which is exactly the
                    // right meaning for pre-upgrade pins.
                    let has_col: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM pragma_table_info('file_placement') WHERE name = 'file_id'",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| {
                        StateBackendError::Storage(format!("migrate v→6 (probe column): {}", e))
                    })?;
                    if has_col == 0 {
                        conn.execute(
                            "ALTER TABLE file_placement ADD COLUMN file_id INTEGER NOT NULL DEFAULT 0",
                            [],
                        )
                        .map_err(|e| {
                            StateBackendError::Storage(format!("migrate v→6 (alter file_placement): {}", e))
                        })?;
                    }
                    tracing::info!("SqliteBackend: migrating schema → 6 (file_placement.file_id)");
                }
                conn.execute(
                    "UPDATE schema_version SET version = ?1 WHERE id = 1",
                    params![SCHEMA_VERSION],
                )
                .map_err(|e| {
                    StateBackendError::Storage(format!("schema_version migrate: {}", e))
                })?;
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

        if durable {
            conn.execute_batch("PRAGMA synchronous=FULL;")
                .map_err(|e| StateBackendError::Storage(format!("synchronous=FULL: {}", e)))?;
        }

        // Schema is settled — hand the connection to its writer thread.
        let (tx, rx) = unbounded::<Req>();
        let writer = std::thread::Builder::new()
            .name("flint-state-writer".into())
            .spawn(move || writer_loop(conn, rx))
            .map_err(|e| StateBackendError::Storage(format!("writer thread: {}", e)))?;

        Ok(Self {
            tx: Some(tx),
            writer: Some(writer),
        })
    }

    fn sender(&self) -> &Sender<Req> {
        self.tx.as_ref().expect("sender present until Drop")
    }

    /// Awaited write: enqueue and resolve when the op's batch commits.
    async fn write(&self, op: WriteOp) -> StateBackendResult<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.sender()
            .send(Req::Write(op, Some(ack_tx)))
            .map_err(|_| StateBackendError::Storage("state writer thread gone".into()))?;
        ack_rx
            .await
            .map_err(|_| StateBackendError::Storage("state writer thread gone".into()))?
    }

    /// Run a closure on the connection, ordered AFTER everything
    /// already enqueued (the writer flushes its pending batch first) —
    /// so read-your-writes holds. Any rusqlite error maps to `Storage`.
    async fn with_conn<T, F>(&self, f: F) -> StateBackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (res_tx, res_rx) = oneshot::channel();
        self.sender()
            .send(Req::Barrier(Box::new(move |conn: &mut Connection| {
                let _ = res_tx.send(f(conn));
            })))
            .map_err(|_| StateBackendError::Storage("state writer thread gone".into()))?;
        res_rx
            .await
            .map_err(|_| StateBackendError::Storage("state writer thread gone".into()))?
            .map_err(|e| StateBackendError::Storage(format!("sqlite: {}", e)))
    }

    /// Barrier no-op: resolves once everything enqueued before it is
    /// committed. For tests and graceful-shutdown call sites.
    pub async fn flush(&self) -> StateBackendResult<()> {
        self.with_conn(|_| Ok(())).await
    }
}

impl Drop for SqliteBackend {
    fn drop(&mut self) {
        // Close the channel (shutdown signal), then wait for the
        // writer's final flush so every op enqueued before this drop
        // reaches the DB.
        self.tx.take();
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
    }
}

// ── Writer thread ─────────────────────────────────────────────────────

type PendingBatch = HashMap<WriteOpKey, (WriteOp, Vec<oneshot::Sender<StateBackendResult<()>>>)>;

/// Owns the connection for the backend's lifetime. Adaptive group
/// commit: whatever accumulates in the channel while a transaction is
/// committing forms the next batch — a lone sequential caller gets
/// one commit per op (no added latency), a concurrent burst batches
/// automatically. Barriers flush first, preserving order.
fn writer_loop(mut conn: Connection, rx: Receiver<Req>) {
    let mut batch: PendingBatch = HashMap::new();
    loop {
        // Block for the next request; channel closed = shutdown.
        let req = match rx.recv() {
            Ok(r) => r,
            Err(_) => break,
        };
        absorb(req, &mut conn, &mut batch);
        // Drain everything immediately available before committing.
        loop {
            if batch.len() >= MAX_BATCH {
                flush(&mut conn, &mut batch);
            }
            match rx.try_recv() {
                Ok(r) => absorb(r, &mut conn, &mut batch),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    flush(&mut conn, &mut batch);
                    return;
                }
            }
        }
        flush(&mut conn, &mut batch);
    }
    flush(&mut conn, &mut batch);
}

fn absorb(req: Req, conn: &mut Connection, batch: &mut PendingBatch) {
    match req {
        Req::Write(op, ack) => {
            let entry = batch.entry(op.key()).or_insert_with(|| (op.clone(), Vec::new()));
            // Later op for the same key wins (put-then-delete ⇒ delete,
            // put-then-put ⇒ last). Superseded acks stay attached: their
            // effect is subsumed by this batch's commit.
            entry.0 = op;
            if let Some(a) = ack {
                entry.1.push(a);
            }
        }
        Req::Barrier(f) => {
            flush(conn, batch);
            f(conn);
        }
    }
}

fn flush(conn: &mut Connection, batch: &mut PendingBatch) {
    if batch.is_empty() {
        return;
    }
    let ops: Vec<(WriteOp, Vec<oneshot::Sender<StateBackendResult<()>>>)> =
        batch.drain().map(|(_, v)| v).collect();

    let res: rusqlite::Result<()> = (|| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (op, _) in &ops {
            apply_write_op(&tx, op)?;
        }
        tx.commit()
    })();

    match res {
        Ok(()) => {
            for (_, acks) in ops {
                for a in acks {
                    let _ = a.send(Ok(()));
                }
            }
        }
        Err(e) => {
            // Batch failed as a unit (disk full, I/O error, …). Retry
            // each op in its own autocommit transaction so per-op error
            // semantics survive — one poisoned op shouldn't fail its
            // batch-mates.
            tracing::warn!(
                target: "state_persist",
                error = %e,
                ops = ops.len(),
                "group commit failed; retrying ops individually"
            );
            for (op, acks) in ops {
                let r: StateBackendResult<()> = apply_write_op(&conn, &op)
                    .map_err(|e2| StateBackendError::Storage(format!("sqlite: {}", e2)));
                if let Err(ref err) = r {
                    tracing::error!(
                        target: "state_persist",
                        label = op.label(),
                        error = %err,
                        "persist failed"
                    );
                }
                for a in acks {
                    let _ = a.send(r.clone());
                }
            }
        }
    }
}

/// Apply one point write. Runs inside the group-commit transaction
/// (or standalone autocommit on the retry path). `prepare_cached`
/// keeps the statement compile cost off the steady state.
fn apply_write_op(conn: &Connection, op: &WriteOp) -> rusqlite::Result<()> {
    match op {
        WriteOp::PutClient(c) => {
            // INSERT OR REPLACE = upsert, matches MemoryBackend
            // semantics. serde_json for the optional
            // CachedCreateSessionResRecord — small struct, stable
            // schema, easier to migrate than a sub-table.
            let cs_json = match &c.cs_cached_res {
                Some(v) => Some(
                    serde_json::to_string(v)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                ),
                None => None,
            };
            conn.prepare_cached(
                "INSERT OR REPLACE INTO clients
                 (client_id, owner, verifier, server_owner, server_scope,
                  sequence_id, flags, principal, confirmed,
                  last_cs_sequence, cs_cached_res, initial_cs_sequence,
                  reclaim_complete)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?
            .execute(params![
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
                bool_to_i64(c.reclaim_complete),
            ])?;
        }
        WriteOp::DeleteClient(id) => {
            conn.prepare_cached("DELETE FROM clients WHERE client_id = ?1")?
                .execute(params![u64_to_i64(*id)])?;
        }
        WriteOp::PutSession(s) => {
            conn.prepare_cached(
                "INSERT OR REPLACE INTO sessions
                 (session_id, client_id, sequence, flags,
                  fore_chan_maxrequestsize, fore_chan_maxresponsesize,
                  fore_chan_maxresponsesize_cached, fore_chan_maxops,
                  fore_chan_maxrequests, cb_program)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
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
            ])?;
        }
        WriteOp::DeleteSession(id) => {
            conn.prepare_cached("DELETE FROM sessions WHERE session_id = ?1")?
                .execute(params![id.to_vec()])?;
        }
        WriteOp::PutStateid(s) => {
            conn.prepare_cached(
                "INSERT OR REPLACE INTO stateids
                 (other, seqid, state_type, client_id, filehandle, revoked)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                s.other.to_vec(),
                s.seqid as i64,
                state_type_to_i64(s.state_type),
                u64_to_i64(s.client_id),
                s.filehandle,
                bool_to_i64(s.revoked),
            ])?;
        }
        WriteOp::DeleteStateid(o) => {
            conn.prepare_cached("DELETE FROM stateids WHERE other = ?1")?
                .execute(params![o.to_vec()])?;
        }
        WriteOp::PutLock(l) => {
            conn.prepare_cached(
                "INSERT OR REPLACE INTO locks
                 (other, seqid, client_id, owner, filehandle, lock_type, offset, length)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                l.other.to_vec(),
                l.seqid as i64,
                u64_to_i64(l.client_id),
                l.owner,
                l.filehandle,
                l.lock_type as i64,
                u64_to_i64(l.offset),
                u64_to_i64(l.length),
            ])?;
        }
        WriteOp::DeleteLock(o) => {
            conn.prepare_cached("DELETE FROM locks WHERE other = ?1")?
                .execute(params![o.to_vec()])?;
        }
        WriteOp::PutLayout(l) => {
            let segments_json = serde_json::to_string(&l.segments)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            conn.prepare_cached(
                "INSERT OR REPLACE INTO layouts
                 (stateid, owner_client_id, owner_session_id, owner_fsid,
                  filehandle, segments, iomode, return_on_close)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                l.stateid.to_vec(),
                u64_to_i64(l.owner_client_id),
                l.owner_session_id.to_vec(),
                u64_to_i64(l.owner_fsid),
                l.filehandle,
                segments_json,
                iomode_to_i64(l.iomode),
                bool_to_i64(l.return_on_close),
            ])?;
        }
        WriteOp::DeleteLayout(s) => {
            conn.prepare_cached("DELETE FROM layouts WHERE stateid = ?1")?
                .execute(params![s.to_vec()])?;
        }
        WriteOp::PutPlacement(p) => {
            let device_ids_json = serde_json::to_string(&p.device_ids)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            conn.prepare_cached(
                "INSERT OR REPLACE INTO file_placement (file_key, stripe_size, device_ids, file_id)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                p.file_key,
                u64_to_i64(p.stripe_size),
                device_ids_json,
                u64_to_i64(p.file_id),
            ])?;
        }
        WriteOp::DeletePlacement(k) => {
            conn.prepare_cached("DELETE FROM file_placement WHERE file_key = ?1")?
                .execute(params![k])?;
        }
        WriteOp::PutFhMapping(m) => {
            conn.prepare_cached("INSERT OR REPLACE INTO fh_mappings (file_id, path) VALUES (?1, ?2)")?
                .execute(params![u64_to_i64(m.file_id), m.path])?;
        }
        WriteOp::DeleteFhMapping(id) => {
            conn.prepare_cached("DELETE FROM fh_mappings WHERE file_id = ?1")?
                .execute(params![u64_to_i64(*id)])?;
        }
    }
    Ok(())
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
    /// Fire-and-forget, ordered: the channel send happens at the call
    /// site, so channel order = call order (the F27 guarantee). Errors
    /// are logged by the writer under `state_persist`.
    fn enqueue_write(&self, op: WriteOp) {
        if self.sender().send(Req::Write(op, None)).is_err() {
            tracing::error!(
                target: "state_persist",
                "enqueue_write dropped: state writer thread gone"
            );
        }
    }

    async fn put_client(&self, c: &ClientRecord) -> StateBackendResult<()> {
        self.write(WriteOp::PutClient(c.clone())).await
    }

    async fn get_client(&self, client_id: u64) -> StateBackendResult<Option<ClientRecord>> {
        let id = u64_to_i64(client_id);
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT client_id, owner, verifier, server_owner, server_scope,
                            sequence_id, flags, principal, confirmed,
                            last_cs_sequence, cs_cached_res, initial_cs_sequence,
                            reclaim_complete
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
                            last_cs_sequence, cs_cached_res, initial_cs_sequence,
                            reclaim_complete
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
        self.write(WriteOp::DeleteClient(client_id)).await
    }

    async fn put_session(&self, s: &SessionRecord) -> StateBackendResult<()> {
        self.write(WriteOp::PutSession(s.clone())).await
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
        self.write(WriteOp::DeleteSession(*session_id)).await
    }

    async fn put_stateid(&self, s: &StateIdRecord) -> StateBackendResult<()> {
        self.write(WriteOp::PutStateid(s.clone())).await
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
        self.write(WriteOp::DeleteStateid(*other)).await
    }

    async fn put_lock(&self, l: &LockRecord) -> StateBackendResult<()> {
        self.write(WriteOp::PutLock(l.clone())).await
    }

    async fn get_lock(&self, other: &[u8; 12]) -> StateBackendResult<Option<LockRecord>> {
        let key = other.to_vec();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT other, seqid, client_id, owner, filehandle, lock_type, offset, length
                     FROM locks WHERE other = ?1",
                    params![key],
                    decode_lock_row,
                )
                .optional()
            })
            .await?;
        row.transpose()
    }

    async fn list_locks(&self) -> StateBackendResult<Vec<LockRecord>> {
        let rows = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT other, seqid, client_id, owner, filehandle, lock_type, offset, length
                     FROM locks",
                )?;
                let rows: rusqlite::Result<Vec<_>> =
                    stmt.query_map([], decode_lock_row)?.collect();
                rows
            })
            .await?;
        rows.into_iter().collect()
    }

    async fn delete_lock(&self, other: &[u8; 12]) -> StateBackendResult<()> {
        self.write(WriteOp::DeleteLock(*other)).await
    }

    async fn put_layout(&self, l: &LayoutRecord) -> StateBackendResult<()> {
        self.write(WriteOp::PutLayout(l.clone())).await
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
        self.write(WriteOp::DeleteLayout(*stateid)).await
    }

    async fn put_fh_mapping(&self, m: &FhMappingRecord) -> StateBackendResult<()> {
        self.write(WriteOp::PutFhMapping(m.clone())).await
    }

    async fn list_fh_mappings(&self) -> StateBackendResult<Vec<FhMappingRecord>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare("SELECT file_id, path FROM fh_mappings")?;
            let rows = stmt.query_map([], |row| {
                Ok(FhMappingRecord {
                    file_id: i64_to_u64(row.get::<_, i64>(0)?),
                    path: row.get(1)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn delete_fh_mapping(&self, file_id: u64) -> StateBackendResult<()> {
        self.write(WriteOp::DeleteFhMapping(file_id)).await
    }

    async fn put_placement(&self, p: &PlacementRecord) -> StateBackendResult<()> {
        self.write(WriteOp::PutPlacement(p.clone())).await
    }

    async fn get_placement(&self, file_key: &str) -> StateBackendResult<Option<PlacementRecord>> {
        let key = file_key.to_string();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT file_key, stripe_size, device_ids, file_id
                     FROM file_placement WHERE file_key = ?1",
                    params![key],
                    decode_placement_row,
                )
                .optional()
            })
            .await?;
        row.transpose()
    }

    async fn list_placements(&self) -> StateBackendResult<Vec<PlacementRecord>> {
        let rows = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT file_key, stripe_size, device_ids, file_id FROM file_placement",
                )?;
                let rows: rusqlite::Result<Vec<_>> =
                    stmt.query_map([], decode_placement_row)?.collect();
                rows
            })
            .await?;
        rows.into_iter().collect()
    }

    async fn delete_placement(&self, file_key: &str) -> StateBackendResult<()> {
        self.write(WriteOp::DeletePlacement(file_key.to_string())).await
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
    let reclaim_complete: i64 = r.get(12)?;

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
            reclaim_complete: i64_to_bool(reclaim_complete),
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

fn decode_lock_row(r: &rusqlite::Row) -> rusqlite::Result<StateBackendResult<LockRecord>> {
    let other: Vec<u8> = r.get(0)?;
    let seqid: i64 = r.get(1)?;
    let client_id: i64 = r.get(2)?;
    let owner: Vec<u8> = r.get(3)?;
    let filehandle: Vec<u8> = r.get(4)?;
    let lock_type: i64 = r.get(5)?;
    let offset: i64 = r.get(6)?;
    let length: i64 = r.get(7)?;

    Ok((|| -> StateBackendResult<LockRecord> {
        Ok(LockRecord {
            other: blob_to_array::<12>(other, "lock.other")?,
            seqid: seqid as u32,
            client_id: i64_to_u64(client_id),
            owner,
            filehandle,
            lock_type: lock_type as u32,
            offset: i64_to_u64(offset),
            length: i64_to_u64(length),
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

fn decode_placement_row(
    r: &rusqlite::Row,
) -> rusqlite::Result<StateBackendResult<PlacementRecord>> {
    let file_key: String = r.get(0)?;
    let stripe_size: i64 = r.get(1)?;
    let device_ids_json: String = r.get(2)?;
    let file_id: i64 = r.get(3)?;

    Ok((|| -> StateBackendResult<PlacementRecord> {
        let device_ids: Vec<String> = serde_json::from_str(&device_ids_json)
            .map_err(|e| StateBackendError::Serialization(format!("device_ids: {}", e)))?;
        Ok(PlacementRecord {
            file_key,
            stripe_size: i64_to_u64(stripe_size),
            device_ids,
            file_id: i64_to_u64(file_id),
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
    initial_cs_sequence INTEGER NOT NULL,
    -- Schema v3: see SCHEMA_VERSION docs.
    reclaim_complete INTEGER NOT NULL DEFAULT 0
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

-- Schema v4: byte-range locks (see SCHEMA_VERSION docs). Keyed by the
-- lock stateid's `other`, mirroring the in-memory LockManager table.
CREATE TABLE IF NOT EXISTS locks (
    other BLOB PRIMARY KEY,
    seqid INTEGER NOT NULL,
    client_id INTEGER NOT NULL,
    owner BLOB NOT NULL,
    filehandle BLOB NOT NULL,
    lock_type INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    length INTEGER NOT NULL
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

-- Schema v5: per-file stripe placement (durable-DS plan Phase 0).
-- device_ids is an ordered JSON array; order is load-bearing (it IS
-- the stripe map). Keyed by export-relative path.
CREATE TABLE IF NOT EXISTS file_placement (
    file_key TEXT PRIMARY KEY,
    stripe_size INTEGER NOT NULL,
    device_ids TEXT NOT NULL,
    file_id INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS instance_counter (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL
);

-- id<->path mappings behind v2 (id-based) NFSv4 metadata filehandles.
-- Minted only for paths too long to embed in the 128-byte handle;
-- RENAME rewrites path in place (the id follows the file), REMOVE
-- deletes the row. A lost row = NFS4ERR_STALE = client re-walks.
CREATE TABLE IF NOT EXISTS fh_mappings (
    file_id INTEGER PRIMARY KEY,
    path TEXT NOT NULL
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
    use std::sync::Arc;

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
                reclaim_complete: true,
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

    // ── F27 writer tests ─────────────────────────────────────────────

    fn sid(i: u64, seqid: u32) -> StateIdRecord {
        let mut other = [0u8; 12];
        other[0..8].copy_from_slice(&i.to_be_bytes());
        StateIdRecord {
            other,
            seqid,
            state_type: StateTypeRecord::Open,
            client_id: 7,
            filehandle: Some(b"/fh".to_vec()),
            revoked: false,
        }
    }

    /// THE F27 ordering bug, as a regression test: put-then-delete for
    /// a key must end DELETED, and put-then-put must keep the LAST
    /// seqid — under a burst that exercises coalescing and group
    /// commit. Under the old spawn-per-op scheme the equivalent
    /// sequence raced and could resurrect the deleted row.
    #[tokio::test]
    async fn enqueue_write_order_and_coalescing() {
        let b = SqliteBackend::open_in_memory().unwrap();
        for i in 0..1000u64 {
            b.enqueue_write(WriteOp::PutStateid(sid(i, 1)));
            b.enqueue_write(WriteOp::PutStateid(sid(i, 2)));
            if i % 2 == 1 {
                b.enqueue_write(WriteOp::DeleteStateid(sid(i, 0).other));
            }
        }
        b.flush().await.unwrap();
        let all = b.list_stateids().await.unwrap();
        assert_eq!(all.len(), 500, "odd keys deleted, even keys live");
        for r in all {
            assert_eq!(r.seqid, 2, "put-then-put keeps the LAST write");
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&r.other[0..8]);
            assert_eq!(u64::from_be_bytes(buf) % 2, 0, "deleted key resurrected");
        }
    }

    /// Read-your-writes across the barrier: an awaited get placed
    /// after enqueue_writes must observe them (the barrier flushes the
    /// pending batch first).
    #[tokio::test]
    async fn barrier_read_sees_enqueued_writes() {
        let b = SqliteBackend::open_in_memory().unwrap();
        b.enqueue_write(WriteOp::PutStateid(sid(1, 5)));
        b.enqueue_write(WriteOp::PutStateid(sid(1, 6)));
        let got = b.get_stateid(&sid(1, 0).other).await.unwrap().unwrap();
        assert_eq!(got.seqid, 6);
    }

    /// Dropping the backend closes the channel; the writer's final
    /// flush must commit every op enqueued before the drop — this is
    /// the graceful-shutdown loss-window guarantee.
    #[tokio::test]
    async fn drop_flushes_pending_writes() {
        let dir = std::env::temp_dir().join(format!("f27_drop_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let b = SqliteBackend::open(&path).unwrap();
            for i in 0..100u64 {
                b.enqueue_write(WriteOp::PutStateid(sid(i, 1)));
            }
            // No flush — Drop must do it.
        }
        let b2 = SqliteBackend::open(&path).unwrap();
        assert_eq!(b2.list_stateids().await.unwrap().len(), 100);
        drop(b2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Awaited writes racing a same-key enqueue stream still resolve,
    /// and their acks reflect the committed batch (superseded ops ack
    /// on the batch that subsumed them).
    #[tokio::test]
    async fn awaited_write_acks_under_coalescing() {
        let b = Arc::new(SqliteBackend::open_in_memory().unwrap());
        let mut tasks = Vec::new();
        for i in 0..64u64 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                b.put_stateid(&sid(i, 1)).await.unwrap();
                b.delete_stateid(&sid(i, 0).other).await.unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(b.list_stateids().await.unwrap().len(), 0);
    }

    /// F27 throughput gate (informational; run with --ignored):
    /// concurrent awaited put_stateid on a DURABLE (synchronous=FULL)
    /// on-disk DB. Group commit must take this far beyond the old
    /// one-fsync-per-row ceiling (~1k/s); the B2 plan gate is ≥10k/s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn f27_writer_throughput_bench() {
        let dir = std::env::temp_dir().join(format!("f27_bench_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let b = Arc::new(SqliteBackend::open_durable(dir.join("state.db")).unwrap());

        let n: u64 = 20_000;
        let start = std::time::Instant::now();
        let mut tasks = Vec::new();
        for i in 0..n {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                b.put_stateid(&sid(i, 1)).await.unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let elapsed = start.elapsed();
        let ops_s = n as f64 / elapsed.as_secs_f64();
        eprintln!(
            "F27 writer: {} awaited durable puts in {:.2?} = {:.0} ops/s",
            n, elapsed, ops_s
        );
        assert_eq!(b.list_stateids().await.unwrap().len() as u64, n);
        drop(b);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
