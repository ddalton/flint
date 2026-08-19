//! Hydration — L2 step 11 (design review A5/C6, A10; step-9 gate
//! findings).
//!
//! Restores an evicted file's bytes from the bucket **in place, into
//! the marker inode** (open the existing stub, pwrite, fsync — never
//! temp+rename, which splits every F17b/c cached fd and goes STALE on
//! inode-pinned handles). While a restore runs, the eviction marker
//! stays set — every content lane keeps answering NFS4ERR_DELAY, so
//! partial bytes are never observable (C6b). The marker clears (durable
//! row first, in-memory map last) only after the full restore is
//! fsynced and CRC-verified.
//!
//! Crash safety: a durable `hydrating` flag flips on BEFORE the first
//! byte lands. The reconciler (evict.rs) uses it to tell a crashed
//! hydration (partial bytes = garbage ⇒ truncate back to the stub,
//! bucket remains truth) from the C2 eviction crash (full original
//! bytes ⇒ finish the truncate). A restore that fails mid-flight
//! (ENOSPC, network) takes the same truncate-back path in-process and
//! retries with capped backoff.
//!
//! Step-9 gate findings applied:
//! - **Write-pending priority** (finding 2: fsync parks >~2 min trip
//!   the client's hung-task warning): WRITE-triggered hydrations may
//!   take a RESERVED permit besides the shared pool, so a queue of
//!   read hydrations never starves a writer's.
//! - **Parking** (finding 1: the client's flat ~0.1 s retry clock):
//!   after answering an op with DELAY, the handler parks the RPC up to
//!   `hold` (default 15 s, bounded well below timeo) waiting for the
//!   marker to clear — one DELAY per hold instead of ten per second,
//!   and the op serves within ~0.1 s of restore completion either way.
//!
//! A10 admission: a restore whose object exceeds the PVC's
//! headroom-minus-reserve waits (eviction/flush may free space) rather
//! than dying on ENOSPC mid-restore.
//!
//! Foreign overwrite (A6's split): every ranged GET carries If-Match
//! on the marker's ETag. A 412 here is the S3-WINS posture — the
//! bucket's CURRENT object is adopted (marker + rows updated) and the
//! restore starts over on the new version.
//!
//! Cold-read fan-out: chunks fetch with up to `fetch_parallel`
//! concurrent ranged GETs (one S3 stream is ~80-200 MB/s — the L4
//! gate measured the sequential posture at 72.5 s/GiB). Completions
//! are consumed in OFFSET ORDER (`buffered`), so the CRC still runs
//! over the byte stream, writes land sequentially, and every crash /
//! retry / adopt contract is unchanged — only the network is parallel.

use crate::state_backend::StateBackend;
use crate::tier::evict::{self, EvictedMeta};
use crate::tier::gate;
use crate::tier::meter::{self, Counter};
use crate::tier::store::{crc64_to_b64, Crc64Nvme, ObjectStore, StoreError};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use tracing::{error, info, warn};

/// Ranged-GET chunk size: big enough to amortize request overhead,
/// small enough that a multi-GiB restore never buffers meaningfully.
/// (Configurable via HydrateConfig.chunk so tests exercise multi-chunk
/// restores without multi-MiB fixtures; not an operator knob.)
const CHUNK: u64 = 8 * 1024 * 1024;
/// Transport-error retries per chunk before the restore attempt is
/// abandoned (truncate-back + outer backoff). Identity failures (412)
/// never retry — they adopt.
const CHUNK_RETRIES: u32 = 5;
/// Marker-poll cadence while parking an RPC.
const PARK_POLL_MS: u64 = 50;
/// Retry backoff cap for failed restores.
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Restore attempts a WARM item gets before the fill abandons it. A
/// warm item that can never restore (renamed path, persistent 4xx)
/// must not camp in the retry loop and wedge the fill's completion
/// wait; demand tolerates unbounded retries because a live client
/// wants the bytes — the fill has no client.
const WARM_MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Read,
    Write,
}

#[derive(Debug, Clone)]
pub struct HydrateConfig {
    /// In-RPC park bound (A5: well below timeo; default 15 s).
    pub hold: Duration,
    /// Concurrent restores (shared pool; +1 reserved for
    /// write-triggered).
    pub concurrency: usize,
    /// Concurrent ranged GETs per restore — the COLD-READ FAN-OUT. One
    /// S3 GET stream delivers ~80-200 MB/s, so the sequential posture
    /// measured 72.5 s/GiB on the L4 gate; N streams divide it. Peak
    /// buffering ~ concurrency x fetch_parallel x chunk.
    pub fetch_parallel: usize,
    /// Ranged-GET chunk size (tests shrink it; not an operator knob).
    pub chunk: u64,
    /// Concurrent WARM (bulk-fill) restores — a pool of its own, so a
    /// fill never contends with demand hydrations. Warm restores fetch
    /// with fanout 1 (the fill is throughput-bound across files, not
    /// per-file-latency-bound), so peak warm buffering ~ this x chunk.
    pub warm_concurrency: usize,
}

impl Default for HydrateConfig {
    fn default() -> Self {
        HydrateConfig {
            hold: Duration::from_secs(15),
            concurrency: 4,
            fetch_parallel: 6,
            chunk: CHUNK,
            warm_concurrency: 16,
        }
    }
}

pub(crate) struct Inflight {
    pub(crate) write_pri: AtomicBool,
    /// Lane: false = warm (bulk fill), true = a client wants it.
    /// MONOTONIC — upgrades only, never down (`write_pri ⇒ demand`).
    /// A demand request on a warm-restoring file absorbs into it by
    /// flipping this; the running task re-reads it at every lane
    /// decision (permit select, space arm, retry bound).
    pub(crate) demand: AtomicBool,
}

pub struct Hydrator {
    backend: Arc<dyn StateBackend>,
    store: Arc<dyn ObjectStore>,
    cfg: HydrateConfig,
    handle: tokio::runtime::Handle,
    shared: Arc<tokio::sync::Semaphore>,
    write_reserved: Arc<tokio::sync::Semaphore>,
    warm: Arc<tokio::sync::Semaphore>,
    /// Admitted-but-unfinished WARM bytes. Threaded into every
    /// admit_warm as `pending`: N concurrent admissions each checking
    /// a gauge blind to the others' incoming bytes would overshoot by
    /// N x object_size. PAIRING INVARIANT: the run() iteration that
    /// adds captures the amount in an iteration-local and subtracts
    /// exactly that, exactly once, on every iteration exit — never
    /// keyed on the live `demand` flag (an upgrade mid-restore would
    /// skip the subtract: a monotonic leak), never re-read from the
    /// marker (a 412 adopt rewrites its size).
    warm_admitted: std::sync::atomic::AtomicU64,
    inflight: DashMap<(u64, u64), Arc<Inflight>>,
}

// ── the global the handlers reach ────────────────────────────────────

static INSTALLED: AtomicBool = AtomicBool::new(false);

fn active() -> &'static RwLock<Option<Arc<Hydrator>>> {
    static A: OnceLock<RwLock<Option<Arc<Hydrator>>>> = OnceLock::new();
    A.get_or_init(|| RwLock::new(None))
}

fn current() -> Option<Arc<Hydrator>> {
    if !INSTALLED.load(Ordering::Relaxed) {
        return None;
    }
    active().read().unwrap().clone()
}

/// Build and install the hydrator (serve()'s start_tier; tests may
/// re-install). Must run inside a tokio runtime — restores spawn on
/// it.
pub fn install(
    backend: Arc<dyn StateBackend>,
    store: Arc<dyn ObjectStore>,
    cfg: HydrateConfig,
) -> Arc<Hydrator> {
    let h = Arc::new(Hydrator {
        backend,
        store,
        shared: Arc::new(tokio::sync::Semaphore::new(cfg.concurrency.max(1))),
        write_reserved: Arc::new(tokio::sync::Semaphore::new(1)),
        warm: Arc::new(tokio::sync::Semaphore::new(cfg.warm_concurrency.max(1))),
        warm_admitted: std::sync::atomic::AtomicU64::new(0),
        cfg,
        handle: tokio::runtime::Handle::current(),
        inflight: DashMap::new(),
    });
    *active().write().unwrap() = Some(Arc::clone(&h));
    INSTALLED.store(true, Ordering::Relaxed);
    h
}

/// Restores in flight right now (A12 reporter gauge). 0 when no
/// hydrator is installed.
pub fn inflight_count() -> usize {
    current().map(|h| h.inflight.len()).unwrap_or(0)
}

