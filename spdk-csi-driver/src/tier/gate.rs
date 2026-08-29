//! The per-file write gate — L2 step 3 (design review A4, plus A5's
//! flush single-flight).
//!
//! The exclusion primitive the whole tier depends on. Three consumers,
//! one invariant each:
//!
//! - **Mutating ops** hold a [`WriteTicket`] across the byte-mutating
//!   syscall AND its capture note. The ticket is an in-flight counter,
//!   not a lock: concurrent writers never serialize on each other.
//! - **The flusher** calls [`drain_and_take_epoch`]: a momentary
//!   barrier that waits out in-flight tickets and swaps the capture
//!   epoch while entrants hold — A4's "epoch-swapped atomically with
//!   the drain". Because notes happen inside the ticket, a drained
//!   gate has NO straggler notes: every interval in the epoch names
//!   bytes already on disk, and no completed syscall's note can land
//!   in the swapped-out epoch afterward.
//! - **Eviction/hydration** (steps 10/11) hold an [`ExclusionGuard`]:
//!   entrants are REFUSED (not parked) while it lives — the sites map
//!   the refusal to NFS4ERR_DELAY, the client retries, and by the time
//!   eviction truncates or hydration rewrites, no pwrite can be in
//!   flight to resurrect destroyed bytes.
//!
//! Sync by design: every mutating syscall in the MDS lane runs in
//! blocking context (spawn_blocking closures or sync helpers), so the
//! gate blocks a blocking-pool thread, never the executor — and only
//! while a drain's barrier is up, which lasts as long as the slowest
//! in-flight syscall on that one file. The uncontended cost is one
//! mutex lock/unlock per mutating op, and zero when the tier is off.
//!
//! Keyed (dev, ino) like capture — file identity, rename-proof. A
//! [`purge`] hook lets the flusher reap a fully idle cell when the
//! dirty bit clears (step 5); until then a cell is ~100 bytes per
//! file ever written this process lifetime.

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::capture::{self, FileCapture};

#[derive(Default)]
struct GateState {
    /// Mutating ops currently between enter and ticket drop.
    in_flight: u64,
    /// A drain is waiting/swapping: entrants hold (briefly).
    barrier: bool,
    /// Eviction/hydration in progress: entrants are refused.
    excluded: bool,
}

pub struct GateCell {
    st: Mutex<GateState>,
    cv: Condvar,
    /// A5: per-file flush single-flight. Atomic, not in `st` — the
    /// flusher holds it across its whole (async, long) pipeline, and
    /// it must never interact with the barrier's condvar dance.
    flushing: AtomicBool,
}

static GATES: OnceLock<DashMap<(u64, u64), Arc<GateCell>>> = OnceLock::new();

fn gates() -> &'static DashMap<(u64, u64), Arc<GateCell>> {
    GATES.get_or_init(DashMap::new)
}

fn cell(dev: u64, ino: u64) -> Arc<GateCell> {
    gates()
        .entry((dev, ino))
        .or_insert_with(|| {
            Arc::new(GateCell {
                st: Mutex::new(GateState::default()),
                cv: Condvar::new(),
                flushing: AtomicBool::new(false),
            })
        })
        .clone()
}

/// The file is excluded (evicting/hydrating). Sites map this to
/// NFS4ERR_DELAY: the client retries, and by then the exclusion owner
/// has either finished or parked the file behind a stub marker.
#[derive(Debug, PartialEq, Eq)]
pub struct Excluded;

/// RAII in-flight mark. MUST outlive both the syscall and its capture
/// note (bind it `let _gate = ...`, never `let _ = ...`).
pub struct WriteTicket(Option<Arc<GateCell>>);

impl Drop for WriteTicket {
    fn drop(&mut self) {
        if let Some(c) = self.0.take() {
            let mut st = c.st.lock().unwrap();
            st.in_flight -= 1;
            let wake = st.in_flight == 0;
            drop(st);
            if wake {
                c.cv.notify_all();
            }
        }
    }
}

