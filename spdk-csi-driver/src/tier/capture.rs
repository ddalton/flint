//! Mutation-complete dirty capture — amendment A2 of
//! docs/plans/s3-tier-l2-design-review.md.
//!
//! One `note*()` call from EVERY site that mutates regular-file content
//! in the MDS lane. The review confirmed (finding C5) that hooking "the
//! write path" alone misses six mutating lanes — SETATTR-size,
//! OPEN-createattrs size, ALLOCATE, DEALLOCATE, COPY, CLONE — and a
//! missed mutation becomes a stale S3 generation that hydrates wrong
//! bytes after eviction: the F67 disease, one layer up. The call sites
//! are therefore fused with the existing change-counter bumps, which
//! already sit at the exact post-success points (F14 established them);
//! a content mutation that bumps without noting is the bug class this
//! module exists to kill — new content-mutating code MUST note.
//!
//! MODULE INVARIANT (A2): unknown mutation ⇒ whole-file dirty. Anything
//! this log cannot represent precisely is representable as `whole`,
//! which is always correct and merely pessimal (uploads more).
//!
//! Keyed by (dev, ino) like `change_counter` — file identity, so
//! renames never orphan an entry. Inode reuse after unlink can alias a
//! dead file's residue onto a new file; every field here fails SAFE in
//! that direction (extra dirtiness, a lower min_size ⇒ more upload,
//! never less). Hygienic forgetting arrives with the identity-keyed
//! generation rows (implementation step 6, A7).
//!
//! Durability: none, by design — this log is the IN-MEMORY half; the
//! durable per-file dirty BIT (A3, step 2) anchors the crash story. A
//! lost process loses intervals and degrades to whole-file upload for
//! exactly the bit-set files.
//!
//! Capture is OFF by default (`FLINT_TIER_CAPTURE=1` enables; tests use
//! `force_enable()`); disabled cost is one atomic load per call.

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

/// Interval-count cap per file. A workload that shatters a file into
/// more distinct dirty ranges than this collapses to `whole` — correct,
/// pessimal, and bounded-memory (the flush uploads the file it would
/// mostly have uploaded anyway).
pub const MAX_INTERVALS: usize = 256;

/// A content mutation, as observed at its dispatch site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Bytes [offset, offset+len) were written (WRITE, COPY-dest,
    /// CLONE-dest, ALLOCATE — for capture purposes "now holds caller
    /// bytes" and "now holds defined zeros" are both dirty).
    Write { offset: u64, len: u64 },
    /// Bytes [offset, offset+len) became zeros in place (DEALLOCATE
    /// hole-punch, truncate-up gap fill). Distinct from `Write` only as
    /// a semantic hint for future flush optimizations; both dirty the
    /// range.
    Zero { offset: u64, len: u64 },
    /// The file was truncated DOWN to `new_size`. By contract this
    /// variant is shrink-only: a growing set_len is noted by its site
    /// as `Zero { old_size, new - old }` (the kernel zero-fills the
    /// gap), because this module cannot know the pre-op size.
    Truncate { new_size: u64 },
    /// A mutation this log cannot describe. The invariant's escape
    /// hatch — never wrong, only pessimal.
    Whole,
}

/// The captured dirty state of one file since its last epoch swap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileCapture {
    /// Sorted, disjoint, half-open [start, end) dirty byte ranges.
    pub intervals: Vec<(u64, u64)>,
    /// The smallest truncation target seen this epoch. Flush semantics
    /// (A2): parts at or beyond this offset in generation g may NOT be
    /// clean-copied — the file has been shorter than that since g.
    pub min_size: Option<u64>,
    /// Whole-file dirty: the invariant's terminal state.
    pub whole: bool,
}

impl FileCapture {
    pub fn is_dirty(&self) -> bool {
        self.whole || self.min_size.is_some() || !self.intervals.is_empty()
    }

