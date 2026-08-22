//! PVC space model — L2 step 8 (design review A10).
//!
//! Backpressure and ENOSPC truthfulness for the lite hub. Four jobs:
//!
//! - **Admission**: refuse WRITE/CREATE with `NFS4ERR_NOSPC` while
//!   `avail − reserve` cannot cover the request — the errno
//!   applications actually handle, delivered BEFORE hard-full (F55:
//!   EIO makes postgres PANIC). The reserve is a deliberate
//!   over-refusal margin: near-full, some in-place overwrites that
//!   would have succeeded are refused too — that is the price of
//!   never letting client I/O consume the flusher's and state.db's
//!   room. Admission is scoped by path prefix to the configured root,
//!   so only the tiered export pays it.
//! - **Gauge**: cached statvfs (refreshed by a background task; the
//!   hot-path cost is relaxed atomic loads) serving the SPACE_* /
//!   FILES_* attributes — df must read the PVC, not 8 EiB.
//! - **Watermark** (~85%): edge-triggered WARN now, the eviction
//!   trigger when step 10 lands.
//! - **Ballast**: a preallocated file next to state.db, RELEASED when
//!   free space goes critical — the room that lets the durable
//!   bookkeeping (dirty bits, intents, eviction confirmation) keep
//!   committing inside the very fullness it exists to relieve.
//!   Re-armed with hysteresis once admission has pushed usage back.
//!
//! Nothing here is pNFS: the model activates only when `serve()`
//! configures it (tier on), and unconfigured cost is one relaxed
//! atomic load per admission call.

use crate::tier::meter::{self, Counter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use tracing::{error, info, warn};

/// Cadence of the statvfs refresher task `serve()` spawns.
pub const REFRESH_SECS: u64 = 2;

/// Fixed admission cost of creating a new object (inode + dirent +
/// first block, upper-bounded): a CREATE/OPEN-create is refused when
/// headroom cannot cover this.
const CREATE_COST: u64 = 64 * 1024;

/// Ballast is released when `avail` falls below this fraction of the
/// reserve (floored) — by then admission has long been refusing client
/// writes, so the pressure is our own bookkeeping or non-NFS growth.
const RELEASE_DIVISOR: u64 = 8;
const RELEASE_FLOOR_MIN: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SpaceConfig {
    /// statvfs target and admission scope: the export root. Admission
    /// applies only to paths under it.
    pub root: PathBuf,
    /// Headroom admission preserves (default 256 MiB): client writes
    /// are refused NOSPC while `avail − reserve` is exhausted.
    pub reserve_bytes: u64,
    /// Eviction-trigger watermark in percent used (default 85).
    pub watermark_pct: u8,
    /// The state.db-side ballast file; None disables.
    pub ballast_path: Option<PathBuf>,
    pub ballast_bytes: u64,
}

/// A consistent-enough snapshot for attribute encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// `avail − reserve` (saturating): what a client can actually
    /// write — the truthful SPACE_AVAIL.
    pub avail_bytes: u64,
    pub files_total: u64,
    pub files_free: u64,
    pub files_avail: u64,
}

pub struct Space {
    cfg: SpaceConfig,
    total: AtomicU64,
    free: AtomicU64,
    /// Raw f_bavail × frsize (reserve NOT yet subtracted).
    avail_raw: AtomicU64,
    /// Bytes admitted by [`admit_bytes`] since the last gauge refresh.
    ///
    /// The gauge is a 2 s snapshot, and a fast writer outruns it: the
    /// D8 drill leg put 600 MiB onto 539 MiB free, got 201 on every
    /// chunk, drove `df` to 0 and consumed the whole reserve with
    /// `nospcWriteRefusals` still 0 — because each ~1 MiB WRITE was
    /// admitted against a snapshot that had not yet seen its
    /// predecessors. `admit_warm` sixty lines below already carries
    /// exactly this accounting for bulk hydration (its `pending`
    /// argument); demand writes had none.
    admitted_since_refresh: AtomicU64,
    files_total: AtomicU64,
    files_free: AtomicU64,
    files_avail: AtomicU64,
    above_watermark: AtomicBool,
    ballast_present: AtomicBool,
}

