//! Block-layout extent allocator — the transaction core.
//!
//! Spec lineage, in order of authority: `formal/FlintExtents.tla` (the
//! machine-checked state machine; its mutation runs are this module's
//! failure catalogue), then `docs/plans/pnfs-block-layout-design.md` §8.
//! Every function here is one sqlite transaction over the extent tables in
//! `SCHEMA_SQL`, and the tests replay the TLC counterexample traces against
//! the real transactions — the model found the bugs, these tests keep them
//! found.
//!
//! THE TWO CONSTRAINTS THE MODEL PROVED, which are load-bearing here and
//! must survive every refactor:
//!
//! 1. **The free transaction re-validates holders** (`FreeRevalidates`).
//!    A reclaim's start-time holder snapshot cannot be trusted at free
//!    time — a grant published after the snapshot escapes it, and no
//!    grant-time check can compensate, because a freed block has left the
//!    very tables the grant transaction validates against.
//!    `reclaim_complete` therefore refuses while any live unfenced grant
//!    covers a target extent (`NotQuiescent`), no matter what the caller's
//!    bookkeeping says. `FlintExtentsStaleSnapshotFree.cfg` is the world
//!    without this check.
//!
//! 2. **Disjointness is policed in the transaction, not by the PK.** The
//!    extent PKs admit overlapping ranges as distinct rows, so every write
//!    transaction ends with `verify_volume_invariants` — logical
//!    disjointness per file, physical disjointness volume-wide across
//!    extents ∪ free-list ∪ quarantine, watermark containment, and
//!    grant-row referential + generation integrity. `GrantOverlap` (the
//!    §8 PK landmine) is the world without it.
//!
//! Fencing here is BOOKKEEPING ONLY: `fence_client` marks grant rows dead
//! server-side. Whether the NVMe-level preempt actually lands is the
//! model's `FenceReaches`, FALSE until the phase-2 rig proves it — which
//! is why fenced-holder extents QUARANTINE (leaked, metered, operator
//! lever) instead of freeing. A LAYOUTRETURN arriving after a fence
//! upgrades the outcome to a clean free: the return is the client's own
//! promise that no more I/O will be issued under the layout, which is the
//! quiescence the quarantine otherwise cannot observe. (The model agrees:
//! a returned holder leaves `HeldBy` empty and `ReclaimComplete` frees.)
//!
//! Not here yet, by phasing: the LAYOUTGET/LAYOUTCOMMIT wire surface, the
//! truncate-gate interplay (C6 recheck), blksize alignment, split/merge,
//! the merge policy for the free list, and the MDS fallback lane's
//! grant-consultation — each arrives with its own model tranche.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// Everything the allocator can refuse with. Wire-level mapping happens at
/// the dispatcher (e.g. `CommitRejected` → NFS4ERR_BADLAYOUT).
#[derive(Debug)]
pub enum ExtentAllocError {
    /// Another client holds a live unfenced grant on part of the range.
    Conflict { holders: Vec<u64> },
    /// The grant transaction refuses a fenced client outright: no
    /// re-admission before lease recovery.
    FencedClient,
    /// The free transaction's re-validation found live unfenced holders
    /// (the FreeRevalidates belt). Recall/fence them and try again.
    NotQuiescent { holders: Vec<u64> },
    /// Free list + bump watermark cannot satisfy the request.
    NoSpace { needed: u64, ceiling: u64, next_free: u64 },
    /// The volume's extents table would exceed the stated per-volume
    /// row bound (the merge policy's backstop — §8's "stated
    /// extent-count bound"; nothing else bounds table growth). Space
    /// may well remain; ROWS ran out, which is a fragmentation
    /// statement, and the merge policy is what normally keeps it far
    /// away.
    RowBudget { rows: u64, budget: u64 },
    /// LAYOUTCOMMIT validation failed — the (client, gen-at-grant) pair
    /// does not match a live grant on a live extent.
    CommitRejected(&'static str),
    /// A range or argument is malformed (zero length, overflow).
    InvalidRange(&'static str),
    /// An invariant the schema cannot express was found violated. The
    /// enclosing transaction is rolled back; nothing was written.
    Corruption(String),
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for ExtentAllocError {
    fn from(e: rusqlite::Error) -> Self {
        ExtentAllocError::Sql(e)
    }
}

impl std::fmt::Display for ExtentAllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { holders } => write!(f, "range held by clients {holders:?}"),
            Self::FencedClient => write!(f, "client is fenced; no grants before lease recovery"),
            Self::NotQuiescent { holders } => {
                write!(f, "free refused: live unfenced grants by clients {holders:?}")
            }
            Self::NoSpace { needed, ceiling, next_free } => write!(
                f,
                "no space: need {needed}, watermark {next_free} of ceiling {ceiling}"
            ),
            Self::RowBudget { rows, budget } => write!(
                f,
                "extent-row budget: {rows} rows at the {budget}-row per-volume bound \
                 (fragmentation — space may remain; see FLINT_PNFS_EXTENT_ROW_BUDGET)"
            ),
            Self::CommitRejected(r) => write!(f, "commit rejected: {r}"),
            Self::InvalidRange(r) => write!(f, "invalid range: {r}"),
            Self::Corruption(r) => write!(f, "extent-table corruption: {r}"),
            Self::Sql(e) => write!(f, "sqlite: {e}"),
        }
    }
}

impl std::error::Error for ExtentAllocError {}

pub type Result<T> = std::result::Result<T, ExtentAllocError>;

/// One extent as granted to a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedExtent {
    pub logical_offset: u64,
    pub length: u64,
    pub physical_offset: u64,
    pub generation: u64,
    /// `false` = INVALID_DATA (provisional: client must write before it
    /// may read); `true` = READ_WRITE_DATA (committed).
    pub committed: bool,
    /// The range was just re-allocated from the free list and still
    /// carries a previous incarnation's bytes — the wire layer MUST
    /// write_zeroes it before the layout leaves the MDS (the model's
    /// ProvisionalInvisible belt; FlintExtentsBlindProvision.cfg is the
    /// world where nobody does — deleted-data resurrection). Virgin
    /// bump-allocated space reads zeros already (thin-lvol unwritten
    /// clusters), and existing extents are the same incarnation, so both
    /// carry `false`.
    pub needs_scrub: bool,
}

/// What a completed free did with each target extent.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FreeOutcome {
    pub freed_extents: u64,
    pub freed_bytes: u64,
    pub quarantined_extents: u64,
    pub quarantined_bytes: u64,
}

#[derive(Debug, Clone)]
struct ExtentRow {
    logical_offset: i64,
    length: i64,
    physical_offset: i64,
    generation: i64,
    state: String,
}

/// The stated per-volume extent-row bound (§8). 65,536 rows = a 256 GiB
/// volume fully shattered at 4 Mi granularity, or 1 TiB at a 16 Mi
/// average extent — far beyond anything the merge policy lets a sane
/// workload accumulate, so hitting it is a fragmentation pathology the
/// operator should see (the refusal maps to LAYOUTUNAVAILABLE and logs
/// loudly), not a working limit. Override:
/// FLINT_PNFS_EXTENT_ROW_BUDGET (house style: one env var, read once).
const DEFAULT_EXTENT_ROW_BUDGET: i64 = 65_536;

fn extent_row_budget() -> i64 {
    static BUDGET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("FLINT_PNFS_EXTENT_ROW_BUDGET")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_EXTENT_ROW_BUDGET)
    })
}

fn checked_end(offset: u64, length: u64) -> Result<i64> {
    if length == 0 {
        return Err(ExtentAllocError::InvalidRange("zero length"));
    }
    let end = offset
        .checked_add(length)
        .ok_or(ExtentAllocError::InvalidRange("offset + length overflows"))?;
    i64::try_from(end).map_err(|_| ExtentAllocError::InvalidRange("range exceeds i64"))
}

fn as_i64(v: u64, what: &'static str) -> Result<i64> {
    i64::try_from(v).map_err(|_| ExtentAllocError::InvalidRange(what))
}

/// Register a volume's allocation arena. Idempotent; the ceiling of an
/// existing row is left untouched (growth is an expansion-path concern
/// with its own guards, not a re-register side effect).
pub fn register_volume(conn: &mut Connection, volume: &str, size_ceiling: u64) -> Result<()> {
    let ceiling = as_i64(size_ceiling, "ceiling exceeds i64")?;
    conn.execute(
        "INSERT OR IGNORE INTO volume_alloc (volume, size_ceiling, next_free)
         VALUES (?1, ?2, 0)",
        params![volume, ceiling],
    )?;
    Ok(())
}