    fn note(&mut self, m: Mutation) {
        if self.whole {
            // Terminal: whole subsumes everything except that a
            // truncate still tightens the copy watermark.
            if let Mutation::Truncate { new_size } = m {
                self.min_size = Some(self.min_size.map_or(new_size, |s| s.min(new_size)));
            }
            return;
        }
        match m {
            Mutation::Write { offset, len } | Mutation::Zero { offset, len } => {
                if len == 0 {
                    return;
                }
                self.add_range(offset, offset.saturating_add(len));
            }
            Mutation::Truncate { new_size } => {
                self.min_size = Some(self.min_size.map_or(new_size, |s| s.min(new_size)));
                // Bytes at/beyond the new EOF no longer exist; clip.
                // If the file regrows, that site notes the gap dirty.
                self.intervals.retain_mut(|(s, e)| {
                    if *s >= new_size {
                        return false;
                    }
                    if *e > new_size {
                        *e = new_size;
                    }
                    true
                });
            }
            Mutation::Whole => {
                self.whole = true;
                self.intervals.clear();
                self.intervals.shrink_to_fit();
            }
        }
    }

    /// Insert [s, e), merging overlapping/adjacent neighbors; collapse
    /// to `whole` past the cap.
    fn add_range(&mut self, s: u64, e: u64) {
        debug_assert!(s < e);
        // Find the merge window: every existing range with start <= e
        // and end >= s coalesces with the new one.
        let mut lo = s;
        let mut hi = e;
        let mut i = 0;
        let mut first_touch = None;
        while i < self.intervals.len() {
            let (rs, re) = self.intervals[i];
            if re < s {
                i += 1;
                continue;
            }
            if rs > e {
                break;
            }
            lo = lo.min(rs);
            hi = hi.max(re);
            if first_touch.is_none() {
                first_touch = Some(i);
            }
            self.intervals.remove(i);
        }
        let at = first_touch.unwrap_or_else(|| {
            self.intervals.partition_point(|&(rs, _)| rs < lo)
        });
        self.intervals.insert(at, (lo, hi));
        if self.intervals.len() > MAX_INTERVALS {
            overflow_counter().fetch_add(1, Ordering::Relaxed);
            self.note(Mutation::Whole);
        }
    }

    /// Union `other` into self — the failed-flush merge-back: the
    /// swapped-out epoch's dirtiness rejoins whatever accrued since.
    fn absorb(&mut self, other: FileCapture) {
        if let Some(ms) = other.min_size {
            self.note(Mutation::Truncate { new_size: ms });
        }
        if other.whole {
            self.note(Mutation::Whole);
            return;
        }
        for (s, e) in other.intervals {
            // Re-adding beyond a later truncate would resurrect clipped
            // ranges — but absorb() is only ever called with an OLDER
            // epoch, whose ranges were valid before the swap; a
            // truncate since then lives in self.min_size and re-clips.
            let cut = self.min_size.unwrap_or(u64::MAX);
            let (s, e) = (s.min(cut), e.min(cut));
            if s < e {
                self.add_range(s, e);
            }
        }
    }
}

// ── the process-wide table ───────────────────────────────────────────

static MAP: OnceLock<DashMap<(u64, u64), FileCapture>> = OnceLock::new();
static FORCED: AtomicBool = AtomicBool::new(false);

// ── the durable-bit marshalling layer (L2 step 2, A3) ────────────────
//
// The durable BIT itself lives in the state backend; this layer is the
// bridge between the sync note sites and the async backend write. A
// note on a file whose bit is not yet known-durable QUEUES a mark; the
// dispatcher drains the queue to the backend BEFORE any mutating op's
// reply exists (the pre-ack guarantee). DURABLE remembers which files
// already paid — one sqlite write per file per flush cycle.

/// One queued (not yet durable) mark.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Queued {
    path: Option<std::path::PathBuf>,
    /// Monotonic mark sequence (see [`PendingMark::seq`]).
    seq: u64,
}

/// Marks awaiting their durable write. Path upserts: a later note that
/// knows the path fills in an earlier None; every touch refreshes the
/// sequence.
static QUEUED: OnceLock<DashMap<(u64, u64), Queued>> = OnceLock::new();
/// Files whose bit is known-durable this cycle (skip queueing).
static DURABLE: OnceLock<dashmap::DashSet<(u64, u64)>> = OnceLock::new();
/// Fast-path flag: the dispatcher checks this per op result.
static HAS_PENDING: AtomicBool = AtomicBool::new(false);
/// Process-monotonic mark sequence. The flusher's clean-clear deletes
/// the durable row ONLY at the sequence it observed (step 5's A3-safe
/// clear): a drain that re-marks the row after the observation bumps
/// the stored sequence and the conditional delete no-ops — an acked
/// mutation's bit can never be deleted by a racing clear.
static MARK_SEQ: AtomicU64 = AtomicU64::new(1);
/// Last note instant per file — the flusher's quiescence clock (A11:
/// never flush a file whose write stream is still advancing).
static LAST_NOTE: OnceLock<DashMap<(u64, u64), std::time::Instant>> = OnceLock::new();

