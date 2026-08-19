//! Eviction — L2 step 10 (design review A4/A5, findings C2/C3/C6).
//!
//! The tier's ONLY operation that destroys local bytes, so every line
//! here is ordered around one rule set:
//!
//! - **Full precondition set (A4)** before anything durable: flush
//!   single-flight held, gate exclusion held and drained, capture
//!   epoch empty, durable dirty bit clear, a CRC-carrying generation
//!   row whose key matches the path (no re-key pending), no writable
//!   opens/locks (caller-supplied probe — the ANONYMOUS_STATEID means
//!   enumeration alone can't see all writers, which is why the gate
//!   drain and the marker consults exist regardless), the bucket
//!   object HEAD-verified at the recorded ETag, and the LOCAL bytes
//!   CRC-verified against the generation row. Only gate-produced,
//!   CRC-verified generations are eviction-eligible.
//! - **Marker before truncate (A5/C2)**: the durable `tier_evicted`
//!   row (+ best-effort stub xattr, the stub_binding discipline)
//!   commits FIRST; the truncate is second. A crash between the two
//!   leaves marker+full-file — the startup [`reconcile`] finishes or
//!   rolls it back. A bare 0-byte file with no marker can never be
//!   produced by this state machine.
//! - **Marker set ⇒ excluded from flush** (the flusher checks
//!   [`is_evicted`]) and **every content lane consults the marker**
//!   (READ, WRITE, size-SETATTR, ALLOCATE/DEALLOCATE, COPY, CLONE —
//!   both source and destination sides), answering NFS4ERR_DELAY.
//!   Step 11 turns that DELAY into hydrate-then-serve; until then
//!   nothing evicts automatically — the watermark trigger wires up
//!   WITH hydration, deliberately.
//! - **Capacity returns after confirmation**: bytes are counted
//!   evicted only after the truncate + fsync succeed.
//!
//! In-place truncate keeps the inode alive (C6): every cached fd
//! still names the evicted file, and the per-op marker consults are
//! what make those fds safe — a residual fd's READ/WRITE re-checks
//! the marker on every operation.

use crate::state_backend::{StateBackend, TierEvictedRow};
use crate::tier::capture;
use crate::tier::gate;
use crate::tier::meter::{self, Counter};
use crate::tier::store::{crc64_to_b64, ObjectStore, StoreError};
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use tracing::{debug, error, info, warn};

/// Best-effort marker xattr (stub_binding's discipline: same failure
/// domain as the file). Value: `generation:etag`. The durable row is
/// authoritative; the xattr is defense-in-depth for tree-level
/// forensics, and some export filesystems don't support user xattrs —
/// a set failure logs once per eviction and does not fail it.
pub const EVICTED_XATTR: &str = "user.flint.tier.evicted";

/// The in-memory consult map — mirrors the durable rows, loaded by
/// [`reconcile`] at startup, maintained by evict/rollback/hydrate and
/// by the A7 identity events. Hot-path cost when the tier is off: one
/// relaxed atomic load (capture::enabled) per consult.
#[derive(Debug, Clone)]
pub struct EvictedMeta {
    pub size: u64,
    pub key: String,
    pub generation: u64,
    /// The object version hydration must fetch (each ranged GET pins
    /// it with If-Match).
    pub etag: String,
    /// Expected content CRC (wire form) — hydration verifies the
    /// restored stream against it.
    pub crc64_b64: String,
}

fn markers() -> &'static DashMap<(u64, u64), EvictedMeta> {
    static M: OnceLock<DashMap<(u64, u64), EvictedMeta>> = OnceLock::new();
    M.get_or_init(DashMap::new)
}

/// Marker CYCLE counter — bumped on every marker INSERT, and only on
/// inserts.  The un-gated read lanes' post-I/O re-consult is blind to
/// a COMPLETE evict+hydrate cycle that lands inside the read window
/// (the hydration clears the marker before the re-consult looks —
/// FlintTierMarker's CycleBlind counterexample); this counter is the
/// only evidence such a cycle happened.  Insert-only suffices because
/// of C2's marker-BEFORE-truncate order: every byte-destroying event
/// inside a window is preceded by an in-window insert, while a forget
/// only ever follows a completed fsynced restore — a window containing
/// nothing but forgets read consistent bytes (FlintTierMarker: the
/// strict run holds with CycleOnClear=FALSE; InsertBlind must fail).
/// Bumping on forget too — the original design — was safe but not
/// harmless: a warm fill's completion storm (hundreds of forgets/sec)
/// turned every clear into a spurious DELAY on unrelated reads and a
/// livelock on COPY windows longer than the inter-completion gap.
/// Global rather than per-identity: "unchanged" proves no cycle
/// STARTED anywhere, and a false positive from an unrelated file's
/// eviction costs one DELAY retry — evictions are rare; restores need
/// not be.
static MARKER_CYCLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn bump_cycle() {
    MARKER_CYCLE.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Sample the cycle counter BEFORE a read lane's pre-I/O consult; pass
/// it to [`read_window_intact`] after the I/O.
pub fn marker_cycle() -> u64 {
    MARKER_CYCLE.load(std::sync::atomic::Ordering::Acquire)
}

/// The read-window guard: a pread's bytes may be served only if no
/// marker is visible AND no marker cycle completed since `began` (a
/// cycle inside the window means the bytes may be the stub or a
/// partial restore, whatever the marker says NOW).
pub fn read_window_intact(dev: u64, ino: u64, began: u64) -> bool {
    !is_evicted(dev, ino) && marker_cycle() == began
}

/// [`read_window_intact`] through an open fd (the COPY/CLONE closures
/// hold files, not identities).
pub fn file_read_window_intact(f: &std::fs::File, began: u64) -> bool {
    !file_is_evicted(f) && marker_cycle() == began
}

/// Is this file evicted? Consulted by every content lane BEFORE
/// trusting local size or moving a byte (C2's fix).
#[inline]
pub fn is_evicted(dev: u64, ino: u64) -> bool {
    capture::enabled() && markers().contains_key(&(dev, ino))
}

/// Marker consult through an open fd (the COPY/CLONE/ALLOCATE
/// closures hold files, not identities).
pub fn file_is_evicted(f: &std::fs::File) -> bool {
    if !capture::enabled() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        f.metadata()
            .map(|m| markers().contains_key(&(m.dev(), m.ino())))
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        false
    }
}