// ── the process-global hook the handlers read ────────────────────────

/// One relaxed load keeps the unconfigured fast path at capture's cost
/// discipline; the RwLock is only reached once something installed.
static CONFIGURED: AtomicBool = AtomicBool::new(false);

fn active() -> &'static RwLock<Option<Arc<Space>>> {
    static A: OnceLock<RwLock<Option<Arc<Space>>>> = OnceLock::new();
    A.get_or_init(|| RwLock::new(None))
}

fn current() -> Option<Arc<Space>> {
    if !CONFIGURED.load(Ordering::Relaxed) {
        return None;
    }
    active().read().unwrap().clone()
}

/// Build the model, take the first statvfs reading, create the ballast,
/// and install it as the process's admission/gauge authority. Called
/// once by `serve()` when the tier is enabled (tests may re-install;
/// admission's path scoping keeps unrelated tests unaffected).
pub fn configure(cfg: SpaceConfig) -> std::io::Result<Arc<Space>> {
    let s = Arc::new(Space {
        cfg,
        total: AtomicU64::new(0),
        free: AtomicU64::new(0),
        avail_raw: AtomicU64::new(0),
        admitted_since_refresh: AtomicU64::new(0),
        files_total: AtomicU64::new(0),
        files_free: AtomicU64::new(0),
        files_avail: AtomicU64::new(0),
        above_watermark: AtomicBool::new(false),
        ballast_present: AtomicBool::new(false),
    });
    s.create_ballast()?;
    s.refresh();
    *active().write().unwrap() = Some(Arc::clone(&s));
    CONFIGURED.store(true, Ordering::Relaxed);
    Ok(s)
}

/// The gauge for attribute encoding; `None` = unconfigured (serve the
/// historical values).
pub fn view() -> Option<View> {
    current().map(|s| s.view())
}

/// Admission refusal: the caller answers `NFS4ERR_NOSPC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoSpace;

/// A refusal is the expensive-if-wrong branch: before answering
/// NOSPC off the CACHED gauge (2 s refresher), force one statvfs and
/// re-check — a client that just deleted a big file must not spend
/// the refresher's window being refused on stale numbers (the chaos
/// drill caught exactly that: rm 200 MB, then every create for ~1 s
/// refused). Admits never pay this; statvfs is a µs-scale read.
fn headroom_for_refusal(s: &Arc<Space>) -> u64 {
    s.refresh();
    s.headroom()
}

/// A10 admission for byte-extending mutations (WRITE/ALLOCATE/COPY/
/// CLONE). `Err` ⇒ answer `NFS4ERR_NOSPC`. `len` is the upper bound of
/// new allocation; paths outside the configured root always pass.
pub fn admit_bytes(path: &Path, len: u64) -> Result<(), NoSpace> {
    let Some(s) = current() else { return Ok(()) };
    if !path.starts_with(&s.cfg.root) {
        return Ok(());
    }
    let need = len.max(4096);
    // The cached gauge must cover this write PLUS everything admitted
    // since it was taken. Without that second term a writer faster than
    // the 2 s refresher admits straight through the reserve (D8).
    if s.admits_from_cache(need) {
        s.note_admitted(need);
        return Ok(());
    }
    // Deliberately re-checked WITHOUT `inflight`: the refresh above
    // reads real numbers that already include those writes, so adding
    // the tally again would double-count and refuse writes that fit.
    // The tally's whole job is to force this statvfs early enough —
    // it must never be the thing that decides a refusal.
    if headroom_for_refusal(&s) >= need {
        s.note_admitted(need);
        return Ok(());
    }
    meter::bump(Counter::NospcWriteRefusals);
    Err(NoSpace)
}

/// A10 hydration admission (step 11): may an object of `len` bytes be
/// restored right now? False = wait — the watermark pass may be
/// freeing space. Path-scoped like the write admission; headroom
/// already excludes the reserve.
pub fn admit_hydration(path: &Path, len: u64) -> bool {
    let Some(s) = current() else { return true };
    if !path.starts_with(&s.cfg.root) {
        return true;
    }
    s.headroom() >= len || headroom_for_refusal(&s) >= len
}