fn queued() -> &'static DashMap<(u64, u64), Queued> {
    QUEUED.get_or_init(DashMap::new)
}

fn last_note_map() -> &'static DashMap<(u64, u64), std::time::Instant> {
    LAST_NOTE.get_or_init(DashMap::new)
}
fn durable() -> &'static dashmap::DashSet<(u64, u64)> {
    DURABLE.get_or_init(dashmap::DashSet::new)
}

/// One queued durable mark (capture-side twin of TierDirtyEntry —
/// capture must not depend on the state backend's types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMark {
    pub dev: u64,
    pub ino: u64,
    pub path: Option<std::path::PathBuf>,
    /// The mark sequence that rides into the durable row's `mark_seq`
    /// (see [`MARK_SEQ`]).
    pub seq: u64,
}

fn queue_mark(dev: u64, ino: u64, path: Option<&std::path::Path>) {
    let key = (dev, ino);
    if durable().contains(&key) {
        return;
    }
    let seq = MARK_SEQ.fetch_add(1, Ordering::Relaxed);
    match queued().entry(key) {
        dashmap::mapref::entry::Entry::Occupied(mut o) => {
            let q = o.get_mut();
            q.seq = seq;
            if q.path.is_none() {
                if let Some(p) = path {
                    q.path = Some(p.to_path_buf());
                }
            }
        }
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(Queued { path: path.map(|p| p.to_path_buf()), seq });
        }
    }
    HAS_PENDING.store(true, Ordering::Release);
}

/// Is a (not yet durable) mark queued for this file? The flusher's
/// clean-clear refuses to delete the row while one is.
pub fn is_queued(dev: u64, ino: u64) -> bool {
    queued().contains_key(&(dev, ino))
}

/// When was this file last mutated (noted)? None = never this process.
/// The flusher's quiescence clock.
pub fn last_note(dev: u64, ino: u64) -> Option<std::time::Instant> {
    last_note_map().get(&(dev, ino)).map(|e| *e)
}

/// Forget the quiescence clock entry (flusher, after a full clean).
pub fn clear_quiet(dev: u64, ino: u64) {
    last_note_map().remove(&(dev, ino));
}