/// The evicted file's LOGICAL size — what GETATTR serves (the local
/// file is 0 bytes; reporting that would read as truncation).
pub fn logical_size(dev: u64, ino: u64) -> Option<u64> {
    if !capture::enabled() {
        return None;
    }
    markers().get(&(dev, ino)).map(|m| m.size)
}

/// Drop a marker from the consult map (identity events: the inode was
/// removed or renamed-over; step 11's hydration completion).  Does NOT
/// bump the cycle counter — clears carry no read-window hazard (see
/// MARKER_CYCLE's insert-only rationale), and a warm fill clears
/// markers by the hundreds per second.
pub fn forget(dev: u64, ino: u64) {
    markers().remove(&(dev, ino));
}

/// A12 reporter gauge: currently-evicted (files, logical bytes).
/// Process-global like the map itself.
pub fn marker_stats() -> (usize, u64) {
    let m = markers();
    (m.len(), m.iter().map(|e| e.size).sum())
}

/// The full marker (hydration's work order).
pub(crate) fn marker_meta(dev: u64, ino: u64) -> Option<EvictedMeta> {
    markers().get(&(dev, ino)).map(|m| m.clone())
}

/// Hydration updates the marker in place on a foreign-overwrite adopt
/// (S3-wins: the bucket's CURRENT object becomes the restore target).
pub(crate) fn update_marker(dev: u64, ino: u64, meta: EvictedMeta) {
    markers().insert((dev, ino), meta);
}

/// Install the marker for a stub the foreign-key sweep is about to make
/// visible.
///
/// Two things about this are deliberate.
///
/// **It happens BEFORE the name is linked.** The sweep runs behind a
/// live listener, so the instant a stub's name appears a client may
/// GETATTR or READ it. Every one of those paths answers from this map.
/// A stub whose rows exist but whose marker does not reads as an
/// ordinary 0-byte file: GETATTR reports size 0, `cat` returns EOF with
/// no error, and — the part that destroys data — the first small WRITE
/// publishes over the real object under an If-Match that SUCCEEDS,
/// because the generation row's etag is genuinely the bucket's current
/// one. A 10 GiB object becomes 4 KiB and every copy of the original is
/// gone. Pre-listener the same code was safe only because
/// `evict::reconcile` loaded the markers before any client existed.
///
/// **It does NOT bump the cycle counter.** MARKER_CYCLE exists to catch
/// byte-DESTROYING events mid-read: a reader that sampled the counter,
/// read some bytes, and must learn its file was truncated underneath.
/// A sweep insert destroys nothing — the name did not exist a moment
/// ago, so no read can be in flight against it — and bumping would
/// storm every concurrent reader in the volume with spurious DELAYs and
/// livelock the COPY/CLONE windows. On a 200k-object sweep that is
/// 200k false invalidations.
pub fn insert_marker_for_import(dev: u64, ino: u64, meta: EvictedMeta) {
    markers().insert((dev, ino), meta);
}

#[cfg(test)]
pub(crate) fn install_marker_for_tests(dev: u64, ino: u64, size: u64) {
    markers().insert(
        (dev, ino),
        EvictedMeta {
            size,
            key: String::new(),
            generation: 0,
            etag: String::new(),
            crc64_b64: String::new(),
        },
    );
    bump_cycle();
}

// ── refusals ─────────────────────────────────────────────────────────

/// Why an eviction did not happen. Refusals are the NORMAL outcome —
/// eviction is opportunistic; every refusal leaves the file exactly as
/// it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    TierOff,
    AlreadyEvicted,
    /// A flush is running on this file right now.
    Busy,
    /// Captured intervals / queued marks / the durable bit — dirty in
    /// any form.
    Dirty,
    /// No generation row: never published.
    NoGeneration,
    /// Generation row without a CRC — not gate-produced/verifiable;
    /// never eviction-eligible (A4).
    NotEligible,
    /// The row's key differs from the path-derived key: a bucket
    /// re-key is pending; flush first.
    RekeyPending,
    /// The caller's open-state probe reports writable opens,
    /// delegations, or byte-range locks (open-hot files are
    /// non-evictable, A4).
    OpenWriters,
    /// HEAD found no object at the recorded key.
    ObjectMissing,
    /// HEAD found an object whose ETag differs from the generation row
    /// — foreign interference; the flusher's local-wins machinery owns
    /// this, eviction must not.
    ForeignObject,
    /// Local bytes no longer match the published generation's CRC.
    /// With capture honest this should be unreachable — its appearance
    /// means a capture miss, and refusing eviction is what keeps that
    /// bug non-destructive.
    CrcMismatch,
    Io(String),
}

#[derive(Debug)]
pub enum EvictOutcome {
    Evicted { bytes: u64 },
    Refused(Refusal),
}

// ── test failpoint (the C2 crash drill) ──────────────────────────────

#[cfg(test)]
static FAIL_AFTER_ROW: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);

/// The next evict_file OF THIS IDENTITY "crashes" (returns) right
/// after the durable marker commit, before the truncate — the exact
/// C2 window. Targeted, so parallel tests' evictions never consume
/// each other's injections.
#[cfg(test)]
pub(crate) fn fail_after_row_once(dev: u64, ino: u64) {
    *FAIL_AFTER_ROW.lock().unwrap() = Some((dev, ino));
}

#[allow(unused_variables)]
fn take_fail_after_row(dev: u64, ino: u64) -> bool {
    #[cfg(test)]
    {
        let mut g = FAIL_AFTER_ROW.lock().unwrap();
        if *g == Some((dev, ino)) {
            *g = None;
            return true;
        }
        false
    }
    #[cfg(not(test))]
    {
        false
    }
}

// ── the state machine ────────────────────────────────────────────────

