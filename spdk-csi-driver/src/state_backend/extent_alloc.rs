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
    /// `revoke_client` refused: the client's fence is absent or not
    /// CONFIRMED at the target. Revoking (bulk-returning) the rows of a
    /// client whose exclusion is unproven would clear the very
    /// bookkeeping that makes its extents quarantine — LostFence's
    /// corruption through a side door. The sweep retries after the next
    /// fence attempt confirms.
    UnconfirmedFence,
    /// The volume has no serving-target seat — nothing records WHICH
    /// target composes it, so no dial site may guess. Fail-closed by
    /// construction: falling back to the reconciler's own configured
    /// listener is exactly `FlintCompositionStaticTraddr`, the shipped
    /// livelock the target registry exists to delete.
    UnseatedVolume,
    /// The seat names a composer with no registry row — the target has
    /// never self-registered against this MDS (or registered under a
    /// different id). Also fail-closed, and also diagnosable: the
    /// composer's name is in the error.
    UnknownComposer { composer: String },
    /// The promotion CAS lost: the seat is no longer the (epoch,
    /// composer) the caller read. Someone else already advanced it, so
    /// this promotion is stale and must not be retried against the same
    /// expectation — re-read and decide again.
    PromotionRaced { epoch: i64, composer: String },
    /// `ElectInSync` refused: the candidate's leg is not carrying an
    /// in-sync mark, so promoting it would discard acked writes the
    /// record already knows it is missing. `FlintCompositionElectStale.
    /// cfg` is that discard as a counterexample; the volume WAITS
    /// instead, which is availability spent on durability and is priced
    /// by `FlintCompositionWaitsPrice.cfg`.
    NotInSync { candidate: String },
    /// The candidate is the sitting composer. Not an election.
    SelfPromotion { composer: String },
    /// A lease renewal was refused. The reason matters and is carried:
    /// a DEPOSED holder is refused even while perfectly healthy (its
    /// lapsed horizon must stay passed), and a NEW composer is refused
    /// because a lease is granted by assembly, never taken by the
    /// holder that wants one.
    LeaseRefused { reason: String },
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
            Self::UnconfirmedFence => write!(
                f,
                "revoke refused: the client's fence is absent or unconfirmed at the target"
            ),
            Self::UnseatedVolume => write!(
                f,
                "no serving-target seat for this volume — refusing to guess which target \
                 composes it"
            ),
            Self::UnknownComposer { composer } => write!(
                f,
                "the seat names composer '{composer}', which has no target-registry row \
                 (never self-registered against this MDS)"
            ),
            Self::PromotionRaced { epoch, composer } => write!(
                f,
                "promotion lost the CAS: the seat now reads epoch {epoch} composer \
                 '{composer}'"
            ),
            Self::NotInSync { candidate } => write!(
                f,
                "election refused: leg '{candidate}' is not in sync, and promoting it would \
                 discard acked writes the record knows it is missing"
            ),
            Self::SelfPromotion { composer } => {
                write!(f, "'{composer}' is already the composer — not an election")
            }
            Self::LeaseRefused { reason } => write!(f, "serving lease refused: {reason}"),
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

/// Raise a volume's allocation ceiling (the CSI expand path).
///
/// RAISE-ONLY and idempotent: a request at or below the current ceiling
/// is answered with the ceiling in force, never an error — CSI re-drives
/// ExpandVolume until the reported capacity matches, and a "no" to a
/// duplicate would wedge the PVC in `Resizing` over an operation that had
/// already happened. Shrinking is not expressible here at all: extents
/// already handed out past a lower ceiling would be un-representable, and
/// CSI forbids shrink anyway.
///
/// ORDERING RULE for the caller: the backing device must be grown BEFORE
/// this ceiling moves. The ceiling is the allocator's promise that a
/// bump-allocated physical offset is addressable on the lvol; raising it
/// first opens a window where a LAYOUTGET hands a client extents past the
/// end of the namespace, and the client's write fails at the device with
/// no server-side record of why.
pub fn expand_volume(conn: &mut Connection, volume: &str, new_ceiling: u64) -> Result<u64> {
    let want = as_i64(new_ceiling, "ceiling exceeds i64")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: Option<i64> = tx
        .query_row(
            "SELECT size_ceiling FROM volume_alloc WHERE volume = ?1",
            params![volume],
            |r| r.get(0),
        )
        .optional()?;
    let Some(current) = current else {
        // No arena = this is not a block-class volume (or its create
        // never completed). Silently "expanding" it would report success
        // for a ceiling that does not exist.
        return Err(ExtentAllocError::InvalidRange(
            "volume has no extent arena to expand",
        ));
    };
    if want <= current {
        return Ok(current as u64);
    }
    tx.execute(
        "UPDATE volume_alloc SET size_ceiling = ?2 WHERE volume = ?1",
        params![volume, want],
    )?;
    // The watermark/containment invariant is the one this touches; run
    // the same verifier every other write transaction ends with rather
    // than reasoning that a raise is obviously safe.
    verify_volume_invariants(&tx, volume)?;
    tx.commit()?;
    Ok(want as u64)
}

/// Bytes the arena can still hand out to an allocating LAYOUTGET.
///
/// The bump region ONLY (`ceiling - next_free`). Free-list bytes are
/// deliberately excluded: the production grant path runs `fresh_only`
/// (reused ranges would ship a previous incarnation's bytes until the MDS
/// can scrub them — `GrantedExtent::needs_scrub`), so free-list space is
/// not reachable and counting it would make an exhausted volume look
/// healthy to the ENOSPC belt. Zero here means the next write grant
/// returns `NoSpace`.
pub fn volume_headroom(conn: &Connection, volume: &str) -> Result<u64> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT size_ceiling, next_free FROM volume_alloc WHERE volume = ?1",
            params![volume],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((ceiling, next_free)) => Ok((ceiling - next_free).max(0) as u64),
        // No arena row: not a block volume. "Not exhausted" is the safe
        // answer — the belt's other arms own that file.
        None => Ok(u64::MAX),
    }
}