/// WARM restores in flight (reporter gauge; the fill driver's bound).
/// A FILTERED count, deliberately not a decrement-tracked gauge: an
/// entry upgraded to demand stops counting automatically, and there is
/// no wedge-on-missed-decrement.
pub fn warm_inflight() -> usize {
    current().map(|h| warm_inflight_of(&h)).unwrap_or(0)
}

fn warm_inflight_of(h: &Arc<Hydrator>) -> usize {
    h.inflight
        .iter()
        .filter(|e| !e.value().demand.load(Ordering::Relaxed))
        .count()
}

/// A LOCAL hydrator for cross-module drills (step 12's DR drill runs
/// restore_once directly) — deliberately NOT installed globally.
#[cfg(test)]
pub(crate) fn local_for_tests(
    backend: Arc<dyn StateBackend>,
    store: Arc<dyn ObjectStore>,
    concurrency: usize,
) -> Arc<Hydrator> {
    Arc::new(Hydrator {
        backend,
        store,
        cfg: HydrateConfig {
            hold: Duration::from_secs(2),
            concurrency,
            ..Default::default()
        },
        handle: tokio::runtime::Handle::current(),
        shared: Arc::new(tokio::sync::Semaphore::new(concurrency.max(1))),
        write_reserved: Arc::new(tokio::sync::Semaphore::new(1)),
        warm: Arc::new(tokio::sync::Semaphore::new(16)),
        warm_admitted: std::sync::atomic::AtomicU64::new(0),
        inflight: DashMap::new(),
    })
}

/// Request hydration of an evicted file. Sync and cheap — callable
/// from the blocking closures at the marker-consult sites. Idempotent:
/// an in-flight restore absorbs the request (a WRITE trigger upgrades
/// its priority).
pub fn request(dev: u64, ino: u64, path: &Path, trigger: Trigger) {
    let Some(h) = current() else { return };
    request_on(&h, dev, ino, path, trigger)
}

/// [`request`] against an explicit hydrator (the testable half — the
/// module's drills run LOCAL hydrators to keep out of other tests'
/// global installs).
pub(crate) fn request_on(h: &Arc<Hydrator>, dev: u64, ino: u64, path: &Path, trigger: Trigger) {
    if !evict::is_evicted(dev, ino) {
        return;
    }
    use dashmap::mapref::entry::Entry;
    let fresh = match h.inflight.entry((dev, ino)) {
        Entry::Occupied(e) => {
            // A client wants this file: upgrade the lane. On an
            // already-demand entry this is a no-op; on a WARM entry it
            // absorbs the demand into the running restore (monotonic —
            // never downgrades).
            e.get().demand.store(true, Ordering::Relaxed);
            if trigger == Trigger::Write {
                e.get().write_pri.store(true, Ordering::Relaxed);
            }
            None
        }
        Entry::Vacant(v) => {
            let inflight = Arc::new(Inflight {
                write_pri: AtomicBool::new(trigger == Trigger::Write),
                demand: AtomicBool::new(true),
            });
            v.insert(Arc::clone(&inflight));
            Some(inflight)
        }
    };
    if let Some(inflight) = fresh {
        let hh = Arc::clone(&h);
        let p = path.to_path_buf();
        meter::bump(Counter::HydrationsStarted);
        h.handle.spawn(async move {
            run(hh, dev, ino, p, inflight).await;
        });
    }
}

/// Queue a WARM (bulk-fill) restore. Mirrors [`request`]'s dedup but
/// takes the hydrator explicitly (the fill driver holds it) and NEVER
/// downgrades: a file already restoring — warm or demand — absorbs
/// the request as a no-op.
pub(crate) fn request_warm(h: &Arc<Hydrator>, dev: u64, ino: u64, path: &Path) {
    if !evict::is_evicted(dev, ino) {
        return;
    }
    use dashmap::mapref::entry::Entry;
    let fresh = match h.inflight.entry((dev, ino)) {
        Entry::Occupied(_) => None,
        Entry::Vacant(v) => {
            let inflight = Arc::new(Inflight {
                write_pri: AtomicBool::new(false),
                demand: AtomicBool::new(false),
            });
            v.insert(Arc::clone(&inflight));
            Some(inflight)
        }
    };
    if let Some(inflight) = fresh {
        let hh = Arc::clone(h);
        let p = path.to_path_buf();
        meter::bump(Counter::HydrationsStarted);
        h.handle.spawn(async move {
            run(hh, dev, ino, p, inflight).await;
        });
    }
}

/// Park an RPC (post-DELAY-decision) until the marker clears or the
/// configured hold elapses. One stat, then marker polls — the hot path
/// never comes here; only already-refused ops do. No-op when no
/// hydrator is installed (the pure step-10 posture).
pub async fn park(path: &Path) {
    let Some(h) = current() else { return };
    #[cfg(unix)]
    let ident = {
        use std::os::unix::fs::MetadataExt;
        path.symlink_metadata().ok().map(|m| (m.dev(), m.ino()))
    };
    #[cfg(not(unix))]
    let ident: Option<(u64, u64)> = None;
    let Some((dev, ino)) = ident else { return };
    let deadline = tokio::time::Instant::now() + h.cfg.hold;
    while evict::is_evicted(dev, ino) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(PARK_POLL_MS)).await;
    }
}

// ── the warm fill (bulk hydration after a DR import) ─────────────────

/// What a warm fill did. `restored` counts every candidate whose
/// marker is gone at drain — including files a client's demand upgrade
/// finished; `still_cold` is the rest (space skips + retry abandons +
/// anything a client re-evicted mid-fill).
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmFillReport {
    pub candidates: usize,
    pub queued: usize,
    pub restored: usize,
    pub still_cold: usize,
    /// Rows never queued: the driver's smallest-first pre-check hit
    /// the space bound (ascending sizes ⇒ nothing later fits either).
    pub stopped_for_space: usize,
}

/// Driver cadence for its bound-wait and drain polls.
const WARM_POLL_MS: u64 = 50;

/// Bulk-restore every evicted file, smallest first (`aws s3 cp
/// --recursive`, but the work list is our own durable evicted rows —
/// no bucket LIST, every GET If-Match-pinned + CRC-verified by the
/// normal restore machinery). Bounded: at most 2×warm_concurrency
/// restores outstanding, one row Vec. Terminates: every queued item
/// either restores, abandons (WARM_MAX_ATTEMPTS / space), or upgrades
/// to demand (then it is the client's, not the fill's). `note` is the
/// durable re-arm marker (crash-mid-fill resume): removed on ANY
/// completed exit — drain or space-stop — never on crash.
pub async fn warm_fill(h: &Arc<Hydrator>, note: Option<&Path>) -> WarmFillReport {
    // One fill per trigger; any residue in the pending-bytes
    // accumulator is a bug's, not a concurrent fill's.
    h.warm_admitted.store(0, Ordering::Relaxed);

    let mut rows = match h.backend.tier_list_evicted().await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("tier warm: cannot list evicted rows ({}) — fill skipped", e);
            return WarmFillReport::default();
        }
    };
    // One-shot snapshot, RAM-marker-confirmed (reconcile ran before
    // install): a file re-evicted AFTER this snapshot is never
    // re-warmed in the same fill (thrash guard 3).
    rows.retain(|r| evict::is_evicted(r.dev, r.ino));
    rows.sort_by_key(|r| r.size);
    let candidates = rows.len();
    if candidates == 0 {
        if let Some(p) = note {
            let _ = std::fs::remove_file(p);
        }
        return WarmFillReport { candidates, ..Default::default() };
    }
    info!("tier warm: fill starting — {} evicted file(s), smallest first", candidates);

    let bound = h.cfg.warm_concurrency.max(1) * 2;
    let mut queued: Vec<(u64, u64)> = Vec::new();
    let mut stopped_for_space = 0usize;
    for (i, row) in rows.iter().enumerate() {
        // Driver-side pre-check (the task re-checks under its permit):
        // smallest-first means the FIRST refusal ends the whole fill.
        let pending = h.warm_admitted.load(Ordering::Relaxed);
        if !crate::tier::space::admit_warm(Path::new(&row.path), row.size, pending) {
            stopped_for_space = candidates - i;
            meter::bump(Counter::WarmSkippedSpace);
            warn!(
                "tier warm: fill stopped at the space bound — {} of {} file(s) stay cold \
                 (watermark margin; a demand touch still hydrates them)",
                stopped_for_space, candidates
            );
            break;
        }
        request_warm(h, row.dev, row.ino, Path::new(&row.path));
        queued.push((row.dev, row.ino));
        while warm_inflight_of(h) >= bound {
            tokio::time::sleep(Duration::from_millis(WARM_POLL_MS)).await;
        }
    }

    // Drain: warm entries only — an upgraded entry is the client's
    // (its restore continues on the demand lane and still clears the
    // marker; we count it below either way).
    while warm_inflight_of(h) > 0 {
        tokio::time::sleep(Duration::from_millis(WARM_POLL_MS)).await;
    }

    let restored = queued
        .iter()
        .filter(|(dev, ino)| !evict::is_evicted(*dev, *ino))
        .count();
    let report = WarmFillReport {
        candidates,
        queued: queued.len(),
        restored,
        still_cold: candidates - restored,
        stopped_for_space,
    };
    if let Some(p) = note {
        let _ = std::fs::remove_file(p);
    }
    // THE one-line summary the drill greps — keep the shape stable.
    info!(
        "🪣 tier warm fill done: {} restored / {} candidates ({} queued, {} still cold, \
         {} stopped for space)",
        report.restored, report.candidates, report.queued, report.still_cold,
        report.stopped_for_space
    );
    report
}