/// Enter the gate for one mutating op on (dev, ino). Holds while a
/// drain's barrier is up; refuses while the file is excluded. When the
/// tier is off this is one atomic load and a no-op ticket.
pub fn enter(dev: u64, ino: u64) -> Result<WriteTicket, Excluded> {
    if !capture::enabled() {
        return Ok(WriteTicket(None));
    }
    let c = cell(dev, ino);
    let mut st = c.st.lock().unwrap();
    loop {
        if st.excluded {
            return Err(Excluded);
        }
        if !st.barrier {
            break;
        }
        st = c.cv.wait(st).unwrap();
    }
    st.in_flight += 1;
    drop(st);
    Ok(WriteTicket(Some(c)))
}

/// Enter via an open fd (mirrors `capture::note_file`). A failed fstat
/// yields a no-op ticket: the syscall that follows will fail on the
/// same fd, so there is nothing to protect.
pub fn enter_file(f: &std::fs::File) -> Result<WriteTicket, Excluded> {
    if !capture::enabled() {
        return Ok(WriteTicket(None));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match f.metadata() {
            Ok(md) => enter(md.dev(), md.ino()),
            Err(_) => Ok(WriteTicket(None)),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        Ok(WriteTicket(None))
    }
}

/// Enter via a path (mirrors `capture::note_path`). A failed stat
/// yields a no-op ticket for the same reason as `enter_file`.
pub fn enter_path(path: &std::path::Path) -> Result<WriteTicket, Excluded> {
    if !capture::enabled() {
        return Ok(WriteTicket(None));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match path.symlink_metadata() {
            Ok(md) => enter(md.dev(), md.ino()),
            Err(_) => Ok(WriteTicket(None)),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(WriteTicket(None))
    }
}

/// A4's atomic drain-and-swap: raise the barrier, wait out in-flight
/// tickets, swap the capture epoch WHILE HOLDING the gate mutex, drop
/// the barrier. Entrants hold for the duration (bounded by the slowest
/// in-flight syscall on this file); a concurrent drain queues behind
/// the barrier. The returned epoch is the flusher's; a failed flush
/// gives it back via `capture::merge_back`.
pub fn drain_and_take_epoch(dev: u64, ino: u64) -> Option<FileCapture> {
    let c = cell(dev, ino);
    let mut st = c.st.lock().unwrap();
    while st.barrier {
        st = c.cv.wait(st).unwrap();
    }
    st.barrier = true;
    while st.in_flight > 0 {
        st = c.cv.wait(st).unwrap();
    }
    let epoch = capture::take_epoch(dev, ino);
    st.barrier = false;
    drop(st);
    c.cv.notify_all();
    epoch
}

/// Exclusion for eviction/hydration (steps 10/11 own the callers).
/// Waits out in-flight ops; from then until drop, every `enter` on the
/// file is refused with [`Excluded`]. Queues behind a concurrent drain
/// or exclusion.
pub struct ExclusionGuard {
    cell: Arc<GateCell>,
}

/// How long [`exclude`] will wait before giving the file back.
///
/// Generous on purpose — this waits out in-flight syscalls on ONE file,
/// and a healthy-but-slow store under load must not trip it (the F33
/// fence deadline is 90s for the whole backing store, for the same
/// reason). What it must never be is absent; see [`exclude`].
const EXCLUDE_DEADLINE: Duration = Duration::from_secs(30);

/// Take the file out of service, or give up and say so.
///
/// **There is deliberately no unbounded form.** The wait this replaces
/// set `excluded` and THEN waited for `in_flight` to reach zero with no
/// deadline — so one write syscall stuck in D-state left the file
/// refusing every entrant forever, with no `ExclusionGuard` yet in
/// existence for a `Drop` to clear. The gate is a process-local map
/// with no release path and no operator surface, so that state had
/// exactly one remedy: restart the hub. The F33 watchdog does not cover
/// it either — that probes the backing store, and this wedge can happen
/// on a healthy disk.
///
/// `None` means "not now": both callers already answer that way
/// (eviction refuses Busy and retries next tick, hydration backs off),
/// which turns an unrecoverable wedge into an ordinary retry.
pub fn exclude(dev: u64, ino: u64) -> Option<ExclusionGuard> {
    exclude_within(dev, ino, EXCLUDE_DEADLINE)
}

/// [`exclude`] with an explicit deadline. Tests use it to make the
/// give-up path observable without waiting out the real one.
pub fn exclude_within(dev: u64, ino: u64, within: Duration) -> Option<ExclusionGuard> {
    let c = cell(dev, ino);
    let deadline = Instant::now() + within;
    let mut st = c.st.lock().unwrap();

    // Wait out another exclusion or a drain barrier. Nothing is claimed
    // yet, so giving up here costs nothing and changes nothing.
    while st.excluded || st.barrier {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            super::meter::bump(super::meter::Counter::GateExcludeTimeouts);
            return None;
        };
        st = c.cv.wait_timeout(st, left).unwrap().0;
    }

    // Claim it, then wait out the writers already inside.
    st.excluded = true;
    while st.in_flight > 0 {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            // BACK OUT, and this is the whole point of the deadline.
            // The flag is already refusing entrants; returning while it
            // stands would leave the file wedged with nothing to drop.
            st.excluded = false;
            drop(st);
            c.cv.notify_all();
            super::meter::bump(super::meter::Counter::GateExcludeTimeouts);
            return None;
        };
        st = c.cv.wait_timeout(st, left).unwrap().0;
    }
    drop(st);
    Some(ExclusionGuard { cell: c })
}