/// Evict one file: verify the full precondition set, durably commit
/// the marker, truncate in place. `expected_key` is the path-derived
/// bucket key (from `FlushOrchestrator::key_for`); `writable_open_probe`
/// answers "does NFS state show writable opens/delegations/locks for
/// this identity" (server wiring supplies state_mgr; `|_, _| false`
/// only in tests).
pub async fn evict_file(
    backend: &Arc<dyn StateBackend>,
    store: &Arc<dyn ObjectStore>,
    path: &Path,
    expected_key: &str,
    writable_open_probe: &(dyn Fn(u64, u64) -> bool + Sync),
) -> EvictOutcome {
    if !capture::enabled() {
        return EvictOutcome::Refused(Refusal::TierOff);
    }
    let md = match path.symlink_metadata() {
        Ok(m) => m,
        Err(e) => return EvictOutcome::Refused(Refusal::Io(format!("stat: {}", e))),
    };
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (md.dev(), md.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0u64, 0u64);

    if markers().contains_key(&(dev, ino)) {
        return EvictOutcome::Refused(Refusal::AlreadyEvicted);
    }

    // Single-flight vs the flusher: never race a running flush.
    let Some(_flight) = gate::try_begin_flush(dev, ino) else {
        return EvictOutcome::Refused(Refusal::Busy);
    };

    // A4: exclusion — drains in-flight tickets, refuses every entrant
    // (all write lanes answer DELAY) until dropped at the END of this
    // function. The marker lands in the consult map BEFORE the guard
    // drops, so there is no window in which a write could slip in
    // between drain and marker.
    let _excl = gate::exclude(dev, ino);

    // Clean in EVERY form: swept epoch empty, nothing queued, durable
    // bit clear. (take_epoch under exclusion is atomic-with-drain by
    // construction — in_flight is 0 and entrants are refused.)
    if let Some(epoch) = capture::take_epoch(dev, ino) {
        let dirty = epoch.is_dirty();
        capture::merge_back(dev, ino, epoch);
        if dirty {
            meter::bump(Counter::EvictRefusedDirty);
            return EvictOutcome::Refused(Refusal::Dirty);
        }
    }
    if capture::is_queued(dev, ino) {
        meter::bump(Counter::EvictRefusedDirty);
        return EvictOutcome::Refused(Refusal::Dirty);
    }
    match backend.tier_list_dirty().await {
        Ok(rows) => {
            if rows.iter().any(|r| r.dev == dev && r.ino == ino) {
                meter::bump(Counter::EvictRefusedDirty);
                return EvictOutcome::Refused(Refusal::Dirty);
            }
        }
        Err(e) => return EvictOutcome::Refused(Refusal::Io(format!("dirty list: {}", e))),
    }

    // A generation row with a CRC, living at the path's key.
    let gen_row = match backend.tier_list_generations().await {
        Ok(rows) => rows.into_iter().find(|r| r.dev == dev && r.ino == ino),
        Err(e) => return EvictOutcome::Refused(Refusal::Io(format!("gen rows: {}", e))),
    };
    let Some(gen_row) = gen_row else {
        meter::bump(Counter::EvictRefusedPolicy);
        return EvictOutcome::Refused(Refusal::NoGeneration);
    };
    let Some(row_crc) = gen_row.crc64_b64.clone() else {
        meter::bump(Counter::EvictRefusedPolicy);
        return EvictOutcome::Refused(Refusal::NotEligible);
    };
    if gen_row.key != expected_key {
        meter::bump(Counter::EvictRefusedPolicy);
        return EvictOutcome::Refused(Refusal::RekeyPending);
    }

    // Open-hot files are non-evictable (A4).
    if writable_open_probe(dev, ino) {
        meter::bump(Counter::EvictRefusedPolicy);
        return EvictOutcome::Refused(Refusal::OpenWriters);
    }

    // The bucket object must be OUR generation, still.
    match store.head(&gen_row.key).await {
        Ok(meta) => {
            if meta.etag != gen_row.etag {
                meter::bump(Counter::EvictRefusedVerify);
                return EvictOutcome::Refused(Refusal::ForeignObject);
            }
        }
        Err(StoreError::NotFound(_)) => {
            meter::bump(Counter::EvictRefusedVerify);
            return EvictOutcome::Refused(Refusal::ObjectMissing);
        }
        Err(e) => return EvictOutcome::Refused(Refusal::Io(format!("head: {}", e))),
    }

    // Local bytes must equal the published generation — size first
    // (cheap), then the full CRC. Under exclusion, so this is a true
    // point-in-time verification.
    if md.len() != gen_row.size {
        meter::bump(Counter::EvictRefusedVerify);
        return EvictOutcome::Refused(Refusal::CrcMismatch);
    }
    match crate::tier::flush::file_crc(path).await {
        Ok(local) if crc64_to_b64(local) == row_crc => {}
        Ok(_) => {
            meter::bump(Counter::EvictRefusedVerify);
            error!(
                "tier evict: {} local bytes DIVERGE from published gen {} — a capture \
                 miss somewhere; refusing eviction keeps it non-destructive",
                path.display(),
                gen_row.generation
            );
            return EvictOutcome::Refused(Refusal::CrcMismatch);
        }
        Err(e) => return EvictOutcome::Refused(Refusal::Io(e)),
    }

    // ── C2 order: marker durable FIRST ───────────────────────────────
    let row = TierEvictedRow {
        dev,
        ino,
        key: gen_row.key.clone(),
        generation: gen_row.generation,
        etag: gen_row.etag.clone(),
        crc64_b64: row_crc,
        size: gen_row.size,
        path: path.to_string_lossy().into_owned(),
        evicted_unix: now_unix(),
        hydrating_unix: None,
    };
    if let Err(e) = backend.tier_put_evicted(&row).await {
        return EvictOutcome::Refused(Refusal::Io(format!("marker row: {}", e)));
    }
    if let Err(e) = set_xattr(path, EVICTED_XATTR, format!("{}:{}", row.generation, row.etag).as_bytes())
    {
        // Best-effort (xattr-less export filesystems boot fine in
        // lite); the durable row is the authority.
        warn!("tier evict: marker xattr on {}: {} (row is authoritative)", path.display(), e);
    }

    // Consult-map marker BEFORE the truncate — the RAM mirror of C2's
    // durable order. The truncate below includes an fsync (multi-ms):
    // inserting the marker after it would leave a window where the
    // file is already a 0-byte stub but no consult can see a marker —
    // GETATTR serves size 0 and READs serve the empty stub as content
    // (the chaos drill's endurance phase caught git reading empty
    // objects/refs in exactly that window; READs and GETATTRs
    // deliberately take no gate ticket, so the marker's visibility is
    // their ONLY protection). Between insert and truncate the marker
    // merely answers DELAY on a still-intact file — a hydrator racing
    // in bounces off this eviction's gate exclusion and retries after
    // the stub is real.
    markers().insert(
        (dev, ino),
        EvictedMeta {
            size: row.size,
            key: row.key.clone(),
            generation: row.generation,
            etag: row.etag.clone(),
            crc64_b64: row.crc64_b64.clone(),
        },
    );
    bump_cycle();

    if take_fail_after_row(dev, ino) {
        // TEST-ONLY simulated crash in the C2 window: marker durable,
        // bytes intact. The reconciler owns this state.
        return EvictOutcome::Refused(Refusal::Io("injected crash after marker".into()));
    }

    // ── destroy local bytes ──────────────────────────────────────────
    match truncate_in_place(path) {
        Ok(()) => {}
        Err(e) => {
            // Marker is durable but bytes remain — exactly the state
            // the reconciler repairs at next startup; repair it now.
            warn!("tier evict: truncate {} failed: {} — rolling the marker back", path.display(), e);
            forget(dev, ino);
            let _ = backend.tier_delete_evicted(dev, ino).await;
            remove_xattr_best_effort(path);
            return EvictOutcome::Refused(Refusal::Io(format!("truncate: {}", e)));
        }
    }

    // Capacity returns only now — after the destructive step is
    // confirmed complete.
    meter::bump(Counter::FilesEvicted);
    meter::add(Counter::BytesEvicted, row.size);
    info!(
        "tier evict: {} → {} (gen {}, {} bytes) — local bytes released",
        path.display(),
        row.key,
        row.generation,
        row.size
    );
    EvictOutcome::Evicted { bytes: row.size }
}