/// Allocate a mark sequence for a caller OUTSIDE the note path (the
/// identity events' rename mark rides the same monotonic counter).
pub fn next_mark_seq() -> u64 {
    MARK_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Forget every in-memory trace of a dead identity (unlinked by REMOVE
/// or covered by a rename-over) — capture entry, queued mark, durable
/// memo, quiescence clock. The durable rows are the backend
/// transaction's job (tier_apply_remove/rename); this is the RAM half.
pub fn forget(dev: u64, ino: u64) {
    let key = (dev, ino);
    map().remove(&key);
    queued().remove(&key);
    durable().remove(&key);
    last_note_map().remove(&key);
    // The eviction marker is part of this identity's RAM state (step
    // 10) — and on ext4 a REUSED inode number inheriting a dead file's
    // marker would DELAY the new file's I/O, a step worse than the
    // pessimal-capture aliasing this function already guards.
    crate::tier::evict::forget(dev, ino);
    HAS_PENDING.store(!queued().is_empty(), Ordering::Release);
}

/// Tests only: drop EVERY process-global capture map so a rig starts
/// from nothing.
///
/// The maps are keyed by `(dev, ino)` with no path corroboration, and a
/// `TempDir` that has been dropped hands its inode numbers straight back
/// to the next one. On ext4 that reuse is DETERMINISTIC, so a rig
/// inherits the previous rig's identities exactly, every run.
///
/// [`forget`] already names this hazard for the eviction marker. The
/// half it cannot cover is the DURABLE MEMO, because the suppression is
/// silent: `queue_mark` returns early for a known-durable identity, so
/// an adopt reports the file marked (the count is taken at the note
/// call) while no row ever reaches the backend.
///
/// That is a real defect and it cost a Linux-only red suite:
/// `adopt_must_not_tier_the_servers_own_meta_dir` marks `client.txt`
/// durable and drops its tree, and the very next test's
/// `src/nested/main.rs` lands on that exact inode. It reproduced 5/5 in
/// the full suite, in `tier::import` alone, and single-threaded, while
/// passing alone and passing on macOS, where APFS allocates differently.
///
/// PRODUCTION IS NOT EXPOSED TO THIS: a real unlink goes through the
/// server, `tier::identity` fires, and [`forget`] clears the memo. Only
/// a tree that vanishes without the server knowing — a `TempDir` drop —
/// leaves a stale entry, which is why this is a test-only reset and not
/// a change to the product's keying.
///
/// Safe to call from a rig holding [`test_exclusive`]: that lock spans
/// queue-to-drain for every rig, so nothing else can own a pending mark
/// at this moment.
#[cfg(test)]
pub fn reset_for_tests() {
    map().clear();
    queued().clear();
    durable().clear();
    last_note_map().clear();
    HAS_PENDING.store(false, Ordering::Release);
}

/// Cheap per-op check for the dispatcher.
#[inline]
pub fn has_pending() -> bool {
    HAS_PENDING.load(Ordering::Acquire)
}

/// Drain the queue for a backend write. Entries not confirmed (or
/// requeued) are LOST to the durable layer — callers must do one of
/// the two.
pub fn take_pending() -> Vec<PendingMark> {
    let q = queued();
    let keys: Vec<(u64, u64)> = q.iter().map(|e| *e.key()).collect();
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        if let Some((_, m)) = q.remove(&k) {
            out.push(PendingMark { dev: k.0, ino: k.1, path: m.path, seq: m.seq });
        }
    }
    HAS_PENDING.store(!q.is_empty(), Ordering::Release);
    out
}

/// The backend write committed: these files' bits are durable.
pub fn confirm_durable(marks: &[PendingMark]) {
    for m in marks {
        durable().insert((m.dev, m.ino));
    }
}

/// The backend write failed: nothing is durable — put the marks back
/// so the next mutating op retries.
pub fn requeue(marks: Vec<PendingMark>) {
    for m in marks {
        queue_mark(m.dev, m.ino, m.path.as_deref());
    }
}

/// Startup restore: the row exists in the backend, so the bit is
/// already durable. MUST be called BEFORE any note for the file, or
/// the note queues a redundant (harmless) re-mark.
pub fn prime_durable(dev: u64, ino: u64) {
    durable().insert((dev, ino));
}

/// The flusher (step 5) clears the durable memo in the same logical
/// step that clears the backend row — the NEXT mutation then re-marks.
pub fn clear_durable(dev: u64, ino: u64) {
    durable().remove(&(dev, ino));
}

/// Is this file's dirty bit known-durable this cycle?
pub fn is_durable(dev: u64, ino: u64) -> bool {
    durable().contains(&(dev, ino))
}

fn map() -> &'static DashMap<(u64, u64), FileCapture> {
    MAP.get_or_init(DashMap::new)
}

fn env_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("FLINT_TIER_CAPTURE").map(|v| v == "1").unwrap_or(false)
    })
}

/// Is capture on? One relaxed atomic load + one cached-env bool.
#[inline]
pub fn enabled() -> bool {
    FORCED.load(Ordering::Relaxed) || env_enabled()
}

/// Production enable — the config-driven master switch (`serve()`
/// flips it when the MDS config carries an enabled `tier` section,
/// before any state restore or listener bind). Sticky for the process
/// lifetime; there is deliberately no disable — a tier that could turn
/// off mid-run would strand queued marks and un-gate the write lanes.
pub fn enable() {
    FORCED.store(true, Ordering::Relaxed);
}