impl Drop for ExclusionGuard {
    fn drop(&mut self) {
        let mut st = self.cell.st.lock().unwrap();
        st.excluded = false;
        drop(st);
        self.cell.cv.notify_all();
    }
}

/// A5: per-file flush single-flight. `None` means a flush for this
/// file is already running — the caller skips, it does NOT queue (the
/// running flush will observe any dirtiness accrued after its epoch
/// swap on the next cycle).
pub struct FlushTicket {
    cell: Arc<GateCell>,
}

pub fn try_begin_flush(dev: u64, ino: u64) -> Option<FlushTicket> {
    let c = cell(dev, ino);
    if c.flushing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        Some(FlushTicket { cell: c })
    } else {
        None
    }
}

impl Drop for FlushTicket {
    fn drop(&mut self) {
        self.cell.flushing.store(false, Ordering::SeqCst);
    }
}

/// Reap a fully idle cell (the flusher calls this when a file's dirty
/// bit clears, step 5). Returns false if the cell is live in any way.
/// Safe against a racing `enter`: the map shard's write guard (Entry
/// API) is held across the strong-count check, so no one can clone the
/// Arc between the check and the remove.
pub fn purge(dev: u64, ino: u64) -> bool {
    use dashmap::mapref::entry::Entry;
    match gates().entry((dev, ino)) {
        Entry::Occupied(e) => {
            if Arc::strong_count(e.get()) != 1 {
                return false;
            }
            let idle = {
                let st = e.get().st.lock().unwrap();
                st.in_flight == 0 && !st.barrier && !st.excluded
            } && !e.get().flushing.load(Ordering::SeqCst);
            if idle {
                e.remove();
                true
            } else {
                false
            }
        }
        Entry::Vacant(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture::Mutation;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    // Distinct (dev, ino) keys per test: the gate map is process-global
    // and lib tests run in parallel (same discipline as capture/durable
    // tests).
    const DEV: u64 = 0x6A7E;

    /// The step-3 drill, half one: a storm of enter/apply/note/exit
    /// against concurrent drains must lose nothing — the union of all
    /// drained epochs plus the residual capture covers every noted
    /// offset exactly once. Offsets are sequential (from one counter),
    /// so intervals merge as they land: an epoch's fragmentation is
    /// bounded by the writer count and can never overflow to `whole`,
    /// which keeps the coverage check exact.
    #[test]
    fn storm_vs_drain_loses_no_interval() {
        // Queues and/or drains the PROCESS-GLOBAL capture queue.
        // Held for the whole body: the theft window is queue-to-drain,
        // not the drain alone. See `capture::test_exclusive`.
        let _excl = crate::tier::capture::test_exclusive();
        // A dropped TempDir hands its inodes to the next one, and on
        // ext4 that reuse is deterministic — so start from no
        // process-global capture state at all. See reset_for_tests.
        crate::tier::capture::reset_for_tests();
        capture::force_enable();
        let ino = 0x31_u64;
        const WRITERS: usize = 8;
        const PER: u64 = 200;

        let next = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..WRITERS {
            let next = Arc::clone(&next);
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER {
                    let off = next.fetch_add(1, Ordering::SeqCst);
                    let _gate = enter(DEV, ino).expect("no exclusion in this drill");
                    // "syscall" then note, both inside the ticket —
                    // the production ordering.
                    capture::note(DEV, ino, Mutation::Write { offset: off, len: 1 });
                }
            }));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let drainer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut epochs = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    if let Some(e) = drain_and_take_epoch(DEV, ino) {
                        epochs.push(e);
                    }
                    std::thread::sleep(Duration::from_micros(100));
                }
                epochs
            })
        };
        for h in handles {
            h.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        let mut epochs = drainer.join().unwrap();
        if let Some(residual) = drain_and_take_epoch(DEV, ino) {
            epochs.push(residual);
        }

        let total = WRITERS as u64 * PER;
        let mut covered = vec![false; total as usize];
        for e in &epochs {
            assert!(!e.whole, "disjoint 1-byte writes must never overflow to whole");
            for &(s, end) in &e.intervals {
                for o in s..end {
                    assert!(!covered[o as usize], "offset {} drained twice", o);
                    covered[o as usize] = true;
                }
            }
        }
        let missing = covered.iter().filter(|c| !**c).count();
        assert_eq!(missing, 0, "{} noted offsets lost across epoch swaps", missing);
    }

    /// The step-3 drill, half two: exclusion drains in-flight tickets
    /// and refuses new ones for its whole lifetime — the "no pwrite can
    /// be in flight while eviction truncates" guarantee.
    #[test]
    fn exclusion_drains_and_refuses_until_dropped() {
        capture::force_enable();
        let ino = 0x32_u64;
        let in_syscall = Arc::new(AtomicU64::new(0));
        let refused = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let in_syscall = Arc::clone(&in_syscall);
            let refused = Arc::clone(&refused);
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match enter(DEV, ino) {
                        Ok(_t) => {
                            in_syscall.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_micros(50));
                            in_syscall.fetch_sub(1, Ordering::SeqCst);
                        }
                        Err(Excluded) => {
                            refused.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_micros(50));
                        }
                    }
                }
            }));
        }
        std::thread::sleep(Duration::from_millis(10));
        {
            let _excl = exclude(DEV, ino);
            // Drained on return, and STAYS drained: sample across 20ms
            // while the storm hammers enter().
            for _ in 0..20 {
                assert_eq!(
                    in_syscall.load(Ordering::SeqCst),
                    0,
                    "a mutating op ran while the file was excluded"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(refused.load(Ordering::SeqCst) > 0, "storm never hit the refusal path");
        }
        // Guard dropped: writers must resume.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if in_syscall.load(Ordering::SeqCst) > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "writers never resumed after the exclusion dropped"
            );
            std::thread::yield_now();
        }
        stop.store(true, Ordering::SeqCst);
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn flush_single_flight() {
        let ino = 0x33_u64;
        let t1 = try_begin_flush(DEV, ino).expect("first flush must win the ticket");
        assert!(try_begin_flush(DEV, ino).is_none(), "second flush must be refused");
        drop(t1);
        assert!(try_begin_flush(DEV, ino).is_some(), "ticket must free on drop");
    }

    #[test]
    fn purge_refuses_live_cells_and_reaps_idle_ones() {
        capture::force_enable();
        let ino = 0x34_u64;
        let t = enter(DEV, ino).unwrap();
        assert!(!purge(DEV, ino), "must refuse: a ticket is in flight");
        drop(t);
        let f = try_begin_flush(DEV, ino).unwrap();
        assert!(!purge(DEV, ino), "must refuse: a flush holds the cell");
        drop(f);
        assert!(purge(DEV, ino), "idle cell must reap");
        assert!(purge(DEV, ino), "absent cell is trivially purged");
    }

    #[test]
    fn drain_is_atomic_with_the_swap() {
        // Queues and/or drains the PROCESS-GLOBAL capture queue.
        // Held for the whole body: the theft window is queue-to-drain,
        // not the drain alone. See `capture::test_exclusive`.
        let _excl = crate::tier::capture::test_exclusive();
        // A dropped TempDir hands its inodes to the next one, and on
        // ext4 that reuse is deterministic — so start from no
        // process-global capture state at all. See reset_for_tests.
        crate::tier::capture::reset_for_tests();
        capture::force_enable();
        let ino = 0x35_u64;
        // A ticket in flight holds the drain; the note lands before the
        // swap completes, so the epoch must contain it.
        let t = enter(DEV, ino).unwrap();
        let h = std::thread::spawn(move || drain_and_take_epoch(DEV, ino));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!h.is_finished(), "drain must wait for the in-flight ticket");
        capture::note(DEV, ino, Mutation::Write { offset: 7, len: 3 });
        drop(t);
        let epoch = h.join().unwrap().expect("the note must be in the drained epoch");
        assert_eq!(epoch.intervals, vec![(7, 10)]);
    }
    /// The deadline exists so a wedged writer cannot take a file out of
    /// service permanently — and giving up must LEAVE NO TRACE.
    ///
    /// The bug this guards is specific: the old wait set `excluded` and
    /// then drained with no deadline, so a stuck in-flight ticket left
    /// the flag standing with no `ExclusionGuard` in existence for a
    /// `Drop` to clear. Every writer to that file was refused forever,
    /// and the gate has no release path — the only remedy was
    /// restarting the hub. A give-up that forgot to clear the flag
    /// would reproduce it exactly.
    #[test]
    fn a_timed_out_exclusion_gives_the_file_back() {
        capture::force_enable();
        let ino = 0x9001_u64;

        // A writer inside the gate that outlives the attempt.
        let held = enter(DEV, ino).expect("idle file");

        let t0 = Instant::now();
        assert!(
            exclude_within(DEV, ino, Duration::from_millis(50)).is_none(),
            "exclusion completed while a writer was still in flight"
        );
        assert!(t0.elapsed() >= Duration::from_millis(50), "it did not actually wait");

        // THE ASSERTION THAT MATTERS: the file still works. A new
        // writer must not be refused by a flag nobody owns.
        let another = enter(DEV, ino).expect("the give-up wedged the file");
        drop(another);
        drop(held);

        // And with the writer gone, exclusion succeeds — so the refusal
        // above was the deadline doing its job, not the cell being
        // broken.
        assert!(
            exclude_within(DEV, ino, Duration::from_secs(5)).is_some(),
            "exclusion failed on an idle file"
        );
    }

    /// Giving up is counted. A file whose writes never drain is
    /// otherwise invisible: the gate has no other instrumentation, and
    /// the caller's retry hides the symptom.
    #[test]
    fn a_timed_out_exclusion_is_metered() {
        capture::force_enable();
        let ino = 0x9002_u64;
        let held = enter(DEV, ino).expect("idle file");

        let before = super::super::meter::snapshot().gate_exclude_timeouts;
        assert!(exclude_within(DEV, ino, Duration::from_millis(20)).is_none());
        let after = super::super::meter::snapshot().gate_exclude_timeouts;
        assert!(after > before, "a give-up left no trace in the meter");
        drop(held);
    }

    /// A second exclusion queues behind the first and honours the same
    /// deadline — the wait-for-the-other-holder arm, which claims
    /// nothing and so must also change nothing.
    #[test]
    fn exclusion_waits_for_an_incumbent_then_gives_up() {
        capture::force_enable();
        let ino = 0x9003_u64;

        let first = exclude_within(DEV, ino, Duration::from_secs(5)).expect("idle file");
        assert!(
            exclude_within(DEV, ino, Duration::from_millis(30)).is_none(),
            "two exclusions were held at once"
        );

        // The incumbent is untouched by the failed attempt.
        assert!(matches!(enter(DEV, ino), Err(Excluded)), "the incumbent lost its exclusion");
        drop(first);
        assert!(enter(DEV, ino).is_ok(), "the file did not come back");
    }

}