// ── the restore task ─────────────────────────────────────────────────

/// How a warm abandon resolved. `remove_if`'s bool alone cannot tell
/// a raced upgrade from a successor entry — the two demand OPPOSITE
/// epilogues (keep driving vs. get out of the way) — so the abandon
/// re-inspects on failure.
enum WarmAbandon {
    /// Our still-warm entry was removed: bump the counter and return.
    Removed,
    /// Our entry, but a client upgraded it mid-abandon: it now has a
    /// waiter and WE are its only driver — continue the run() loop
    /// (the next iteration selects the demand lane).
    Upgraded,
    /// A different Arc (or none): a successor owns the key — return
    /// WITHOUT touching it (the epilogue's ptr_eq guard would refuse
    /// too, but don't even get there).
    Superseded,
}

/// Abandon a WARM restore: remove the inflight entry IFF it is still
/// ours and still warm. The predicate runs under the shard lock, so an
/// upgrade either lands before it (remove refused, entry kept) or
/// after the removal (the successor request() re-inserts and spawns —
/// the marker is still set, so nothing is lost).
fn abandon_warm(h: &Arc<Hydrator>, dev: u64, ino: u64, inflight: &Arc<Inflight>) -> WarmAbandon {
    let removed = h.inflight.remove_if(&(dev, ino), |_, v| {
        Arc::ptr_eq(v, inflight) && !v.demand.load(Ordering::Relaxed)
    });
    if removed.is_some() {
        return WarmAbandon::Removed;
    }
    match h.inflight.get(&(dev, ino)) {
        Some(e) if Arc::ptr_eq(e.value(), inflight) => WarmAbandon::Upgraded,
        _ => WarmAbandon::Superseded,
    }
}

