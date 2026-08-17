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

/// Tests (and only tests) flip capture on process-wide. There is
/// deliberately no `force_disable`: concurrent tests share this flag,
/// and per-file keys keep them isolated.
pub fn force_enable() {
    FORCED.store(true, Ordering::Relaxed);
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
pub fn note(dev: u64, ino: u64, m: Mutation) {
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
            note(md.dev(), md.ino(), m);
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
        // Cannot force-disable (shared flag), but a fresh key with the
        // env unset and FORCED possibly set by other tests — so this
        // test only asserts the enabled()==false path when it can.
        if !enabled() {
            note(0xD15A, 0xB1ED, W(0, 10));
            assert!(snapshot(0xD15A, 0xB1ED).is_none());
        }
    }

    #[test]
    fn dirty_predicate() {
        assert!(!FileCapture::default().is_dirty());
        assert!(cap(&[W(0, 1)]).is_dirty());
        assert!(cap(&[Mutation::Truncate { new_size: 0 }]).is_dirty());
        assert!(cap(&[Mutation::Whole]).is_dirty());
    }
}