/// Could an object of `len` bytes EVER be hydrated into this volume?
///
/// This is the question [`admit_hydration`] cannot answer. A refusal
/// there means "not right now" and the caller waits, because the
/// watermark pass may be freeing space this second. But eviction
/// reclaims only what is already local, so for an object larger than
/// the whole filesystem minus the reserve there is nothing to free
/// that would ever help: evicting every other byte still leaves
/// headroom below `len`. Waiting on that is a livelock, and the client
/// sees it as a read that never returns.
///
/// Deliberately measured against `total`, not `avail` — the answer has
/// to be a property of the volume rather than of the moment, or it
/// would flap with ordinary cache pressure and turn a slow read into a
/// spurious ENOSPC. A false here is permanent for as long as the PVC
/// is this size; expanding it makes the same object admissible, which
/// is why the block is re-evaluated on every request rather than
/// cached forever.
pub fn hydration_ever_possible(path: &Path, len: u64) -> bool {
    let Some(s) = current() else { return true };
    if !path.starts_with(&s.cfg.root) {
        return true;
    }
    s.ever_possible(len)
}

/// The gap the warm fill keeps below the eviction watermark: fill and
/// eviction must never share an operating region, or a fill under
/// pressure becomes a hydrate/evict churn loop.
const WARM_MARGIN_PCT: u64 = 2;

/// Warm-fill admission (bulk hydration) — STRICTER than the demand
/// admission on both axes: headroom must cover `len` PLUS the fill's
/// admitted-but-unfinished bytes (`pending` — N blind concurrent
/// admissions would otherwise overshoot by N × object size, and the
/// fill must never eat the demand/write reserve), AND projected usage
/// must stay at or below `watermarkPct − WARM_MARGIN_PCT`. The refresh
/// is unconditional: statvfs is µs-scale at ≤ warm-concurrency calls
/// per restore-duration, and successive admits must see the fill's own
/// landed bytes. False = the fill stops/skips (never waits).
pub fn admit_warm(path: &Path, len: u64, pending: u64) -> bool {
    let Some(s) = current() else { return true };
    if !path.starts_with(&s.cfg.root) {
        return true;
    }
    s.refresh();
    s.admit_warm_after_refresh(len, pending)
}

/// A10 admission for object creation (OPEN-create / CREATE).
pub fn admit_create(path: &Path) -> Result<(), NoSpace> {
    let Some(s) = current() else { return Ok(()) };
    if !path.starts_with(&s.cfg.root) {
        return Ok(());
    }
    if s.headroom() >= CREATE_COST {
        return Ok(());
    }
    if headroom_for_refusal(&s) >= CREATE_COST {
        return Ok(());
    }
    meter::bump(Counter::NospcCreateRefusals);
    Err(NoSpace)
}

/// Step 10's eviction trigger reads this.
pub fn above_watermark() -> bool {
    current().is_some_and(|s| s.above_watermark.load(Ordering::Relaxed))
}

/// Synchronous statvfs refresh of the installed model (the evict pass
/// calls this between files so freed bytes stop it promptly instead
/// of waiting out the 2 s refresher).
pub fn refresh_now() {
    if let Some(s) = current() {
        s.refresh();
    }
}

impl Space {
    fn view(&self) -> View {
        View {
            total_bytes: self.total.load(Ordering::Relaxed),
            free_bytes: self.free.load(Ordering::Relaxed),
            avail_bytes: self.headroom(),
            files_total: self.files_total.load(Ordering::Relaxed),
            files_free: self.files_free.load(Ordering::Relaxed),
            files_avail: self.files_avail.load(Ordering::Relaxed),
        }
    }

    /// `avail − reserve`, saturating: the admission budget.
    fn headroom(&self) -> u64 {
        self.avail_raw
            .load(Ordering::Relaxed)
            .saturating_sub(self.cfg.reserve_bytes)
    }