pub(crate) async fn run(
    h: Arc<Hydrator>,
    dev: u64,
    ino: u64,
    path: PathBuf,
    inflight: Arc<Inflight>,
) {
    let mut attempt: u32 = 0;
    loop {
        if !evict::is_evicted(dev, ino) {
            break; // completed elsewhere / marker removed (REMOVE etc.)
        }
        // Permit, three-way by lane. Every non-select acquire keeps the
        // 500ms-timeout + `continue` pattern: an untimed acquire would
        // never observe an upgrade (the write-pri starvation lesson,
        // reapplied to the warm→demand edge).
        let _permit = if inflight.write_pri.load(Ordering::Relaxed) {
            // Write-priority may ALSO take the reserved slot.
            tokio::select! {
                p = Arc::clone(&h.write_reserved).acquire_owned() => p.ok(),
                p = Arc::clone(&h.shared).acquire_owned() => p.ok(),
            }
        } else if inflight.demand.load(Ordering::Relaxed) {
            match tokio::time::timeout(
                Duration::from_millis(500),
                Arc::clone(&h.shared).acquire_owned(),
            )
            .await
            {
                Ok(p) => p.ok(),
                Err(_) => continue, // re-check write_pri
            }
        } else {
            match tokio::time::timeout(
                Duration::from_millis(500),
                Arc::clone(&h.warm).acquire_owned(),
            )
            .await
            {
                Ok(p) => p.ok(),
                Err(_) => continue, // re-check demand/write_pri
            }
        };

        // Space admission, split by lane (the lane is re-read HERE, not
        // reused from the permit select — an upgrade during the permit
        // wait must take the demand path). The subtract side of the
        // warm reservation lives in `warm_reserved`: exactly the added
        // amount, exactly once, on every exit of this iteration.
        let mut warm_reserved: Option<u64> = None;
        if let Some(meta) = evict::marker_meta(dev, ino) {
            if inflight.demand.load(Ordering::Relaxed) {
                // A10 admission: wait while the object cannot fit in
                // headroom-minus-reserve (the watermark pass may be
                // freeing space right now). Path-scoped to the root.
                if !crate::tier::space::admit_hydration(&path, meta.size) {
                    warn!(
                        "tier hydrate: {} needs {} bytes past the reserve — waiting",
                        path.display(),
                        meta.size,
                    );
                    drop(_permit);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            } else {
                // WARM: a stricter bound (watermark − margin, pending
                // bytes counted), and refusal ABANDONS — the 5s camp
                // is right for a client that will wait, wrong for a
                // fill that must terminate.
                let pending = h.warm_admitted.load(Ordering::Relaxed);
                if !crate::tier::space::admit_warm(&path, meta.size, pending) {
                    drop(_permit);
                    match abandon_warm(&h, dev, ino, &inflight) {
                        WarmAbandon::Removed => {
                            meter::bump(Counter::WarmSkippedSpace);
                            info!(
                                "tier warm: {} skipped at the space bound ({} bytes)",
                                path.display(),
                                meta.size
                            );
                            // `return`, not `break`: the epilogue must
                            // not run — a successor entry a
                            // post-abandon request() inserts is NOT
                            // ours to remove.
                            return;
                        }
                        WarmAbandon::Upgraded => continue,
                        WarmAbandon::Superseded => return,
                    }
                }
                h.warm_admitted.fetch_add(meta.size, Ordering::Relaxed);
                warm_reserved = Some(meta.size);
            }
        }

        // Warm restores fetch sequentially (fanout 1): the fill's
        // throughput is its file-level concurrency, and inheriting the
        // demand fanout would buffer warm_concurrency x fetch_parallel
        // x chunk at the fill's big-file tail.
        let fanout = if inflight.demand.load(Ordering::Relaxed) {
            h.cfg.fetch_parallel.max(1)
        } else {
            1
        };
        let began = std::time::Instant::now();
        match restore_once_fanout(&h, dev, ino, &path, fanout).await {
            Ok(bytes) => {
                if let Some(n) = warm_reserved.take() {
                    h.warm_admitted.fetch_sub(n, Ordering::Relaxed);
                    // The evict-pass posture: successive warm admits
                    // must see the fill's own landed bytes.
                    crate::tier::space::refresh_now();
                }
                meter::bump(Counter::HydrationsCompleted);
                meter::add(Counter::HydrationBytes, bytes);
                meter::add(Counter::HydrationMillis, began.elapsed().as_millis() as u64);
                info!("tier hydrate: {} restored ({} bytes)", path.display(), bytes);
                break;
            }
            Err(e) => {
                if let Some(n) = warm_reserved.take() {
                    h.warm_admitted.fetch_sub(n, Ordering::Relaxed);
                }
                meter::bump(Counter::HydrationFailures);
                attempt += 1;
                // Warm retry bound. The lane is re-read HERE: an
                // upgrade that landed during the attempt (the window
                // spans the whole restore) converts to demand-retry,
                // never abandon.
                if !inflight.demand.load(Ordering::Relaxed) && attempt >= WARM_MAX_ATTEMPTS {
                    drop(_permit);
                    match abandon_warm(&h, dev, ino, &inflight) {
                        WarmAbandon::Removed => {
                            meter::bump(Counter::WarmAbandoned);
                            warn!(
                                "tier warm: {} ABANDONED after {} attempts (last: {}) — \
                                 still evicted; a demand touch will retry it",
                                path.display(),
                                attempt,
                                e
                            );
                            return;
                        }
                        WarmAbandon::Upgraded => continue,
                        WarmAbandon::Superseded => return,
                    }
                }
                let backoff = Duration::from_secs(1 << attempt.min(5)).min(BACKOFF_CAP);
                warn!(
                    "tier hydrate: {} attempt {} failed: {} — retrying in {:?}",
                    path.display(),
                    attempt,
                    e,
                    backoff
                );
                drop(_permit);
                tokio::time::sleep(backoff).await;
            }
        }
    }
    // ptr_eq-guarded: never destroy a successor entry (a re-evicted
    // file's fresh request() may legitimately own the key by now).
    h.inflight
        .remove_if(&(dev, ino), |_, v| Arc::ptr_eq(v, &inflight));
}

/// One restore attempt at the configured (demand) fanout. On ANY
/// failure the file is truncated back to the stub and the hydrating
/// flag cleared — partial bytes never survive an error path.
/// (Production code goes through [`restore_once_fanout`] — run()
/// chooses the fanout by lane; this wrapper is the test surface.)
#[cfg(test)]
pub(crate) async fn restore_once(
    h: &Arc<Hydrator>,
    dev: u64,
    ino: u64,
    path: &Path,
) -> Result<u64, String> {
    let fanout = h.cfg.fetch_parallel.max(1);
    restore_once_fanout(h, dev, ino, path, fanout).await
}

/// [`restore_once`] with an explicit fetch fanout — run() passes 1 for
/// warm-lane attempts (see HydrateConfig::warm_concurrency).
pub(crate) async fn restore_once_fanout(
    h: &Arc<Hydrator>,
    dev: u64,
    ino: u64,
    path: &Path,
    fanout: usize,
) -> Result<u64, String> {
    // Exclusion for the restore window (A5: behind the A4 gate). The
    // marker already refuses content ops; the exclusion is the
    // belt-and-suspenders against any un-consulted lane.
    let excl = tokio::task::spawn_blocking({
        let (d, i) = (dev, ino);
        move || gate::exclude(d, i)
    })
    .await
    .map_err(|e| format!("exclude join: {}", e))?;

    let mut meta = evict::marker_meta(dev, ino).ok_or("marker vanished")?;

    // Identity check: the path must still name the marker inode.
    let stub_mtime;
    {
        use std::os::unix::fs::MetadataExt;
        let md = path
            .symlink_metadata()
            .map_err(|e| format!("stat: {}", e))?;
        if (md.dev(), md.ino()) != (dev, ino) {
            return Err("path no longer names the marker inode".into());
        }
        // Hydration is NOT a modification (step 12's POSIX-fidelity
        // posture): the stub's mtime — which eviction/import gave it —
        // is re-applied after the restore's writes.
        stub_mtime = filetime::FileTime::from_last_modification_time(&md);
        if md.len() != 0 {
            // A failed earlier attempt (or an adopt restart) left
            // bytes; back to the stub first.
            truncate_stub(path)?;
        }
    }

    // Durable hydrating flag BEFORE the first byte (the reconciler's
    // disambiguation hinge).
    h.backend
        .tier_set_hydrating(dev, ino, Some(now_unix()))
        .await
        .map_err(|e| format!("hydrating flag: {}", e))?;

    let result = stream_restore(h, dev, ino, path, &mut meta, fanout).await;
    match result {
        Ok(bytes) => {
            // Completion order: durable rows first, RAM marker LAST —
            // the moment the map clears, ops serve, and everything
            // they can observe is already consistent.
            let gen_row = crate::state_backend::TierGenerationRow {
                dev,
                ino,
                key: meta.key.clone(),
                generation: meta.generation,
                etag: meta.etag.clone(),
                crc64_b64: Some(meta.crc64_b64.clone()),
                size: meta.size,
                copy_allowed: true,
                updated_unix: now_unix(),
            };
            if let Err(e) = h.backend.tier_upsert_generation(&gen_row).await {
                warn!("tier hydrate: generation row upsert: {} (HEAD rediscovers)", e);
            }
            let _ = filetime::set_file_mtime(path, stub_mtime);
            h.backend
                .tier_delete_evicted(dev, ino)
                .await
                .map_err(|e| format!("marker delete: {}", e))?;
            evict::remove_xattr_best_effort(path);
            evict::forget(dev, ino);
            drop(excl);
            Ok(bytes)
        }
        Err(e) => {
            // ENOSPC / network / verify failure: never leave partial
            // bytes. Stub + flag clear, marker stays, retry later.
            if let Err(t) = truncate_stub(path) {
                error!(
                    "tier hydrate: {} failed AND could not reset the stub: {} — \
                     reconciler repairs at next startup",
                    path.display(),
                    t
                );
            }
            let _ = h.backend.tier_set_hydrating(dev, ino, None).await;
            drop(excl);
            Err(e)
        }
    }
}

/// What a single chunk fetch can come back with. Identity failures
/// (412 / vanished key) are not retryable — they demand the adopt.
enum ChunkFail {
    Adopt,
    Fatal(String),
}

/// Ranged, If-Match-guarded restore into the stub inode, fetching up
/// to `fetch_parallel` chunks CONCURRENTLY (the cold-read fix: one GET
/// stream is ~80-200 MB/s against S3 — the L4 gate measured the
/// sequential posture at 72.5 s/GiB). `buffered` yields completions in
/// OFFSET ORDER, so the CRC still runs over the byte stream and writes
/// land sequentially; only the network fan-out is parallel. Handles
/// the S3-wins adopt on 412 by updating `meta` + the durable marker
/// and signalling a retry (the caller loop restarts).
async fn stream_restore(
    h: &Arc<Hydrator>,
    dev: u64,
    ino: u64,
    path: &Path,
    meta: &mut EvictedMeta,
    fanout: usize,
) -> Result<u64, String> {
    use futures::StreamExt;
    // Internal-write open: a read-only stub (0444 git objects) must
    // still restore — DAC applies to clients, not to the tier's own
    // maintenance I/O.
    let file = evict::open_for_internal_write(path).map_err(|e| format!("open stub: {}", e))?;
    let file = Arc::new(file);
    let mut crc = Crc64Nvme::new();

    let chunk_bytes = h.cfg.chunk.max(1);
    let fanout = fanout.max(1);
    let mut parts: Vec<(u64, u64)> = Vec::new();
    let mut at = 0u64;
    while at < meta.size {
        let len = chunk_bytes.min(meta.size - at);
        parts.push((at, len));
        at += len;
    }

    // Each chunk future owns clones of key/etag (no borrow of `meta`
    // crosses the stream — the adopt below rewrites it) and carries
    // its own retry budget: transport errors retry the CHUNK, not the
    // restore — a connection cut at chunk N of a multi-GiB file must
    // not throw away N chunks of progress (a flaky network would
    // otherwise starve large hydrations forever — chaos phase L's
    // finding). The If-Match guard keeps every retry pinned to the
    // same object; identity failures still adopt immediately.
    let store = Arc::clone(&h.store);
    let key = meta.key.clone();
    let etag = meta.etag.clone();
    let mut chunks = futures::stream::iter(parts.into_iter().map(move |(off, len)| {
        let store = Arc::clone(&store);
        let key = key.clone();
        let etag = etag.clone();
        async move {
            let mut attempt: u32 = 0;
            loop {
                match store.get_range(&key, off, len, &etag).await {
                    Ok(b) => return Ok((off, b)),
                    Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                        // A6: hydration-GET 412 (or a deleted-and-
                        // replaced key) is S3-WINS — surfaced to the
                        // caller, which adopts the bucket's CURRENT
                        // object and restarts the restore on it.
                        return Err(ChunkFail::Adopt);
                    }
                    Err(e) if attempt < CHUNK_RETRIES => {
                        attempt += 1;
                        warn!(
                            "tier hydrate: {} range {}+{} attempt {} failed: {} — retrying the \
                             chunk (parallel siblings keep their progress)",
                            key, off, len, attempt, e
                        );
                        tokio::time::sleep(Duration::from_millis(300 * u64::from(attempt))).await;
                    }
                    Err(e) => {
                        return Err(ChunkFail::Fatal(format!(
                            "get_range after {} chunk retries: {}",
                            CHUNK_RETRIES, e
                        )))
                    }
                }
            }
        }
    }))
    .buffered(fanout);

    let mut failed: Option<ChunkFail> = None;
    while let Some(next) = chunks.next().await {
        match next {
            Ok((at, chunk)) => {
                if chunk.is_empty() {
                    failed = Some(ChunkFail::Fatal(
                        "short object: empty range before expected end".into(),
                    ));
                    break;
                }
                crc.update(&chunk);
                let f = Arc::clone(&file);
                let wrote = tokio::task::spawn_blocking(move || {
                    use std::os::unix::fs::FileExt;
                    f.write_all_at(&chunk, at)
                })
                .await
                .map_err(|e| format!("pwrite join: {}", e))
                .and_then(|r| r.map_err(|e| format!("pwrite: {}", e)));
                if let Err(e) = wrote {
                    failed = Some(ChunkFail::Fatal(e));
                    break;
                }
            }
            Err(f) => {
                failed = Some(f);
                break;
            }
        }
    }
    // Dropping the stream cancels every in-flight sibling fetch BEFORE
    // any adopt rewrites the marker underneath them.
    drop(chunks);
    match failed {
        Some(ChunkFail::Adopt) => {
            return match adopt_foreign(h, dev, ino, meta).await {
                Ok(()) => Err("foreign overwrite adopted — restarting restore".into()),
                Err(e) => Err(e),
            };
        }
        Some(ChunkFail::Fatal(e)) => return Err(e),
        None => {}
    }
    let f = Arc::clone(&file);
    let size = meta.size;
    tokio::task::spawn_blocking(move || f.set_len(size).and_then(|_| f.sync_all()))
        .await
        .map_err(|e| format!("fsync join: {}", e))?
        .map_err(|e| format!("fsync: {}", e))?;

    let got = crc64_to_b64(crc.finalize());
    if !meta.crc64_b64.is_empty() && got != meta.crc64_b64 {
        return Err(format!(
            "restored stream CRC {} != expected {} — refusing to serve",
            got, meta.crc64_b64
        ));
    }
    if meta.crc64_b64.is_empty() {
        // Adopted foreign object without a CRC: the stream we fetched
        // (If-Match-pinned end to end) IS the content; record its CRC
        // forward.
        meta.crc64_b64 = got;
    }
    Ok(meta.size)
}