/// The highest logical end covered by a file's COMMITTED extents, or 0
/// if it has none.
///
/// The MDS stub's length is normally the file's size, but it only
/// advances at LAYOUTCOMMIT — so between a client's write and its
/// commit, the extent map knows about bytes the stub does not. Anything
/// that wants to answer "is this offset past the end of the file?" has
/// to ask both, or it will call a write-in-flight an EOF.
pub fn committed_end(conn: &Connection, volume: &str, file_id: u64) -> Result<u64> {
    let end: Option<i64> = conn.query_row(
        "SELECT MAX(logical_offset + length) FROM extents
         WHERE volume = ?1 AND file_id = ?2 AND state = 'rw'",
        params![volume, file_id as i64],
        |r| r.get(0),
    )?;
    Ok(end.unwrap_or(0).max(0) as u64)
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
    // THE GENERATION MEMORY, written before the rows go. A returning
    // client is not a holder — that is the whole point of the return, and
    // every free/conflict belt keeps reading extent_grants alone — but it
    // may still have written bytes it has not committed yet, and the
    // Linux client routinely returns first and commits second. Without
    // this row its LAYOUTCOMMIT has nothing to validate against and its
    // data stays provisional forever (rig-found, 2026-08-11).
    //
    // FENCED rows are deliberately excluded: a fenced client's exclusion
    // must not be undone by the very revocation that swept its rows.
    tx.execute(
        "INSERT OR REPLACE INTO extent_commit_grace
             (volume, file_id, logical_offset, client_id, gen)
         SELECT volume, file_id, logical_offset, client_id, gen
           FROM extent_grants
          WHERE volume = ?1 AND file_id = ?2 AND client_id = ?3 AND fenced = 0
            AND logical_offset IN
              (SELECT logical_offset FROM extents
               WHERE volume = ?1 AND file_id = ?2
                 AND logical_offset < ?5 AND logical_offset + length > ?4)",
        params![volume, file_id as i64, client, start, end],
    )?;
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
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let n = tx.execute(
        "UPDATE extent_grants SET fenced = 1
         WHERE volume = ?1 AND client_id = ?2 AND fenced = 0",
        params![volume, client],
    )?;
    // …and shut the commit-grace door in the same transaction. A client
    // being fenced must not be able to promote extents through a
    // generation record left by an earlier, unfenced return — the fence
    // is an exclusion, and a half-excluded client is not one.
    tx.execute(
        "DELETE FROM extent_commit_grace WHERE volume = ?1 AND client_id = ?2",
        params![volume, client],
    )?;
    tx.commit()?;
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
    // Rows removed whole, versus rows that only lost a piece — the
    // extent_rows counter below has to be told the difference.
    let mut rows_removed: i64 = 0;
    let mut rows_added: i64 = 0;
    for t in &targets {
        // FREE THE INTERSECTION, NEVER THE ROW. `overlapping_extents`
        // matches on PARTIAL overlap, so a target may extend past either
        // end of [start, end) — and everything outside that window is
        // data the caller asked to KEEP.
        //
        // Taking the whole row here is how a shrinking ftruncate
        // destroyed the prefix it promised to keep: the truncate point
        // lands mid-row, and `merge_extents_window` makes one row the
        // normal shape of a sequentially-written file, so the whole file
        // went. Silent — the client simply read zeros.
        let row_start = t.logical_offset;
        let row_end = row_start + t.length;
        let cut_start = row_start.max(start);
        let cut_end = row_end.min(end);
        let cut_len = cut_end - cut_start;
        // Physical is contiguous within a row, so the cut's physical
        // base is its logical distance from the row's start.
        let cut_phys = t.physical_offset + (cut_start - row_start);
        let keeps_head = cut_start > row_start;
        let keeps_tail = cut_end < row_end;

        let fenced: Vec<(i64, i64)> = fenced_stmt
            .query_map(params![volume, file_id as i64, t.logical_offset], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let all_delivered = fenced.iter().all(|(_, d)| *d > 0);
        if fenced.is_empty() || all_delivered {
            free_insert_coalescing(&tx, volume, cut_phys, cut_len, t.generation)?;
            out.freed_extents += 1;
            out.freed_bytes += cut_len as u64;
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
                params![volume, cut_phys, cut_len, t.generation, csv, now_unix],
            )?;
            out.quarantined_extents += 1;
            out.quarantined_bytes += cut_len as u64;
            tx.execute(
                "DELETE FROM extent_grants
                 WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
                params![volume, file_id as i64, t.logical_offset],
            )?;
        }
        // The row goes only if the cut consumed all of it. Otherwise the
        // surviving side(s) stay mapped at their ORIGINAL physical bytes
        // — that is the whole point: those bytes were never reclaimed,
        // so nothing may move or re-point them.
        tx.execute(
            "DELETE FROM extents
             WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
            params![volume, file_id as i64, t.logical_offset],
        )?;
        rows_removed += 1;
        if keeps_head {
            // Head keeps the row's logical offset, physical base, state
            // and generation; only its length shrinks to the cut.
            tx.execute(
                "INSERT INTO extents
                   (volume, file_id, logical_offset, length, physical_offset, gen, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    volume,
                    file_id as i64,
                    row_start,
                    cut_start - row_start,
                    t.physical_offset,
                    t.generation,
                    t.state
                ],
            )?;
            rows_added += 1;
        }
        if keeps_tail {
            // Tail starts where the cut ended, at the matching physical
            // displacement. Reached when the reclaimed range sits INSIDE
            // a row (a mid-file hole punch), not on the truncate path.
            tx.execute(
                "INSERT INTO extents
                   (volume, file_id, logical_offset, length, physical_offset, gen, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    volume,
                    file_id as i64,
                    cut_end,
                    row_end - cut_end,
                    t.physical_offset + (cut_end - row_start),
                    t.generation,
                    t.state
                ],
            )?;
            rows_added += 1;
        }
        // Hygiene, not safety: the extent is gone, so any commit-grace
        // record over it is dead weight. Safety is the generation check
        // in `commit_extents` — a record that outlived its extent
        // refuses there anyway, which is why this can be a plain
        // cleanup and never a correctness dependency.
        tx.execute(
            "DELETE FROM extent_commit_grace
             WHERE volume = ?1 AND file_id = ?2 AND logical_offset = ?3",
            params![volume, file_id as i64, t.logical_offset],
        )?;
    }
    drop(fenced_stmt);

    // NET, not `targets.len()`: a row clipped on one side leaves one row
    // behind, and a range reclaimed from the MIDDLE of a row leaves two.
    // Counting every target as removed would drift the budget counter
    // down until the full verifier's cross-check against COUNT(*) caught
    // it — which only runs in test/debug.
    tx.execute(
        "UPDATE volume_alloc SET extent_rows = extent_rows - ?2 WHERE volume = ?1",
        params![volume, rows_removed - rows_added],
    )?;
    // Windowed: this transaction DELETED extents, re-inserted the
    // surviving side(s) of any clipped row AT THEIR ORIGINAL PLACEMENT,
    // and INSERTED free/quarantine ranges over exactly the reclaimed
    // piece. Coverage only ever SHRINKS here — no row claims a byte it
    // did not already hold — so no reshape can create an overlap and
    // there is no touched row worth probing. Tests and debug builds
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
        // The grant the client wrote under, live or just returned. The
        // second door is not a relaxation of the belt — the generation
        // check below is identical on both — it only stops us from
        // demanding that a client still HOLD a layout it has already
        // given back. Linux returns before it commits; without this the
        // written bytes never leave 'invalid' (rig-found, 2026-08-11).
        let gen_held: Option<i64> = match row {
            Some((_, fenced)) if fenced != 0 => {
                return Err(ExtentAllocError::CommitRejected("grant is fenced"))
            }
            Some((g, _)) => Some(g),
            // Windowed, not exact-offset: LAYOUTRETURN merges adjacent
            // quiescent extents in the same transaction that writes the
            // grace rows, so by commit time one extent row can cover
            // several of them. Every grace row inside the extent must
            // agree on the generation — MIN = MAX = the extent's — or the
            // client did not write the whole of it under one incarnation
            // and the conservative refusal is right.
            None => {
                let (lo, hi, n): (Option<i64>, Option<i64>, i64) = tx.query_row(
                    "SELECT MIN(gen), MAX(gen), COUNT(*) FROM extent_commit_grace
                     WHERE volume = ?1 AND file_id = ?2 AND client_id = ?3
                       AND logical_offset >= ?4
                       AND logical_offset < ?5",
                    params![
                        volume,
                        file_id as i64,
                        client,
                        t.logical_offset,
                        t.logical_offset + t.length
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                match (lo, hi, n) {
                    (Some(a), Some(b), c) if c > 0 && a == b => Some(a),
                    _ => None,
                }
            }
        };
        match gen_held {
            None => return Err(ExtentAllocError::CommitRejected("no grant for this client")),
            // THE BELT. After a free+reuse the extent's generation has
            // moved, so a stale record — live or grace — refuses here.
            // This is what makes the grace row safe to forget lazily.
            Some(g) if g != t.generation => {
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
/// and whole-volume by design — nothing calls this automatically.
///
/// The old reason ("until FenceReaches is proven on hardware") is
/// retired: `FenceReaches` was superseded by the per-fence delivered
/// belt and is never going TRUE — it would assert that every fence
/// lands, which is false of a best-effort preempt arm. The reason it
/// stays manual is narrower and still stands: these ranges were
/// quarantined because a fence was NOT confirmed at the target, so
/// freeing them wholesale is exactly `FlintExtentsLostFence.cfg`'s
/// machine-checked corruption. The automatable successor is a
/// delivery RETRY plus a sweep gated on `extent_quarantine.
/// fenced_clients` all being delivered — the same predicate the reclaim
/// already uses, evaluated later.
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

/// THE QUARANTINE SWEEP — `release_quarantine`'s gated, automatic
/// successor, and the thing that stops an unconfirmed fence from leaking
/// the range FOREVER. A parked range frees when every client named in
/// its own `fenced_clients` CSV is CONFIRMED excluded at the target
/// (`fenced_clients.delivered_unix > 0`) — the same predicate
/// `reclaim_complete` applied and refused on, re-applied later, once the
/// reconcile pass's preempt retry has had a chance to land the fence
/// that had not landed then.
///
/// Returns `(ranges, bytes)` released.
///
/// THREE THINGS THE MODEL FORCED, each a counterexample before it was a
/// line of code (`formal/FlintExtents.tla`, the QuarantineEnabled
/// tranche; `FlintExtentsQuarantineBlindRelease.cfg` and
/// `FlintExtentsQuarantineVisible.cfg` are its two A/Bs):
///
/// 1. **It reads the range's OWN provenance, not the live tables.** A
///    sweep gated on "every current holder is fenced" frees ranges that
///    were never quarantined, skipping the recall entirely. The CSV this
///    range was parked with is the whole point of storing it.
/// 2. **An absent fence record is UNDELIVERED** (the `COALESCE 0`
///    discipline `reclaim_complete` already uses). A client that was
///    UNFENCED has no row, so its ranges stay parked — conservatively,
///    until the operator lever. Reading "no row" as "nothing to wait
///    for" would free exactly the ranges whose exclusion was released.
/// 3. **A parked range is invisible to the allocator** — not because
///    of a check here, but because the quarantine branch moved it OUT of
///    `extents` into this third table, and `grant` allocates from
///    `extent_free`/the watermark and re-grants from `extents`. Keep
///    that structure: the moment a parked range is reachable as an
///    orphan extent, this sweep frees it under its new owner's live
///    grant (the QuarantineVisible A/B, in nine states).
pub fn sweep_quarantine_delivered(
    conn: &mut Connection,
    volume: &str,
) -> Result<(u64, u64)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let parked: Vec<(i64, i64, i64, String)> = {
        let mut stmt = tx.prepare(
            "SELECT physical_offset, length, gen, fenced_clients
               FROM extent_quarantine WHERE volume = ?1",
        )?;
        let rows = stmt
            .query_map(params![volume], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    if parked.is_empty() {
        return Ok((0, 0));
    }

    let mut delivered_stmt = tx.prepare(
        "SELECT COALESCE(delivered_unix, 0) FROM fenced_clients
          WHERE volume = ?1 AND client_id = ?2",
    )?;
    let mut ranges = 0u64;
    let mut bytes = 0u64;
    let mut released: Vec<(i64, i64, i64)> = Vec::new();
    for (phys, len, generation, csv) in &parked {
        // A malformed id is NOT a free pass: an entry we cannot parse is
        // an exclusion we cannot verify, so the range stays parked.
        let mut all_delivered = true;
        for field in csv.split(',').filter(|s| !s.is_empty()) {
            let Ok(client) = field.trim().parse::<i64>() else {
                all_delivered = false;
                break;
            };
            let d: i64 = delivered_stmt
                .query_row(params![volume, client], |r| r.get(0))
                .optional()?
                .unwrap_or(0);
            if d <= 0 {
                all_delivered = false;
                break;
            }
        }
        // An EMPTY CSV never reaches quarantine (the branch fires only
        // with a fenced holder), so treat it as unverifiable rather than
        // as "nobody to wait for" — the same conservative side as above.
        if all_delivered && !csv.trim().is_empty() {
            released.push((*phys, *len, *generation));
            ranges += 1;
            bytes += *len as u64;
        }
    }
    drop(delivered_stmt);

    for (phys, len, generation) in &released {
        free_insert_coalescing(&tx, volume, *phys, *len, *generation)?;
        tx.execute(
            "DELETE FROM extent_quarantine WHERE volume = ?1 AND physical_offset = ?2",
            params![volume, phys],
        )?;
    }
    if ranges > 0 {
        // The free list just grew over ranges that left the quarantine —
        // exactly the two homes whose disjointness this checks. Cheap
        // relative to a sweep that only runs when something was parked.
        verify_volume_invariants_conn(&tx, volume)?;
    }
    tx.commit()?;
    Ok((ranges, bytes))
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
        "block_node_attach",
        "fenced_clients",
        // The seat goes with the volume: a re-created volume of the
        // same name must be seated afresh by whoever provisions it,
        // never inherit an epoch and a composer from a dead namesake.
        // Its legs go for the same reason — an inherited in-sync mark
        // would vouch for bytes that are gone.
        "block_volume_target",
        "block_volume_legs",
        // And the lease: the right to serve bytes that no longer exist
        // is not a right worth keeping.
        "block_leases",
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
///
/// The evicted NQNs' NODE-attach rows go in the same transaction: an
/// attach row surviving its client's fence would keep the NQN on the
/// allow-list, and the fenced node could simply reconnect — the exact
/// door the durable eviction exists to close (the rig's R4 assertion).
/// A node whose fence is later lifted re-attaches through the normal
/// ControllerPublish path.
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
    for nqn in &evicted {
        tx.execute(
            "DELETE FROM block_node_attach WHERE volume = ?1 AND host_nqn = ?2",
            params![volume, nqn],
        )?;
    }
    let remaining = hosts_for_volume_conn(&tx, volume)?;
    tx.commit()?;
    Ok((evicted, remaining))
}

/// Record that the NODE `node_name` (NVMe identity `host_nqn`) has the
/// volume attached (CSI ControllerPublish) and return the full desired
/// allow-list after the upsert. This is the admission that runs BEFORE
/// any NFS traffic exists — the nvme session must be up before the
/// client's first LAYOUTGET resolves the device, and `block_hosts` rows
/// can't carry it because their key (the NFS client_id) is minted at
/// EXCHANGE_ID, later still.
///
/// Refused while ANY fence record on the volume names this NQN: attach
/// would re-admit a fenced node through a side door the per-client guard
/// (`is_fenced`) cannot see, because the attaching node has no client_id
/// yet. Idempotent; a re-attach refreshes node_name (host rename).
pub fn node_attach(
    conn: &mut Connection,
    volume: &str,
    host_nqn: &str,
    node_name: &str,
    now_unix: i64,
) -> Result<Vec<String>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let fenced: i64 = tx.query_row(
        "SELECT COUNT(*) FROM fenced_clients WHERE volume = ?1 AND host_nqn = ?2",
        params![volume, host_nqn],
        |r| r.get(0),
    )?;
    if fenced > 0 {
        return Err(ExtentAllocError::FencedClient);
    }
    tx.execute(
        "INSERT INTO block_node_attach (volume, host_nqn, node_name, attached_unix)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (volume, host_nqn) DO UPDATE SET node_name = ?3",
        params![volume, host_nqn, node_name, now_unix],
    )?;
    let hosts = hosts_for_volume_conn(&tx, volume)?;
    tx.commit()?;
    Ok(hosts)
}

/// Drop the node's attach row (CSI ControllerUnpublish) and return
/// `(row_removed, remaining_desired_list)`. Idempotent — detaching an
/// absent row is a replay, not an error. The NQN can survive in
/// `remaining` via `block_hosts` rows: a client that earned a
/// LAYOUTGET-time admission keeps it until its own lifecycle (return /
/// lease sweep / fence) ends it — detach only withdraws the node-level
/// grant it made.
pub fn node_detach(
    conn: &mut Connection,
    volume: &str,
    host_nqn: &str,
) -> Result<(bool, Vec<String>)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let removed = tx.execute(
        "DELETE FROM block_node_attach WHERE volume = ?1 AND host_nqn = ?2",
        params![volume, host_nqn],
    )? > 0;
    let remaining = hosts_for_volume_conn(&tx, volume)?;
    tx.commit()?;
    Ok((removed, remaining))
}

/// The volume's full desired allow-list (distinct, ordered for stable
/// comparison in tests and logs).
pub fn hosts_for_volume(conn: &Connection, volume: &str) -> Result<Vec<String>> {
    hosts_for_volume_conn(conn, volume)
}

// ---------------------------------------------------------------------
// The target registry and the per-volume serving seat (design §12).
//
// `FlintCompositionStaticTraddr.cfg` is a REQUIRED-TO-FAIL run: with the
// preempt aimed at a constructor-held address instead of at whatever the
// record names, every post-failover fence confirmation livelocks and the
// quarantine sweep parks ranges forever. These two tables are the
// record it must follow.
//
// They are deliberately SEPARATE. A target's coordinates change without
// its identity changing (a node restarts on a new address); the composer
// of a volume changes only by promotion, which bumps the epoch. Folding
// them into one row would make a re-addressed node indistinguishable
// from a failover — and the epoch is the thing every belt keys on.
// ---------------------------------------------------------------------

/// A target that has announced where it can be dialed. One row per
/// spdk-tgt this MDS can reach; today's phase-1 shard registers exactly
/// itself, which is why the seat below can be written at provision time
/// with no election.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTargetRow {
    pub target_id: String,
    pub traddr: String,
    pub trsvcid: u16,
    /// First self-registration. Kept across re-registrations: a target
    /// that comes back on a new address is the SAME target, and the
    /// distinction between "new" and "moved" is one the failover work
    /// will need.
    pub registered_unix: i64,
    pub updated_unix: i64,
}

/// The volume's serving seat — the model's `[epoch, composer]` record,
/// one row per volume. Nothing moves it yet (promotion is the failover
/// tranche); what exists today is the READ side, so every dial site is
/// already record-driven and the tranche that lands promotion only has
/// to CAS this row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSeat {
    pub volume: String,
    /// Composition epoch. Starts at 1 and advances only by promotion.
    pub epoch: i64,
    /// `target_id` of the target composing this volume.
    pub composer: String,
    pub seated_unix: i64,
}

/// A target announces (or re-announces) its dial coordinates. Idempotent
/// and level-triggered — the MDS calls this every reconcile pass, so a
/// chart change to the listener converges without an operator.
///
/// Coordinates are overwritten in place: they are a fact about the
/// target's present, and a stale address that stayed because it was
/// written first is precisely the bug being deleted here.
pub fn target_register(
    conn: &mut Connection,
    target_id: &str,
    traddr: &str,
    trsvcid: u16,
    now_unix: i64,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO block_targets (target_id, traddr, trsvcid, registered_unix, updated_unix)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (target_id) DO UPDATE SET traddr = ?2, trsvcid = ?3, updated_unix = ?4",
        params![target_id, traddr, trsvcid as i64, now_unix],
    )?;
    tx.commit()?;
    Ok(())
}

/// Every registered target, ordered. Observability and the startup
/// audit — an unseated or unresolvable volume is diagnosed against this.
pub fn target_list(conn: &Connection) -> Result<Vec<BlockTargetRow>> {
    let mut stmt = conn.prepare(
        "SELECT target_id, traddr, trsvcid, registered_unix, updated_unix
         FROM block_targets ORDER BY target_id",
    )?;
    let rows = stmt.query_map([], |r| {
        let port: i64 = r.get(2)?;
        Ok(BlockTargetRow {
            target_id: r.get(0)?,
            traddr: r.get(1)?,
            trsvcid: port as u16,
            registered_unix: r.get(3)?,
            updated_unix: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// One leg's sync state for a volume. The election gate's input, and the
/// only thing that makes `ElectInSync` more than a wish.
///
/// Today a volume has exactly one leg — its composer's — marked in sync
/// when the volume is seated, so promotion has no candidate and refuses.
/// That refusal is the correct answer for a single-copy volume, not a
/// gap: there is nowhere to promote TO. The mark's LIFECYCLE (the
/// degrade barrier writing stale marks on a solo ack, a rebuild clearing
/// them) belongs to the replication tranche.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLeg {
    pub volume: String,
    pub target_id: String,
    /// `"insync"` or `"stale"`.
    pub sync_state: String,
    pub marked_unix: i64,
}

pub const LEG_INSYNC: &str = "insync";
pub const LEG_STALE: &str = "stale";

/// Seat a volume at `composer` if it has no seat, and return the seat
/// that stands either way. Seating also records the composer's own leg
/// as in-sync, IN THE SAME TRANSACTION — a seated volume with no in-sync
/// leg would be a volume the election gate could never promote away from
/// even when a good copy existed.
///
/// INSERT-if-absent, never an upsert: a seat is a claim about who serves
/// the volume's bytes, and silently moving it would be a survivor
/// adopting a volume with no election — `RecordAssemblyOnly`'s
/// counterexample, minted by the provisioner. The caller compares the
/// returned seat with what it asked for and refuses on a mismatch.
///
/// The leg row is insert-if-absent for a sharper reason: re-marking an
/// existing leg in-sync here would let an ordinary converge pass clear a
/// STALE mark with no copy behind it, which is precisely
/// `FlintCompositionSelfRejoin.cfg` — auto-examine declaring a stale leg
/// clean, so the honest election gate elects it in good faith and its
/// assembly discards the survivor's acked bytes. A stale mark is cleared
/// by a completed rebuild and by nothing else.
pub fn seat_volume(
    conn: &mut Connection,
    volume: &str,
    composer: &str,
    now_unix: i64,
    lease_expires_unix: i64,
) -> Result<BlockSeat> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seated = tx.execute(
        "INSERT INTO block_volume_target (volume, epoch, composer, seated_unix)
         VALUES (?1, 1, ?2, ?3)
         ON CONFLICT (volume) DO NOTHING",
        params![volume, composer, now_unix],
    )?;
    if seated > 0 {
        tx.execute(
            "INSERT INTO block_volume_legs (volume, target_id, sync_state, marked_unix)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (volume, target_id) DO NOTHING",
            params![volume, composer, LEG_INSYNC, now_unix],
        )?;
        // The first composition is an assembly, so it is also the first
        // lease grant — same transaction, because "assembly is the lease
        // grant" is one act and a seated volume with no lease is a
        // volume its own composer may not serve.
        tx.execute(
            "INSERT INTO block_leases (volume, epoch, holder, expires_unix)
             VALUES (?1, 1, ?2, ?3)
             ON CONFLICT (volume) DO NOTHING",
            params![volume, composer, lease_expires_unix],
        )?;
    }
    let seat = tx.query_row(
        "SELECT volume, epoch, composer, seated_unix FROM block_volume_target WHERE volume = ?1",
        params![volume],
        |r| {
            Ok(BlockSeat {
                volume: r.get(0)?,
                epoch: r.get(1)?,
                composer: r.get(2)?,
                seated_unix: r.get(3)?,
            })
        },
    )?;
    tx.commit()?;
    Ok(seat)
}

/// The volume's seat, if it has one.
pub fn volume_seat(conn: &Connection, volume: &str) -> Result<Option<BlockSeat>> {
    Ok(conn
        .query_row(
            "SELECT volume, epoch, composer, seated_unix FROM block_volume_target
             WHERE volume = ?1",
            params![volume],
            |r| {
                Ok(BlockSeat {
                    volume: r.get(0)?,
                    epoch: r.get(1)?,
                    composer: r.get(2)?,
                    seated_unix: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// THE RESOLUTION every dial site goes through: volume → seat → dialable
/// coordinates, in ONE read so the pair cannot be torn by a concurrent
/// re-registration.
///
/// Both failure shapes are refusals, never fallbacks. A volume with no
/// seat and a seat naming an unregistered composer are different
/// operator stories and are reported as different errors, but they have
/// the same answer: this MDS does not know where to dial, so it does not
/// dial.
pub fn resolve_volume_target(
    conn: &Connection,
    volume: &str,
) -> Result<(BlockSeat, BlockTargetRow)> {
    let Some(seat) = volume_seat(conn, volume)? else {
        return Err(ExtentAllocError::UnseatedVolume);
    };
    let target = conn
        .query_row(
            "SELECT target_id, traddr, trsvcid, registered_unix, updated_unix
             FROM block_targets WHERE target_id = ?1",
            params![seat.composer],
            |r| {
                let port: i64 = r.get(2)?;
                Ok(BlockTargetRow {
                    target_id: r.get(0)?,
                    traddr: r.get(1)?,
                    trsvcid: port as u16,
                    registered_unix: r.get(3)?,
                    updated_unix: r.get(4)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| ExtentAllocError::UnknownComposer {
            composer: seat.composer.clone(),
        })?;
    Ok((seat, target))
}

// ---------------------------------------------------------------------
// THE SERVING LEASE. FlintComposition tranche 3's finding, and both
// halves of it were forced by counterexamples:
//
//   (a) Renewal is RECORD-CONDITIONED. A deposed composer that recovers
//       and re-arms its own lease leaves the eviction horizon forever in
//       the future, and promotion wedges with every process healthy. The
//       MDS refuses a deposed node's renewal even when that node is
//       perfectly alive: its lapsed horizon must STAY passed.
//
//   (b) ASSEMBLY IS THE LEASE GRANT. A node whose lease lapsed under an
//       EARLIER epoch, later composing leaseless, gets deposed — and the
//       promoter reads that ancient lapse as an already-passed horizon
//       and assembles over a still-serving zombie (Inv_NoStaleServe).
//       So activate-the-composition and grant-the-epoch's-lease are ONE
//       act, and a holder can never take a lease for itself.
//
// Hence the lease names (volume, epoch, holder) and lives in its OWN
// table rather than as columns on the seat. The two have different
// lifetimes on purpose: the CAS moves the seat, and the lease stays with
// the OLD epoch, expiring — that gap IS the eviction horizon, and a
// shared row would collapse it, which is exactly the bug (b) describes.
// ---------------------------------------------------------------------

/// The right to serve a volume's bytes, held by one target, for one
/// composition, until a stated moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLease {
    pub volume: String,
    /// The composition this lease was granted FOR. A lease for epoch 1
    /// licenses nothing at epoch 2.
    pub epoch: i64,
    pub holder: String,
    pub expires_unix: i64,
}

impl BlockLease {
    pub fn is_live_at(&self, now_unix: i64) -> bool {
        now_unix < self.expires_unix
    }
}

/// Grant the lease for a composition — assembly's act, and the ONLY way
/// a lease comes into being. Upsert, because assembly at a new epoch
/// legitimately replaces the previous composition's lease.
///
/// There is deliberately no "take a lease" entry point. A holder asking
/// for one is the shape counterexample (b) is made of.
pub fn lease_grant(
    conn: &mut Connection,
    volume: &str,
    epoch: i64,
    holder: &str,
    expires_unix: i64,
) -> Result<BlockLease> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO block_leases (volume, epoch, holder, expires_unix)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (volume) DO UPDATE SET epoch = ?2, holder = ?3, expires_unix = ?4",
        params![volume, epoch, holder, expires_unix],
    )?;
    tx.commit()?;
    Ok(BlockLease {
        volume: volume.to_string(),
        epoch,
        holder: holder.to_string(),
        expires_unix,
    })
}

/// Extend a standing lease — RECORD-CONDITIONED, which is finding (a).
///
/// Two refusals, and they are different facts about the world:
///   * the seat no longer names this holder — it has been deposed, and
///     is refused however healthy it is;
///   * the seat names it, but the standing lease belongs to another
///     holder or another epoch — it has been ELECTED and not yet
///     assembled, and a lease is granted, never taken.
pub fn lease_renew(
    conn: &mut Connection,
    volume: &str,
    holder: &str,
    expires_unix: i64,
) -> Result<BlockLease> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seat: Option<(i64, String)> = tx
        .query_row(
            "SELECT epoch, composer FROM block_volume_target WHERE volume = ?1",
            params![volume],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((seat_epoch, composer)) = seat else {
        return Err(ExtentAllocError::UnseatedVolume);
    };
    if composer != holder {
        return Err(ExtentAllocError::LeaseRefused {
            reason: format!(
                "'{holder}' is not the composer of '{volume}' — the record seats it at \
                 '{composer}' (epoch {seat_epoch})"
            ),
        });
    }
    let lease: Option<(i64, String)> = tx
        .query_row(
            "SELECT epoch, holder FROM block_leases WHERE volume = ?1",
            params![volume],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match lease {
        Some((e, h)) if e == seat_epoch && h == holder => {}
        Some((e, h)) => {
            return Err(ExtentAllocError::LeaseRefused {
                reason: format!(
                    "the standing lease on '{volume}' belongs to '{h}' at epoch {e}, not to \
                     '{holder}' at epoch {seat_epoch} — assembly grants it, the holder does not \
                     take it"
                ),
            })
        }
        None => {
            return Err(ExtentAllocError::LeaseRefused {
                reason: format!("no lease on '{volume}' — assembly has not granted one"),
            })
        }
    }
    tx.execute(
        "UPDATE block_leases SET expires_unix = ?1 WHERE volume = ?2",
        params![expires_unix, volume],
    )?;
    tx.commit()?;
    Ok(BlockLease {
        volume: volume.to_string(),
        epoch: seat_epoch,
        holder: holder.to_string(),
        expires_unix,
    })
}

/// The standing lease on a volume, whoever holds it. The eviction
/// horizon is read from here: the DEPOSED composer's lease is what must
/// have expired before anything may be torn out from under it.
pub fn lease_get(conn: &Connection, volume: &str) -> Result<Option<BlockLease>> {
    Ok(conn
        .query_row(
            "SELECT volume, epoch, holder, expires_unix FROM block_leases WHERE volume = ?1",
            params![volume],
            |r| {
                Ok(BlockLease {
                    volume: r.get(0)?,
                    epoch: r.get(1)?,
                    holder: r.get(2)?,
                    expires_unix: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Every lease a target holds — the dead-man's work list, and the only
/// honest answer to "what am I entitled to be serving right now".
pub fn leases_held_by(conn: &Connection, holder: &str) -> Result<Vec<BlockLease>> {
    let mut stmt = conn.prepare(
        "SELECT volume, epoch, holder, expires_unix FROM block_leases WHERE holder = ?1
         ORDER BY volume",
    )?;
    let rows = stmt.query_map(params![holder], |r| {
        Ok(BlockLease {
            volume: r.get(0)?,
            epoch: r.get(1)?,
            holder: r.get(2)?,
            expires_unix: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Give up a lease. The dead-man's own act once it has suspended the
/// export: the entitlement is surrendered explicitly rather than left to
/// rot, so nothing downstream has to distinguish "expired" from
/// "expired and acted upon".
pub fn lease_drop(conn: &mut Connection, volume: &str) -> Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let n = tx.execute("DELETE FROM block_leases WHERE volume = ?1", params![volume])?;
    tx.commit()?;
    Ok(n > 0)
}

/// Record a leg's sync state. Upsert — this is the write the degrade
/// barrier and the rebuild will both use, and both legitimately move an
/// existing mark. It is deliberately NOT what `seat_volume` calls.
pub fn leg_mark(
    conn: &mut Connection,
    volume: &str,
    target_id: &str,
    sync_state: &str,
    now_unix: i64,
) -> Result<()> {
    if sync_state != LEG_INSYNC && sync_state != LEG_STALE {
        return Err(ExtentAllocError::InvalidRange("leg sync state"));
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO block_volume_legs (volume, target_id, sync_state, marked_unix)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (volume, target_id) DO UPDATE SET sync_state = ?3, marked_unix = ?4",
        params![volume, target_id, sync_state, now_unix],
    )?;
    tx.commit()?;
    Ok(())
}

/// The volume's legs and their marks.
pub fn legs_for_volume(conn: &Connection, volume: &str) -> Result<Vec<BlockLeg>> {
    let mut stmt = conn.prepare(
        "SELECT volume, target_id, sync_state, marked_unix FROM block_volume_legs
         WHERE volume = ?1 ORDER BY target_id",
    )?;
    let rows = stmt.query_map(params![volume], |r| {
        Ok(BlockLeg {
            volume: r.get(0)?,
            target_id: r.get(1)?,
            sync_state: r.get(2)?,
            marked_unix: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// THE PROMOTION CAS — `FlintComposition`'s `PromoteCAS`, which is the
/// one act that moves a volume from one composer to another.
///
/// The caller supplies what it read (`expected_epoch`, `expected_
/// composer`) and the transaction refuses if the seat has moved since.
/// With one arbiter per shard today there is no peer to race, so the
/// compare is really about ordering a retry against its own earlier
/// attempt — and about being correct on the day there IS a second
/// arbiter, which is the day it stops being cheap to add.
///
/// The guards, in the model's own terms:
///   * `ElectInSync` — the candidate's leg must carry an in-sync mark.
///     Promoting a leg the record already knows is stale discards every
///     acked solo write (`FlintCompositionElectStale.cfg`). The price of
///     refusing is that a degraded volume whose composer then dies WAITS
///     (`FlintCompositionWaitsPrice.cfg`), and that is the trade.
///   * the candidate must be a REGISTERED target — an elected composer
///     nobody can dial is a promotion into a black hole.
///   * the epoch advances by exactly one, monotonically.
///
/// What it deliberately does NOT do: mark the deposed leg stale. That
/// belongs to assembly, and the model is emphatic about the order (CAS →
/// horizon → evict → assemble): between the CAS and assembly the deposed
/// composer may still be acking, and its leg is not yet behind.
pub fn promote_volume(
    conn: &mut Connection,
    volume: &str,
    expected_epoch: i64,
    expected_composer: &str,
    candidate: &str,
    now_unix: i64,
) -> Result<BlockSeat> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seat: Option<BlockSeat> = tx
        .query_row(
            "SELECT volume, epoch, composer, seated_unix FROM block_volume_target
             WHERE volume = ?1",
            params![volume],
            |r| {
                Ok(BlockSeat {
                    volume: r.get(0)?,
                    epoch: r.get(1)?,
                    composer: r.get(2)?,
                    seated_unix: r.get(3)?,
                })
            },
        )
        .optional()?;
    let Some(seat) = seat else {
        return Err(ExtentAllocError::UnseatedVolume);
    };
    if seat.epoch != expected_epoch || seat.composer != expected_composer {
        return Err(ExtentAllocError::PromotionRaced {
            epoch: seat.epoch,
            composer: seat.composer,
        });
    }
    if candidate == seat.composer {
        return Err(ExtentAllocError::SelfPromotion { composer: seat.composer });
    }
    let registered: bool = tx
        .query_row(
            "SELECT 1 FROM block_targets WHERE target_id = ?1",
            params![candidate],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !registered {
        return Err(ExtentAllocError::UnknownComposer {
            composer: candidate.to_string(),
        });
    }
    let insync: bool = tx
        .query_row(
            "SELECT sync_state FROM block_volume_legs WHERE volume = ?1 AND target_id = ?2",
            params![volume, candidate],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|s| s == LEG_INSYNC)
        .unwrap_or(false);
    if !insync {
        return Err(ExtentAllocError::NotInSync {
            candidate: candidate.to_string(),
        });
    }
    // The CAS repeated in the WHERE clause, not because the read above
    // could go stale inside an IMMEDIATE transaction, but so the swap
    // stays a swap if this statement is ever lifted out of one.
    let moved = tx.execute(
        "UPDATE block_volume_target SET epoch = epoch + 1, composer = ?1, seated_unix = ?2
         WHERE volume = ?3 AND epoch = ?4 AND composer = ?5",
        params![candidate, now_unix, volume, expected_epoch, expected_composer],
    )?;
    if moved != 1 {
        return Err(ExtentAllocError::PromotionRaced {
            epoch: seat.epoch,
            composer: seat.composer,
        });
    }
    let promoted = BlockSeat {
        volume: volume.to_string(),
        epoch: expected_epoch + 1,
        composer: candidate.to_string(),
        seated_unix: now_unix,
    };
    tx.commit()?;
    Ok(promoted)
}

/// Every seat, for the startup audit and `BlockExportStatus`.
pub fn seat_list(conn: &Connection) -> Result<Vec<BlockSeat>> {
    let mut stmt = conn.prepare(
        "SELECT volume, epoch, composer, seated_unix FROM block_volume_target ORDER BY volume",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(BlockSeat {
            volume: r.get(0)?,
            epoch: r.get(1)?,
            composer: r.get(2)?,
            seated_unix: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Where a live admission came from. The distinction matters to the
/// roller: an attachment names its node, a client-earned admission
/// cannot (its row is keyed by NFS client id, which is minted long
/// after — and because of — the nvme session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockInitiatorSource {
    /// `block_node_attach` — CSI ControllerPublish.
    NodeAttach,
    /// `block_hosts` — earned at LAYOUTGET.
    ClientEarned,
}

impl BlockInitiatorSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NodeAttach => "attach",
            Self::ClientEarned => "client",
        }
    }
}

/// One live block-layout initiator, as the allow-list tables record it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInitiatorRow {
    pub volume: String,
    pub host_nqn: String,
    /// Empty for `ClientEarned` rows — never read this as "no node".
    pub node_name: String,
    /// 0 for `NodeAttach` rows.
    pub client_id: u64,
    pub source: BlockInitiatorSource,
    pub since_unix: i64,
}

/// Every live initiator this MDS knows about, across every volume — the
/// fact the maintenance roller has no other way to learn (design §11).
///
/// The two tables are reported SEPARATELY rather than unioned by NQN,
/// which `hosts_for_volume` does: the roller wants to name what it is
/// refusing for, and "3 initiators on 2 volumes, one of them
/// client-earned with no node name" is a materially different operator
/// story from a deduped list of strings. Callers that want the allow-list
/// still want `hosts_for_volume`.
///
/// Fenced clients never appear: the fence deletes the `block_hosts` row
/// and the node's `block_node_attach` row in the same transaction. That
/// is the correct reading for the roller too — a fenced client is
/// already cut off at the device, so the tgt restart takes nothing from
/// it that the fence has not already taken.
pub fn list_initiators(conn: &Connection) -> Result<Vec<BlockInitiatorRow>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT volume, host_nqn, node_name, attached_unix FROM block_node_attach
         ORDER BY volume, host_nqn",
    )?;
    let attaches = stmt.query_map([], |r| {
        Ok(BlockInitiatorRow {
            volume: r.get(0)?,
            host_nqn: r.get(1)?,
            node_name: r.get(2)?,
            client_id: 0,
            source: BlockInitiatorSource::NodeAttach,
            since_unix: r.get(3)?,
        })
    })?;
    for row in attaches {
        out.push(row?);
    }
    let mut stmt = conn.prepare(
        "SELECT volume, host_nqn, client_id, admitted_unix FROM block_hosts
         ORDER BY volume, host_nqn",
    )?;
    let earned = stmt.query_map([], |r| {
        let client: i64 = r.get(2)?;
        Ok(BlockInitiatorRow {
            volume: r.get(0)?,
            host_nqn: r.get(1)?,
            node_name: String::new(),
            client_id: client as u64,
            source: BlockInitiatorSource::ClientEarned,
            since_unix: r.get(3)?,
        })
    })?;
    for row in earned {
        out.push(row?);
    }
    Ok(out)
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

/// Every DISTINCT (volume, client_id) pair holding grant rows — the
/// lease sweep's other candidate source (a client can hold rows whose
/// in-memory layout handle died with a previous MDS incarnation, so
/// enumerating layout owners alone would miss it). Served by
/// idx_grants_client.
pub fn grant_clients(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT volume, client_id FROM extent_grants
         ORDER BY volume, client_id",
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

/// The lease sweep's bulk return: delete EVERY grant row the client
/// holds on the volume — read and rw, fenced included — as if the
/// client had returned each layout, which it never will (its lease
/// expired). Extents are deliberately untouched, mirroring
/// `layout_return`: provisional rows become re-grantable orphans,
/// committed rows are the file's data; freeing stays reclaim's job.
/// Touched files get the windowed merge (the rows just became
/// quiescent — a dead writer's file coalesces here instead of never).
///
/// GATED, in-transaction, on a CONFIRMED fence (`fenced_clients` row
/// with `delivered_unix > 0`): the fenced grant rows are the very
/// bookkeeping that makes an unconfirmed-fence client's extents
/// quarantine rather than free — deleting them without the target-side
/// exclusion proven would be LostFence's corruption through a side
/// door. `UnconfirmedFence` tells the sweep to retry after the next
/// fence attempt confirms. Idempotent: a revoked client has no rows,
/// and the delete of nothing is 0.
pub fn revoke_client(conn: &mut Connection, volume: &str, client_id: u64) -> Result<u64> {
    let client = as_i64(client_id, "client id exceeds i64")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let delivered: Option<i64> = tx
        .query_row(
            "SELECT delivered_unix FROM fenced_clients
             WHERE volume = ?1 AND client_id = ?2",
            params![volume, client],
            |r| r.get(0),
        )
        .optional()?;
    if !matches!(delivered, Some(d) if d > 0) {
        return Err(ExtentAllocError::UnconfirmedFence);
    }

    // The windows to merge (file_id, min_off, max_end), captured before
    // the rows go.
    let files: Vec<(i64, i64, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT g.file_id, MIN(g.logical_offset), MAX(e.logical_offset + e.length)
             FROM extent_grants g JOIN extents e
               ON e.volume = g.volume AND e.file_id = g.file_id
              AND e.logical_offset = g.logical_offset
             WHERE g.volume = ?1 AND g.client_id = ?2
             GROUP BY g.file_id",
        )?;
        let rows = stmt
            .query_map(params![volume, client], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let removed = tx.execute(
        "DELETE FROM extent_grants WHERE volume = ?1 AND client_id = ?2",
        params![volume, client],
    )? as u64;
    for (file_id, min_off, max_end) in files {
        merge_extents_window(&tx, volume, file_id, min_off, max_end)?;
    }
    tx.commit()?;
    Ok(removed)
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
    // Client-earned admissions ∪ node-level attaches — UNION dedups, so a
    // node that both attached and earned a LAYOUTGET admission appears
    // once. Either row alone keeps the NQN desired.
    let mut stmt = conn.prepare(
        "SELECT host_nqn FROM block_hosts WHERE volume = ?1
         UNION
         SELECT host_nqn FROM block_node_attach WHERE volume = ?1
         ORDER BY host_nqn",
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

    /// THE SWEEP, end to end: a range parked by an UNCONFIRMED fence is
    /// released the moment that same fence is confirmed — and not one
    /// step sooner. This is the whole point of the tranche: before it,
    /// the range leaked until an operator ran the whole-volume lever.
    ///
    /// TLC trace: FlintExtentsProbeQuarantineRelease.cfg is the
    /// non-vacuity witness that this sequence is reachable at all;
    /// FlintExtentsQuarantineBlindRelease.cfg is the world where the
    /// sweep skips the delivered re-check and hands the range back while
    /// the client is still writing to it.
    #[test]
    fn the_sweep_releases_a_parked_range_only_once_its_fence_confirms() {
        let mut conn = setup();
        let g1 = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        let phys1 = g1[0].physical_offset;
        fence_record(&mut conn, VOL, C1, 500).unwrap();
        fence_client(&mut conn, VOL, C1).unwrap();
        // The preempt did NOT confirm, so the reclaim parks the range.
        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();
        assert_eq!(out.quarantined_extents, 1);
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));

        // Sweeping now must free NOTHING: the exclusion is still unproven,
        // and freeing here is LostFence's corruption exactly.
        assert_eq!(sweep_quarantine_delivered(&mut conn, VOL).unwrap(), (0, 0));
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));

        // The reconcile pass's preempt retry lands and marks it delivered.
        assert!(mark_fence_delivered(&mut conn, VOL, C1, 900).unwrap());
        assert_eq!(sweep_quarantine_delivered(&mut conn, VOL).unwrap(), (1, 8192));
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (0, 0));

        // ...and the released range is a REUSE, with everything a reuse owes.
        let g2 = grant(&mut conn, VOL, F, C2, 32768, 8192, false).unwrap();
        assert_eq!(g2[0].physical_offset, phys1, "swept range is first-fit reused");
        assert_eq!(g2[0].generation, 2, "reuse after a sweep still bumps the generation");
        assert!(g2[0].needs_scrub, "a swept range still carries the prior incarnation's bytes");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// The sweep reads the range's OWN provenance — the client ids it was
    /// parked with — never the live tables. A second client's confirmed
    /// fence says nothing about the range C1 was parked for, and a sweep
    /// that looked at "whoever is fenced now" would free it.
    ///
    /// TLC found this on the tranche's first draft: a release gated on
    /// the current holders frees blocks that were never quarantined,
    /// skipping the recall entirely.
    #[test]
    fn the_sweep_checks_the_range_provenance_not_whoever_is_fenced_now() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        fence_record(&mut conn, VOL, C1, 500).unwrap();
        fence_client(&mut conn, VOL, C1).unwrap();
        reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));

        // A DIFFERENT client is fenced and confirmed. C1's range is not
        // its business, and the parked range must not move.
        fence_record(&mut conn, VOL, C2, 600).unwrap();
        assert!(mark_fence_delivered(&mut conn, VOL, C2, 700).unwrap());
        assert_eq!(sweep_quarantine_delivered(&mut conn, VOL).unwrap(), (0, 0));
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// An UNFENCED client leaves no fence record, and the sweep reads
    /// that absence as UNDELIVERED — the COALESCE-0 discipline
    /// `reclaim_complete` already applies. Reading "no row" as "nobody
    /// left to wait for" would free precisely the ranges whose exclusion
    /// was deliberately released, which is the corruption with an extra
    /// step.
    #[test]
    fn unfencing_a_client_does_not_make_its_parked_ranges_sweepable() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        fence_record(&mut conn, VOL, C1, 500).unwrap();
        fence_client(&mut conn, VOL, C1).unwrap();
        reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));

        assert!(unfence_record(&mut conn, VOL, C1).unwrap(), "the record is gone");
        assert_eq!(
            sweep_quarantine_delivered(&mut conn, VOL).unwrap(),
            (0, 0),
            "an absent fence record is UNDELIVERED, not 'nothing to wait for'"
        );
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (1, 8192));
        // The operator lever is still the way out — deliberately manual.
        assert_eq!(release_quarantine(&mut conn, VOL).unwrap(), 8192);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// A range parked for TWO clients frees only when BOTH are
    /// confirmed. One delivered fence out of two is a range one client
    /// may still be writing to. (Second holder row manufactured directly,
    /// as in `a_shared_extent_frees_only_when_every_fence_is_delivered` —
    /// the live multi-holder path is the READ-grant tiling, whose
    /// ceremony would obscure what this pins.)
    #[test]
    fn a_range_parked_for_two_clients_needs_both_confirmed() {
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
            fence_record(&mut conn, VOL, c, 500).unwrap();
            fence_client(&mut conn, VOL, c).unwrap();
        }
        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();
        assert_eq!(out.quarantined_extents, 1);

        assert!(mark_fence_delivered(&mut conn, VOL, C1, 900).unwrap());
        assert_eq!(
            sweep_quarantine_delivered(&mut conn, VOL).unwrap(),
            (0, 0),
            "one of two confirmed is not quiescence"
        );
        assert!(mark_fence_delivered(&mut conn, VOL, C2, 950).unwrap());
        assert_eq!(sweep_quarantine_delivered(&mut conn, VOL).unwrap(), (1, 8192));
        assert_eq!(quarantine_stats(&conn, VOL).unwrap(), (0, 0));
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// A parked range is invisible to the allocator — and it is invisible
    /// because it LEFT the extents table for a third one, not because of
    /// a check. That structure is what
    /// FlintExtentsQuarantineVisible.cfg pins: keep the extent row and
    /// flag it, and the parked range looks exactly like an ORPHAN
    /// (allocated, no live holder — the quarantine branch swept the
    /// grant rows itself), the grant path re-hands it out at its old
    /// generation, and the sweep then frees it under the new owner's
    /// live grant. TLC finds that in nine states.
    #[test]
    fn a_parked_range_is_in_no_table_the_allocator_reads() {
        let mut conn = setup();
        let g1 = grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        let phys1 = g1[0].physical_offset;
        fence_record(&mut conn, VOL, C1, 500).unwrap();
        fence_client(&mut conn, VOL, C1).unwrap();
        reclaim_complete(&mut conn, VOL, F, 0, 8192, 1234).unwrap();

        // Not an extent: the row is gone, so nothing can re-grant it.
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1 AND physical_offset = ?2",
                params![VOL, phys1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "a parked range must not keep its extents row");
        // Not free either: nothing can allocate it.
        let free_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extent_free WHERE volume = ?1 AND physical_offset = ?2",
                params![VOL, phys1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(free_rows, 0, "a parked range must not be in the free list");
        // ...which is what makes this hold, through both doors at once.
        let g2 = grant(&mut conn, VOL, F, C2, 0, 8192, false).unwrap();
        assert_ne!(g2[0].physical_offset, phys1, "the parked range was re-granted");
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

    /// THE LEASE SWEEP's transaction: revoke refuses without a
    /// CONFIRMED fence, then bulk-returns every row the client held
    /// (read and rw), merges the dead writer's file, and replays as a
    /// no-op.
    #[test]
    fn revoke_requires_a_delivered_fence_then_bulk_returns_and_merges() {
        let mut conn = setup();
        // The dead writer: two contiguous provisional extents plus a
        // committed one. (A read grant on its own rw extent would be
        // the same PK row — mode is not keyed — so three rows is the
        // client's whole footprint.)
        for i in 0..3u64 {
            grant(&mut conn, VOL, F, C1, i * 8192, 8192, false).unwrap();
        }
        commit_extents(&mut conn, VOL, F, C1, 2 * 8192, 8192).unwrap();

        // No fence at all → refused.
        match revoke_client(&mut conn, VOL, C1) {
            Err(ExtentAllocError::UnconfirmedFence) => {}
            other => panic!("expected UnconfirmedFence, got {other:?}"),
        }
        // Fenced but UNCONFIRMED → still refused.
        fence_record(&mut conn, VOL, C1, 100).unwrap();
        fence_client(&mut conn, VOL, C1).unwrap();
        match revoke_client(&mut conn, VOL, C1) {
            Err(ExtentAllocError::UnconfirmedFence) => {}
            other => panic!("expected UnconfirmedFence, got {other:?}"),
        }

        // Confirmed → the bulk return: every row goes (3 rw + 1 read),
        // and the two contiguous same-state provisional extents merge.
        assert!(mark_fence_delivered(&mut conn, VOL, C1, 200).unwrap());
        let removed = revoke_client(&mut conn, VOL, C1).unwrap();
        assert_eq!(removed, 3, "every row the client held");
        let grants_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extent_grants WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(grants_left, 0);
        let extents_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            extents_left, 2,
            "two provisional rows merged to one; the committed row stays apart (state boundary)"
        );
        // Extents were NOT freed — the return shape, not the reclaim
        // shape: the committed row is file data, the merged provisional
        // row is a re-grantable orphan.
        let (free_rows,): (i64,) = conn
            .query_row(
                "SELECT COUNT(*) FROM extent_free WHERE volume = ?1",
                params![VOL],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_eq!(free_rows, 0, "revoke returns, never frees");
        // Replay: nothing left, still Ok(0).
        assert_eq!(revoke_client(&mut conn, VOL, C1).unwrap(), 0);
        // The orphan is genuinely re-grantable by a successor client.
        grant(&mut conn, VOL, F, C2, 0, 2 * 8192, false).expect("orphan re-granted");
        verify_volume_invariants(&conn, VOL).unwrap();
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

    /// THE COMMIT-GRACE TRANCHE — rig-found 2026-08-11, and it was live
    /// data loss.
    ///
    /// The Linux client writing through 1 MiB grant windows does
    /// LAYOUTRETURN and only then LAYOUTCOMMIT, on every window. With the
    /// commit validated against a LIVE grant row, half the commits were
    /// refused with "no grant for this client", the extents stayed
    /// `invalid`, the stub's size never advanced — the drill's 8 MiB file
    /// was durably 4 MiB. Returning is not disowning what you already
    /// wrote.
    #[test]
    fn a_client_may_commit_the_range_it_just_returned() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        assert_eq!(layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap(), 1);
        // The shape that used to fail: commit AFTER the return.
        assert_eq!(commit_extents(&mut conn, VOL, F, C1, 0, 8192).unwrap(), 1);
        let state: String = conn
            .query_row(
                "SELECT state FROM extents WHERE volume = ?1 AND file_id = ?2",
                params![VOL, F as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "rw", "the returned client's bytes must commit");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// …and the belt that makes the grace door safe is the GENERATION,
    /// not the row's liveness. Once the range is freed and re-granted,
    /// the old holder's grace record names a generation the extent no
    /// longer carries, and its commit refuses — the same answer a stale
    /// LIVE grant would get. This is the property that lets grace rows be
    /// pruned lazily instead of exactly.
    #[test]
    fn commit_grace_refuses_once_the_range_has_been_reused() {
        let mut conn = fresh();
        register_volume(&mut conn, VOL, 65536).unwrap();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        // The range is reclaimed and handed to somebody else at gen+1…
        reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000).unwrap();
        let g = grant(&mut conn, VOL, F, C2, 0, 8192, false).unwrap();
        assert_eq!(g[0].generation, 2, "reuse bumps the generation");
        // …and C1's commit refuses. (The reclaim also PRUNED its grace
        // row, so the refusal names the missing record; the test below
        // proves the generation belt independently, which is what makes
        // that pruning hygiene rather than load-bearing.)
        assert!(
            commit_extents(&mut conn, VOL, F, C1, 0, 8192).is_err(),
            "the previous owner must not commit a reused range"
        );
        let state: String = conn
            .query_row(
                "SELECT state FROM extents WHERE volume = ?1 AND file_id = ?2",
                params![VOL, F as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "invalid", "the new owner's extent was not promoted by the old one");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// The rig's actual shape, which is why the grace lookup is
    /// WINDOWED: the client takes a grant per 1 MiB window, returns, and
    /// LAYOUTRETURN merges the adjacent quiescent extents in the same
    /// transaction — so by commit time ONE extent row covers several
    /// grace rows. An exact-offset lookup would find nothing for the
    /// merged row and refuse the very commit this fix exists to allow.
    #[test]
    fn commit_grace_survives_the_merge_that_layout_return_performs() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 4096, false).unwrap();
        grant(&mut conn, VOL, F, C1, 4096, 4096, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1 AND file_id = ?2",
                params![VOL, F as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "the return merged the two extents (else this proves nothing)");
        assert_eq!(
            commit_extents(&mut conn, VOL, F, C1, 0, 8192).unwrap(),
            1,
            "the merged extent must still be committable by the client that wrote it"
        );
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// The generation belt, tested WITHOUT relying on pruning: a grace
    /// record that outlives a generation bump (any path that reshapes an
    /// extent without deleting it — merge, split, a future reclaim
    /// variant) must refuse on the generation alone. This is the
    /// assertion that lets the cleanup elsewhere be best-effort.
    #[test]
    fn a_stale_grace_record_refuses_on_the_generation_alone() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        // Simulate the record outliving its generation: the extent moves
        // to gen 2 while C1's grace row still says gen 1.
        conn.execute(
            "UPDATE extents SET gen = 2 WHERE volume = ?1 AND file_id = ?2",
            params![VOL, F as i64],
        )
        .unwrap();
        match commit_extents(&mut conn, VOL, F, C1, 0, 8192) {
            Err(ExtentAllocError::CommitRejected(r)) => assert_eq!(r, "generation mismatch"),
            other => panic!("expected a generation mismatch, got {other:?}"),
        }
    }

    /// A FENCED client must not walk through the grace door. Two arms,
    /// because there are two ways in: a fence lands while its rows are
    /// live (the row is marked, and the fence drops any earlier grace
    /// record), and a fenced row that is later swept must leave no grace
    /// behind. A half-excluded client is not excluded.
    #[test]
    fn a_fenced_client_cannot_commit_through_the_grace_door() {
        // Arm A: returned first (grace exists), fenced afterwards.
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        fence_client(&mut conn, VOL, C1).unwrap();
        match commit_extents(&mut conn, VOL, F, C1, 0, 8192) {
            Err(ExtentAllocError::CommitRejected(r)) => assert_eq!(r, "no grant for this client"),
            other => panic!("fenced client committed through grace: {other:?}"),
        }

        // Arm B: fenced while holding, then its rows are returned. The
        // return must not mint a grace row for a fenced grant.
        let mut conn = setup();
        grant(&mut conn, VOL, F, C2, 0, 8192, false).unwrap();
        fence_client(&mut conn, VOL, C2).unwrap();
        layout_return(&mut conn, VOL, F, C2, 0, 8192).unwrap();
        match commit_extents(&mut conn, VOL, F, C2, 0, 8192) {
            Err(ExtentAllocError::CommitRejected(r)) => assert_eq!(r, "no grant for this client"),
            other => panic!("fenced-then-returned client committed through grace: {other:?}"),
        }
    }

    /// Grace is NOT holdership — the property the whole design rests on.
    /// A returned client leaves a grace record behind, and the reclaim
    /// must still see an unheld range and free it (FreeRevalidates reads
    /// `extent_grants` alone). If grace ever leaked into a holder query,
    /// this frees nothing and the volume leaks forever.
    #[test]
    fn a_grace_record_does_not_block_the_reclaim() {
        let mut conn = setup();
        grant(&mut conn, VOL, F, C1, 0, 8192, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        assert!(
            reclaim_snapshot(&mut conn, VOL, F, 0, 8192).unwrap().is_empty(),
            "a returned client is not a holder, grace record or not"
        );
        let out = reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000).unwrap();
        assert_eq!(out.freed_extents, 1);
        assert_eq!(out.quarantined_extents, 0);
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// THE EXPAND PATH, end to end at this layer: a volume that has hit
    /// its ceiling grants again after the ceiling rises. Before this
    /// existed, CSI expand acked a resize that raised nothing — the PVC
    /// reported the new size and every LAYOUTGET past the old ceiling
    /// still answered NoSpace (design doc §7).
    #[test]
    fn expand_raises_the_ceiling_and_unblocks_the_next_grant() {
        let mut conn = fresh();
        register_volume(&mut conn, VOL, 16384).unwrap();
        grant(&mut conn, VOL, F, C1, 0, 16384, false).unwrap();
        assert!(matches!(
            grant(&mut conn, VOL, F, C1, 16384, 4096, false),
            Err(ExtentAllocError::NoSpace { .. })
        ));
        assert_eq!(volume_headroom(&conn, VOL).unwrap(), 0, "arena exhausted");

        assert_eq!(expand_volume(&mut conn, VOL, 65536).unwrap(), 65536);
        assert_eq!(volume_headroom(&conn, VOL).unwrap(), 65536 - 16384);
        let g = grant(&mut conn, VOL, F, C1, 16384, 4096, false).unwrap();
        assert_eq!(g[0].physical_offset, 16384, "bump-allocated into the new room");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// external-resizer re-drives ExpandVolume freely, and a PVC edited
    /// back down must not be answered with an error that wedges it in
    /// `Resizing`: at-or-below the ceiling in force is a no-op that
    /// reports the truth.
    #[test]
    fn expand_is_idempotent_and_never_shrinks() {
        let mut conn = fresh();
        register_volume(&mut conn, VOL, 65536).unwrap();
        assert_eq!(expand_volume(&mut conn, VOL, 65536).unwrap(), 65536, "no-op");
        assert_eq!(expand_volume(&mut conn, VOL, 4096).unwrap(), 65536, "never shrinks");
        assert_eq!(expand_volume(&mut conn, VOL, 1 << 20).unwrap(), 1 << 20);
        assert_eq!(expand_volume(&mut conn, VOL, 1 << 20).unwrap(), 1 << 20, "replay");
        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// A volume with no arena is not a block volume. Reporting success
    /// for a ceiling that does not exist is exactly the lie the whole
    /// change is removing.
    #[test]
    fn expand_refuses_a_volume_with_no_arena() {
        let mut conn = fresh();
        match expand_volume(&mut conn, "never-registered", 1 << 20) {
            Err(ExtentAllocError::InvalidRange(m)) => assert!(m.contains("no extent arena")),
            other => panic!("expected InvalidRange, got {other:?}"),
        }
    }

    /// Headroom is the BUMP REGION only. The production grant path runs
    /// `fresh_only`, so free-list bytes cannot be handed out — counting
    /// them would tell the ENOSPC belt a full volume is healthy, and the
    /// app would get EIO where ENOSPC is the truth.
    #[test]
    fn headroom_ignores_the_free_list_because_grants_are_fresh_only() {
        let mut conn = fresh();
        register_volume(&mut conn, VOL, 16384).unwrap();
        grant(&mut conn, VOL, F, C1, 0, 16384, false).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, 8192).unwrap();
        reclaim_complete(&mut conn, VOL, F, 0, 8192, 1000).unwrap();
        assert_eq!(
            volume_headroom(&conn, VOL).unwrap(),
            0,
            "8 KiB sits on the free list, but fresh_only cannot reach it"
        );
        // And the allocator agrees: a fresh_only grant still refuses.
        assert!(matches!(
            grant(&mut conn, VOL, F, C1, 16384, 4096, true),
            Err(ExtentAllocError::NoSpace { .. })
        ));
    }

    /// An unregistered volume must not read as "full" — the belt's other
    /// arms own non-block files, and a bogus zero here would turn every
    /// file-layout fallback write into ENOSPC.
    #[test]
    fn headroom_of_an_unknown_volume_is_not_zero() {
        let conn = fresh();
        assert_eq!(volume_headroom(&conn, "not-a-block-volume").unwrap(), u64::MAX);
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

    /// A SHRINKING TRUNCATE MUST NOT TAKE THE BYTES IT KEEPS.
    ///
    /// `note_truncate` turns `ftruncate(fd, n)` into
    /// `reclaim_complete(.., n, i64::MAX - n)`, and `overlapping_extents`
    /// matches on PARTIAL overlap — so a row that STRADDLES `n` is a
    /// target, and the free/DELETE below it took the whole row. The
    /// prefix `[row_start, n)` is committed data the syscall promises to
    /// keep, and the client read zeros back with no error anywhere.
    ///
    /// This is the common case, not an edge: `merge_extents_window`
    /// collapses a sequentially-written file into ONE row, so truncating
    /// such a file to any non-zero size dropped ALL of it.
    ///
    /// THE ORACLE HAS TO BE SURVIVAL. `physical_space_stays_disjoint_
    /// under_churn` above already truncates at non-zero offsets and
    /// passes — because it asks whether physical space stays disjoint,
    /// and freeing a whole row satisfies that perfectly. An invariant
    /// checker cannot see data loss; only reading the bytes back can.
    #[test]
    fn a_shrinking_truncate_keeps_the_prefix_it_promised_to_keep() {
        let mut conn = setup();
        const WHOLE: u64 = 64 * 1024;
        const KEEP: u64 = 32 * 1024;

        // One contiguous committed extent spanning the truncation point.
        let granted = grant(&mut conn, VOL, F, C1, 0, WHOLE, true).unwrap();
        assert!(!granted.is_empty(), "nothing granted, test would be vacuous");
        let phys0 = granted[0].physical_offset;
        commit_extents(&mut conn, VOL, F, C1, 0, WHOLE).unwrap();
        // Return it, or the reclaim refuses NotQuiescent and this test
        // would pass without ever reaching the free.
        layout_return(&mut conn, VOL, F, C1, 0, WHOLE).unwrap();

        // Exactly what note_truncate does for ftruncate(fd, KEEP).
        reclaim_complete(&mut conn, VOL, F, KEEP, (i64::MAX as u64) - KEEP, 0).unwrap();

        // THE ASSERTION: the kept prefix is still mapped, at the same
        // physical bytes. Before the clip this comes back empty.
        let kept = overlapping_extents(&conn, VOL, F, 0, KEEP as i64).unwrap();
        assert!(
            !kept.is_empty(),
            "ftruncate to {KEEP} destroyed the prefix it was asked to keep"
        );
        assert_eq!(kept[0].logical_offset, 0, "prefix moved: {kept:?}");
        assert_eq!(
            kept[0].physical_offset, phys0 as i64,
            "prefix repointed at different physical bytes: {kept:?}"
        );
        assert_eq!(
            kept.iter().map(|r| r.length).sum::<i64>(),
            KEEP as i64,
            "prefix is short or long: {kept:?}"
        );

        // And the tail really went: nothing may remain at or beyond KEEP.
        let beyond = overlapping_extents(&conn, VOL, F, KEEP as i64, i64::MAX).unwrap();
        assert!(beyond.is_empty(), "tail survived the truncate: {beyond:?}");

        verify_volume_invariants(&conn, VOL).unwrap();
    }

    /// The other two shapes of a partial cut, so the clip is not merely
    /// "correct for truncate".
    ///
    /// A range reclaimed from the MIDDLE of a row must leave BOTH sides
    /// mapped, each at its own original physical bytes. That shape is
    /// NOT reachable from production today — `reclaim_scsi_extents` is
    /// the only caller and always reclaims to i64::MAX — so this covers
    /// the branch a hole-punch (DEALLOCATE) would be the first to use,
    /// before it is the first to find it broken.
    #[test]
    fn reclaiming_the_middle_of_an_extent_keeps_both_sides() {
        let mut conn = setup();
        const WHOLE: u64 = 64 * 1024;
        let granted = grant(&mut conn, VOL, F, C1, 0, WHOLE, true).unwrap();
        let phys0 = granted[0].physical_offset as i64;
        commit_extents(&mut conn, VOL, F, C1, 0, WHOLE).unwrap();
        layout_return(&mut conn, VOL, F, C1, 0, WHOLE).unwrap();

        // Punch out [16K, 48K), keeping [0,16K) and [48K,64K).
        let out = reclaim_complete(&mut conn, VOL, F, 16 * 1024, 32 * 1024, 0).unwrap();
        assert_eq!(out.freed_bytes, 32 * 1024, "freed the hole, not the row");

        let head = overlapping_extents(&conn, VOL, F, 0, 16 * 1024).unwrap();
        assert_eq!(head.len(), 1, "head lost: {head:?}");
        assert_eq!((head[0].logical_offset, head[0].length), (0, 16 * 1024));
        assert_eq!(head[0].physical_offset, phys0, "head repointed");

        let tail = overlapping_extents(&conn, VOL, F, 48 * 1024, 64 * 1024).unwrap();
        assert_eq!(tail.len(), 1, "tail lost: {tail:?}");
        assert_eq!((tail[0].logical_offset, tail[0].length), (48 * 1024, 16 * 1024));
        // Physical must track the LOGICAL displacement within the row,
        // or the tail reads someone else's bytes.
        assert_eq!(tail[0].physical_offset, phys0 + 48 * 1024, "tail repointed");

        // The hole is really gone.
        assert!(overlapping_extents(&conn, VOL, F, 16 * 1024, 48 * 1024)
            .unwrap()
            .is_empty());

        // And the row budget counted the split (1 row became 2), not a
        // bare removal — drift here only surfaces in the full verifier.
        let rows: i64 = conn
            .query_row(
                "SELECT extent_rows FROM volume_alloc WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        let actual: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extents WHERE volume = ?1",
                params![VOL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, actual, "extent_rows drifted from COUNT(*)");
        verify_volume_invariants(&conn, VOL).unwrap();
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

    /// Node-level attach (ControllerPublish): shapes the desired list on
    /// its own, unions with client-earned rows, detaches idempotently,
    /// and detach never touches a client-earned admission.
    #[test]
    fn node_attach_and_detach_shape_the_desired_list() {
        let mut conn = setup();
        let h1 = "nqn.2024-11.com.flint:node:a".to_string();
        let h2 = "nqn.2024-11.com.flint:node:b".to_string();

        assert_eq!(node_attach(&mut conn, VOL, &h1, "node-a", 0).unwrap(), vec![h1.clone()]);
        assert_eq!(
            node_attach(&mut conn, VOL, &h1, "node-a", 5).unwrap(),
            vec![h1.clone()],
            "re-attach is idempotent"
        );

        // The same NQN earns a client admission too (its LAYOUTGET) —
        // the union stays DISTINCT.
        host_admit(&mut conn, VOL, C1, &h1, 0).unwrap();
        assert_eq!(hosts_for_volume(&conn, VOL).unwrap(), vec![h1.clone()]);

        // Detach withdraws only the node-level grant; the client-earned
        // row keeps the NQN desired.
        let (removed, remaining) = node_detach(&mut conn, VOL, &h1).unwrap();
        assert!(removed);
        assert_eq!(remaining, vec![h1.clone()], "client admission keeps the NQN");
        let (removed, _) = node_detach(&mut conn, VOL, &h1).unwrap();
        assert!(!removed, "double-detach is a clean replay");

        // A second node's attach stands alone after the first's detach.
        node_attach(&mut conn, VOL, &h2, "node-b", 0).unwrap();
        host_evict(&mut conn, VOL, C1).unwrap();
        assert_eq!(hosts_for_volume(&conn, VOL).unwrap(), vec![h2]);
    }

    /// The roller's read: both admission tables, across volumes, each
    /// row keeping the provenance the roller needs — an attach names
    /// its node, a client-earned admission cannot, and the same NQN
    /// holding both appears TWICE (unlike `hosts_for_volume`, which
    /// dedups for the allow-list). Two rows for one NQN is the honest
    /// answer to "who is connected": two independent admissions, either
    /// of which alone keeps the client on the export.
    #[test]
    fn list_initiators_reports_both_tables_with_their_provenance() {
        let mut conn = setup();
        let h1 = "nqn.2024-11.com.flint:node:a".to_string();
        let h2 = "nqn.2024-11.com.flint:node:b".to_string();
        assert!(list_initiators(&conn).unwrap().is_empty(), "nothing admitted yet");

        node_attach(&mut conn, VOL, &h1, "node-a", 11).unwrap();
        node_attach(&mut conn, "vol-other", &h2, "node-b", 12).unwrap();
        host_admit(&mut conn, VOL, C1, &h1, 13).unwrap();

        let rows = list_initiators(&conn).unwrap();
        assert_eq!(rows.len(), 3, "{rows:?}");
        let attach: Vec<_> = rows
            .iter()
            .filter(|r| r.source == BlockInitiatorSource::NodeAttach)
            .collect();
        assert_eq!(attach.len(), 2);
        assert!(attach.iter().all(|r| !r.node_name.is_empty() && r.client_id == 0));
        assert_eq!(
            attach.iter().map(|r| r.volume.as_str()).collect::<Vec<_>>(),
            vec![VOL, "vol-other"],
            "cross-volume by design — one node's tgt serves them all"
        );
        let earned: Vec<_> = rows
            .iter()
            .filter(|r| r.source == BlockInitiatorSource::ClientEarned)
            .collect();
        assert_eq!(earned.len(), 1);
        assert_eq!(earned[0].client_id, C1);
        assert_eq!(earned[0].since_unix, 13);
        assert!(
            earned[0].node_name.is_empty(),
            "the client-earned row has no node to name, and must not invent one"
        );
    }

    /// A fenced client is not an initiator. It is already cut off at the
    /// device, so a tgt restart takes nothing from it that the fence has
    /// not — and counting it would refuse the roll of a node whose only
    /// "client" is one we deliberately evicted.
    #[test]
    fn a_fenced_client_is_absent_from_the_initiator_list() {
        let mut conn = setup();
        let nqn = "nqn.2024-11.com.flint:node:a".to_string();
        node_attach(&mut conn, VOL, &nqn, "node-a", 1).unwrap();
        host_admit(&mut conn, VOL, C1, &nqn, 2).unwrap();
        assert_eq!(list_initiators(&conn).unwrap().len(), 2);

        fence_record(&mut conn, VOL, C1, 100).unwrap();
        host_evict(&mut conn, VOL, C1).unwrap();
        assert!(
            list_initiators(&conn).unwrap().is_empty(),
            "the fence deletes the client row AND the node's attach row"
        );
    }

    /// The attach-side fence guard: a fence record naming the NQN
    /// refuses node_attach outright. Attach is the one admission door
    /// the per-client `is_fenced` guard cannot see (the attaching node
    /// has no client_id yet), so it carries its own.
    #[test]
    fn a_fenced_nqn_cannot_node_attach() {
        let mut conn = setup();
        let nqn = "nqn.2024-11.com.flint:node:a".to_string();
        host_admit(&mut conn, VOL, C1, &nqn, 0).unwrap();
        fence_record(&mut conn, VOL, C1, 100).unwrap();

        assert!(
            matches!(
                node_attach(&mut conn, VOL, &nqn, "node-a", 0),
                Err(ExtentAllocError::FencedClient)
            ),
            "attach must not re-admit a fenced NQN through the side door"
        );
        // A different node attaches fine; and once the fence clears,
        // the refused node does too.
        node_attach(&mut conn, VOL, "nqn.2024-11.com.flint:node:b", "node-b", 0).unwrap();
        unfence_record(&mut conn, VOL, C1).unwrap();
        node_attach(&mut conn, VOL, &nqn, "node-a", 0).unwrap();
    }

    /// The fence's eviction purges the fenced NQN's attach row in the
    /// same transaction — an attach row surviving the fence would keep
    /// the NQN on the allow-list and the fenced node could reconnect
    /// (the rig's R4 assertion, defeated durably).
    #[test]
    fn host_evict_purges_the_attach_row_too() {
        let mut conn = setup();
        let nqn = "nqn.2024-11.com.flint:node:a".to_string();
        node_attach(&mut conn, VOL, &nqn, "node-a", 0).unwrap();
        host_admit(&mut conn, VOL, C1, &nqn, 0).unwrap();

        let (evicted, remaining) = host_evict(&mut conn, VOL, C1).unwrap();
        assert_eq!(evicted, vec![nqn.clone()]);
        assert!(
            remaining.is_empty(),
            "the attach row must not keep the fenced NQN desired: {remaining:?}"
        );
        let (removed, _) = node_detach(&mut conn, VOL, &nqn).unwrap();
        assert!(!removed, "the eviction already swept the attach row");
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
        node_attach(&mut conn, VOL, "nqn.2024-11.com.flint:node:b", "node-b", 0).unwrap();
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

    /// A target's coordinates are its PRESENT, so re-registration
    /// overwrites them — that is the mechanism by which a target
    /// returning on a new address stops being dialed at the old one.
    /// Its identity and its first-seen stamp are not rewritten: same
    /// target, moved.
    #[test]
    fn target_registration_moves_the_address_and_keeps_the_identity() {
        let mut conn = fresh();
        target_register(&mut conn, "node-a", "10.0.0.9", 4420, 100).unwrap();
        target_register(&mut conn, "node-a", "10.0.0.42", 4421, 900).unwrap();

        let all = target_list(&conn).unwrap();
        assert_eq!(all.len(), 1, "re-registration is not a second target");
        assert_eq!(all[0].traddr, "10.0.0.42");
        assert_eq!(all[0].trsvcid, 4421);
        assert_eq!(all[0].registered_unix, 100, "first-seen survives the move");
        assert_eq!(all[0].updated_unix, 900);
    }

    /// Seating is INSERT-if-absent, and the distinction matters: a
    /// second target calling `seat_volume` on a volume that is already
    /// someone else's gets the STANDING seat back, not its own claim.
    /// Moving a seat is promotion's job, and promotion is a CAS this
    /// tranche does not ship — so nothing here may move one.
    #[test]
    fn a_seat_is_never_taken_by_a_second_claimant() {
        let mut conn = fresh();
        let first = seat_volume(&mut conn, VOL, "node-a", 100, 100 + 120).unwrap();
        assert_eq!(first.composer, "node-a");
        assert_eq!(first.epoch, 1, "epochs start at 1 and only promotion moves them");

        let second = seat_volume(&mut conn, VOL, "node-b", 200, 200 + 120).unwrap();
        assert_eq!(second.composer, "node-a", "the standing seat comes back");
        assert_eq!(second.seated_unix, 100, "and it was not rewritten");
    }

    /// Resolution has two distinct failure shapes and NO third
    /// (fall-back-to-something) shape. They are different operator
    /// stories — nobody has seated this volume vs. its composer has
    /// never announced itself — so they are different errors.
    #[test]
    fn resolution_refuses_unseated_and_unregistered_separately() {
        let mut conn = fresh();
        assert!(
            matches!(
                resolve_volume_target(&conn, VOL),
                Err(ExtentAllocError::UnseatedVolume)
            ),
            "an unseated volume resolves to nothing"
        );

        seat_volume(&mut conn, VOL, "node-gone", 100, 100 + 120).unwrap();
        match resolve_volume_target(&conn, VOL) {
            Err(ExtentAllocError::UnknownComposer { composer }) => {
                assert_eq!(composer, "node-gone", "the error names who to look for")
            }
            other => panic!("expected UnknownComposer, got {other:?}"),
        }

        // Registering a DIFFERENT target does not make the seat
        // resolvable: the registry is looked up by the composer the
        // record names, never by "whoever is around".
        target_register(&mut conn, "node-here", "10.0.0.9", 4420, 100).unwrap();
        assert!(matches!(
            resolve_volume_target(&conn, VOL),
            Err(ExtentAllocError::UnknownComposer { .. })
        ));

        target_register(&mut conn, "node-gone", "10.0.0.7", 4421, 100).unwrap();
        let (seat, target) = resolve_volume_target(&conn, VOL).expect("now resolvable");
        assert_eq!(seat.composer, "node-gone");
        assert_eq!((target.traddr.as_str(), target.trsvcid), ("10.0.0.7", 4421));
    }

    /// The promotion CAS, all the way through: it moves the seat by
    /// exactly one epoch, and every guard it is made of refuses on its
    /// own terms.
    #[test]
    fn the_promotion_cas_advances_one_epoch_and_every_guard_refuses_alone() {
        let mut conn = fresh();
        target_register(&mut conn, "node-a", "10.0.0.1", 4420, 100).unwrap();
        target_register(&mut conn, "node-b", "10.0.0.2", 4420, 100).unwrap();
        seat_volume(&mut conn, VOL, "node-a", 100, 100 + 120).unwrap();

        // Unseated volumes have nothing to promote.
        assert!(matches!(
            promote_volume(&mut conn, "pvc-nope", 1, "node-a", "node-b", 200),
            Err(ExtentAllocError::UnseatedVolume)
        ));

        // The sitting composer is not a candidate.
        assert!(matches!(
            promote_volume(&mut conn, VOL, 1, "node-a", "node-a", 200),
            Err(ExtentAllocError::SelfPromotion { .. })
        ));

        // ElectInSync: node-b holds no in-sync leg yet. This is the
        // single-copy volume's permanent answer, and the degraded
        // volume's answer too.
        match promote_volume(&mut conn, VOL, 1, "node-a", "node-b", 200) {
            Err(ExtentAllocError::NotInSync { candidate }) => assert_eq!(candidate, "node-b"),
            other => panic!("expected NotInSync, got {other:?}"),
        }

        // A STALE mark is not an in-sync mark — the whole point of the
        // gate is that these are different.
        leg_mark(&mut conn, VOL, "node-b", LEG_STALE, 150).unwrap();
        assert!(matches!(
            promote_volume(&mut conn, VOL, 1, "node-a", "node-b", 200),
            Err(ExtentAllocError::NotInSync { .. })
        ));

        // An in-sync leg on an UNREGISTERED target is still no
        // candidate: an elected composer nobody can dial is a promotion
        // into a black hole.
        leg_mark(&mut conn, VOL, "node-ghost", LEG_INSYNC, 150).unwrap();
        match promote_volume(&mut conn, VOL, 1, "node-a", "node-ghost", 200) {
            Err(ExtentAllocError::UnknownComposer { composer }) => {
                assert_eq!(composer, "node-ghost")
            }
            other => panic!("expected UnknownComposer, got {other:?}"),
        }

        // In sync, registered, not the sitting composer: elected.
        leg_mark(&mut conn, VOL, "node-b", LEG_INSYNC, 160).unwrap();
        let promoted = promote_volume(&mut conn, VOL, 1, "node-a", "node-b", 200).unwrap();
        assert_eq!(promoted.epoch, 2, "exactly one epoch, monotone");
        assert_eq!(promoted.composer, "node-b");
        assert_eq!(volume_seat(&conn, VOL).unwrap().unwrap(), promoted, "durable");

        // The deposed leg's mark is UNTOUCHED. Marking it stale belongs
        // to assembly, not to the CAS: between the two the deposed
        // composer may still be acking, and its leg is not yet behind.
        let legs = legs_for_volume(&conn, VOL).unwrap();
        let a = legs.iter().find(|l| l.target_id == "node-a").unwrap();
        assert_eq!(a.sync_state, LEG_INSYNC, "the CAS does not degrade the deposed leg");

        // And the losing retry: the same call again reads the seat it
        // no longer matches and refuses, naming what stands now.
        match promote_volume(&mut conn, VOL, 1, "node-a", "node-b", 300) {
            Err(ExtentAllocError::PromotionRaced { epoch, composer }) => {
                assert_eq!((epoch, composer.as_str()), (2, "node-b"))
            }
            other => panic!("expected PromotionRaced, got {other:?}"),
        }
    }

    /// THE LEASE BELONGS TO THE EPOCH, NOT THE NODE — both halves, each
    /// of which was forced by a TLC counterexample before any of this
    /// existed.
    ///
    /// (a) A DEPOSED composer is refused its renewal however healthy it
    ///     is. Let it re-arm its own lease and the eviction horizon
    ///     never comes: promotion wedges with every process alive.
    /// (b) An ELECTED composer is refused too, because assembly grants a
    ///     lease and a holder never takes one. Let it self-grant and it
    ///     serves on an earlier epoch's lapse — the promoter then reads
    ///     that ancient lapse as an already-passed horizon and assembles
    ///     over a still-serving zombie.
    #[test]
    fn a_lease_is_refused_to_the_deposed_and_to_the_merely_elected() {
        let mut conn = fresh();
        target_register(&mut conn, "node-a", "10.0.0.1", 4420, 100).unwrap();
        target_register(&mut conn, "node-b", "10.0.0.2", 4420, 100).unwrap();
        seat_volume(&mut conn, VOL, "node-a", 100, 220).unwrap();

        // The sitting composer renews freely.
        let l = lease_renew(&mut conn, VOL, "node-a", 400).unwrap();
        assert_eq!((l.epoch, l.holder.as_str(), l.expires_unix), (1, "node-a", 400));
        assert!(l.is_live_at(399) && !l.is_live_at(400), "expiry is exclusive");

        // Promote. The CAS moves the SEAT; the lease stays with epoch 1
        // — and that gap is the eviction horizon.
        leg_mark(&mut conn, VOL, "node-b", LEG_INSYNC, 150).unwrap();
        promote_volume(&mut conn, VOL, 1, "node-a", "node-b", 200).unwrap();
        let standing = lease_get(&conn, VOL).unwrap().unwrap();
        assert_eq!(
            (standing.epoch, standing.holder.as_str(), standing.expires_unix),
            (1, "node-a", 400),
            "the CAS must not touch the lease — collapsing the two erases the horizon"
        );

        // (a) node-a is deposed. Healthy, running, and refused.
        match lease_renew(&mut conn, VOL, "node-a", 9_999) {
            Err(ExtentAllocError::LeaseRefused { reason }) => {
                assert!(reason.contains("not the composer"), "{reason}")
            }
            other => panic!("a deposed holder must not renew: {other:?}"),
        }
        assert_eq!(
            lease_get(&conn, VOL).unwrap().unwrap().expires_unix,
            400,
            "a refused renewal must not move the horizon"
        );

        // (b) node-b is elected but not assembled. Also refused.
        match lease_renew(&mut conn, VOL, "node-b", 9_999) {
            Err(ExtentAllocError::LeaseRefused { reason }) => {
                assert!(reason.contains("assembly grants it"), "{reason}")
            }
            other => panic!("an elected holder must not take a lease: {other:?}"),
        }

        // Assembly grants it, and only then does node-b hold one.
        lease_grant(&mut conn, VOL, 2, "node-b", 600).unwrap();
        let l = lease_renew(&mut conn, VOL, "node-b", 800).unwrap();
        assert_eq!((l.epoch, l.holder.as_str(), l.expires_unix), (2, "node-b", 800));
        // And node-a is still refused, now for the first reason.
        assert!(matches!(
            lease_renew(&mut conn, VOL, "node-a", 9_999),
            Err(ExtentAllocError::LeaseRefused { .. })
        ));
    }

    /// The dead-man's work list is "what do I hold", and a surrendered
    /// lease leaves it. Also: a lease dies with its volume — the right
    /// to serve bytes that no longer exist is not a right worth keeping.
    #[test]
    fn leases_are_listed_by_holder_surrendered_and_swept() {
        let mut conn = setup();
        target_register(&mut conn, "node-a", "10.0.0.1", 4420, 100).unwrap();
        seat_volume(&mut conn, VOL, "node-a", 100, 220).unwrap();
        seat_volume(&mut conn, "pvc-other", "node-a", 100, 220).unwrap();
        lease_grant(&mut conn, "pvc-theirs", 1, "node-b", 220).unwrap();

        let mine = leases_held_by(&conn, "node-a").unwrap();
        assert_eq!(mine.len(), 2, "both of node-a's, and not node-b's");
        assert!(mine.iter().all(|l| l.holder == "node-a"));

        assert!(lease_drop(&mut conn, VOL).unwrap(), "surrendered");
        assert!(!lease_drop(&mut conn, VOL).unwrap(), "and idempotent");
        assert_eq!(leases_held_by(&conn, "node-a").unwrap().len(), 1);

        drop_volume(&mut conn, "pvc-other").unwrap();
        assert!(leases_held_by(&conn, "node-a").unwrap().is_empty(), "swept with the volume");
    }

    /// Seating records the composer's own leg in sync — otherwise a
    /// volume could never be promoted AWAY from a target that does hold
    /// a good copy. But a re-seat must never re-mark: that would let an
    /// ordinary converge clear a stale mark with no copy behind it,
    /// which is `FlintCompositionSelfRejoin.cfg`'s counterexample
    /// (auto-examine declaring a stale leg clean, so the honest election
    /// gate elects it in good faith).
    #[test]
    fn seating_marks_its_own_leg_but_a_reseat_never_clears_a_stale_mark() {
        let mut conn = fresh();
        seat_volume(&mut conn, VOL, "node-a", 100, 100 + 120).unwrap();
        let legs = legs_for_volume(&conn, VOL).unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!((legs[0].target_id.as_str(), legs[0].sync_state.as_str()), ("node-a", LEG_INSYNC));

        leg_mark(&mut conn, VOL, "node-a", LEG_STALE, 200).unwrap();
        seat_volume(&mut conn, VOL, "node-a", 300, 300 + 120).unwrap();
        let legs = legs_for_volume(&conn, VOL).unwrap();
        assert_eq!(
            legs[0].sync_state, LEG_STALE,
            "a converge pass must not vouch for bytes it has not copied"
        );
    }

    /// DeleteVolume takes the seat with it. A re-created namesake must
    /// be seated afresh by whoever provisions it — inheriting an epoch
    /// and a composer from a dead volume of the same name is the
    /// stale-arena class of bug one table over.
    #[test]
    fn dropping_a_volume_drops_its_seat() {
        let mut conn = setup();
        seat_volume(&mut conn, VOL, "node-a", 100, 100 + 120).unwrap();
        target_register(&mut conn, "node-a", "10.0.0.9", 4420, 100).unwrap();
        assert!(volume_seat(&conn, VOL).unwrap().is_some());

        drop_volume(&mut conn, VOL).unwrap();
        assert!(volume_seat(&conn, VOL).unwrap().is_none(), "seat swept");
        assert!(legs_for_volume(&conn, VOL).unwrap().is_empty(), "legs swept");
        assert_eq!(
            target_list(&conn).unwrap().len(),
            1,
            "the TARGET outlives the volume — it serves others"
        );
    }
}