/// Open a file for the tier's own maintenance writes (evict truncate,
/// hydration restore). This is NOT client I/O — DAC on the file's mode
/// must not apply (a 0444 git object is the proof workload: the owner
/// cannot open it O_WRONLY, yet evicting and restoring it is exactly
/// the tier's job; the MinIO drill caught hydration wedged forever on
/// one). On EACCES: grant owner-write for the instant of the open and
/// restore the mode immediately — the open fd keeps its write
/// permission regardless of the file's mode.
pub(crate) fn open_for_internal_write(path: &Path) -> std::io::Result<std::fs::File> {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = path.symlink_metadata()?.permissions().mode() & 0o7777;
                std::fs::set_permissions(
                    path,
                    std::fs::Permissions::from_mode(mode | 0o200),
                )?;
                let r = std::fs::OpenOptions::new().write(true).open(path);
                // Restore UNCONDITIONALLY — the mode was only borrowed.
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
                r
            }
            #[cfg(not(unix))]
            Err(e)
        }
        r => r,
    }
}

/// In-place: open WITHOUT create (an absent file must never
/// materialize — C6), set_len(0), fsync. The inode survives, so every
/// cached fd stays a valid handle whose ops re-check the marker.
fn truncate_in_place(path: &Path) -> std::io::Result<()> {
    let f = open_for_internal_write(path)?;
    f.set_len(0)?;
    f.sync_all()
}

// ── startup reconciler (C2: finish or roll back half-evictions) ──────

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Rows whose file was already 0 bytes — markers loaded.
    pub loaded: usize,
    /// Marker-durable, bytes-intact (the C2 crash window): CRC
    /// re-verified, truncate FINISHED.
    pub finished: usize,
    /// Marker present but local bytes diverge from the recorded CRC —
    /// local wins: marker rolled back, file untouched.
    pub rolled_back: usize,
    /// Step 11: a hydration crashed AFTER its restore completed but
    /// before the marker delete — CRC-verified complete, marker
    /// cleared, bytes kept.
    pub hydrations_finished: usize,
    /// Step 11: a hydration crashed mid-restore — the partial bytes
    /// are garbage (the bucket is the truth): truncated back to the
    /// stub, flag cleared, still evicted.
    pub hydrations_reset: usize,
    /// Rows whose path no longer resolves to their identity — kept,
    /// warned (step 12's hygiene owns them).
    pub orphaned: usize,
}