/// S3-wins: point the marker (durable + RAM) at the bucket's current
/// object. The caller restarts the restore against it.
async fn adopt_foreign(
    h: &Arc<Hydrator>,
    dev: u64,
    ino: u64,
    meta: &mut EvictedMeta,
) -> Result<(), String> {
    let head = h
        .store
        .head(&meta.key)
        .await
        .map_err(|e| format!("adopt HEAD: {}", e))?;
    warn!(
        "tier hydrate: FOREIGN overwrite at {} (etag {} → {}) — S3-wins: adopting the \
         bucket's current object",
        meta.key, meta.etag, head.etag
    );
    meter::bump(Counter::HydrationForeignAdopts);
    let stamped_gen = crate::tier::store::GenerationStamps::from_meta(&head.meta)
        .map(|s| s.generation)
        .unwrap_or(meta.generation + 1);
    meta.etag = head.etag.clone();
    meta.size = head.size;
    meta.generation = stamped_gen;
    meta.crc64_b64 = head.crc64_b64.clone().unwrap_or_default();
    // Durable first, RAM second (GETATTR's logical size follows).
    let rows = h
        .backend
        .tier_list_evicted()
        .await
        .map_err(|e| format!("adopt list: {}", e))?;
    if let Some(mut row) = rows.into_iter().find(|r| r.dev == dev && r.ino == ino) {
        row.etag = meta.etag.clone();
        row.size = meta.size;
        row.generation = meta.generation;
        if !meta.crc64_b64.is_empty() {
            row.crc64_b64 = meta.crc64_b64.clone();
        }
        h.backend
            .tier_put_evicted(&row)
            .await
            .map_err(|e| format!("adopt row: {}", e))?;
    }
    evict::update_marker(dev, ino, meta.clone());
    Ok(())
}