    /// The pure half of [`admit_warm`] (unit-tested via `force_gauge`
    /// — the public fn's unconditional refresh would overwrite a
    /// forced gauge with real statvfs numbers).
    /// The volume-shaped half of [`hydration_ever_possible`], split out
    /// so it can be tested without installing a global.
    fn ever_possible(&self, len: u64) -> bool {
        self.total
            .load(Ordering::Relaxed)
            .saturating_sub(self.cfg.reserve_bytes)
            >= len
    }

    /// The cached-gauge half of [`admit_bytes`]: does the snapshot
    /// cover this write ON TOP OF everything admitted since it was
    /// taken? Split out so the D8 regression can be exercised without
    /// installing a process-global gauge — the suite runs in parallel.
    fn admits_from_cache(&self, need: u64) -> bool {
        let inflight = self.admitted_since_refresh.load(Ordering::Relaxed);
        self.headroom() >= need.saturating_add(inflight)
    }

    /// Record bytes this admission is about to allow.
    fn note_admitted(&self, need: u64) {
        self.admitted_since_refresh.fetch_add(need, Ordering::Relaxed);
    }

    fn admit_warm_after_refresh(&self, len: u64, pending: u64) -> bool {
        let add = len.saturating_add(pending);
        if self.headroom() < add {
            return false;
        }
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return false;
        }
        let avail = self.avail_raw.load(Ordering::Relaxed);
        let used_after = total.saturating_sub(avail).saturating_add(add);
        let pct_after = used_after.saturating_mul(100) / total;
        pct_after <= (self.cfg.watermark_pct as u64).saturating_sub(WARM_MARGIN_PCT)
    }

    /// One statvfs reading into the gauge + watermark edge log +
    /// ballast management. The refresher task calls this every
    /// [`REFRESH_SECS`]; startup calls it inline (statvfs is
    /// microseconds).
    pub fn refresh(&self) {
        let sv = match nix::sys::statvfs::statvfs(&self.cfg.root) {
            Ok(sv) => sv,
            Err(e) => {
                // Stale gauge beats a poisoned one; admission keeps
                // using the last reading.
                warn!("tier space: statvfs {}: {}", self.cfg.root.display(), e);
                return;
            }
        };
        let frsize = sv.fragment_size() as u64;
        let total = sv.blocks() as u64 * frsize;
        let free = sv.blocks_free() as u64 * frsize;
        let avail = sv.blocks_available() as u64 * frsize;
        self.total.store(total, Ordering::Relaxed);
        self.free.store(free, Ordering::Relaxed);
        self.avail_raw.store(avail, Ordering::Relaxed);
        // This reading already accounts for everything admitted before
        // it, so the in-flight tally starts again from here.
        self.admitted_since_refresh.store(0, Ordering::Relaxed);
        self.files_total.store(sv.files() as u64, Ordering::Relaxed);
        self.files_free.store(sv.files_free() as u64, Ordering::Relaxed);
        self.files_avail.store(sv.files_available() as u64, Ordering::Relaxed);

        // Watermark, edge-triggered both ways.
        let used_pct = (total.saturating_sub(avail))
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(0);
        let above = used_pct >= self.cfg.watermark_pct as u64;
        if above != self.above_watermark.swap(above, Ordering::Relaxed) {
            if above {
                warn!(
                    "tier space: {}% used — above the {}% eviction watermark \
                     (avail {} MiB, reserve {} MiB); eviction lands at step 10",
                    used_pct,
                    self.cfg.watermark_pct,
                    avail / (1024 * 1024),
                    self.cfg.reserve_bytes / (1024 * 1024)
                );
            } else {
                info!(
                    "tier space: back below the {}% watermark ({}% used)",
                    self.cfg.watermark_pct, used_pct
                );
            }
        }

        self.manage_ballast(avail);
    }

    fn release_floor(&self) -> u64 {
        (self.cfg.reserve_bytes / RELEASE_DIVISOR).max(RELEASE_FLOOR_MIN)
    }

    /// Release when critical, re-arm with hysteresis. `avail` is the
    /// RAW figure (reserve not subtracted): by release time admission
    /// has been refusing client writes for a while — what is still
    /// growing is our own bookkeeping, exactly what the ballast's room
    /// is for.
    fn manage_ballast(&self, avail: u64) {
        let Some(bp) = &self.cfg.ballast_path else { return };
        if self.cfg.ballast_bytes == 0 {
            return;
        }
        let present = self.ballast_present.load(Ordering::Relaxed);
        if present && avail < self.release_floor() {
            match std::fs::remove_file(bp) {
                Ok(()) => {
                    self.ballast_present.store(false, Ordering::Relaxed);
                    meter::bump(Counter::BallastReleases);
                    error!(
                        "tier space: CRITICAL — {} MiB available; RELEASED the {} MiB \
                         ballast {} so state.db can keep committing. Free space or \
                         grow the PVC.",
                        avail / (1024 * 1024),
                        self.cfg.ballast_bytes / (1024 * 1024),
                        bp.display()
                    );
                }
                Err(e) => error!("tier space: ballast release {}: {}", bp.display(), e),
            }
        } else if !present && avail > self.cfg.reserve_bytes.saturating_mul(2) {
            match self.write_ballast(bp) {
                Ok(()) => {
                    self.ballast_present.store(true, Ordering::Relaxed);
                    info!(
                        "tier space: ballast re-armed ({} MiB at {})",
                        self.cfg.ballast_bytes / (1024 * 1024),
                        bp.display()
                    );
                }
                Err(e) => warn!("tier space: ballast re-arm {}: {}", bp.display(), e),
            }
        }
    }

    /// Startup ballast creation (idempotent). REAL bytes, not
    /// ftruncate — a sparse ballast reserves nothing.
    fn create_ballast(&self) -> std::io::Result<()> {
        let Some(bp) = &self.cfg.ballast_path else { return Ok(()) };
        if self.cfg.ballast_bytes == 0 {
            return Ok(());
        }
        if let Ok(md) = std::fs::metadata(bp) {
            if md.len() == self.cfg.ballast_bytes {
                self.ballast_present.store(true, Ordering::Relaxed);
                return Ok(());
            }
        }
        self.write_ballast(bp)?;
        self.ballast_present.store(true, Ordering::Relaxed);
        info!(
            "tier space: ballast created ({} MiB at {})",
            self.cfg.ballast_bytes / (1024 * 1024),
            bp.display()
        );
        Ok(())
    }

    fn write_ballast(&self, bp: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(bp)?;
        let chunk = vec![0u8; 1024 * 1024];
        let mut left = self.cfg.ballast_bytes;
        while left > 0 {
            let n = left.min(chunk.len() as u64) as usize;
            f.write_all(&chunk[..n])?;
            left -= n as u64;
        }
        f.sync_all()
    }

    #[cfg(test)]
    fn force_gauge(&self, total: u64, avail_raw: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.free.store(avail_raw, Ordering::Relaxed);
        self.avail_raw.store(avail_raw, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &Path, reserve: u64) -> SpaceConfig {
        SpaceConfig {
            root: root.to_path_buf(),
            reserve_bytes: reserve,
            watermark_pct: 85,
            ballast_path: None,
            ballast_bytes: 0,
        }
    }

    /// Local (uninstalled) instance: pure admission math.
    fn local(cfg_: SpaceConfig) -> Space {
        Space {
            cfg: cfg_,
            total: AtomicU64::new(0),
            free: AtomicU64::new(0),
            avail_raw: AtomicU64::new(0),
            admitted_since_refresh: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            files_free: AtomicU64::new(0),
            files_avail: AtomicU64::new(0),
            above_watermark: AtomicBool::new(false),
            ballast_present: AtomicBool::new(false),
        }
    }

    /// The "can this EVER fit" question is a property of the volume,
    /// not of the moment — so it reads `total`, never `avail`.
    ///
    /// The distinction is the whole point: a full disk with a small
    /// object must answer YES (eviction will free the room and the
    /// caller should wait), while an empty disk with an oversized
    /// object must answer NO (nothing will ever free enough, and a
    /// caller that waits waits forever). An implementation that used
    /// `avail` would inverse both, so this fixes avail at 0 and total
    /// high, where the two answers disagree.
    #[test]
    fn ever_possible_asks_about_the_volume_not_the_moment() {
        let d = tempfile::tempdir().unwrap();
        let s = local(cfg(d.path(), 256));
        s.total.store(1000, Ordering::Relaxed);
        s.avail_raw.store(0, Ordering::Relaxed); // disk is FULL right now

        assert!(s.ever_possible(744), "744 = total - reserve: the largest that fits");
        assert!(
            s.ever_possible(1),
            "a full disk must still say a small object is possible — eviction is what frees it"
        );
        assert!(!s.ever_possible(745), "one byte past the reserve can never fit");
        assert!(!s.ever_possible(u64::MAX));
    }

    /// A reserve larger than the disk saturates to zero rather than
    /// wrapping — the shape that would otherwise admit everything.
    #[test]
    fn ever_possible_saturates_when_the_reserve_exceeds_the_disk() {
        let d = tempfile::tempdir().unwrap();
        let s = local(cfg(d.path(), u64::MAX));
        s.total.store(1000, Ordering::Relaxed);
        assert!(!s.ever_possible(1));
        assert!(s.ever_possible(0), "a zero-length object always fits");
    }

    #[test]
    fn headroom_is_avail_minus_reserve_saturating() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = local(cfg(dir.path(), 100));
        s.force_gauge(1000, 250);
        assert_eq!(s.headroom(), 150);
        s.force_gauge(1000, 40);
        assert_eq!(s.headroom(), 0, "reserve larger than avail saturates to zero");
        assert_eq!(s.view().avail_bytes, 0);
    }

    #[test]
    fn admit_warm_counts_pending_and_honors_margin_and_reserve() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = local(cfg(dir.path(), 100)); // watermark 85 ⇒ margin 83
        s.force_gauge(1000, 500); // used 50%, headroom 400

        assert!(s.admit_warm_after_refresh(100, 0), "plain fit admits");
        assert!(
            s.admit_warm_after_refresh(100, 100),
            "modest pending still fits (projected 70%)"
        );
        assert!(
            !s.admit_warm_after_refresh(100, 350),
            "pending bytes count: 450 total exceeds the 400 headroom"
        );
        assert!(
            !s.admit_warm_after_refresh(401, 0),
            "past headroom = into the demand/write reserve — never"
        );

        // The watermark margin, isolated (no reserve in play).
        let s2 = local(cfg(dir.path(), 0));
        s2.force_gauge(1000, 400); // used 60%, headroom 400
        assert!(
            s2.admit_warm_after_refresh(230, 0),
            "83% projected sits ON the watermark−2 boundary: admit"
        );
        assert!(
            !s2.admit_warm_after_refresh(240, 0),
            "84% projected crosses watermark−2: refuse (eviction fires at 85%)"
        );
    }

    #[test]
    fn real_statvfs_populates_the_gauge() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = local(cfg(dir.path(), 0));
        s.refresh();
        let v = s.view();
        assert!(v.total_bytes > 0, "statvfs must report a real filesystem size");
        assert!(v.avail_bytes > 0 && v.avail_bytes <= v.total_bytes);
        assert!(v.files_total > 0);
    }

    /// D8, the drill leg that put 600 MiB onto 539 MiB free and got
    /// **201 on every chunk** — `df` to 0, the whole reserve gone,
    /// `nospcWriteRefusals` still 0.
    ///
    /// The gauge is a 2 s snapshot and a streaming PUT arrives as many
    /// small WRITEs, so every chunk was measured against a reading that
    /// had not yet seen its predecessors. This asserts the cached half
    /// STOPS admitting once the snapshot's headroom is spoken for —
    /// which is what forces the real statvfs that then refuses.
    #[test]
    fn d8_a_fast_writer_cannot_outrun_the_stale_gauge() {
        const MIB: u64 = 1024 * 1024;
        let dir = tempfile::TempDir::new().unwrap();
        // The drill's exact shape: 539 MiB avail, 256 MiB reserve, so
        // 283 MiB of real headroom.
        let s = local(cfg(dir.path(), 256 * MIB));
        s.force_gauge(1024 * MIB, 539 * MIB);
        assert_eq!(s.headroom(), 283 * MIB);

        // 600 × 1 MiB, and the gauge NEVER refreshes — that is the
        // point, the writer is faster than the refresher.
        let mut admitted = 0u64;
        for _ in 0..600 {
            if !s.admits_from_cache(MIB) {
                break;
            }
            s.note_admitted(MIB);
            admitted += MIB;
        }

        assert_eq!(
            admitted,
            283 * MIB,
            "the cached gauge admitted {admitted} bytes against 283 MiB of headroom — \
             before the in-flight tally this was the full 600 MiB and the reserve was gone"
        );
        assert!(
            !s.admits_from_cache(MIB),
            "once headroom is spoken for the cache must stop admitting, so the next \
             write pays for a real statvfs instead of riding a stale reading"
        );
    }

    /// The tally must never be what refuses a write — only what forces
    /// the statvfs early. A real reading already includes those bytes,
    /// so counting them again would refuse writes that genuinely fit.
    #[test]
    fn the_inflight_tally_resets_on_a_real_reading() {
        const MIB: u64 = 1024 * 1024;
        let dir = tempfile::TempDir::new().unwrap();
        let s = local(cfg(dir.path(), 0));
        s.force_gauge(1024 * MIB, 100 * MIB);
        s.note_admitted(100 * MIB);
        assert!(!s.admits_from_cache(MIB), "headroom is fully spoken for");

        // A real statvfs over the (empty) temp dir: whatever it reads,
        // the tally starts again from it.
        s.refresh();
        assert_eq!(
            s.admitted_since_refresh.load(Ordering::Relaxed),
            0,
            "a fresh reading already accounts for prior admissions"
        );
    }

    #[test]
    fn ballast_lifecycle_create_release_rearm() {
        let dir = tempfile::TempDir::new().unwrap();
        let bp = dir.path().join("flint-ballast.bin");
        let mut c = cfg(dir.path(), 8 * 1024 * 1024);
        c.ballast_path = Some(bp.clone());
        c.ballast_bytes = 2 * 1024 * 1024;
        let s = local(c);
        s.create_ballast().unwrap();
        assert_eq!(std::fs::metadata(&bp).unwrap().len(), 2 * 1024 * 1024);

        // Critical: below the release floor (max(16 MiB, reserve/8) =
        // 16 MiB here) ⇒ released.
        s.manage_ballast(1024 * 1024);
        assert!(!bp.exists(), "critical avail must release the ballast");

        // Hysteresis: not re-armed until avail > 2×reserve.
        s.manage_ballast(12 * 1024 * 1024);
        assert!(!bp.exists(), "re-arm must wait for real recovery");
        s.manage_ballast(64 * 1024 * 1024);
        assert_eq!(
            std::fs::metadata(&bp).unwrap().len(),
            2 * 1024 * 1024,
            "recovered avail re-arms the ballast"
        );
    }

    #[test]
    fn admission_is_scoped_and_refuses_only_past_the_reserve() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("scope-a");
        std::fs::create_dir_all(&root).unwrap();
        let s = configure(cfg(&root, u64::MAX)).unwrap(); // reserve > any disk ⇒ headroom 0
        // Retry-loop discipline: another test may install its own
        // instance concurrently; converge on OUR install.
        let inside = root.join("f.bin");
        let outside = dir.path().join("elsewhere.bin");
        let mut ok = false;
        for _ in 0..50 {
            let refused = admit_bytes(&inside, 1).is_err();
            let outside_admitted = admit_bytes(&outside, u64::MAX / 2).is_ok();
            let create_refused = admit_create(&inside).is_err();
            if refused && outside_admitted && create_refused {
                ok = true;
                break;
            }
            *active().write().unwrap() = Some(Arc::clone(&s));
        }
        assert!(ok, "exhausted headroom must refuse inside the root and only there");
    }
}