fn overlapping_extents(
    conn: &Connection,
    volume: &str,
    file_id: u64,
    start: i64,
    end: i64,
) -> rusqlite::Result<Vec<ExtentRow>> {
    let mut stmt = conn.prepare(
        "SELECT logical_offset, length, physical_offset, gen, state FROM extents
         WHERE volume = ?1 AND file_id = ?2
           AND logical_offset < ?4 AND logical_offset + length > ?3
         ORDER BY logical_offset",
    )?;
    let rows = stmt
        .query_map(params![volume, file_id as i64, start, end], |r| {
            Ok(ExtentRow {
                logical_offset: r.get(0)?,
                length: r.get(1)?,
                physical_offset: r.get(2)?,
                generation: r.get(3)?,
                state: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Live unfenced holders (other than `except`, if given) over a set of
/// extent rows. Distinct, ordered, for error reporting.
fn live_unfenced_holders(
    conn: &Connection,
    volume: &str,
    file_id: u64,
    extents: &[ExtentRow],
    except: Option<u64>,
) -> rusqlite::Result<Vec<u64>> {
    let mut holders = std::collections::BTreeSet::new();
    let mut stmt = conn.prepare(
        "SELECT client_id FROM extent_grants
         WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3 AND fenced = 0",
    )?;
    for e in extents {
        let ids = stmt
            .query_map(params![volume, file_id as i64, e.logical_offset], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            let id = id as u64;
            if Some(id) != except {
                holders.insert(id);
            }
        }
    }
    Ok(holders.into_iter().collect())
}

/// LAYOUTGET's allocation+grant transaction. Grants attach at extent
/// granularity: every existing extent intersecting the range is granted
/// whole (layouts may exceed the request, per RFC 8154), and gaps are
/// allocated as fresh INVALID_DATA extents — free-list first (reuse mints
/// `last_gen + 1`, the stale-holder detector), bump watermark second.
///
/// This is LAYOUTGET **step 2** only. Step 1 — the in-memory gate reads
/// (truncate gate, and the post-publish recheck the C6 lesson requires) —
/// belongs to the wire layer, which owns those gates. Nothing here may be
/// weakened on the argument that step 1 already checked: the model's
/// two-step honesty is exactly about the daylight between the steps.
/// `fresh_only` refuses free-list reuse: gaps allocate only from the
/// bump watermark (virgin space, reads zeros). The wire layer passes
/// TRUE until the MDS has an NVMe initiator that can write_zeroes a
/// reused range before the layout leaves the server — shipping a
/// needs_scrub extent unscrubbed is FlintExtentsBlindProvision.cfg's
/// deleted-data resurrection, and refusing reuse merely leaks freed
/// space the way the quarantine already deliberately does.
pub fn grant(
    conn: &mut Connection,
    volume: &str,
    file_id: u64,
    client_id: u64,
    logical_offset: u64,
    length: u64,
    fresh_only: bool,
) -> Result<Vec<GrantedExtent>> {
    let start = as_i64(logical_offset, "offset exceeds i64")?;
    let end = checked_end(logical_offset, length)?;
    let client = as_i64(client_id, "client id exceeds i64")?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    // No re-admission before lease recovery: a fenced client gets nothing.
    let fenced_rows: i64 = tx.query_row(
        "SELECT COUNT(*) FROM extent_grants WHERE volume = ?1 AND client_id = ?2 AND fenced = 1",
        params![volume, client],
        |r| r.get(0),
    )?;
    if fenced_rows > 0 {
        return Err(ExtentAllocError::FencedClient);
    }

    let existing = overlapping_extents(&tx, volume, file_id, start, end)?;

    // GrantsExclusive, in the transaction: another client's live unfenced
    // grant on any covered extent refuses the whole request.
    let holders = live_unfenced_holders(&tx, volume, file_id, &existing, Some(client_id))?;
    if !holders.is_empty() {
        return Err(ExtentAllocError::Conflict { holders });
    }

    // Gaps in [start, end) not covered by existing extents.
    let mut gaps: Vec<(i64, i64)> = Vec::new();
    let mut cursor = start;
    for e in &existing {
        if e.logical_offset > cursor {
            gaps.push((cursor, e.logical_offset - cursor));
        }
        cursor = cursor.max(e.logical_offset + e.length);
    }
    if cursor < end {
        gaps.push((cursor, end - cursor));
    }

    // The stated per-volume row bound (§8), enforced at the only place
    // rows are minted. O(1): the counter lives in volume_alloc,
    // maintained by every minting/deleting/merging transaction.
    if !gaps.is_empty() {
        let cur_rows: Option<i64> = tx
            .query_row(
                "SELECT extent_rows FROM volume_alloc WHERE volume = ?1",
                params![volume],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(rows) = cur_rows {
            let budget = extent_row_budget();
            if rows + gaps.len() as i64 > budget {
                return Err(ExtentAllocError::RowBudget {
                    rows: rows as u64,
                    budget: budget as u64,
                });
            }
        }
        // Absent arena: the allocation arm below fails with its own
        // error; no budget verdict on a volume that cannot allocate.
    }

    let mut new_rows: Vec<ExtentRow> = Vec::new();
    // Physical offsets allocated from the free list this transaction:
    // those ranges carry a previous incarnation's bytes until the wire
    // layer write_zeroes them (GrantedExtent::needs_scrub).
    let mut reused_phys: Vec<i64> = Vec::new();
    for (g_start, g_len) in gaps {
        // First fit from the free list; reuse bumps the generation.
        let hit: Option<(i64, i64, i64)> = if fresh_only {
            None
        } else {
            tx.query_row(
                "SELECT physical_offset, length, last_gen FROM extent_free
                 WHERE volume = ?1 AND length >= ?2
                 ORDER BY physical_offset LIMIT 1",
                params![volume, g_len],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
        };
        let (phys, generation) = match hit {
            Some((f_phys, f_len, f_gen)) => {
                tx.execute(
                    "DELETE FROM extent_free WHERE volume = ?1 AND physical_offset = ?2",
                    params![volume, f_phys],
                )?;
                if f_len > g_len {
                    // The remainder keeps the old generation: it has not
                    // been re-owned yet. (Coalescing is a no-op here —
                    // free neighbours never coexist un-merged — but the
                    // shared insert path keeps that property true by
                    // construction rather than by argument.)
                    free_insert_coalescing(&tx, volume, f_phys + g_len, f_len - g_len, f_gen)?;
                }
                reused_phys.push(f_phys);
                (f_phys, f_gen + 1)
            }
            None => {
                let (ceiling, next_free): (i64, i64) = tx.query_row(
                    "SELECT size_ceiling, next_free FROM volume_alloc WHERE volume = ?1",
                    params![volume],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if next_free + g_len > ceiling {
                    return Err(ExtentAllocError::NoSpace {
                        needed: g_len as u64,
                        ceiling: ceiling as u64,
                        next_free: next_free as u64,
                    });
                }
                tx.execute(
                    "UPDATE volume_alloc SET next_free = next_free + ?2 WHERE volume = ?1",
                    params![volume, g_len],
                )?;
                (next_free, 1)
            }
        };
        tx.execute(
            "INSERT INTO extents
               (volume, file_id, logical_offset, length, physical_offset, gen, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'invalid')",
            params![volume, file_id as i64, g_start, g_len, phys, generation],
        )?;
        new_rows.push(ExtentRow {
            logical_offset: g_start,
            length: g_len,
            physical_offset: phys,
            generation,
            state: "invalid".into(),
        });
    }

    if !new_rows.is_empty() {
        tx.execute(
            "UPDATE volume_alloc SET extent_rows = extent_rows + ?2 WHERE volume = ?1",
            params![volume, new_rows.len() as i64],
        )?;
    }

    // One grant row per covered extent for this client. INSERT OR IGNORE
    // makes re-grant idempotent — and an ignored row is necessarily at the
    // extent's own gen already, because gen cannot move under a live grant
    // (verify_volume_invariants pins that as an every-transaction check).
    let mut all: Vec<ExtentRow> = existing;
    all.extend(new_rows);
    all.sort_by_key(|e| e.logical_offset);
    for e in &all {
        tx.execute(
            "INSERT OR IGNORE INTO extent_grants
               (volume, file_id, logical_offset, client_id, mode, gen, fenced)
             VALUES (?1, ?2, ?3, ?4, 'rw', ?5, 0)",
            params![volume, file_id as i64, e.logical_offset, client, e.generation],
        )?;
    }

    // Windowed: every row this grant touched (new placements AND
    // existing rows that took grant rows), against their neighbours.
    let touched: Vec<TouchedExtent> = all
        .iter()
        .map(|e| TouchedExtent {
            file_id: file_id as i64,
            logical_offset: e.logical_offset,
            length: e.length,
            physical_offset: e.physical_offset,
        })
        .collect();
    verify_window_invariants_conn(&tx, volume, &touched)?;
    let granted = all
        .iter()
        .map(|e| GrantedExtent {
            logical_offset: e.logical_offset as u64,
            length: e.length as u64,
            physical_offset: e.physical_offset as u64,
            generation: e.generation as u64,
            committed: e.state == "rw",
            needs_scrub: reused_phys.contains(&e.physical_offset),
        })
        .collect();
    tx.commit()?;
    Ok(granted)
}

/// READ-layout query: the committed extents overlapping the range, with a
/// grant row per extent for this client — and NO allocation, ever. A
/// reader must not mint space (the kernel's 1 GiB-window LAYOUTGET on a
/// 64 MiB file would otherwise allocate ~1 GiB of arena for zeros), and
/// it must not see uncommitted extents (INVALID rows carry either nothing
/// or a prior owner's bytes; the wire layer presents the gaps as
/// NONE_DATA, which reads as zeros client-side — kernel-verified:
/// `verify_extent` refuses RW_DATA/INVALID_DATA in a read layout
/// outright). The grant rows are what make readers VISIBLE to
/// FreeRevalidates — a truncate cannot free an extent out from under a
/// layout-holding reader.
pub fn grant_read(
    conn: &mut Connection,
    volume: &str,
    file_id: u64,
    client_id: u64,
    logical_offset: u64,
    length: u64,
) -> Result<Vec<GrantedExtent>> {
    let start = as_i64(logical_offset, "offset exceeds i64")?;
    let end = checked_end(logical_offset, length)?;
    let client = as_i64(client_id, "client id exceeds i64")?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let fenced_rows: i64 = tx.query_row(
        "SELECT COUNT(*) FROM extent_grants WHERE volume = ?1 AND client_id = ?2 AND fenced = 1",
        params![volume, client],
        |r| r.get(0),
    )?;
    if fenced_rows > 0 {
        return Err(ExtentAllocError::FencedClient);
    }

    let committed: Vec<ExtentRow> = overlapping_extents(&tx, volume, file_id, start, end)?
        .into_iter()
        .filter(|e| e.state == "rw")
        .collect();
    for e in &committed {
        tx.execute(
            "INSERT OR IGNORE INTO extent_grants
               (volume, file_id, logical_offset, client_id, mode, gen, fenced)
             VALUES (?1, ?2, ?3, ?4, 'read', ?5, 0)",
            params![volume, file_id as i64, e.logical_offset, client, e.generation],
        )?;
    }
    // Windowed: a read grant reshapes nothing — only the touched
    // extents' grant-integrity can have changed.
    let touched: Vec<TouchedExtent> = committed
        .iter()
        .map(|e| TouchedExtent {
            file_id: file_id as i64,
            logical_offset: e.logical_offset,
            length: e.length,
            physical_offset: e.physical_offset,
        })
        .collect();
    verify_window_invariants_conn(&tx, volume, &touched)?;
    let granted = committed
        .iter()
        .map(|e| GrantedExtent {
            logical_offset: e.logical_offset as u64,
            length: e.length as u64,
            physical_offset: e.physical_offset as u64,
            generation: e.generation as u64,
            committed: true,
            needs_scrub: false,
        })
        .collect();
    tx.commit()?;
    Ok(granted)
}

/// LAYOUTRETURN: drop this client's grant rows on extents overlapping the
/// range. Returns the number of grant rows removed. A fenced client's
/// return is accepted — the return is the client's promise that no more
/// I/O will be issued under the layout, which upgrades a pending
/// quarantine to a clean free (see module docs).
///
/// The return is also THE merge trigger: it is the moment rows become
/// quiescent (return_on_close means most files pass through here right
/// after their writer closes), so the windowed merge runs on the
/// returned range in the same transaction — a sequentially-written
/// file's N contiguous extents collapse to one row the moment its
/// layout comes back.
pub fn layout_return(
    conn: &mut Connection,
    volume: &str,
    file_id: u64,
    client_id: u64,
    logical_offset: u64,
    length: u64,
) -> Result<usize> {
    let start = as_i64(logical_offset, "offset exceeds i64")?;
    let end = checked_end(logical_offset, length)?;
    let client = as_i64(client_id, "client id exceeds i64")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let n = tx.execute(
        "DELETE FROM extent_grants
         WHERE volume = ?1 AND file_id = ?2 AND client_id = ?3
           AND logical_offset IN
             (SELECT logical_offset FROM extents
              WHERE volume = ?1 AND file_id = ?2
                AND logical_offset < ?5 AND logical_offset + length > ?4)",
        params![volume, file_id as i64, client, start, end],
    )?;
    merge_extents_window(&tx, volume, file_id as i64, start, end)?;
    tx.commit()?;
    Ok(n)
}

/// Server-side revocation bookkeeping for an unresponsive holder: every
/// live grant row of this client on this volume is marked fenced. Returns
/// the number of rows marked. THIS DOES NOT FENCE THE DATA PATH — the
/// NVMe preempt is the wire layer's job, its delivery is unproven
/// (FenceReaches), and that is exactly why the extents under these rows
/// will quarantine rather than free.
pub fn fence_client(conn: &mut Connection, volume: &str, client_id: u64) -> Result<usize> {
    let client = as_i64(client_id, "client id exceeds i64")?;
    let n = conn.execute(
        "UPDATE extent_grants SET fenced = 1
         WHERE volume = ?1 AND client_id = ?2 AND fenced = 0",
        params![volume, client],
    )?;
    Ok(n)
}

/// The recall snapshot: distinct live unfenced holders over the range,
/// for the caller to CB_LAYOUTRECALL. This is bookkeeping the free does
/// NOT trust — `reclaim_complete` re-validates regardless (the
/// FreeRevalidates belt), so a stale snapshot costs a retry, never
/// a corruption.
pub fn reclaim_snapshot(
    conn: &mut Connection,
    volume: &str,
    file_id: u64,
    logical_offset: u64,
    length: u64,
) -> Result<Vec<u64>> {
    let start = as_i64(logical_offset, "offset exceeds i64")?;
    let end = checked_end(logical_offset, length)?;
    let extents = overlapping_extents(conn, volume, file_id, start, end)?;
    Ok(live_unfenced_holders(conn, volume, file_id, &extents, None)?)
}

/// The free transaction. Every target extent must be quiescent: any live
/// unfenced grant refuses the whole free (`NotQuiescent` — the
/// machine-checked FreeRevalidates belt; the caller's snapshot is not
/// consulted, deliberately). Quiescent extents partition three ways:
/// no grant rows at all → clean free into the free list; fenced rows
/// whose fences were ALL CONFIRMED at the target (`fenced_clients.
/// delivered_unix` — set only on a verified preempt) → clean free too,
/// the 2026-08-10 graduation (FreeRequiresDelivered, model-gated: the
/// rig proved a confirmed exclusion is real, so the fenced holder can
/// never touch these bytes again); any UNCONFIRMED fence → QUARANTINE,
/// exactly as before the flip (freeing on an unconfirmed fence is
/// FlintExtentsLostFence.cfg's machine-checked corruption — the
/// never-excluded client's raw write lands in the new owner's bytes).
/// A stale delivered bit after ptpl loss is belted by the durable
/// eviction: the fenced client is off the allow-list and the admission
/// guard refuses its return, so freeing stays safe even in the
/// restart window before the startup re-fence lands.
pub fn reclaim_complete(
    conn: &mut Connection,
    volume: &str,
    file_id: u64,
    logical_offset: u64,
    length: u64,
    now_unix: i64,
) -> Result<FreeOutcome> {
    let start = as_i64(logical_offset, "offset exceeds i64")?;
    let end = checked_end(logical_offset, length)?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let targets = overlapping_extents(&tx, volume, file_id, start, end)?;
    if targets.is_empty() {
        return Ok(FreeOutcome::default());
    }

    let holders = live_unfenced_holders(&tx, volume, file_id, &targets, None)?;
    if !holders.is_empty() {
        return Err(ExtentAllocError::NotQuiescent { holders });
    }

    let mut out = FreeOutcome::default();
    // Each fenced holder joined with its fence record's delivered bit
    // (COALESCE 0: no record — e.g. post-unfence lingering rows — is
    // UNDELIVERED, the conservative side).
    let mut fenced_stmt = tx.prepare(
        "SELECT g.client_id,
                COALESCE((SELECT f.delivered_unix FROM fenced_clients f
                          WHERE f.volume = g.volume AND f.client_id = g.client_id), 0)
         FROM extent_grants g
         WHERE g.volume = ?1 AND g.file_id = ?2 AND g.logical_offset = ?3
           AND g.fenced = 1
         ORDER BY g.client_id",
    )?;
    for t in &targets {
        let fenced: Vec<(i64, i64)> = fenced_stmt
            .query_map(params![volume, file_id as i64, t.logical_offset], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let all_delivered = fenced.iter().all(|(_, d)| *d > 0);
        if fenced.is_empty() || all_delivered {
            free_insert_coalescing(&tx, volume, t.physical_offset, t.length, t.generation)?;
            out.freed_extents += 1;
            out.freed_bytes += t.length as u64;
            if !fenced.is_empty() {
                // Delivered-fence clean free: the grant rows go with the
                // extent (the quarantine branch's discipline; leaving
                // them would refuse the client future grants forever on
                // rows whose extents no longer exist).
                tx.execute(
                    "DELETE FROM extent_grants
                     WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
                    params![volume, file_id as i64, t.logical_offset],
                )?;
            }
        } else {
            let csv = fenced
                .iter()
                .map(|(c, _)| c.to_string())
                .collect::<Vec<_>>()
                .join(",");
            tx.execute(
                "INSERT INTO extent_quarantine
                   (volume, physical_offset, length, gen, fenced_clients, quarantined_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![volume, t.physical_offset, t.length, t.generation, csv, now_unix],
            )?;
            out.quarantined_extents += 1;
            out.quarantined_bytes += t.length as u64;
            tx.execute(
                "DELETE FROM extent_grants
                 WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
                params![volume, file_id as i64, t.logical_offset],
            )?;
        }
        tx.execute(
            "DELETE FROM extents
             WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
            params![volume, file_id as i64, t.logical_offset],
        )?;
    }
    drop(fenced_stmt);

    tx.execute(
        "UPDATE volume_alloc SET extent_rows = extent_rows - ?2 WHERE volume = ?1",
        params![volume, targets.len() as i64],
    )?;
    // Windowed: this transaction only DELETED extents and INSERTED
    // free/quarantine ranges over exactly the deleted placements — no
    // extents row was added or reshaped, so there is no touched row to
    // probe (deletions cannot create overlap, and the freed placements
    // re-occupy space the deleted rows held). Tests and debug builds
    // still cross-check the whole volume (bench opt-out as in
    // verify_window_invariants_conn).
    #[cfg(any(test, debug_assertions))]
    if !BENCH_SKIP_FULL_VERIFY.load(std::sync::atomic::Ordering::Relaxed) {
        verify_volume_invariants_conn(&tx, volume)?;
    }
    tx.commit()?;
    Ok(out)
}

/// LAYOUTCOMMIT's allocator half: promote INVALID_DATA → READ_WRITE_DATA
/// over the range, refusing unless every target extent carries a live
/// UNFENCED grant row for THIS client at the extent's CURRENT generation.
/// Not optional politeness (§8): reservations fence only the NVMe data
/// path — the NFS control path stays open — so this check is what fences
/// a fenced or stale client's LAYOUTCOMMIT, which would otherwise promote
/// extents that were freed and reused (the new owner's data). Size/stub
/// coupling arrives with the commit tranche.
pub fn commit_extents(
    conn: &mut Connection,
    volume: &str,
    file_id: u64,
    client_id: u64,
    logical_offset: u64,
    length: u64,
) -> Result<u64> {
    let start = as_i64(logical_offset, "offset exceeds i64")?;
    let end = checked_end(logical_offset, length)?;
    let client = as_i64(client_id, "client id exceeds i64")?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let targets = overlapping_extents(&tx, volume, file_id, start, end)?;
    if targets.is_empty() {
        return Err(ExtentAllocError::CommitRejected("no extents under the range"));
    }
    for t in &targets {
        let row: Option<(i64, i64)> = tx
            .query_row(
                "SELECT gen, fenced FROM extent_grants
                 WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3 AND client_id = ?4",
                params![volume, file_id as i64, t.logical_offset, client],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => return Err(ExtentAllocError::CommitRejected("no grant for this client")),
            Some((_, fenced)) if fenced != 0 => {
                return Err(ExtentAllocError::CommitRejected("grant is fenced"))
            }
            Some((g, _)) if g != t.generation => {
                return Err(ExtentAllocError::CommitRejected("generation mismatch"))
            }
            Some(_) => {}
        }
    }
    let mut promoted = 0u64;
    for t in &targets {
        promoted += tx.execute(
            "UPDATE extents SET state = 'rw'
             WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3 AND state = 'invalid'",
            params![volume, file_id as i64, t.logical_offset],
        )? as u64;
    }
    tx.commit()?;
    Ok(promoted)
}

/// The operator lever: move every quarantined range of the volume into
/// the free list, generations remembered so reuse still bumps. Explicit
/// and whole-volume by design — nothing calls this automatically, and it
/// must stay that way until FenceReaches is proven on hardware.
pub fn release_quarantine(conn: &mut Connection, volume: &str) -> Result<u64> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut bytes = 0u64;
    {
        let mut stmt = tx.prepare(
            "SELECT physical_offset, length, gen FROM extent_quarantine WHERE volume = ?1",
        )?;
        let rows: Vec<(i64, i64, i64)> = stmt
            .query_map(params![volume], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (phys, len, generation) in rows {
            free_insert_coalescing(&tx, volume, phys, len, generation)?;
            bytes += len as u64;
        }
    }
    tx.execute(
        "DELETE FROM extent_quarantine WHERE volume = ?1",
        params![volume],
    )?;
    verify_volume_invariants_conn(&tx, volume)?;
    tx.commit()?;
    Ok(bytes)
}

/// DeleteVolume's sweep: drop every extent-allocator row for the
/// volume — extents, grants, free list, quarantine, and the arena
/// itself. Without this, a re-created volume of the same name would
/// inherit the old arena and extent rows: stale grants blocking every
/// reclaim, and a watermark claiming space the new lvol never
/// allocated. Returns rows dropped, for the delete path's log.
pub fn drop_volume(conn: &mut Connection, volume: &str) -> Result<u64> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut n = 0u64;
    for table in [
        "extent_grants",
        "extents",
        "extent_free",
        "extent_quarantine",
        "volume_alloc",
        "block_hosts",
        "fenced_clients",
    ] {
        n += tx.execute(&format!("DELETE FROM {table} WHERE volume = ?1"), params![volume])?
            as u64;
    }
    tx.commit()?;
    Ok(n)
}

/// Record that `client_id` (whose NVMe identity is `host_nqn`) belongs on
/// `volume`'s export allow-list, and return the full DISTINCT desired list
/// after the upsert — the level the reconciler converges spdk-tgt onto.
/// Idempotent; a client re-appearing with a DIFFERENT host_nqn (node
/// rename between mounts) simply replaces its row.
pub fn host_admit(
    conn: &mut Connection,
    volume: &str,
    client_id: u64,
    host_nqn: &str,
    now_unix: i64,
) -> Result<Vec<String>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO block_hosts (volume, client_id, host_nqn, admitted_unix)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (volume, client_id) DO UPDATE SET host_nqn = ?3",
        params![volume, client_id as i64, host_nqn, now_unix],
    )?;
    let hosts = hosts_for_volume_conn(&tx, volume)?;
    tx.commit()?;
    Ok(hosts)
}

/// Drop `client_id`'s admission rows for `volume` and return
/// `(evicted_nqns, remaining_desired_list)`. The eviction is the durable
/// half of the functional fence (allow-list yank + qpair drain); the
/// caller converges the subsystem onto the remaining list. An NQN shared
/// with another live client stays in `remaining` — host-level fencing
/// cannot split two NFS clients on one node, same as NVMe reservations
/// (per Host Identifier).
pub fn host_evict(
    conn: &mut Connection,
    volume: &str,
    client_id: u64,
) -> Result<(Vec<String>, Vec<String>)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut stmt = tx.prepare(
        "SELECT host_nqn FROM block_hosts WHERE volume = ?1 AND client_id = ?2",
    )?;
    let evicted: Vec<String> = stmt
        .query_map(params![volume, client_id as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    tx.execute(
        "DELETE FROM block_hosts WHERE volume = ?1 AND client_id = ?2",
        params![volume, client_id as i64],
    )?;
    let remaining = hosts_for_volume_conn(&tx, volume)?;
    tx.commit()?;
    Ok((evicted, remaining))
}

/// The volume's full desired allow-list (distinct, ordered for stable
/// comparison in tests and logs).
pub fn hosts_for_volume(conn: &Connection, volume: &str) -> Result<Vec<String>> {
    hosts_for_volume_conn(conn, volume)
}

/// Write the durable fence record for `client_id` on `volume` (the
/// positive record — see the `fenced_clients` schema comment). Captures
/// the client's `host_nqn` from `block_hosts` IN THE SAME TRANSACTION,
/// so it must run BEFORE `host_evict` deletes that row; returns the
/// captured nqn (empty if the client held no admission) for the caller's
/// log. Idempotent: re-fencing refreshes the timestamp.
pub fn fence_record(
    conn: &mut Connection,
    volume: &str,
    client_id: u64,
    now_unix: i64,
) -> Result<String> {
    let client = as_i64(client_id, "client id exceeds i64")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let host_nqn: String = tx
        .query_row(
            "SELECT host_nqn FROM block_hosts WHERE volume = ?1 AND client_id = ?2",
            params![volume, client],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();
    tx.execute(
        "INSERT INTO fenced_clients (volume, client_id, host_nqn, fenced_unix)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (volume, client_id) DO UPDATE SET fenced_unix = ?4",
        params![volume, client, host_nqn, now_unix],
    )?;
    tx.commit()?;
    Ok(host_nqn)
}

/// Is `client_id` fenced on `volume`? The admission guard: a fenced
/// client's fresh LAYOUTGET must not re-admit it to the allow-list.
pub fn is_fenced(conn: &Connection, volume: &str, client_id: u64) -> Result<bool> {
    let client = as_i64(client_id, "client id exceeds i64")?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fenced_clients WHERE volume = ?1 AND client_id = ?2",
        params![volume, client],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Every `(volume, client_id)` fence record, for startup re-establishment
/// (re-acquire EA-RO on each fenced volume — the PTPL-loss recovery
/// path). Ordered for deterministic replay.
pub fn fenced_all(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT volume, client_id FROM fenced_clients ORDER BY volume, client_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let v: String = r.get(0)?;
            let c: i64 = r.get(1)?;
            Ok((v, c as u64))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Mark a client's fence DELIVERED: the reservation preempt was
/// confirmed at the target (post-report verified — MDS key holds EA-RO,
/// victim absent). This is what licenses the reclaim to FREE the
/// client's fenced extents instead of quarantining them (the
/// FreeRequiresDelivered belt, machine-checked: freeing on an
/// unconfirmed fence is FlintExtentsLostFence.cfg's corruption).
/// Returns whether a fence record existed to mark.
pub fn mark_fence_delivered(
    conn: &mut Connection,
    volume: &str,
    client_id: u64,
    now_unix: i64,
) -> Result<bool> {
    let client = as_i64(client_id, "client id exceeds i64")?;
    let n = conn.execute(
        "UPDATE fenced_clients SET delivered_unix = ?3
         WHERE volume = ?1 AND client_id = ?2 AND delivered_unix = 0",
        params![volume, client, now_unix],
    )?;
    Ok(n > 0)
}

/// Clear a client's fence (the release / lease-recovery path). Returns
/// whether a row was removed.
pub fn unfence_record(conn: &mut Connection, volume: &str, client_id: u64) -> Result<bool> {
    let client = as_i64(client_id, "client id exceeds i64")?;
    let n = conn.execute(
        "DELETE FROM fenced_clients WHERE volume = ?1 AND client_id = ?2",
        params![volume, client],
    )?;
    Ok(n > 0)
}

fn hosts_for_volume_conn(conn: &Connection, volume: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT host_nqn FROM block_hosts WHERE volume = ?1 ORDER BY host_nqn",
    )?;
    let hosts = stmt
        .query_map(params![volume], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(hosts)
}

/// The quarantine meter: (ranges, bytes) currently leaked for the volume.
pub fn quarantine_stats(conn: &Connection, volume: &str) -> Result<(u64, u64)> {
    let (n, bytes): (i64, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), SUM(length) FROM extent_quarantine WHERE volume = ?1",
        params![volume],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((n as u64, bytes.unwrap_or(0) as u64))
}

/// The app-level assertion §8 mandates because the PKs cannot: run inside
/// every writing transaction, before commit. On violation the transaction
/// rolls back and nothing is written — a corrupted table stops the
/// allocator loudly instead of aliasing someone's bytes.
pub fn verify_volume_invariants(conn: &Connection, volume: &str) -> Result<()> {
    verify_volume_invariants_conn(conn, volume)
}

/// One row's-worth of state a transaction touched, for the windowed
/// verifier. `logical` is (file_id, offset, length) of an extents row
/// added or reshaped; `physical` is a physical placement added or
/// reshaped in `phys_home`.
struct TouchedExtent {
    file_id: i64,
    logical_offset: i64,
    length: i64,
    physical_offset: i64,
}

/// THE WINDOWED VERIFIER — the merge-policy tranche's answer to the
/// cost gate's measured debt (`verify_volume_invariants` cost ~0.9 µs
/// per VOLUME row per writing transaction; a 262k-row volume would pay
/// ~230 ms per grant). Completeness argument, stated once: a
/// transaction can only violate an invariant on state it TOUCHED — if
/// the pre-state satisfied the invariants (every prior transaction
/// verified its own window, anchored at the empty arena), then checking
/// each touched row against its immediate neighbours is a COMPLETE
/// check of the post-state. The full verifier remains: in every test
/// and debug build it runs right after this one (the whole suite is a
/// windowed-vs-full differential), and whole-volume operations
/// (release_quarantine, the corruption tests) still call it outright.
///
/// Probes are all index-served: logical neighbours via the extents PK,
/// physical neighbours via idx_extents_phys / the phys-keyed PKs of
/// extent_free and extent_quarantine.
fn verify_window_invariants_conn(
    conn: &Connection,
    volume: &str,
    touched: &[TouchedExtent],
) -> Result<()> {
    if touched.is_empty() {
        return Ok(());
    }
    let (next_free,): (i64,) = conn.query_row(
        "SELECT next_free FROM volume_alloc WHERE volume = ?1",
        params![volume],
        |r| Ok((r.get(0)?,)),
    )?;
    for t in touched {
        // 1. Logical disjointness against the two neighbours (self is
        //    keyed at exactly logical_offset, so strict < / > excludes
        //    it).
        let prev: Option<(i64, i64)> = conn
            .query_row(
                "SELECT logical_offset, length FROM extents
                 WHERE volume = ?1 AND file_id = ?2 AND logical_offset < ?3
                 ORDER BY logical_offset DESC LIMIT 1",
                params![volume, t.file_id, t.logical_offset],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((po, pl)) = prev {
            if po + pl > t.logical_offset {
                return Err(ExtentAllocError::Corruption(format!(
                    "logical overlap in {volume} file {}: [{po},{}) vs [{},…)",
                    t.file_id,
                    po + pl,
                    t.logical_offset
                )));
            }
        }
        let next: Option<i64> = conn
            .query_row(
                "SELECT logical_offset FROM extents
                 WHERE volume = ?1 AND file_id = ?2 AND logical_offset > ?3
                 ORDER BY logical_offset ASC LIMIT 1",
                params![volume, t.file_id, t.logical_offset],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(no) = next {
            if t.logical_offset + t.length > no {
                return Err(ExtentAllocError::Corruption(format!(
                    "logical overlap in {volume} file {}: [{},{}) vs [{no},…)",
                    t.file_id,
                    t.logical_offset,
                    t.logical_offset + t.length
                )));
            }
        }

        // 2. Physical disjointness of the touched placement against all
        //    three homes, excluding self by extents identity.
        let p_end = t.physical_offset + t.length;
        let ext_overlaps: i64 = conn.query_row(
            "SELECT COUNT(*) FROM extents
             WHERE volume = ?1 AND physical_offset < ?2
               AND physical_offset + length > ?3
               AND NOT (file_id = ?4 AND logical_offset = ?5)",
            params![volume, p_end, t.physical_offset, t.file_id, t.logical_offset],
            |r| r.get(0),
        )?;
        if ext_overlaps > 0 {
            return Err(ExtentAllocError::Corruption(format!(
                "physical overlap in {volume}: extent [{},{p_end}) vs another extent",
                t.physical_offset
            )));
        }
        for home in ["extent_free", "extent_quarantine"] {
            let n: i64 = conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {home}
                     WHERE volume = ?1 AND physical_offset < ?2
                       AND physical_offset + length > ?3"
                ),
                params![volume, p_end, t.physical_offset],
                |r| r.get(0),
            )?;
            if n > 0 {
                return Err(ExtentAllocError::Corruption(format!(
                    "physical overlap in {volume}: extent [{},{p_end}) vs {home}",
                    t.physical_offset
                )));
            }
        }

        // 3. Watermark containment for the touched placement.
        if p_end > next_free {
            return Err(ExtentAllocError::Corruption(format!(
                "extent range [{},{p_end}) beyond watermark {next_free} in {volume}",
                t.physical_offset
            )));
        }

        // 4. Grant integrity on the touched extent: unfenced rows match
        //    its generation (the transactional form of the model's
        //    Inv_RecallCompletesBeforeReuse).
        let stale: i64 = conn.query_row(
            "SELECT COUNT(*) FROM extent_grants g JOIN extents e
                ON e.volume = g.volume AND e.file_id = g.file_id
               AND e.logical_offset = g.logical_offset
             WHERE g.volume = ?1 AND g.file_id = ?2 AND g.logical_offset = ?3
               AND g.fenced = 0 AND g.gen <> e.gen",
            params![volume, t.file_id, t.logical_offset],
            |r| r.get(0),
        )?;
        if stale > 0 {
            return Err(ExtentAllocError::Corruption(format!(
                "unfenced grant at stale gen on {volume} file {} offset {}",
                t.file_id, t.logical_offset
            )));
        }
    }

    // The differential belt: every test and debug build re-checks the
    // WHOLE volume, so any incompleteness in the windowing shows up as
    // a full-verifier failure in the existing corpus. The bench opts
    // out (BENCH_SKIP_FULL_VERIFY) — it exists to measure the
    // PRODUCTION path, and the belt here is the exact O(rows) slope
    // the windowing removed; leaving it on would bench the belt.
    #[cfg(any(test, debug_assertions))]
    if !BENCH_SKIP_FULL_VERIFY.load(std::sync::atomic::Ordering::Relaxed) {
        verify_volume_invariants_conn(conn, volume)?;
    }

    Ok(())
}

/// See the differential-belt note in `verify_window_invariants_conn`.
/// Only `extent_bench` flips it, only in its own single-test process.
#[cfg(any(test, debug_assertions))]
pub(crate) static BENCH_SKIP_FULL_VERIFY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Insert into the free list, coalescing with physically-adjacent free
/// rows. `last_gen` of a coalesced row is the MAX of its constituents —
/// reuse mints max+1, which dominates every constituent's history, so
/// the stale-holder detector stays monotone across merges (the
/// cross-incarnation half of the gen argument; the block model's
/// MergeMin run machine-checks the intra-incarnation half).
fn free_insert_coalescing(
    conn: &Connection,
    volume: &str,
    physical_offset: i64,
    length: i64,
    last_gen: i64,
) -> rusqlite::Result<()> {
    let mut phys = physical_offset;
    let mut len = length;
    let mut generation = last_gen;
    // Absorb the row ending exactly at our start.
    let prev: Option<(i64, i64, i64)> = conn
        .query_row(
            "SELECT physical_offset, length, last_gen FROM extent_free
             WHERE volume = ?1 AND physical_offset < ?2
             ORDER BY physical_offset DESC LIMIT 1",
            params![volume, phys],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((pp, pl, pg)) = prev {
        if pp + pl == phys {
            conn.execute(
                "DELETE FROM extent_free WHERE volume = ?1 AND physical_offset = ?2",
                params![volume, pp],
            )?;
            phys = pp;
            len += pl;
            generation = generation.max(pg);
        }
    }
    // Absorb the row starting exactly at our end.
    let next: Option<(i64, i64)> = conn
        .query_row(
            "SELECT length, last_gen FROM extent_free
             WHERE volume = ?1 AND physical_offset = ?2",
            params![volume, phys + len],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((nl, ng)) = next {
        conn.execute(
            "DELETE FROM extent_free WHERE volume = ?1 AND physical_offset = ?2",
            params![volume, phys + len],
        )?;
        len += nl;
        generation = generation.max(ng);
    }
    conn.execute(
        "INSERT INTO extent_free (volume, physical_offset, length, last_gen)
         VALUES (?1, ?2, ?3, ?4)",
        params![volume, phys, len, generation],
    )?;
    Ok(())
}

/// THE MERGE POLICY (§8, model-gated — FlintExtents' merge tranche):
/// coalesce adjacent extents of one file when they are logically AND
/// physically contiguous, in the same state, and QUIESCENT — zero grant
/// rows, fenced included, on every row swallowed (MergeHeld.cfg is the
/// machine-checked reason: coarsening under a live grant moves gen
/// under it). The merged row carries MAX(gen) — MergeMin.cfg proved the
/// choice safety-irrelevant under quiescence, so MAX is monotonicity
/// hygiene for the free-list's cross-incarnation history. Windowed:
/// only rows overlapping [start, end) plus one neighbour each side are
/// considered, so the pass costs O(rows-in-window), never O(volume).
/// Returns rows merged away (the caller decrements the row counter).
fn merge_extents_window(
    conn: &Connection,
    volume: &str,
    file_id: i64,
    start: i64,
    end: i64,
) -> Result<u64> {
    // The window plus one row either side (a boundary row may merge
    // outward).
    let mut rows: Vec<ExtentRow> = Vec::new();
    if let Some(prev) = conn
        .query_row(
            "SELECT logical_offset, length, physical_offset, gen, state FROM extents
             WHERE volume = ?1 AND file_id = ?2 AND logical_offset < ?3
             ORDER BY logical_offset DESC LIMIT 1",
            params![volume, file_id, start],
            |r| {
                Ok(ExtentRow {
                    logical_offset: r.get(0)?,
                    length: r.get(1)?,
                    physical_offset: r.get(2)?,
                    generation: r.get(3)?,
                    state: r.get(4)?,
                })
            },
        )
        .optional()?
    {
        rows.push(prev);
    }
    rows.extend(overlapping_extents(conn, volume, file_id as u64, start, end)?);
    if let Some(next) = conn
        .query_row(
            "SELECT logical_offset, length, physical_offset, gen, state FROM extents
             WHERE volume = ?1 AND file_id = ?2 AND logical_offset >= ?3
             ORDER BY logical_offset ASC LIMIT 1",
            params![volume, file_id, end],
            |r| {
                Ok(ExtentRow {
                    logical_offset: r.get(0)?,
                    length: r.get(1)?,
                    physical_offset: r.get(2)?,
                    generation: r.get(3)?,
                    state: r.get(4)?,
                })
            },
        )
        .optional()?
    {
        rows.push(next);
    }
    if rows.len() < 2 {
        return Ok(0);
    }

    // A row is mergeable only when NOTHING references it.
    let held = |off: i64| -> rusqlite::Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM extent_grants
             WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
            params![volume, file_id, off],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    };

    let mut merged_away = 0u64;
    let mut run_start = 0usize;
    let mut i = 1usize;
    let mut touched: Vec<TouchedExtent> = Vec::new();
    while i <= rows.len() {
        let extend = i < rows.len() && {
            let a = &rows[i - 1];
            let b = &rows[i];
            a.logical_offset + a.length == b.logical_offset
                && a.physical_offset + a.length == b.physical_offset
                && a.state == b.state
                && !held(a.logical_offset)?
                && !held(b.logical_offset)?
        };
        if !extend {
            if i - run_start >= 2 {
                let head = &rows[run_start];
                let total: i64 = rows[run_start..i].iter().map(|e| e.length).sum();
                let gen_max =
                    rows[run_start..i].iter().map(|e| e.generation).max().unwrap();
                for e in &rows[run_start + 1..i] {
                    conn.execute(
                        "DELETE FROM extents
                         WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
                        params![volume, file_id, e.logical_offset],
                    )?;
                    merged_away += 1;
                }
                conn.execute(
                    "UPDATE extents SET length = ?4, gen = ?5
                     WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
                    params![volume, file_id, head.logical_offset, total, gen_max],
                )?;
                touched.push(TouchedExtent {
                    file_id,
                    logical_offset: head.logical_offset,
                    length: total,
                    physical_offset: head.physical_offset,
                });
            }
            run_start = i;
        }
        i += 1;
    }
    if merged_away > 0 {
        conn.execute(
            "UPDATE volume_alloc SET extent_rows = extent_rows - ?2 WHERE volume = ?1",
            params![volume, merged_away as i64],
        )?;
        verify_window_invariants_conn(conn, volume, &touched)?;
    }
    Ok(merged_away)
}

fn verify_volume_invariants_conn(conn: &Connection, volume: &str) -> Result<()> {
    // 1. Logical disjointness per file.
    {
        let mut stmt = conn.prepare(
            "SELECT file_id, logical_offset, length FROM extents
             WHERE volume = ?1 ORDER BY file_id, logical_offset",
        )?;
        let rows: Vec<(i64, i64, i64)> = stmt
            .query_map(params![volume], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for w in rows.windows(2) {
            let (fa, oa, la) = w[0];
            let (fb, ob, _) = w[1];
            if fa == fb && oa + la > ob {
                return Err(ExtentAllocError::Corruption(format!(
                    "logical overlap in {volume} file {fa}: [{oa},{}) vs [{ob},…)",
                    oa + la
                )));
            }
        }
    }

    // 2. Physical disjointness volume-wide, across all three homes a
    //    physical range can live in.
    let mut phys: Vec<(i64, i64, &'static str)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT physical_offset, length FROM extents WHERE volume = ?1",
        )?;
        let rows: Vec<(i64, i64)> = stmt
            .query_map(params![volume], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        phys.extend(rows.into_iter().map(|(p, l)| (p, l, "extent")));
    }
    {
        let mut stmt = conn.prepare(
            "SELECT physical_offset, length FROM extent_free WHERE volume = ?1",
        )?;
        let rows: Vec<(i64, i64)> = stmt
            .query_map(params![volume], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        phys.extend(rows.into_iter().map(|(p, l)| (p, l, "free")));
    }
    {
        let mut stmt = conn.prepare(
            "SELECT physical_offset, length FROM extent_quarantine WHERE volume = ?1",
        )?;
        let rows: Vec<(i64, i64)> = stmt
            .query_map(params![volume], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        phys.extend(rows.into_iter().map(|(p, l)| (p, l, "quarantine")));
    }
    phys.sort_unstable();
    for w in phys.windows(2) {
        let (pa, la, ta) = w[0];
        let (pb, _, tb) = w[1];
        if pa + la > pb {
            return Err(ExtentAllocError::Corruption(format!(
                "physical overlap in {volume}: {ta} [{pa},{}) vs {tb} [{pb},…)",
                pa + la
            )));
        }
    }

    // 3. Watermark containment (only when the volume is registered; a
    //    volume with rows but no arena is itself corruption).
    let arena: Option<(i64, i64)> = conn
        .query_row(
            "SELECT size_ceiling, next_free FROM volume_alloc WHERE volume = ?1",
            params![volume],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match arena {
        Some((ceiling, next_free)) => {
            if next_free > ceiling {
                return Err(ExtentAllocError::Corruption(format!(
                    "watermark {next_free} beyond ceiling {ceiling} in {volume}"
                )));
            }
            // Row-counter drift check (the O(1) budget counter vs the
            // O(rows) truth — full-verifier only, deliberately: the
            // COUNT here is the very slope the windowed verifier
            // removed from the hot paths).
            let counted: i64 = conn.query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1",
                params![volume],
                |r| r.get(0),
            )?;
            let recorded: i64 = conn.query_row(
                "SELECT extent_rows FROM volume_alloc WHERE volume = ?1",
                params![volume],
                |r| r.get(0),
            )?;
            if counted != recorded {
                return Err(ExtentAllocError::Corruption(format!(
                    "extent_rows counter drift in {volume}: recorded {recorded}, \
                     counted {counted}"
                )));
            }
            if let Some((p, l, tag)) = phys.iter().find(|(p, l, _)| p + l > next_free) {
                return Err(ExtentAllocError::Corruption(format!(
                    "{tag} range [{p},{}) beyond watermark {next_free} in {volume}",
                    p + l
                )));
            }
        }
        None if !phys.is_empty() => {
            return Err(ExtentAllocError::Corruption(format!(
                "{volume} has extent rows but no volume_alloc arena"
            )));
        }
        None => {}
    }

    // 4. Grant integrity: every grant row references a live extent, and an
    //    UNFENCED row's gen matches its extent's gen — the transactional
    //    form of the model's Inv_RecallCompletesBeforeReuse (gen cannot
    //    move under a live unfenced grant).
    let orphans: i64 = conn.query_row(
        "SELECT COUNT(*) FROM extent_grants g
         WHERE g.volume = ?1 AND NOT EXISTS
           (SELECT 1 FROM extents e
            WHERE e.volume = g.volume AND e.file_id = g.file_id
              AND e.logical_offset = g.logical_offset)",
        params![volume],
        |r| r.get(0),
    )?;
    if orphans > 0 {
        return Err(ExtentAllocError::Corruption(format!(
            "{orphans} grant rows reference no extent in {volume}"
        )));
    }
    let stale: i64 = conn.query_row(
        "SELECT COUNT(*) FROM extent_grants g JOIN extents e
            ON e.volume = g.volume AND e.file_id = g.file_id
           AND e.logical_offset = g.logical_offset
         WHERE g.volume = ?1 AND g.fenced = 0 AND g.gen <> e.gen",
        params![volume],
        |r| r.get(0),
    )?;
    if stale > 0 {
        return Err(ExtentAllocError::Corruption(format!(
            "{stale} unfenced grant rows at a stale generation in {volume}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::sqlite::SCHEMA_SQL;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA_SQL).expect("schema");
        conn
    }

    const VOL: &str = "pvc-test";
    const F: u64 = 7;
    const C1: u64 = 1;
    const C2: u64 = 2;

    fn setup() -> Connection {
        let mut conn = fresh();
        register_volume(&mut conn, VOL, 1 << 20).unwrap();
        conn
    }

    #[test]
    fn grant_allocates_and_regrant_is_idempotent() {
        let mut conn = setup();
        let g1 = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        assert_eq!(g1.len(), 1);
        assert_eq!(g1[0].generation, 1);
        assert!(!g1[0].committed);
        assert!(!g1[0].needs_scrub, "virgin bump-allocated space reads zeros already");
        let g2 = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        assert_eq!(g1, g2, "re-grant returns the same extents, no new allocation");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// TLC trace: FlintExtentsGrantOverlap.cfg (GrantsExclusive=FALSE finds
    /// two live grants sharing a block in 5 states). The real transaction
    /// must be the belt: the second client's request refuses.
    #[test]
    fn tlc_grant_overlap_two_clients_refused_in_the_transaction() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        match grant(&mut conn, VOL, F, C2, 4096, 8192, false) {
            Err(ExtentAllocError::Conflict { holders }) => assert_eq!(holders, vec![C1]),
            other => panic!("expected Conflict, got {other:?}"),
        }
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// TLC trace: FlintExtentsStaleSnapshotFree.cfg — THE TRANCHE-1
    /// FINDING. A grant lands after the reclaim's holder snapshot; the
    /// free transaction must re-validate and refuse, no matter what the
    /// snapshot said.
    #[test]
    fn tlc_stale_snapshot_free_the_free_transaction_revalidates_holders() {
        let mut conn = setup();
        // c2 is granted and returns: the extent is an orphan.
        grant(&mut conn, VOL, F, C2, 0, 8192, false).unwrap();
        layout_return(&mut conn, VOL, F, C2, 0, 8192).unwrap();
        // The reclaim takes its snapshot: no holders.
        let snap = reclaim_snapshot(&mut conn, VOL, F, 0, 8192).unwrap();
        assert!(snap.is_empty(), "snapshot legitimately sees no holders");
        // c1's grant publishes after the snapshot (the C6-shaped window).
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        // The free MUST refuse — trusting the snapshot here is the
        // machine-refuted design.
        match reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000) {
            Err(ExtentAllocError::NotQuiescent { holders }) => assert_eq!(holders, vec![C1]),
            other => panic!("expected NotQuiescent, got {other:?}"),
        }
        // Resolution: the re-snapshot sees c1; c1 returns; the free lands.
        assert_eq!(reclaim_snapshot(&mut conn, VOL, F, 0, 8192).unwrap(), vec![C1]);
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000).unwrap();
        assert_eq!(out.freed_extents, 1);
        assert_eq!(out.freed_bytes, 8192);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// TLC trace family: FlintExtentsReuseUnderGrant.cfg (the harm is a
    /// stale holder over a reused range). The code has no free-that-skips-
    /// revalidation to mutate, so the pinned behaviours are the detector
    /// and the control-path fence: reuse mints gen+1, and a commit against
    /// a gone or regenerated extent refuses.
    #[test]
    fn tlc_reuse_bumps_gen_and_a_stale_commit_is_refused() {
        let mut conn = setup();
        let g1 = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        assert_eq!(g1[0].generation, 1);
        let phys1 = g1[0].physical_offset;
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000).unwrap();
        // Reuse: same physical range, next generation.
        let g2 = grant(&mut conn, VOL, F, C2, 0, 8192, false).unwrap();
        assert_eq!(g2[0].physical_offset, phys1, "free list reuses the range");
        assert_eq!(g2[0].generation, 2, "reuse bumps the generation");
        assert!(
            g2[0].needs_scrub,
            "a reused range carries the prior incarnation's bytes until zeroed \
             (FlintExtentsBlindProvision.cfg is the world where nobody does)"
        );
        // c1's LAYOUTCOMMIT arrives late (control path is never fenced by
        // the reservation): it must refuse — c1 has no grant row on the
        // reincarnated extent.
        match commit_extents(&mut conn, VOL, F, C1, 0, 8192) {
            Err(ExtentAllocError::CommitRejected(r)) => assert_eq!(r, "no grant for this client"),
            other => panic!("expected CommitRejected, got {other:?}"),
        }
        // The rightful owner's commit promotes.
        assert_eq!(commit_extents(&mut conn, VOL, F, C2, 0, 8192).unwrap(), 1);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// TLC trace family: FlintExtentsLostFence.cfg / TgtAmnesia.cfg — the
    /// worlds where a fence is believed and is not (or stops being) real.
    /// The code's mitigation, per the FreeRequiresDelivered belt: an
    /// UNCONFIRMED fence's ranges (no delivered mark — here the fence
    /// never ran a preempt at all) quarantine (leaked, metered), never
    /// enter the free list, and only the operator lever releases them —
    /// after which reuse still bumps the generation. The delivered
    /// (confirmed) side frees cleanly — see the tests below.
    #[test]
    fn fenced_holder_extents_quarantine_not_free_and_the_lever_releases() {
        let mut conn = setup();
        let g1 = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        let phys1 = g1[0].physical_offset;
        assert_eq!(fence_client(&mut conn, VOL, C1).unwrap(), 1);
        // A fenced client gets nothing new.
        match grant(&mut conn, VOL, F, C1, 16384, 4096, false) {
            Err(ExtentAllocError::FencedClient) => {}
            other => panic!("expected FencedClient, got {other:?}"),
        }
        // The reclaim completes — into quarantine, not the free list.
        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();
        assert_eq!(out.quarantined_extents, 1);
        assert_eq!(out.quarantined_bytes, 8192);
        assert_eq!(out.freed_extents, 0);
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));
        // A new grant must NOT be handed the quarantined range.
        let g2 = grant(&mut conn, VOL, F, C2, 0, 8192, false).unwrap();
        assert_ne!(g2[0].physical_offset, phys1, "quarantined range not reused");
        // Operator lever: release, then the range is reusable at gen+1.
        assert_eq!(release_quarantine(&mut conn, VOL).unwrap(), 8192);
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (0, 0));
        let g3 = grant(&mut conn, VOL, F, C2, 32768, 8192, false).unwrap();
        assert_eq!(g3[0].physical_offset, phys1, "released range is first-fit reused");
        assert_eq!(g3[0].generation, 2, "reuse after release still bumps the generation");
        assert!(g3[0].needs_scrub, "a released quarantine range is still a reuse");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// A LAYOUTRETURN after a fence is the client's own quiescence
    /// promise: the extent frees cleanly instead of quarantining (and the
    /// model agrees — a returned holder leaves HeldBy empty).
    #[test]
    fn return_after_fence_upgrades_quarantine_to_clean_free() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        fence_client(&mut conn, VOL, C1).unwrap();
        assert_eq!(layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap(), 1);
        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000).unwrap();
        assert_eq!(out.freed_extents, 1);
        assert_eq!(out.quarantined_extents, 0);
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (0, 0));
    }

    /// THE MERGE POLICY: a sequentially-written file's N contiguous
    /// extents collapse to ONE row at LAYOUTRETURN (the quiescence
    /// moment), gen = MAX, counter maintained, and the merged row still
    /// reclaims cleanly.
    #[test]
    fn merge_collapses_a_sequential_file_at_return() {
        let mut conn = setup();
        // Four sequential grants = four rows, physically contiguous by
        // construction (bump allocation).
        for i in 0..4u64 {
            grant(&mut conn, VOL, F, C1, i * 8192, 8192, false).unwrap();
        }
        let rows = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(rows(&conn), 4);

        assert_eq!(layout_return(&mut conn, VOL, F, C1, 0, 4 * 8192).unwrap(), 4);
        assert_eq!(rows(&conn), 1, "four contiguous quiescent rows merged to one");
        let (len, st): (i64, String) = conn
            .query_row(
                "SELECT length, state FROM extents WHERE volume = ?1 AND logical_offset = 0",
                params![VOL],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(len, 4 * 8192);
        assert_eq!(st, "invalid");
        // The counter followed (the full verifier cross-checks it, but
        // assert the visible value too).
        let recorded: i64 = conn
            .query_row(
                "SELECT extent_rows FROM volume_alloc WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 1);
        // And the merged row reclaims like any other.
        let out = reclaim_complete(&mut conn, VOL, F, 0, 4 * 8192, 99).unwrap();
        assert_eq!(out.freed_extents, 1);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// Merge refusals, all three: a live grant on either row, a state
    /// boundary, and physical discontiguity each block coalescing.
    #[test]
    fn merge_respects_holders_state_and_physical_contiguity() {
        let mut conn = setup();
        // Two contiguous rows; C2 still holds the second — returning
        // C1's rows must NOT merge across C2's held row.
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        grant(&mut conn, VOL, F, C2, 8192, 8192, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "no merge under a live grant (MergeHeld.cfg's teeth)");

        // C2 commits its row then returns: state 'rw' vs 'invalid' —
        // still no merge across the state boundary.
        commit_extents(&mut conn, VOL, F, C2, 8192, 8192).unwrap();
        layout_return(&mut conn, VOL, F, C2, 8192, 8192).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "no merge across a state boundary");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// Logically-adjacent rows whose PHYSICAL placements are not
    /// adjacent stay separate — an extent is a physical mapping, and
    /// merging would fabricate one.
    #[test]
    fn merge_requires_physical_contiguity() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        // A second file's grant lands physically between them.
        grant(&mut conn, VOL, 99, C1, 0, 4096, false).unwrap();
        grant(&mut conn, VOL, F, C1, 8192, 8192, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 2 * 8192).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1 AND file_id = ?2",
                params![VOL, F as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "logical neighbours with a physical gap stay separate");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// The stated per-volume row bound: grants refuse at the budget with
    /// the fragmentation error, and dropping rows (as a merge/reclaim
    /// does) reopens room. The shattered volume is manufactured as REAL
    /// rows (1-byte extents via a recursive CTE) with the counter
    /// maintained — the full verifier's drift check runs inside every
    /// grant here and would refuse a faked counter (it caught this
    /// test's first draft doing exactly that).
    #[test]
    fn row_budget_refuses_and_merge_reopens() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        // 65,535 one-byte extents of another file, physically packed
        // after the real grant, inside the watermark.
        let fake = DEFAULT_EXTENT_ROW_BUDGET - 1;
        conn.execute(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < ?3 - 1)
             INSERT INTO extents
               (volume, file_id, logical_offset, length, physical_offset, gen, state)
             SELECT ?1, ?2, i, 1, 8192 + i, 1, 'invalid' FROM n",
            params![VOL, 99i64, fake],
        )
        .unwrap();
        conn.execute(
            "UPDATE volume_alloc SET extent_rows = extent_rows + ?2, next_free = next_free + ?2
             WHERE volume = ?1",
            params![VOL, fake],
        )
        .unwrap();
        verify_volume_invariants(&conn, VOL).expect("the shattered volume is consistent");

        match grant(&mut conn, VOL, F, C1, 1 << 19, 8192, false) {
            Err(ExtentAllocError::RowBudget { rows, budget }) => {
                assert_eq!(rows, DEFAULT_EXTENT_ROW_BUDGET as u64);
                assert_eq!(budget, DEFAULT_EXTENT_ROW_BUDGET as u64);
            }
            other => panic!("expected RowBudget, got {other:?}"),
        }
        // A re-grant of an EXISTING extent mints nothing — no budget
        // verdict on a zero-mint request.
        grant(&mut conn, VOL, F, C1, 0, 8192, false).expect("zero-mint grant passes");
        // Room reopens when rows genuinely go (the merge/reclaim shape).
        conn.execute(
            "DELETE FROM extents WHERE volume = ?1 AND file_id = 99",
            params![VOL],
        )
        .unwrap();
        conn.execute(
            "UPDATE volume_alloc SET extent_rows = extent_rows - ?2 WHERE volume = ?1",
            params![VOL, fake],
        )
        .unwrap();
        grant(&mut conn, VOL, F, C1, 1 << 19, 8192, false).expect("room after merge");
    }

    /// Free-list coalescing: freeing contiguous ranges one at a time
    /// leaves ONE free row, last_gen = MAX — and the windowed verifier
    /// still catches a manufactured overlap (its teeth, the corrupted-
    /// table method).
    #[test]
    fn free_list_coalesces_and_windowed_verify_has_teeth() {
        let mut conn = setup();
        for i in 0..3u64 {
            grant(&mut conn, VOL, F, C1, i * 8192, 8192, false).unwrap();
            layout_return(&mut conn, VOL, F, C1, i * 8192, 8192).unwrap();
        }
        // The three returns merged the rows; reclaim frees the (merged)
        // extents — the free list should coalesce to a single row.
        reclaim_complete(&mut conn, VOL, F, 0, 3 * 8192, 7).unwrap();
        let (free_rows, free_len): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(length), 0) FROM extent_free WHERE volume = ?1",
                params![VOL],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(free_rows, 1, "contiguous frees coalesce");
        assert_eq!(free_len, 3 * 8192);

        // Windowed-verifier teeth: manufacture a physical overlap inside
        // the window a fresh grant will touch — the grant transaction
        // must refuse with Corruption, not persist it.
        conn.execute(
            "INSERT INTO extents
               (volume, file_id, logical_offset, length, physical_offset, gen, state)
             VALUES (?1, ?2, 900000, 8192, 0, 1, 'invalid')",
            params![VOL, F as i64],
        )
        .unwrap();
        conn.execute(
            "UPDATE volume_alloc SET extent_rows = extent_rows + 1 WHERE volume = ?1",
            params![VOL],
        )
        .unwrap();
        match grant(&mut conn, VOL, F, C2, 0, 8192, false) {
            Err(ExtentAllocError::Corruption(m)) => {
                assert!(m.contains("physical overlap"), "{m}")
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    /// THE FLIP (FreeRequiresDelivered, model-gated): a fenced holder
    /// whose fence was CONFIRMED at the target frees cleanly — no
    /// quarantine, no leak, grant rows swept, range first-fit reusable
    /// at gen+1 with the scrub contract intact.
    #[test]
    fn delivered_fence_extents_free_cleanly_and_reuse() {
        let mut conn = setup();
        let g1 = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        let phys1 = g1[0].physical_offset;
        fence_record(&mut conn, VOL, C1, 100).unwrap();
        assert_eq!(fence_client(&mut conn, VOL, C1).unwrap(), 1);
        assert!(mark_fence_delivered(&mut conn, VOL, C1, 200).unwrap());
        // Marking is one-shot: an already-delivered fence is a no-op.
        assert!(!mark_fence_delivered(&mut conn, VOL, C1, 300).unwrap());
        // And marking without a record is a no-op, not an error.
        assert!(!mark_fence_delivered(&mut conn, VOL, C2, 200).unwrap());

        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();
        assert_eq!(out.freed_extents, 1, "delivered fence frees");
        assert_eq!(out.freed_bytes, 8192);
        assert_eq!(out.quarantined_extents, 0, "no quarantine, no leak");
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (0, 0));
        // The fenced grant rows went with the extent — nothing left to
        // refuse over.
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extent_grants WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "grant rows swept with the free");
        // The range is genuinely reusable: first-fit, gen bumped, scrub.
        let g2 = grant(&mut conn, VOL, F, C2, 32768, 8192, false).unwrap();
        assert_eq!(g2[0].physical_offset, phys1, "freed range first-fit reused");
        assert_eq!(g2[0].generation, 2, "reuse bumps the generation");
        assert!(g2[0].needs_scrub, "reuse still scrubs (ProvisionalInvisible)");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// The partition is PER-EXTENT and per-fence: in one reclaim pass, a
    /// delivered fence's extent frees while an undelivered fence's
    /// extent quarantines.
    #[test]
    fn mixed_delivery_partitions_per_extent() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        grant(&mut conn, VOL, F, C2, 16384, 8192, false).unwrap();
        for c in [C1, C2] {
            fence_record(&mut conn, VOL, c, 100).unwrap();
            fence_client(&mut conn, VOL, c).unwrap();
        }
        // Only C1's fence is confirmed at the target.
        assert!(mark_fence_delivered(&mut conn, VOL, C1, 200).unwrap());

        let out =
            reclaim_complete(&mut conn, VOL, F, 0, (i64::MAX as u64) - 1, 1234).unwrap();
        assert_eq!(out.freed_extents, 1, "C1's extent (delivered) freed");
        assert_eq!(out.quarantined_extents, 1, "C2's extent (unconfirmed) quarantined");
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// One undelivered fence poisons a SHARED extent: with two fenced
    /// holders on the same rows, `all delivered` is what frees — any
    /// unconfirmed exclusion quarantines the whole extent (freeing past
    /// it is FlintExtentsLostFence's machine-checked corruption). The
    /// second holder row is manufactured directly: in the live schema
    /// multi-holder extents arise through the READ-grant tiling, whose
    /// ceremony would obscure what this test pins.
    #[test]
    fn a_shared_extent_frees_only_when_every_fence_is_delivered() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        conn.execute(
            "INSERT INTO extent_grants
               (volume, file_id, logical_offset, client_id, mode, gen, fenced)
             SELECT volume, file_id, logical_offset, ?1, mode, gen, 0
             FROM extent_grants WHERE volume = ?2 AND client_id = ?3",
            params![C2 as i64, VOL, C1 as i64],
        )
        .unwrap();
        for c in [C1, C2] {
            fence_record(&mut conn, VOL, c, 100).unwrap();
            fence_client(&mut conn, VOL, c).unwrap();
        }
        assert!(mark_fence_delivered(&mut conn, VOL, C1, 200).unwrap());

        // C2's fence unconfirmed → the shared extent quarantines whole.
        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();
        assert_eq!(out.freed_extents, 0);
        assert_eq!(out.quarantined_extents, 1);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    #[test]
    fn ceiling_is_enforced_after_free_list_first_fit() {
        let mut conn = fresh();
        register_volume(&mut conn, VOL, 16384).unwrap();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        grant(&mut conn, VOL, F, C1, 8192, 8192, false).unwrap();
        match grant(&mut conn, VOL, F, C1, 16384, 4096, false) {
            Err(ExtentAllocError::NoSpace { needed, ceiling, next_free }) => {
                assert_eq!((needed, ceiling, next_free), (4096, 16384, 16384));
            }
            other => panic!("expected NoSpace, got {other:?}"),
        }
        // Free the first extent; the free list satisfies what the
        // watermark cannot.
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000).unwrap();
        let g = grant(&mut conn, VOL, F, C1, 16384, 4096, false).unwrap();
        assert_eq!(g[0].physical_offset, 0, "carved from the freed range");
        assert_eq!(g[0].generation, 2);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// The §8 mandate with teeth: the PK admits overlapping rows, so the
    /// assertion must catch a corrupted table and refuse to write past it.
    #[test]
    fn the_disjointness_assertion_has_teeth_on_a_corrupted_table() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        // Simulate the aliasing bug class directly: a second extent row
        // over the same physical range, distinct PK.
        conn.execute(
            "INSERT INTO extents
               (volume, file_id, logical_offset, length, physical_offset, gen, state)
             VALUES (?1, ?2, 65536, 8192, 0, 1, 'invalid')",
            params![VOL, F as i64],
        )
        .unwrap();
        match verify_volume_invariants(&conn, VOL) {
            Err(ExtentAllocError::Corruption(msg)) => {
                assert!(msg.contains("physical overlap"), "got: {msg}")
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
        // And every writing transaction refuses to commit past it: the
        // grant below is rolled back in its entirety.
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM extent_grants WHERE volume = ?1", params![VOL], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(matches!(
            grant(&mut conn, VOL, F, C2, 131072, 4096, false),
            Err(ExtentAllocError::Corruption(_))
        ));
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM extent_grants WHERE volume = ?1", params![VOL], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(before, after, "the failed transaction wrote nothing");
    }

    /// Churn: grant/return/free/re-grant across two files with the
    /// invariant oracle after every step — the test-level analog of the
    /// model's 1.96M-state sweep.
    #[test]
    fn physical_space_stays_disjoint_under_churn() {
        let mut conn = setup();
        let files = [7u64, 9u64];
        // Deterministic mixed sequence; no rand dependency.
        for step in 0u64..60 {
            let f = files[(step % 2) as usize];
            let c = if step % 3 == 0 { C1 } else { C2 };
            let off = (step % 5) * 8192;
            match step % 4 {
                0 | 1 => {
                    // Grants may legitimately Conflict; both outcomes keep
                    // the invariants.
                    let _ = grant(&mut conn, VOL, f, c, off, 8192, false);
                }
                2 => {
                    let _ = layout_return(&mut conn, VOL, f, c, off, 8192);
                }
                _ => {
                    // Frees may legitimately be NotQuiescent.
                    let _ = reclaim_complete(&mut conn, VOL, f, off, 8192, step as i64);
                }
            }
            verify_volume_invariants(&conn, VOL).unwrap();
        }
    }

    #[test]
    fn commit_promotes_invalid_to_rw_only_under_a_live_matching_grant() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        // Another client cannot commit someone else's provisional extents.
        assert!(matches!(
            commit_extents(&mut conn, VOL, F, C2, 0, 8192),
            Err(ExtentAllocError::CommitRejected("no grant for this client"))
        ));
        // A fenced client's control path is fenced here too (§8).
        fence_client(&mut conn, VOL, C1).unwrap();
        assert!(matches!(
            commit_extents(&mut conn, VOL, F, C1, 0, 8192),
            Err(ExtentAllocError::CommitRejected("grant is fenced"))
        ));
        // Clean path: return, re-grant, commit, and the re-grant after a
        // commit reports committed extents.
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        // (fence marks are per-grant-row; with the row returned, C1 is no
        // longer fenced on this volume and may be re-admitted.)
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        assert_eq!(commit_extents(&mut conn, VOL, F, C1, 0, 8192).unwrap(), 1);
        let g = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        assert!(g[0].committed, "committed state survives re-grant");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// block_hosts: admit is an idempotent upsert returning the DISTINCT
    /// desired list; evict names what fell off and what remains; a
    /// client re-admitted under a new NQN (node rename) replaces its row.
    #[test]
    fn host_rows_admit_evict_and_replace() {
        let mut conn = setup();
        let h1 = "nqn.2024-11.com.flint:node:a".to_string();
        let h2 = "nqn.2024-11.com.flint:node:b".to_string();
        assert_eq!(host_admit(&mut conn, VOL, C1, &h1, 0).unwrap(), vec![h1.clone()]);
        assert_eq!(
            host_admit(&mut conn, VOL, C1, &h1, 5).unwrap(),
            vec![h1.clone()],
            "re-admit is idempotent"
        );
        assert_eq!(
            host_admit(&mut conn, VOL, C2, &h1, 0).unwrap(),
            vec![h1.clone()],
            "two clients, one node: DISTINCT list has one entry"
        );
        // C1 re-appears from a renamed node: its row is replaced.
        let hosts = host_admit(&mut conn, VOL, C1, &h2, 9).unwrap();
        assert_eq!(hosts, vec![h1.clone(), h2.clone()]);

        let (evicted, remaining) = host_evict(&mut conn, VOL, C2).unwrap();
        assert_eq!(evicted, vec![h1.clone()]);
        assert_eq!(remaining, vec![h2.clone()], "C1's new identity survives");
        let (evicted, remaining) = host_evict(&mut conn, VOL, C2).unwrap();
        assert!(evicted.is_empty(), "double-evict is a clean no-op");
        assert_eq!(remaining, vec![h2]);
    }

    /// READ grants never allocate, never show uncommitted extents, and
    /// leave holder rows that block a reclaim (reader visibility — the
    /// FreeRevalidates belt covers readers too).
    #[test]
    fn read_grant_is_nonallocating_committed_only_and_visible_to_reclaim() {
        let mut conn = setup();
        // C1 writes two 4k extents, commits only the first, returns.
        // (Commit promotes whole extents, so the split shapes the state.)
        grant(&mut conn, VOL, F, C1, 0, 4096, false).unwrap();
        grant(&mut conn, VOL, F, C1, 4096, 4096, false).unwrap();
        commit_extents(&mut conn, VOL, F, C1, 0, 4096).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();

        // C2 asks to READ a huge window: only the committed extent comes
        // back, and nothing was allocated for the rest of the window.
        let r = grant_read(&mut conn, VOL, F, C2, 0, 1 << 20).unwrap();
        assert_eq!(r.len(), 1, "uncommitted extents must not appear in a read grant");
        assert_eq!((r[0].logical_offset, r[0].length), (0, 4096));
        assert!(r[0].committed);
        let (_, next_free): (i64, i64) = conn
            .query_row(
                "SELECT size_ceiling, next_free FROM volume_alloc WHERE volume = ?1",
                params![VOL],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(next_free, 8192, "a read must never move the watermark");

        // The reader's grant row blocks the free until it returns.
        assert!(matches!(
            reclaim_complete(&mut conn, VOL, F, 0, 1 << 20, 0),
            Err(ExtentAllocError::NotQuiescent { .. })
        ));
        layout_return(&mut conn, VOL, F, C2, 0, 1 << 20).unwrap();
        let out = reclaim_complete(&mut conn, VOL, F, 0, 1 << 20, 0).unwrap();
        assert_eq!(out.freed_extents, 2);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    #[test]
    fn drop_volume_sweeps_host_rows_too() {
        let mut conn = setup();
        host_admit(&mut conn, VOL, C1, "nqn.2024-11.com.flint:node:a", 0).unwrap();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        fence_record(&mut conn, VOL, C1, 0).unwrap();
        assert!(drop_volume(&mut conn, VOL).unwrap() > 0);
        assert!(
            hosts_for_volume(&conn, VOL).unwrap().is_empty(),
            "a re-created same-name volume must not inherit admissions"
        );
        assert!(
            !is_fenced(&conn, VOL, C1).unwrap(),
            "nor a re-created same-name volume inherit a fence"
        );
    }

    /// The durable fence record: written, queried by the admission guard,
    /// captures the host_nqn from block_hosts at fence time, and clears.
    #[test]
    fn fence_record_is_the_durable_positive_signal() {
        let mut conn = setup();
        let nqn = "nqn.2024-11.com.flint:node:a".to_string();
        host_admit(&mut conn, VOL, C1, &nqn, 0).unwrap();
        assert!(!is_fenced(&conn, VOL, C1).unwrap(), "not fenced before the record");

        // Recording captures the client's nqn (still in block_hosts) —
        // this is why it must run BEFORE the eviction.
        let captured = fence_record(&mut conn, VOL, C1, 100).unwrap();
        assert_eq!(captured, nqn, "the record grabs the nqn before eviction removes it");
        assert!(is_fenced(&conn, VOL, C1).unwrap(), "the guard sees the fence");
        assert!(!is_fenced(&conn, VOL, C2).unwrap(), "another client is untouched");

        // The eviction can now remove the block_hosts row; the fence
        // record (and its captured nqn) outlives it.
        host_evict(&mut conn, VOL, C1).unwrap();
        assert!(is_fenced(&conn, VOL, C1).unwrap(), "the fence survives the eviction");
        assert_eq!(
            fenced_all(&conn).unwrap(),
            vec![(VOL.to_string(), C1)],
            "startup replay sees the fenced (volume, client)"
        );

        // Re-fencing refreshes, does not duplicate (PK on volume,client).
        fence_record(&mut conn, VOL, C1, 200).unwrap();
        assert_eq!(fenced_all(&conn).unwrap().len(), 1, "idempotent by PK");

        // Clearing removes it — the release / lease-recovery path.
        assert!(unfence_record(&mut conn, VOL, C1).unwrap(), "a row was cleared");
        assert!(!is_fenced(&conn, VOL, C1).unwrap(), "unfenced");
        assert!(!unfence_record(&mut conn, VOL, C1).unwrap(), "double-clear is a no-op");
        assert!(fenced_all(&conn).unwrap().is_empty());
    }

    /// A fence recorded for a client that held NO admission (fenced by
    /// the operator before it ever mounted) still records — with an
    /// empty captured nqn, which is fine: the guard keys on client_id.
    #[test]
    fn fence_record_without_a_prior_admission_is_empty_nqn() {
        let mut conn = setup();
        let captured = fence_record(&mut conn, VOL, C1, 0).unwrap();
        assert_eq!(captured, "", "no block_hosts row → empty nqn");
        assert!(is_fenced(&conn, VOL, C1).unwrap());
    }
}