/// Serialises tests that queue marks and later drain them.
///
/// **Every test that queues OR drains must take this** — not just the
/// tier ones. The queue is process-global and `drain_pending` takes ALL
/// of it, so a test that merely drains into its own backend steals the
/// marks of any test that only queued. That is not hypothetical: it was
/// applied to the four `tier::{import,flush,hydrate,evict}` rigs and
/// left off `tier::{identity,durable,gate}` and the NFS server tests,
/// which drain the same queue — and those became the next flake
/// (`tier::identity::tests::remove_tombstones_and_forgets` failing
/// `!capture::is_durable`, ~1 run in 25, because a foreign drain's
/// `confirm_durable` landed after its own forget).
///
/// [`force_enable`]'s note that "per-file keys keep them isolated"
/// holds for the capture maps and NOT for the pending queue:
/// `durable::drain_pending` takes the WHOLE queue and writes it to
/// whichever backend called it, so two tier tests running in parallel
/// steal each other's marks and the loser finds its own files missing
/// from its own backend. The tests already guarded the case where
/// ANOTHER test's notes land here; the inverse — its own notes drained
/// elsewhere — was the ~1-in-5 flake in `tier::import`, on Linux and
/// macOS alike.
///
/// Held from rig construction to rig drop, because it must cover the
/// queue AND the drain as one critical section: serialising only the
/// drain leaves the theft window open between another test's queue and
/// its own drain.
#[cfg(test)]
pub fn test_exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that panics holding this must not cascade into every test
    // that runs after it.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tests (and only tests) flip capture on process-wide. There is
/// deliberately no `force_disable`: concurrent tests share this flag,
/// and per-file keys keep them isolated — except for the pending
/// queue, which is what [`test_exclusive`] exists for.
pub fn force_enable() {
    enable();
}

// Census counters — the observability seed (A12 grows this).
static NOTES: [AtomicU64; 4] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static OVERFLOWS: AtomicU64 = AtomicU64::new(0);

fn overflow_counter() -> &'static AtomicU64 {
    &OVERFLOWS
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Census {
    pub writes: u64,
    pub zeros: u64,
    pub truncates: u64,
    pub wholes: u64,
    pub overflows: u64,
    pub tracked_files: u64,
}

pub fn census() -> Census {
    Census {
        writes: NOTES[0].load(Ordering::Relaxed),
        zeros: NOTES[1].load(Ordering::Relaxed),
        truncates: NOTES[2].load(Ordering::Relaxed),
        wholes: NOTES[3].load(Ordering::Relaxed),
        overflows: OVERFLOWS.load(Ordering::Relaxed),
        tracked_files: map().len() as u64,
    }
}

/// The chokepoint. Every content-mutating dispatch site funnels here.
/// `path`, when the site has one, rides into the durable mark so the
/// backend row can name the file (best-effort until A7).
pub fn note_at(dev: u64, ino: u64, path: Option<&std::path::Path>, m: Mutation) {
    if !enabled() {
        return;
    }
    let idx = match m {
        Mutation::Write { .. } => 0,
        Mutation::Zero { .. } => 1,
        Mutation::Truncate { .. } => 2,
        Mutation::Whole => 3,
    };
    NOTES[idx].fetch_add(1, Ordering::Relaxed);
    map().entry((dev, ino)).or_default().note(m);
    last_note_map().insert((dev, ino), std::time::Instant::now());
    queue_mark(dev, ino, path);
}

/// `note_at` without a path (sites that only hold an fd or identity).
pub fn note(dev: u64, ino: u64, m: Mutation) {
    note_at(dev, ino, None, m);
}

/// Note via an open fd — rename-proof, the preferred form (mirrors
/// perfops' `bump_change_counter(&File)` rationale).
pub fn note_file(f: &std::fs::File, m: Mutation) {
    if !enabled() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(md) = f.metadata() {
            note(md.dev(), md.ino(), m);
        }
    }
    #[cfg(not(unix))]
    let _ = (f, m);
}

/// Note via a path. Best effort like `change_counter::bump_path`: a
/// failed stat means the object raced away; its content no longer
/// exists to be flushed.
pub fn note_path(path: &std::path::Path, m: Mutation) {
    if !enabled() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(md) = path.symlink_metadata() {
            note_at(md.dev(), md.ino(), Some(path), m);
        }
    }
    #[cfg(not(unix))]
    let _ = (path, m);
}

/// Current capture for a file, if any (test/observability surface).
pub fn snapshot(dev: u64, ino: u64) -> Option<FileCapture> {
    map().get(&(dev, ino)).map(|e| e.clone())
}