/// Runs at startup (before the listener binds): load every marker row,
/// finishing or rolling back half-evictions. The consult map is
/// rebuilt from scratch — the rows are the truth.
pub async fn reconcile(backend: &Arc<dyn StateBackend>) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    let rows = match backend.tier_list_evicted().await {
        Ok(r) => r,
        Err(e) => {
            error!("tier evict: cannot list markers: {} — consult map stays empty; nothing served wrong, nothing evicted", e);
            return report;
        }
    };
    // NO map clear here: at real startup the map is empty anyway, and
    // arms below insert/forget per row — while in the test universe a
    // clear would wipe markers belonging to OTHER backends (the
    // one-backend production assumption doesn't hold there).
    for row in rows {
        let path = std::path::PathBuf::from(&row.path);
        #[cfg(unix)]
        let resolved = path.symlink_metadata().ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            (m.dev(), m.ino(), m.len())
        });
        #[cfg(not(unix))]
        let resolved: Option<(u64, u64, u64)> = None;
        let meta_of = |row: &TierEvictedRow| EvictedMeta {
            size: row.size,
            key: row.key.clone(),
            generation: row.generation,
            etag: row.etag.clone(),
            crc64_b64: row.crc64_b64.clone(),
        };
        match resolved {
            // ── step 11: a hydration was in flight when we died ──────
            Some((d, i, len)) if d == row.dev && i == row.ino && row.hydrating_unix.is_some() => {
                let complete = len == row.size
                    && matches!(
                        crate::tier::flush::file_crc(&path).await,
                        Ok(crc) if crc64_to_b64(crc) == row.crc64_b64
                    );
                if complete {
                    // Restore finished; only the marker delete was
                    // lost. Finish it — bytes are verified.
                    let _ = backend.tier_delete_evicted(row.dev, row.ino).await;
                    remove_xattr_best_effort(&path);
                    forget(row.dev, row.ino);
                    report.hydrations_finished += 1;
                    info!(
                        "tier evict reconcile: hydration of {} had completed — marker \
                         cleared, bytes kept",
                        path.display()
                    );
                } else {
                    // Partial restore = garbage; the bucket is the
                    // truth. Back to the stub; hydration re-runs on
                    // demand.
                    match truncate_in_place(&path) {
                        Ok(()) => {
                            let _ = backend.tier_set_hydrating(row.dev, row.ino, None).await;
                            markers().insert((row.dev, row.ino), meta_of(&row));
                    bump_cycle();
                            report.hydrations_reset += 1;
                            warn!(
                                "tier evict reconcile: crashed hydration of {} — partial \
                                 bytes truncated back to the stub (bucket remains truth)",
                                path.display()
                            );
                        }
                        Err(e) => {
                            // Cannot restore the stub shape: leave the
                            // row + flag; ops keep parking; retried at
                            // next startup.
                            error!(
                                "tier evict reconcile: cannot reset crashed hydration of {}: {}",
                                path.display(),
                                e
                            );
                            markers().insert((row.dev, row.ino), meta_of(&row));
                    bump_cycle();
                            report.hydrations_reset += 1;
                        }
                    }
                }
            }
            Some((d, i, len)) if d == row.dev && i == row.ino => {
                if len == 0 {
                    markers().insert((row.dev, row.ino), meta_of(&row));
                    bump_cycle();
                    report.loaded += 1;
                } else {
                    // The C2 window: marker committed, truncate never
                    // ran. Local bytes must still equal the published
                    // generation before we destroy them.
                    match crate::tier::flush::file_crc(&path).await {
                        Ok(crc)
                            if crc64_to_b64(crc) == row.crc64_b64 && len == row.size =>
                        {
                            match truncate_in_place(&path) {
                                Ok(()) => {
                                    markers().insert((row.dev, row.ino), meta_of(&row));
                    bump_cycle();
                                    report.finished += 1;
                                    info!(
                                        "tier evict reconcile: finished the half-eviction of {} \
                                         ({} bytes released)",
                                        path.display(),
                                        row.size
                                    );
                                }
                                Err(e) => {
                                    // Neither finished nor safe to
                                    // trust the marker at runtime —
                                    // roll back; the file is whole.
                                    warn!(
                                        "tier evict reconcile: cannot finish truncate of {} ({}); \
                                         rolling the marker back",
                                        path.display(),
                                        e
                                    );
                                    let _ = backend.tier_delete_evicted(row.dev, row.ino).await;
                                    remove_xattr_best_effort(&path);
                                    forget(row.dev, row.ino);
                                    report.rolled_back += 1;
                                }
                            }
                        }
                        _ => {
                            // Diverged (or unreadable): LOCAL WINS —
                            // the marker dies, the file stays, and the
                            // dirty machinery owns any re-publish.
                            warn!(
                                "tier evict reconcile: {} no longer matches its published \
                                 generation — marker ROLLED BACK, local bytes kept",
                                path.display()
                            );
                            let _ = backend.tier_delete_evicted(row.dev, row.ino).await;
                            remove_xattr_best_effort(&path);
                            forget(row.dev, row.ino);
                            report.rolled_back += 1;
                        }
                    }
                }
            }
            _ => {
                // Path no longer names this identity. Keep the row (the
                // bucket object it names is still real); nothing local
                // to serve or truncate.
                warn!(
                    "tier evict reconcile: marker for ({}, {}) no longer resolves at {} — kept, \
                     unserved",
                    row.dev, row.ino, row.path
                );
                report.orphaned += 1;
            }
        }
    }
    if report != ReconcileReport::default() {
        info!(
            "tier evict reconcile: {} loaded, {} finished, {} rolled back, {} orphaned",
            report.loaded, report.finished, report.rolled_back, report.orphaned
        );
    }
    report
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── xattr helpers (stub_binding's libc pattern) ──────────────────────

#[cfg(target_os = "linux")]
pub(crate) fn set_xattr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    let n = std::ffi::CString::new(name).unwrap();
    let rc = unsafe {
        libc::setxattr(p.as_ptr(), n.as_ptr(), value.as_ptr() as *const libc::c_void, value.len(), 0)
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
pub(crate) fn set_xattr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    let n = std::ffi::CString::new(name).unwrap();
    let rc = unsafe {
        libc::setxattr(p.as_ptr(), n.as_ptr(), value.as_ptr() as *const libc::c_void, value.len(), 0, 0)
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn set_xattr(_path: &Path, _name: &str, _value: &[u8]) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn remove_xattr_best_effort(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let (Ok(p), Ok(n)) = (
        std::ffi::CString::new(path.as_os_str().as_bytes()),
        std::ffi::CString::new(EVICTED_XATTR),
    ) {
        unsafe { libc::removexattr(p.as_ptr(), n.as_ptr()) };
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn remove_xattr_best_effort(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let (Ok(p), Ok(n)) = (
        std::ffi::CString::new(path.as_os_str().as_bytes()),
        std::ffi::CString::new(EVICTED_XATTR),
    ) {
        unsafe { libc::removexattr(p.as_ptr(), n.as_ptr(), 0) };
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn remove_xattr_best_effort(_path: &Path) {}

// ── the watermark-driven pass (step 11 wires it into serve()) ────────

/// Evict eligible clean files, oldest-published first, while `should`
/// keeps answering true (production: `space::above_watermark()` after
/// a forced refresh — so freed bytes stop the pass promptly).
/// Candidate paths derive from the generation rows' keys (a clean
/// file has no dirty row to carry its path); a row whose key no
/// longer names its identity (rename pending) refuses inside
/// `evict_file` and is skipped.
pub async fn evict_pass(
    backend: &Arc<dyn StateBackend>,
    store: &Arc<dyn ObjectStore>,
    export_root: &Path,
    key_prefix: &str,
    writable_open_probe: &(dyn Fn(u64, u64) -> bool + Sync),
    should: &(dyn Fn() -> bool + Sync),
) -> usize {
    let mut rows = match backend.tier_list_generations().await {
        Ok(r) => r,
        Err(e) => {
            warn!("tier evict pass: cannot list generations: {}", e);
            return 0;
        }
    };
    // Oldest-published first — the closest thing to cold-first the
    // rows can say without atime tracking.
    rows.sort_by_key(|r| r.updated_unix);
    let mut evicted = 0usize;
    for row in rows {
        if !should() {
            break;
        }
        if row.crc64_b64.is_none() || is_evicted(row.dev, row.ino) {
            continue;
        }
        let Some(rel) = row.key.strip_prefix(key_prefix) else { continue };
        let path = export_root.join(rel);
        match evict_file(backend, store, &path, &row.key, writable_open_probe).await {
            EvictOutcome::Evicted { bytes } => {
                evicted += 1;
                meter::bump(Counter::AutoEvictions);
                info!(
                    "tier evict pass: {} evicted ({} bytes) — watermark pressure",
                    path.display(),
                    bytes
                );
            }
            EvictOutcome::Refused(r) => {
                debug!("tier evict pass: {} refused: {:?}", path.display(), r);
            }
        }
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::memory::MemoryBackend;
    use crate::state_backend::TierDirtyEntry;
    use crate::tier::capture::Mutation;
    use crate::tier::flush::{FlushConfig, FlushOrchestrator, Outcome};
    use crate::tier::store::memory::MemoryStore;
    use std::path::PathBuf;
    use std::time::Duration;

    const NO_WRITERS: &(dyn Fn(u64, u64) -> bool + Sync) = &|_, _| false;

    /// FlintTierMarker's CycleBlind counterexample, at helper level: a
    /// COMPLETE marker cycle (evict then hydrate) inside a read window
    /// leaves NO marker for the post-read re-consult to see — only the
    /// cycle counter betrays it, via the cycle's INSERT half (forgets
    /// no longer bump — the insert-only refinement).  (The counter is
    /// process-global, so asserts here are monotone-safe under
    /// parallel tests: the false direction is certain, the true
    /// direction retries.)
    #[test]
    fn read_window_guard_detects_a_complete_marker_cycle() {
        capture::force_enable();
        let (dev, ino) = (0xF11A7_u64, 0xC1C1E_u64); // synthetic identity
        let began = marker_cycle();

        install_marker_for_tests(dev, ino, 10);
        forget(dev, ino); // the cycle completes: marker gone again

        assert!(!is_evicted(dev, ino), "the naive re-consult would pass");
        assert!(
            marker_cycle() > began,
            "the cycle's insert is the only evidence the cycle happened"
        );
        assert!(
            !read_window_intact(dev, ino, began),
            "the guard must refuse a window a full cycle passed through"
        );

        // Positive direction: a quiet window serves (retry absorbs
        // unrelated parallel tests' cycles).
        let mut served = false;
        for _ in 0..200 {
            let fresh = marker_cycle();
            if read_window_intact(dev, ino, fresh) {
                served = true;
                break;
            }
        }
        assert!(served, "a quiet window must serve");
    }

    /// The insert-only refinement's own contract (the warm fill's
    /// license to clear markers by the hundreds per second): a window
    /// that contains ONLY a forget stays intact — the counter moves on
    /// inserts alone.  Retry absorbs unrelated parallel tests' inserts
    /// (a bumping forget would fail every iteration by its own +1, so
    /// a single success is proof).
    #[test]
    fn read_window_survives_a_forget_alone() {
        capture::force_enable();
        let (dev, ino) = (0xF11A8_u64, 0xF09E7_u64); // synthetic identity
        let mut proven = false;
        for _ in 0..200 {
            install_marker_for_tests(dev, ino, 10); // insert, pre-window
            let began = marker_cycle(); // window opens: marker present
            forget(dev, ino); // restore completion inside the window
            if read_window_intact(dev, ino, began) {
                proven = true;
                break;
            }
        }
        assert!(
            proven,
            "a forget alone must never break a read window (insert-only evidence)"
        );
    }

    struct Rig {
        _dir: tempfile::TempDir,
        root: PathBuf,
        mem: Arc<MemoryStore>,
        backend: Arc<dyn StateBackend>,
        orch: FlushOrchestrator,
    }

    fn rig() -> Rig {
        capture::force_enable();
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let mem = Arc::new(MemoryStore::new());
        let backend: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let mut cfg = FlushConfig::new(root.clone(), "t/".into());
        cfg.floor = Duration::ZERO;
        cfg.quiesce = Duration::ZERO;
        cfg.whole_put_max = 1024;
        cfg.part_floor = 256;
        let store_dyn: Arc<dyn ObjectStore> = mem.clone();
        let orch = FlushOrchestrator::new(
            store_dyn,
            backend.clone(),
            cfg,
            crate::tier::epoch::EpochGuard::held(1),
        );
        Rig { _dir: dir, root, mem, backend, orch }
    }

    fn ident(path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let m = path.symlink_metadata().unwrap();
        (m.dev(), m.ino())
    }

    fn store_of(r: &Rig) -> Arc<dyn ObjectStore> {
        r.mem.clone()
    }

    /// Note + land the durable row (theft-repair: the capture queue is
    /// process-global; a parallel test's dispatcher can steal our
    /// mark into its backend).
    async fn note_and_land(r: &Rig, path: &Path, m: Mutation) {
        let (dev, ino) = ident(path);
        capture::note_path(path, m);
        for _ in 0..50 {
            let _ = crate::tier::durable::drain_pending(&r.backend).await;
            let landed = r
                .backend
                .tier_list_dirty()
                .await
                .unwrap()
                .iter()
                .any(|x| x.dev == dev && x.ino == ino && x.path.is_some());
            if landed {
                return;
            }
            capture::clear_durable(dev, ino);
            capture::note_path(path, m);
        }
        panic!("dirty row never landed");
    }

    /// Create, capture-forget residue (the ext4 ino-reuse lesson),
    /// note, land, publish. Returns (dev, ino, key).
    async fn published_file(r: &Rig, name: &str, content: &[u8]) -> (u64, u64, String) {
        let f = r.root.join(name);
        std::fs::write(&f, content).unwrap();
        let (dev, ino) = ident(&f);
        capture::forget(dev, ino);
        forget(dev, ino);
        note_and_land(r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        let g = r
            .orch
            .generation_of(dev, ino)
            .expect("publish must land a generation");
        (dev, ino, g.key)
    }

    #[tokio::test]
    async fn evict_happy_path_marker_stub_and_lane_refusals() {
        let r = rig();
        let (dev, ino, key) = published_file(&r, "victim.bin", b"sixteen bytes!!!").await;
        let f = r.root.join("victim.bin");

        let out = evict_file(&r.backend, &store_of(&r), &f, &key, NO_WRITERS).await;
        let EvictOutcome::Evicted { bytes } = out else {
            panic!("clean published file must evict, got {:?}", out);
        };
        assert_eq!(bytes, 16);
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 0, "local bytes released");
        assert!(is_evicted(dev, ino));
        assert_eq!(logical_size(dev, ino), Some(16), "GETATTR serves the LOGICAL size");
        assert_eq!(r.backend.tier_list_evicted().await.unwrap().len(), 1);
        // The bucket object is untouched.
        let meta = r.mem.head(&key).await.unwrap();
        assert_eq!(meta.size, 16);

        // C2: marker-set ⇒ unconditionally excluded from flush — even
        // with a (synthetic) dirty bit, nothing may upload the stub.
        let row = TierDirtyEntry {
            dev,
            ino,
            path: Some(f.to_string_lossy().into_owned()),
            dirtied_unix: 1,
            mark_seq: u64::MAX,
        };
        assert!(matches!(r.orch.flush_file(&row).await, Outcome::SkippedEvicted));
        assert_eq!(r.mem.head(&key).await.unwrap().size, 16, "stub never uploaded");

        // Double-evict refuses.
        assert!(matches!(
            evict_file(&r.backend, &store_of(&r), &f, &key, NO_WRITERS).await,
            EvictOutcome::Refused(Refusal::AlreadyEvicted)
        ));
        forget(dev, ino);
    }

    #[tokio::test]
    async fn eviction_refusal_matrix() {
        let r = rig();
        let store = store_of(&r);

        // Dirty (queued mark, never flushed).
        let d = r.root.join("dirty.bin");
        std::fs::write(&d, b"dirty").unwrap();
        let (dd, di) = ident(&d);
        capture::forget(dd, di);
        capture::note_path(&d, Mutation::Whole);
        let out = evict_file(&r.backend, &store, &d, "t/dirty.bin", NO_WRITERS).await;
        assert!(
            matches!(out, EvictOutcome::Refused(Refusal::Dirty)),
            "queued mark must refuse: {:?}",
            out
        );

        // No generation (clean but never published).
        let n = r.root.join("nogen.bin");
        std::fs::write(&n, b"nogen").unwrap();
        let (nd, ni) = ident(&n);
        capture::forget(nd, ni);
        assert!(matches!(
            evict_file(&r.backend, &store, &n, "t/nogen.bin", NO_WRITERS).await,
            EvictOutcome::Refused(Refusal::NoGeneration)
        ));

        // Re-key pending: expected key differs from the row's key.
        let (pd, pi, _key) = published_file(&r, "rekey.bin", b"rekey contents!!").await;
        let p = r.root.join("rekey.bin");
        assert!(matches!(
            evict_file(&r.backend, &store, &p, "t/RENAMED.bin", NO_WRITERS).await,
            EvictOutcome::Refused(Refusal::RekeyPending)
        ));

        // Open writers (probe says yes).
        let writers: &(dyn Fn(u64, u64) -> bool + Sync) = &|_, _| true;
        let (_, _, k2) = (pd, pi, ());
        let _ = k2;
        let g = r.orch.generation_of(pd, pi).unwrap();
        assert!(matches!(
            evict_file(&r.backend, &store, &p, &g.key, writers).await,
            EvictOutcome::Refused(Refusal::OpenWriters)
        ));

        // Busy: a "flush" holds the single-flight.
        let ticket = gate::try_begin_flush(pd, pi).expect("free");
        assert!(matches!(
            evict_file(&r.backend, &store, &p, &g.key, NO_WRITERS).await,
            EvictOutcome::Refused(Refusal::Busy)
        ));
        drop(ticket);
    }

    #[tokio::test]
    async fn eviction_refuses_foreign_object_and_diverged_local_bytes() {
        let r = rig();
        let store = store_of(&r);

        // Foreign: the bucket object was overwritten by someone else.
        let (fd, fi, key) = published_file(&r, "foreign.bin", b"original 16 byte").await;
        r.mem.raw_put(&key, bytes::Bytes::from_static(b"foreign overwrite"), vec![]);
        let f = r.root.join("foreign.bin");
        assert!(matches!(
            evict_file(&r.backend, &store, &f, &key, NO_WRITERS).await,
            EvictOutcome::Refused(Refusal::ForeignObject)
        ));
        let _ = (fd, fi);

        // Diverged: same length, different local bytes — a capture
        // miss shape. The CRC verify is what keeps it non-destructive.
        let (_, _, key2) = published_file(&r, "diverge.bin", b"published bytes!").await;
        let g = r.root.join("diverge.bin");
        std::fs::write(&g, b"TAMPERED bytes!!").unwrap(); // same 16 bytes
        assert!(matches!(
            evict_file(&r.backend, &store, &g, &key2, NO_WRITERS).await,
            EvictOutcome::Refused(Refusal::CrcMismatch)
        ));
        assert_eq!(
            std::fs::read(&g).unwrap(),
            b"TAMPERED bytes!!",
            "a refused eviction must leave the file untouched"
        );
    }

    /// THE C2 DRILL: crash between the durable marker and the
    /// truncate. The reconciler must FINISH the eviction (CRC still
    /// matches), and until it runs the file is full-length — never a
    /// bare 0-byte stub.
    #[tokio::test]
    async fn crash_between_marker_and_truncate_reconciler_finishes() {
        let r = rig();
        let store = store_of(&r);
        let (dev, ino, key) = published_file(&r, "c2.bin", b"the c2 window!!!").await;
        let f = r.root.join("c2.bin");

        fail_after_row_once(dev, ino);
        let out = evict_file(&r.backend, &store, &f, &key, NO_WRITERS).await;
        assert!(matches!(out, EvictOutcome::Refused(Refusal::Io(_))));
        assert_eq!(
            std::fs::metadata(&f).unwrap().len(),
            16,
            "the crash window leaves bytes INTACT (marker-before-truncate)"
        );
        assert_eq!(r.backend.tier_list_evicted().await.unwrap().len(), 1, "marker durable");
        // The consult marker goes in BEFORE the truncate (the chaos
        // drill's find: the truncate fsync is a multi-ms window in
        // which un-gated READs/GETATTRs would otherwise see a bare
        // 0-byte stub) — so at the crash point it IS visible.
        assert!(is_evicted(dev, ino), "marker visible before any destruction");
        forget(dev, ino); // process death wipes RAM

        // "Restart": reconcile from the durable rows.
        let report = reconcile(&r.backend).await;
        assert_eq!(report.finished, 1, "the half-eviction must be FINISHED");
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 0);
        assert!(is_evicted(dev, ino));
        assert_eq!(logical_size(dev, ino), Some(16));
        forget(dev, ino);
    }

    #[tokio::test]
    async fn reconciler_rolls_back_a_diverged_half_eviction() {
        let r = rig();
        let f = r.root.join("rollback.bin");
        std::fs::write(&f, b"local truth wins!").unwrap();
        let (dev, ino) = ident(&f);
        capture::forget(dev, ino);
        forget(dev, ino);
        // A marker whose CRC does NOT match the local bytes (the
        // marker was committed against different content).
        r.backend
            .tier_put_evicted(&TierEvictedRow {
                dev,
                ino,
                key: "t/rollback.bin".into(),
                generation: 1,
                etag: "\"x\"".into(),
                crc64_b64: "AAAAAAAAAAA=".into(),
                size: 17,
                path: f.to_string_lossy().into_owned(),
                evicted_unix: 1,
                hydrating_unix: None,
            })
            .await
            .unwrap();

        let report = reconcile(&r.backend).await;
        assert_eq!(report.rolled_back, 1, "diverged half-eviction must ROLL BACK");
        assert_eq!(std::fs::read(&f).unwrap(), b"local truth wins!");
        assert!(r.backend.tier_list_evicted().await.unwrap().is_empty());
        assert!(!is_evicted(dev, ino));
    }

    #[tokio::test]
    async fn identity_applies_clear_or_carry_markers() {
        let r = rig();
        let store = store_of(&r);
        let (dev, ino, key) = published_file(&r, "ident.bin", b"identity bytes!!").await;
        let f = r.root.join("ident.bin");
        assert!(matches!(
            evict_file(&r.backend, &store, &f, &key, NO_WRITERS).await,
            EvictOutcome::Evicted { .. }
        ));

        // Rename (moved): the identity-keyed marker survives; the
        // durable row's path handle follows.
        r.backend
            .tier_apply_rename(Some((dev, ino)), "/renamed/ident.bin", 999, None, 2)
            .await
            .unwrap();
        assert!(is_evicted(dev, ino), "a moved evicted file stays evicted");
        let rows = r.backend.tier_list_evicted().await.unwrap();
        assert_eq!(rows[0].path, "/renamed/ident.bin");

        // Remove: the marker dies with the inode (backend tx), and the
        // drain's forget clears RAM.
        r.backend.tier_apply_remove((dev, ino), 3).await.unwrap();
        forget(dev, ino);
        assert!(!is_evicted(dev, ino));
        assert!(r.backend.tier_list_evicted().await.unwrap().is_empty());
        assert!(
            r.backend
                .tier_list_tombstones()
                .await
                .unwrap()
                .iter()
                .any(|t| t.key == key),
            "the removed evicted file's object must be tombstoned"
        );
    }

    /// A write storm racing eviction: eviction only ever succeeds on a
    /// provably clean file, and no written byte is lost — the bucket's
    /// final object equals the file's final content.
    #[tokio::test]
    async fn write_storm_racing_eviction_loses_nothing() {
        let r = rig();
        let store = store_of(&r);
        let f = r.root.join("storm.bin");
        std::fs::write(&f, vec![0u8; 512]).unwrap();
        let (dev, ino) = ident(&f);
        capture::forget(dev, ino);
        forget(dev, ino);

        // Storm: 4 writers × 25 gated writes, racing evict attempts.
        let path = f.clone();
        let storm: Vec<_> = (0..4u8)
            .map(|w| {
                let p = path.clone();
                std::thread::spawn(move || {
                    use std::os::unix::fs::FileExt;
                    let mut i = 0u64;
                    while i < 25 {
                        let fh = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
                        // A racing evict attempt's exclusion REFUSES the
                        // ticket — that is the DELAY shape; retry like
                        // the kernel client does.
                        let Ok(ticket) = gate::enter_file(&fh) else {
                            std::thread::yield_now();
                            continue;
                        };
                        let off = (w as u64 * 25 + i) % 512;
                        fh.write_at(&[w ^ i as u8], off).unwrap();
                        capture::note_path(&p, Mutation::Write { offset: off, len: 1 });
                        drop(ticket);
                        i += 1;
                    }
                })
            })
            .collect();

        // Evict attempts DURING the storm must refuse (dirty/busy),
        // never destroy.
        for _ in 0..10 {
            let out = evict_file(&r.backend, &store, &f, "t/storm.bin", NO_WRITERS).await;
            assert!(
                matches!(out, EvictOutcome::Refused(_)),
                "eviction during a write storm must refuse, got {:?}",
                out
            );
        }
        for h in storm {
            h.join().unwrap();
        }

        // Quiesce: land the bit, publish, then evict for real.
        let expected = std::fs::read(&f).unwrap();
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        let g = r.orch.generation_of(dev, ino).expect("storm file must publish");
        let out = evict_file(&r.backend, &store, &f, &g.key, NO_WRITERS).await;
        assert!(matches!(out, EvictOutcome::Evicted { .. }), "clean file must evict: {:?}", out);
        let (_, bytes) = r.mem.get_whole(&g.key, None).await.unwrap();
        assert_eq!(
            bytes.as_ref(),
            expected.as_slice(),
            "every stormed byte must be in the bucket — nothing lost to eviction"
        );
        forget(dev, ino);
    }

    /// Step 11: the watermark pass evicts only clean, CRC-eligible,
    /// key-consistent files (oldest-published first) and stops the
    /// moment `should` flips false.
    #[tokio::test]
    async fn evict_pass_takes_only_eligible_files_and_respects_should() {
        let r = rig();
        let store = store_of(&r);
        let (_ad, _ai, _ka) = published_file(&r, "pass-a.bin", b"file a contents!").await;
        let (_bd, _bi, _kb) = published_file(&r, "pass-b.bin", b"file b contents!").await;
        let (cd, ci, _kc) = published_file(&r, "pass-c.bin", b"file c contents!").await;
        // Dirty C after its publish: not eligible.
        let c = r.root.join("pass-c.bin");
        note_and_land(&r, &c, Mutation::Write { offset: 0, len: 4 }).await;

        // should=false: nothing moves.
        let n = evict_pass(&r.backend, &store, &r.root, "t/", NO_WRITERS, &|| false).await;
        assert_eq!(n, 0);

        let n = evict_pass(&r.backend, &store, &r.root, "t/", NO_WRITERS, &|| true).await;
        assert_eq!(n, 2, "exactly the two clean files evict");
        assert_eq!(std::fs::metadata(r.root.join("pass-a.bin")).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(r.root.join("pass-b.bin")).unwrap().len(), 0);
        assert_eq!(
            std::fs::read(&c).unwrap(),
            b"file c contents!",
            "the dirty file must be untouched"
        );
        assert!(!is_evicted(cd, ci));
    }
}