fn truncate_stub(path: &Path) -> Result<(), String> {
    let f = evict::open_for_internal_write(path)
        .map_err(|e| format!("open for stub reset: {}", e))?;
    f.set_len(0).map_err(|e| format!("truncate: {}", e))?;
    f.sync_all().map_err(|e| format!("fsync: {}", e))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::memory::MemoryBackend;
    use crate::tier::capture::{self, Mutation};
    use crate::tier::evict::{evict_file, EvictOutcome};
    use crate::tier::flush::{FlushConfig, FlushOrchestrator};
    use crate::tier::store::memory::MemoryStore;

    const NO_WRITERS: &(dyn Fn(u64, u64) -> bool + Sync) = &|_, _| false;

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

    /// A LOCAL hydrator, deliberately NOT installed — module drills
    /// must not race other tests' global installs.
    fn local_hydrator(r: &Rig, concurrency: usize) -> Arc<Hydrator> {
        local_hydrator_cfg(
            r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency,
                ..Default::default()
            },
        )
    }

    /// Same, with full config control (the parallel-fetch drills shrink
    /// the chunk so small fixtures span many chunks).
    fn local_hydrator_cfg(r: &Rig, cfg: HydrateConfig) -> Arc<Hydrator> {
        let concurrency = cfg.concurrency;
        let warm_concurrency = cfg.warm_concurrency;
        Arc::new(Hydrator {
            backend: r.backend.clone(),
            store: r.mem.clone(),
            cfg,
            handle: tokio::runtime::Handle::current(),
            shared: Arc::new(tokio::sync::Semaphore::new(concurrency.max(1))),
            write_reserved: Arc::new(tokio::sync::Semaphore::new(1)),
            warm: Arc::new(tokio::sync::Semaphore::new(warm_concurrency.max(1))),
            warm_admitted: std::sync::atomic::AtomicU64::new(0),
            inflight: DashMap::new(),
        })
    }

    fn ident(path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let m = path.symlink_metadata().unwrap();
        (m.dev(), m.ino())
    }

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

    /// Create → publish → evict. Returns (dev, ino, key, content).
    async fn evicted_file(r: &Rig, name: &str, content: Vec<u8>) -> (u64, u64, String) {
        let f = r.root.join(name);
        std::fs::write(&f, &content).unwrap();
        let (dev, ino) = ident(&f);
        capture::forget(dev, ino);
        note_and_land(r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        let g = r.orch.generation_of(dev, ino).expect("must publish");
        let store: Arc<dyn ObjectStore> = r.mem.clone();
        let out = evict_file(&r.backend, &store, &f, &g.key, NO_WRITERS).await;
        assert!(matches!(out, EvictOutcome::Evicted { .. }), "{:?}", out);
        (dev, ino, g.key)
    }

    #[tokio::test]
    async fn read_only_file_evicts_and_hydrates_mode_kept() {
        // The MinIO drill's find: git object files are 0444 — the
        // owner cannot open them O_WRONLY, yet evicting and restoring
        // them is the tier's job. DAC applies to clients, not to the
        // tier's internal maintenance I/O; the mode must survive.
        use std::os::unix::fs::PermissionsExt;
        let r = rig();
        let content: Vec<u8> = (0..1024u32).map(|i| (i % 199) as u8).collect();
        let f = r.root.join("obj444.bin");
        std::fs::write(&f, &content).unwrap();
        let (dev, ino) = ident(&f);
        capture::forget(dev, ino);
        note_and_land(&r, &f, Mutation::Whole).await;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
        r.orch.tick().await;
        let g = r.orch.generation_of(dev, ino).expect("must publish");
        let store: Arc<dyn ObjectStore> = r.mem.clone();
        let out = evict_file(&r.backend, &store, &f, &g.key, NO_WRITERS).await;
        assert!(matches!(out, EvictOutcome::Evicted { .. }), "{:?}", out);
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 0);
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o7777,
            0o444,
            "the borrowed write bit must be returned after the truncate"
        );

        let h = local_hydrator(&r, 2);
        let n = restore_once(&h, dev, ino, &f).await.expect("restore must succeed");
        assert_eq!(n, 1024);
        assert_eq!(std::fs::read(&f).unwrap(), content);
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o7777,
            0o444,
            "mode survives the restore"
        );
    }

    #[tokio::test]
    async fn hydrate_restores_in_place_and_clears_marker() {
        let r = rig();
        let content: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
        let (dev, ino, key) = evicted_file(&r, "h1.bin", content.clone()).await;
        let f = r.root.join("h1.bin");
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 0);

        let h = local_hydrator(&r, 2);
        let bytes = restore_once(&h, dev, ino, &f).await.expect("restore must succeed");
        assert_eq!(bytes, 2048);
        assert_eq!(std::fs::read(&f).unwrap(), content, "bytes restored byte-identical");
        // IN PLACE: the inode survived (C6 — cached fds stay valid).
        assert_eq!(ident(&f), (dev, ino), "restore must reuse the marker inode");
        assert!(!evict::is_evicted(dev, ino));
        assert!(r.backend.tier_list_evicted().await.unwrap().is_empty());
        // The generation row is intact — the file is CLEAN at gen g.
        let g = r
            .backend
            .tier_list_generations()
            .await
            .unwrap()
            .into_iter()
            .find(|x| x.dev == dev && x.ino == ino)
            .expect("gen row survives hydration");
        assert_eq!(g.key, key);
        // Nothing dirty: hydration is not a mutation.
        assert!(
            capture::snapshot(dev, ino).is_none_or(|c| !c.is_dirty()),
            "hydration must not mark the file dirty"
        );
    }

    #[tokio::test]
    async fn transient_chunk_failures_are_absorbed_without_losing_progress() {
        // Chaos phase L's finding: a mid-stream connection cut used to
        // fail the WHOLE restore back to the stub — a flaky network
        // starved large hydrations forever. Transport errors now retry
        // the chunk in place.
        let r = rig();
        let content = vec![0x5Au8; 1500];
        let (dev, ino, _key) = evicted_file(&r, "h2.bin", content.clone()).await;
        let f = r.root.join("h2.bin");

        let h = local_hydrator(&r, 2);
        r.mem.inject_get_range_failures(2);
        let bytes = restore_once(&h, dev, ino, &f)
            .await
            .expect("two transient chunk failures must be absorbed in-attempt");
        assert_eq!(bytes, 1500);
        assert_eq!(std::fs::read(&f).unwrap(), content);
        assert!(!evict::is_evicted(dev, ino), "restore committed");
    }

    #[tokio::test]
    async fn exhausted_chunk_retries_truncate_back_and_retry_succeeds() {
        let r = rig();
        let content = vec![0x6Bu8; 1500];
        let (dev, ino, _key) = evicted_file(&r, "h2x.bin", content.clone()).await;
        let f = r.root.join("h2x.bin");

        let h = local_hydrator(&r, 2);
        // One more failure than the per-chunk budget: the attempt must
        // give up into the truncate-back path.
        r.mem
            .inject_get_range_failures(u64::from(super::CHUNK_RETRIES) + 1);
        let err = restore_once(&h, dev, ino, &f).await.unwrap_err();
        assert!(err.contains("chunk retries"), "{}", err);
        assert_eq!(
            std::fs::metadata(&f).unwrap().len(),
            0,
            "a failed restore must leave the STUB, never partial bytes"
        );
        assert!(evict::is_evicted(dev, ino), "still evicted after the failure");
        let row = &r.backend.tier_list_evicted().await.unwrap()[0];
        assert_eq!(row.hydrating_unix, None, "flag cleared on the error path");

        let bytes = restore_once(&h, dev, ino, &f).await.expect("retry succeeds");
        assert_eq!(bytes, 1500);
        assert_eq!(std::fs::read(&f).unwrap(), content);
    }

    #[tokio::test]
    async fn crashed_mid_restore_reconciler_resets_to_stub() {
        let r = rig();
        let content = vec![0xC3u8; 900];
        let (dev, ino, _key) = evicted_file(&r, "h3.bin", content.clone()).await;
        let f = r.root.join("h3.bin");

        // "Crash" mid-restore: flag durable, partial garbage on disk.
        r.backend.tier_set_hydrating(dev, ino, Some(1)).await.unwrap();
        std::fs::write(&f, b"partial garbage from a dead restore").unwrap();

        let report = evict::reconcile(&r.backend).await;
        assert_eq!(report.hydrations_reset, 1, "partial bytes must reset to the stub");
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 0);
        assert!(evict::is_evicted(dev, ino), "still evicted — bucket remains truth");
        assert_eq!(
            r.backend.tier_list_evicted().await.unwrap()[0].hydrating_unix,
            None
        );

        // And hydration then works.
        let h = local_hydrator(&r, 2);
        restore_once(&h, dev, ino, &f).await.expect("post-reset restore");
        assert_eq!(std::fs::read(&f).unwrap(), content);
    }

    #[tokio::test]
    async fn crashed_after_complete_restore_reconciler_commits() {
        let r = rig();
        let content = vec![0x11u8; 640];
        let (dev, ino, _key) = evicted_file(&r, "h4.bin", content.clone()).await;
        let f = r.root.join("h4.bin");

        // "Crash" after the restore finished but before the marker
        // delete: full verified bytes + flag still set.
        r.backend.tier_set_hydrating(dev, ino, Some(1)).await.unwrap();
        std::fs::write(&f, &content).unwrap();

        let report = evict::reconcile(&r.backend).await;
        assert_eq!(report.hydrations_finished, 1, "verified-complete restore must COMMIT");
        assert_eq!(std::fs::read(&f).unwrap(), content, "bytes kept");
        assert!(!evict::is_evicted(dev, ino));
        assert!(r.backend.tier_list_evicted().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn foreign_overwrite_adopts_s3_wins() {
        let r = rig();
        let (dev, ino, key) = evicted_file(&r, "h5.bin", vec![0xAAu8; 512]).await;
        let f = r.root.join("h5.bin");

        // Someone replaced the object while the file was evicted.
        let foreign = b"the bucket's newer truth".to_vec();
        r.mem.raw_put(&key, bytes::Bytes::from(foreign.clone()), vec![]);

        let h = local_hydrator(&r, 2);
        // First attempt trips the 412 → adopts → asks for a restart.
        let err = restore_once(&h, dev, ino, &f).await.unwrap_err();
        assert!(err.contains("adopted"), "{}", err);
        assert!(evict::is_evicted(dev, ino), "adoption keeps the marker until restored");
        // Second attempt restores the ADOPTED object.
        let bytes = restore_once(&h, dev, ino, &f).await.expect("adopted restore");
        assert_eq!(bytes as usize, foreign.len());
        assert_eq!(std::fs::read(&f).unwrap(), foreign, "S3-wins: bucket content served");
        // The generation row follows the adopted object.
        let g = r
            .backend
            .tier_list_generations()
            .await
            .unwrap()
            .into_iter()
            .find(|x| x.dev == dev && x.ino == ino)
            .unwrap();
        let head = r.mem.head(&key).await.unwrap();
        assert_eq!(g.etag, head.etag);
    }

    /// The cold-read fan-out restores a multi-chunk file byte-identical:
    /// chunks fetch in parallel but land at their own offsets, and the
    /// stream-order CRC still verifies.
    #[tokio::test]
    async fn parallel_multi_chunk_restore_is_byte_identical() {
        let r = rig();
        // Offset-dependent bytes: any chunk landing at the wrong offset
        // (or dropped) breaks both the equality AND the CRC.
        let content: Vec<u8> = (0..1500u32).map(|i| (i.wrapping_mul(31) % 251) as u8).collect();
        let (dev, ino, _key) = evicted_file(&r, "hp1.bin", content.clone()).await;
        let f = r.root.join("hp1.bin");

        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 2,
                fetch_parallel: 4,
                chunk: 256, // 1500 bytes => 6 chunks, 4 in flight
                ..Default::default()
            },
        );
        let bytes = restore_once(&h, dev, ino, &f).await.expect("parallel restore");
        assert_eq!(bytes, 1500);
        assert_eq!(std::fs::read(&f).unwrap(), content, "chunks landed at their offsets");
        assert!(!evict::is_evicted(dev, ino));
    }

    /// A 412 on ANY chunk of a parallel fetch adopts exactly once and
    /// restarts the restore on the bucket's current object.
    #[tokio::test]
    async fn foreign_overwrite_adopts_under_parallel_chunks() {
        let r = rig();
        let (dev, ino, key) = evicted_file(&r, "hp2.bin", vec![0xABu8; 1500]).await;
        let f = r.root.join("hp2.bin");

        let foreign: Vec<u8> = (0..900u32).map(|i| (i % 197) as u8).collect();
        r.mem.raw_put(&key, bytes::Bytes::from(foreign.clone()), vec![]);

        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 2,
                fetch_parallel: 4,
                chunk: 256,
                ..Default::default()
            },
        );
        let err = restore_once(&h, dev, ino, &f).await.unwrap_err();
        assert!(err.contains("adopted"), "{}", err);
        let bytes = restore_once(&h, dev, ino, &f).await.expect("adopted restore");
        assert_eq!(bytes as usize, foreign.len());
        assert_eq!(std::fs::read(&f).unwrap(), foreign, "S3-wins under parallel fetch");
    }

    /// Exhausting one chunk's retry budget under parallel fetch fails
    /// the ATTEMPT (truncate-back, stub, marker kept) — and the next
    /// attempt succeeds. Injection is a global counter, so saturating
    /// every chunk's budget (+1) guarantees some chunk exhausts.
    #[tokio::test]
    async fn parallel_chunk_exhaustion_truncates_back_and_retry_succeeds() {
        let r = rig();
        let content = vec![0x3Cu8; 1500];
        let (dev, ino, _key) = evicted_file(&r, "hp3.bin", content.clone()).await;
        let f = r.root.join("hp3.bin");

        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 2,
                fetch_parallel: 4,
                chunk: 256, // 6 chunks x CHUNK_RETRIES budget = 30 absorbable
                ..Default::default()
            },
        );
        r.mem
            .inject_get_range_failures(6 * u64::from(super::CHUNK_RETRIES) + 1);
        let err = restore_once(&h, dev, ino, &f).await.unwrap_err();
        assert!(err.contains("chunk retries"), "{}", err);
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 0, "stub, never partial bytes");
        assert!(evict::is_evicted(dev, ino));

        let bytes = restore_once(&h, dev, ino, &f).await.expect("retry succeeds");
        assert_eq!(bytes, 1500);
        assert_eq!(std::fs::read(&f).unwrap(), content);
    }

    /// Step-9 finding 2: a WRITE-triggered hydration must not queue
    /// behind read hydrations — the reserved permit admits it while
    /// the shared pool is exhausted.
    #[tokio::test]
    async fn write_priority_takes_the_reserved_permit() {
        let r = rig();
        let (dev, ino, _key) = evicted_file(&r, "h6.bin", vec![0x77u8; 300]).await;
        let f = r.root.join("h6.bin");

        let h = local_hydrator(&r, 1);
        // Exhaust the shared pool (a long read-hydration elsewhere).
        let _hog = Arc::clone(&h.shared).acquire_owned().await.unwrap();

        let inflight = Arc::new(Inflight {
            write_pri: AtomicBool::new(true),
            demand: AtomicBool::new(true),
        });
        h.inflight.insert((dev, ino), Arc::clone(&inflight));
        let hh = Arc::clone(&h);
        let p = f.clone();
        let task = tokio::spawn(async move { run(hh, dev, ino, p, inflight).await });
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("write-priority hydration must not starve behind the shared pool")
            .unwrap();
        assert!(!evict::is_evicted(dev, ino), "restored via the reserved permit");
        assert_eq!(std::fs::read(&f).unwrap(), vec![0x77u8; 300]);
    }

    // ── the warm fill ────────────────────────────────────────────────

    async fn wait_inflight_empty(h: &Arc<Hydrator>, secs: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while !h.inflight.is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "inflight never drained: {} entr(ies) left",
                h.inflight.len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The fill restores every evicted file byte-identical — and its
    /// completion storm moves the marker-cycle counter by ~nothing
    /// (insert-only evidence: with forget-bumps this delta would be
    /// ≥ the file count deterministically; parallel tests may add a
    /// little noise, hence < not ==).
    #[tokio::test]
    async fn warm_fill_restores_every_evicted_file() {
        let r = rig();
        let mut files: Vec<(u64, u64, PathBuf, Vec<u8>)> = Vec::new();
        for i in 0..8u32 {
            // Varying sizes so smallest-first ordering is exercised.
            let content: Vec<u8> = (0..(200 + i * 137)).map(|j| ((i + j) % 251) as u8).collect();
            let name = format!("wf{}.bin", i);
            let (dev, ino, _key) = evicted_file(&r, &name, content.clone()).await;
            files.push((dev, ino, r.root.join(&name), content));
        }
        let began_cycle = evict::marker_cycle();

        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 2,
                warm_concurrency: 3,
                ..Default::default()
            },
        );
        let rep = warm_fill(&h, None).await;
        assert_eq!(rep.candidates, 8);
        assert_eq!(rep.queued, 8);
        assert_eq!(rep.restored, 8, "{:?}", rep);
        assert_eq!(rep.still_cold, 0);
        assert_eq!(rep.stopped_for_space, 0);
        for (dev, ino, path, content) in &files {
            assert!(!evict::is_evicted(*dev, *ino));
            assert_eq!(&std::fs::read(path).unwrap(), content, "{}", path.display());
        }
        wait_inflight_empty(&h, 5).await;
        assert_eq!(h.warm_admitted.load(Ordering::Relaxed), 0, "pending bytes paired");
        let delta = evict::marker_cycle() - began_cycle;
        assert!(
            delta < 8,
            "8 warm completions moved the cycle counter by {} — forgets must not bump",
            delta
        );
    }

    /// The two pools never touch: a fill runs with the demand pool
    /// exhausted, and a demand restore runs with the warm pool
    /// exhausted.
    #[tokio::test]
    async fn warm_and_demand_pools_are_independent() {
        let r = rig();
        let (dev_w, ino_w, _k) = evicted_file(&r, "pool-w.bin", vec![0x21u8; 400]).await;
        let (dev_d, ino_d, _k) = evicted_file(&r, "pool-d.bin", vec![0x42u8; 400]).await;
        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 1,
                warm_concurrency: 1,
                ..Default::default()
            },
        );

        // Demand pool exhausted → the warm restore still runs.
        let shared_hog = Arc::clone(&h.shared).acquire_owned().await.unwrap();
        request_warm(&h, dev_w, ino_w, &r.root.join("pool-w.bin"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while evict::is_evicted(dev_w, ino_w) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "warm restore starved by a hogged DEMAND pool"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Warm pool exhausted → a demand restore still runs.
        let _warm_hog = Arc::clone(&h.warm).acquire_owned().await.unwrap();
        drop(shared_hog);
        request_on(&h, dev_d, ino_d, &r.root.join("pool-d.bin"), Trigger::Read);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while evict::is_evicted(dev_d, ino_d) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "demand restore starved by a hogged WARM pool"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        wait_inflight_empty(&h, 5).await;
    }

    /// The absorb upgrade: a client read on a file the fill is (still
    /// only trying to) restore flips its lane, and the restore
    /// completes through the DEMAND pool while warm stays hogged — the
    /// 500ms-timeout acquire is what makes the upgrade observable.
    #[tokio::test]
    async fn demand_read_upgrades_a_warm_restore() {
        let r = rig();
        let (dev, ino, _k) = evicted_file(&r, "up1.bin", vec![0x5Eu8; 600]).await;
        let f = r.root.join("up1.bin");
        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 1,
                warm_concurrency: 1,
                ..Default::default()
            },
        );
        let _warm_hog = Arc::clone(&h.warm).acquire_owned().await.unwrap();
        request_warm(&h, dev, ino, &f);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(evict::is_evicted(dev, ino), "hogged warm pool: still cold");

        // The client arrives (request()'s Occupied arm): absorb.
        request_on(&h, dev, ino, &f, Trigger::Read);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while evict::is_evicted(dev, ino) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "upgraded restore never completed via the demand lane"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(std::fs::read(&f).unwrap(), vec![0x5Eu8; 600]);
        wait_inflight_empty(&h, 5).await;
    }

    /// Space refusal ABANDONS a warm restore cleanly: entry removed,
    /// counter bumped, stub untouched, hydrating flag never set.
    /// (Global-Space discipline: another test may install its own
    /// instance concurrently — re-install ours and retry on a fresh
    /// file until the refusal is observed.)
    #[tokio::test]
    async fn warm_space_refusal_abandons_cleanly() {
        let r = rig();
        let h = local_hydrator(&r, 2);
        let mut observed = false;
        for i in 0..5u32 {
            crate::tier::space::configure(crate::tier::space::SpaceConfig {
                root: r.root.clone(),
                reserve_bytes: u64::MAX, // headroom saturates to 0
                watermark_pct: 85,
                ballast_path: None,
                ballast_bytes: 0,
            })
            .unwrap();
            let name = format!("sp{}.bin", i);
            let (dev, ino, _k) = evicted_file(&r, &name, vec![0x77u8; 300]).await;
            let before = crate::tier::meter::snapshot();
            request_warm(&h, dev, ino, &r.root.join(&name));
            wait_inflight_empty(&h, 10).await;
            if evict::is_evicted(dev, ino) {
                let after = crate::tier::meter::snapshot();
                assert!(
                    after.warm_skipped_space > before.warm_skipped_space,
                    "abandon must be counted"
                );
                assert_eq!(
                    std::fs::metadata(r.root.join(&name)).unwrap().len(),
                    0,
                    "stub untouched"
                );
                let row = r
                    .backend
                    .tier_list_evicted()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|x| x.dev == dev && x.ino == ino)
                    .unwrap();
                assert_eq!(row.hydrating_unix, None, "flag never set — restore never started");
                observed = true;
                break;
            }
            // A racing global-Space install admitted it: try again.
        }
        assert!(observed, "space refusal never observed across 5 attempts");
    }

    /// WARM_MAX_ATTEMPTS bounds a warm item that can never restore
    /// (here: a poisoned CRC — every attempt fails fast), so the fill
    /// completes instead of wedging on its drain wait.
    #[tokio::test]
    async fn warm_retry_bound_abandons_and_fill_completes() {
        let r = rig();
        let (dev, ino, _k) = evicted_file(&r, "poison.bin", vec![0x13u8; 500]).await;
        let mut meta = evict::marker_meta(dev, ino).unwrap();
        meta.crc64_b64 = "bogusbogusbo".into(); // every restore attempt fails verify
        evict::update_marker(dev, ino, meta);

        let h = local_hydrator(&r, 2);
        let before = crate::tier::meter::snapshot();
        let rep = tokio::time::timeout(Duration::from_secs(30), warm_fill(&h, None))
            .await
            .expect("the fill must terminate despite an unrestorable item");
        assert_eq!(rep.candidates, 1);
        assert_eq!(rep.restored, 0);
        assert_eq!(rep.still_cold, 1);
        assert!(evict::is_evicted(dev, ino), "still evicted — a demand touch retries it");
        let after = crate::tier::meter::snapshot();
        assert!(after.warm_abandoned > before.warm_abandoned, "abandon counted");
        wait_inflight_empty(&h, 5).await;
        assert_eq!(h.warm_admitted.load(Ordering::Relaxed), 0, "reservation released");
    }

    /// The driver bound: with the warm pool hogged nothing completes,
    /// so the fill parks at ≤ 2×warm_concurrency queued restores — it
    /// never spawns the whole tree.
    #[tokio::test]
    async fn warm_driver_keeps_a_bounded_frontier() {
        let r = rig();
        for i in 0..6u32 {
            evicted_file(&r, &format!("b{}.bin", i), vec![0x99u8; 200 + i as usize]).await;
        }
        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 2,
                warm_concurrency: 1, // bound = 2
                ..Default::default()
            },
        );
        let warm_hog = Arc::clone(&h.warm).acquire_owned().await.unwrap();
        let hh = Arc::clone(&h);
        let fill = tokio::spawn(async move { warm_fill(&hh, None).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            h.inflight.len() <= 2,
            "driver frontier {} exceeded 2×warm_concurrency",
            h.inflight.len()
        );
        assert!(!fill.is_finished(), "fill must be parked at the bound, not done");
        drop(warm_hog);
        let rep = tokio::time::timeout(Duration::from_secs(30), fill)
            .await
            .expect("fill completes once the pool frees")
            .unwrap();
        assert_eq!(rep.restored, 6, "{:?}", rep);
    }

    /// The three-outcome abandon contract (review critical 1): the
    /// remove_if bool alone cannot tell a raced upgrade from a
    /// successor entry, and the two demand opposite epilogues.
    #[tokio::test]
    async fn abandon_warm_resolves_all_three_outcomes() {
        let r = rig();
        let h = local_hydrator(&r, 1);
        let (dev, ino) = (7u64, 42u64);

        // (a) still-warm entry ⇒ Removed, map cleared.
        let mine = Arc::new(Inflight {
            write_pri: AtomicBool::new(false),
            demand: AtomicBool::new(false),
        });
        h.inflight.insert((dev, ino), Arc::clone(&mine));
        assert!(matches!(abandon_warm(&h, dev, ino, &mine), WarmAbandon::Removed));
        assert!(h.inflight.get(&(dev, ino)).is_none());

        // (b) our entry, upgraded mid-abandon ⇒ Upgraded, entry KEPT
        // (it has a waiter and we are its only driver).
        let upgraded = Arc::new(Inflight {
            write_pri: AtomicBool::new(false),
            demand: AtomicBool::new(true),
        });
        h.inflight.insert((dev, ino), Arc::clone(&upgraded));
        assert!(matches!(abandon_warm(&h, dev, ino, &upgraded), WarmAbandon::Upgraded));
        assert!(h.inflight.get(&(dev, ino)).is_some(), "upgraded entry must survive");
        h.inflight.remove(&(dev, ino));

        // (c) a successor owns the key ⇒ Superseded, successor KEPT.
        let successor = Arc::new(Inflight {
            write_pri: AtomicBool::new(false),
            demand: AtomicBool::new(false),
        });
        h.inflight.insert((dev, ino), Arc::clone(&successor));
        assert!(matches!(abandon_warm(&h, dev, ino, &mine), WarmAbandon::Superseded));
        assert!(
            h.inflight
                .get(&(dev, ino))
                .is_some_and(|e| Arc::ptr_eq(e.value(), &successor)),
            "successor entry must survive a stale abandon"
        );
    }

    /// The warm_admitted pairing survives its two breakers: a 412
    /// adopt that REWRITES the marker size mid-fill, and a demand
    /// upgrade landing mid-restore. Whatever interleaves, the
    /// accumulator returns to exactly zero.
    #[tokio::test]
    async fn warm_admitted_returns_to_zero_under_adopt_and_upgrade() {
        let r = rig();
        let mut idents: Vec<(u64, u64)> = Vec::new();
        let (d1, i1, key) = evicted_file(&r, "adopt.bin", vec![0xAAu8; 512]).await;
        idents.push((d1, i1));
        for i in 0..3u32 {
            let (d, ino, _k) =
                evicted_file(&r, &format!("z{}.bin", i), vec![0x31u8; 300 + i as usize]).await;
            idents.push((d, ino));
        }
        // Foreign overwrite with a DIFFERENT size: the adopt rewrites
        // marker size between this iteration's add and its subtract.
        r.mem.raw_put(&key, bytes::Bytes::from(vec![0x0Fu8; 97]), vec![]);

        let h = local_hydrator_cfg(
            &r,
            HydrateConfig {
                hold: Duration::from_secs(2),
                concurrency: 2,
                warm_concurrency: 2,
                ..Default::default()
            },
        );
        // Best-effort mid-restore upgrade: flip the first warm entry
        // we can catch (timing-dependent; the invariant must hold
        // whether or not it lands). An upgraded entry is the CLIENT's
        // — the fill's drain may legitimately finish before it does,
        // so the asserts below are on the eventual state, not the
        // report's snapshot.
        let hh = Arc::clone(&h);
        let flipper = tokio::spawn(async move {
            for _ in 0..40 {
                if let Some(e) = hh.inflight.iter().next() {
                    e.value().demand.store(true, Ordering::Relaxed);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let rep = tokio::time::timeout(Duration::from_secs(30), warm_fill(&h, None))
            .await
            .expect("fill terminates");
        let _ = flipper.await;
        assert_eq!(rep.candidates, 4);
        assert_eq!(rep.queued, 4);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while idents.iter().any(|(d, i)| evict::is_evicted(*d, *i)) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "every file must eventually restore (fill or absorbed demand)"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            std::fs::read(r.root.join("adopt.bin")).unwrap(),
            vec![0x0Fu8; 97],
            "S3-wins under the fill"
        );
        wait_inflight_empty(&h, 10).await;
        assert_eq!(
            h.warm_admitted.load(Ordering::Relaxed),
            0,
            "pending-bytes accumulator must pair back to zero"
        );
    }

    /// The durable pending note re-arms a crashed fill; a COMPLETED
    /// fill removes it — including the nothing-to-do fill.
    #[tokio::test]
    async fn warm_fill_removes_the_pending_note() {
        let r = rig();
        evicted_file(&r, "n1.bin", vec![0x44u8; 256]).await;
        let note = r.root.join("flint-warm-fill-pending");
        std::fs::write(&note, b"warm-fill\n").unwrap();

        let h = local_hydrator(&r, 2);
        let rep = warm_fill(&h, Some(&note)).await;
        assert_eq!(rep.restored, 1);
        assert!(!note.exists(), "drained fill must remove the note");

        // Nothing evicted: the no-op fill still clears its note.
        std::fs::write(&note, b"warm-fill\n").unwrap();
        let rep = warm_fill(&h, Some(&note)).await;
        assert_eq!(rep.candidates, 0);
        assert!(!note.exists(), "no-op fill must remove the note");
    }
}