/// Atomically swap a file's capture out for flush: the returned epoch
/// belongs to the flusher; new mutations accrue in a fresh entry. The
/// A4 write gate will wrap this with its drain; the swap itself is the
/// "epoch-swapped atomically" half of that amendment.
pub fn take_epoch(dev: u64, ino: u64) -> Option<FileCapture> {
    map().remove(&(dev, ino)).map(|(_, v)| v)
}

/// A failed flush returns its epoch: union it back so nothing captured
/// is ever lost to an S3 error.
pub fn merge_back(dev: u64, ino: u64, epoch: FileCapture) {
    if !epoch.is_dirty() {
        return;
    }
    let mut cur = map().entry((dev, ino)).or_default();
    let newer = std::mem::take(&mut *cur);
    *cur = epoch;
    cur.absorb(newer);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(notes: &[Mutation]) -> FileCapture {
        let mut c = FileCapture::default();
        for m in notes {
            c.note(*m);
        }
        c
    }
    const W: fn(u64, u64) -> Mutation = |o, l| Mutation::Write { offset: o, len: l };

    #[test]
    fn ranges_merge_overlapping_and_adjacent() {
        let c = cap(&[W(0, 10), W(20, 10), W(10, 10)]);
        assert_eq!(c.intervals, vec![(0, 30)]);
        let c = cap(&[W(0, 10), W(10, 5)]);
        assert_eq!(c.intervals, vec![(0, 15)]);
        let c = cap(&[W(100, 10), W(0, 10)]);
        assert_eq!(c.intervals, vec![(0, 10), (100, 110)]);
    }

    #[test]
    fn zero_len_write_is_a_no_op() {
        assert!(!cap(&[W(5, 0)]).is_dirty());
    }

    #[test]
    fn truncate_clips_and_sets_watermark() {
        let c = cap(&[W(0, 100), W(200, 50), Mutation::Truncate { new_size: 120 }]);
        assert_eq!(c.intervals, vec![(0, 100)]);
        assert_eq!(c.min_size, Some(120));
        // A range straddling the cut is trimmed, not dropped.
        let c = cap(&[W(100, 100), Mutation::Truncate { new_size: 150 }]);
        assert_eq!(c.intervals, vec![(100, 150)]);
    }

    #[test]
    fn shrink_then_regrow_leaves_the_gap_dirty_and_watermark_low() {
        // truncate 1000→500, then the grow site notes Zero{500,500}.
        let c = cap(&[
            Mutation::Truncate { new_size: 500 },
            Mutation::Zero { offset: 500, len: 500 },
        ]);
        assert_eq!(c.min_size, Some(500), "watermark must survive the regrow");
        assert_eq!(c.intervals, vec![(500, 1000)]);
    }

    #[test]
    fn repeated_truncates_keep_the_minimum() {
        let c = cap(&[
            Mutation::Truncate { new_size: 300 },
            Mutation::Truncate { new_size: 700 },
        ]);
        assert_eq!(c.min_size, Some(300));
    }

    #[test]
    fn whole_is_terminal_but_truncate_still_tightens_watermark() {
        let c = cap(&[Mutation::Whole, W(0, 10), Mutation::Truncate { new_size: 5 }]);
        assert!(c.whole);
        assert!(c.intervals.is_empty());
        assert_eq!(c.min_size, Some(5));
    }

    #[test]
    fn interval_overflow_collapses_to_whole() {
        let mut c = FileCapture::default();
        for i in 0..(MAX_INTERVALS as u64 + 1) {
            // Disjoint ranges with gaps: 0, 2, 4, ...
            c.note(W(i * 2, 1));
        }
        assert!(c.whole, "past MAX_INTERVALS the capture must go whole-file");
        assert!(c.intervals.is_empty());
    }

    #[test]
    fn absorb_unions_and_respects_the_newer_watermark() {
        // Old epoch: [0,100) dirty. Since the swap: truncate to 50.
        let old = cap(&[W(0, 100)]);
        let mut newer = cap(&[Mutation::Truncate { new_size: 50 }]);
        newer.absorb(old);
        assert_eq!(newer.min_size, Some(50));
        assert_eq!(
            newer.intervals,
            vec![(0, 50)],
            "absorbed ranges must be re-clipped by the newer truncate"
        );
    }

    #[test]
    fn map_note_take_merge_roundtrip() {
        force_enable();
        let key = (0xF11D_u64, 0x51E1_u64);
        note(key.0, key.1, W(0, 10));
        let epoch = take_epoch(key.0, key.1).expect("captured");
        assert!(snapshot(key.0, key.1).is_none(), "swap must leave a clean slate");
        note(key.0, key.1, W(100, 10)); // accrues during "flush"
        merge_back(key.0, key.1, epoch); // flush failed
        let c = snapshot(key.0, key.1).unwrap();
        assert_eq!(c.intervals, vec![(0, 10), (100, 110)]);
    }

    #[test]
    fn disabled_is_a_no_op() {
        // Cannot force-disable (shared flag), and a PARALLEL test may
        // force_enable at any instant — so assert only when the flag
        // was off across the whole window. The flag is monotonic
        // (no force_disable exists), so "off after" implies "off
        // throughout": the note cannot have recorded.
        if enabled() {
            return;
        }
        note(0xD15A, 0xB1ED, W(0, 10));
        if !enabled() {
            assert!(snapshot(0xD15A, 0xB1ED).is_none());
        }
    }

    #[test]
    fn durable_memo_suppresses_queueing_and_paths_upsert() {
        force_enable();
        // Primed-durable (the startup-restore shape): notes must not
        // re-queue, but the in-memory capture still records.
        let k = (0xD0B1_u64, 0x1_u64);
        prime_durable(k.0, k.1);
        note(k.0, k.1, W(0, 4));
        assert!(!queued().contains_key(&k), "primed-durable file must not queue");
        assert!(snapshot(k.0, k.1).unwrap().is_dirty());

        // Fresh file: queues once; a later note that knows the path
        // fills it in; once confirmed durable, no re-queue. The queue
        // is PROCESS-GLOBAL and parallel tests drain it (note_and_land
        // loops drain_pending — and a drain also confirms the taken
        // marks durable), so the sequence runs under the retry-loop
        // discipline: any step that finds the entry drained clears the
        // memo and starts over.
        let k2 = (0xD0B1_u64, 0x2_u64);
        let mut done = false;
        for _ in 0..50 {
            clear_durable(k2.0, k2.1);
            queued().remove(&k2);
            note(k2.0, k2.1, W(0, 4));
            let Some(seq_before) = queued().get(&k2).map(|q| q.seq) else { continue };
            note_at(k2.0, k2.1, Some(std::path::Path::new("/x/f")), W(4, 4));
            let Some(q) = queued().get(&k2).map(|e| e.clone()) else { continue };
            assert_eq!(
                q.path.as_deref(),
                Some(std::path::Path::new("/x/f")),
                "a later note with a path must upsert an earlier None"
            );
            assert!(q.seq > seq_before, "every queue touch must advance the mark sequence");
            let Some(taken) = queued().remove(&k2).map(|(_, q)| q) else { continue };
            confirm_durable(&[PendingMark {
                dev: k2.0,
                ino: k2.1,
                path: taken.path,
                seq: taken.seq,
            }]);
            note(k2.0, k2.1, W(8, 4));
            assert!(!queued().contains_key(&k2), "confirmed-durable file must not re-queue");
            done = true;
            break;
        }
        assert!(done, "parallel drains raced the queue 50 straight times");

        // requeue puts a failed drain's marks back (same discipline:
        // a parallel drain may take the entry between the requeue and
        // the read).
        let k3 = (0xD0B1_u64, 0x3_u64);
        let mut done = false;
        for _ in 0..50 {
            requeue(vec![PendingMark { dev: k3.0, ino: k3.1, path: None, seq: 0 }]);
            if queued().contains_key(&k3) && has_pending() && is_queued(k3.0, k3.1) {
                done = true;
                break;
            }
        }
        assert!(done, "requeued mark never observed across 50 attempts");
    }

    #[test]
    fn dirty_predicate() {
        assert!(!FileCapture::default().is_dirty());
        assert!(cap(&[W(0, 1)]).is_dirty());
        assert!(cap(&[Mutation::Truncate { new_size: 0 }]).is_dirty());
        assert!(cap(&[Mutation::Whole]).is_dirty());
    }
}
