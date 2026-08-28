//! The flush pipeline — L2 step 5 (design review A11, consuming A2's
//! capture, A3's durable bit, A4's gate, A6's intents, and step 4's
//! store/arbitration).
//!
//! Two halves:
//!
//! - [`plan_parts`] — the PURE planner: dirty intervals → a part
//!   layout. Per-generation part sizing on a fixed grid
//!   (`max(floor, ceil(size/max_parts))`, rounded up to a
//!   power-of-two multiple of the floor), clean-slot classification
//!   honoring the whole flag, the truncate watermark (`min_size` —
//!   bytes at/beyond it may NEVER be clean-copied), the base object's
//!   size, and the A11 IA copy-source guard; adjacent clean slots
//!   coalesce into single copy parts up to the 5 GiB copy-part limit
//!   (the knob that collapses the barely-dirty-file copy fan-out from
//!   ~$8,280/mo/file to ~$1.73). 5 TiB refused with a clear error.
//!
//! - [`FlushOrchestrator`] — drives files through the pipeline:
//!   eligibility (per-file flush floor ~60 s + the quiescence guard —
//!   never flush a file whose write stream is still advancing),
//!   single-flight via the gate, durable intent BEFORE any store op
//!   (A6), epoch swap under the gate barrier, CRC-64/NVME from local
//!   truth, publish (whole-put below threshold, compose above), and
//!   on any 412 the HEAD arbitration — adopt / retry / foreign, never
//!   an operator page. The clean-clear protocol releases the durable
//!   bit ONLY via the observed-sequence conditional delete, so an
//!   acked mutation's bit can never be lost to a racing clear (A3).
//!
//! Generation state (base ETag/gen per file) is IN-MEMORY here; step 6
//! (A7) makes it durable and identity-keyed. Until then a restart
//! rediscovers bases by HEAD — and skips the upload entirely when the
//! bucket's CRC already matches local truth (the provably-clean-skip
//! from step 2's drill list).

use crate::state_backend::{FlushIntentRecord, StateBackend, TierDirtyEntry};
use crate::tier::arbitrate::{arbitrate, IntentProbe, Verdict};
use crate::tier::capture::{self, FileCapture};
use crate::tier::gate;
use crate::tier::meter::{self, Counter};
use crate::tier::store::{
    crc64_to_b64, ComposeSpec, Crc64Nvme, GenerationStamps, ObjectMeta, ObjectStore,
    PartSource, PutCondition, StoreError,
};
use bytes::Bytes;
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// 5 TiB — S3's object ceiling, enforced with a clear error (A11).
pub const MAX_OBJECT_SIZE: u64 = 5 * 1024 * 1024 * 1024 * 1024;
/// 5 GiB — the UploadPartCopy per-part ceiling (A11's coalescing cap).
pub const COPY_PART_MAX: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct FlushConfig {
    /// Filesystem root the hub exports; keys are paths relative to it.
    pub export_root: PathBuf,
    /// Key prefix under the bucket (e.g. "vol1/").
    pub key_prefix: String,
    /// A11 knob: per-file flush-interval floor (~60 s). On-close is
    /// flush-ELIGIBILITY, not flush-now; this floor caps any
    /// continuously-hot file's request bill.
    pub floor: Duration,
    /// A11 knob: quiescence guard — skip files noted more recently
    /// than this.
    pub quiesce: Duration,
    /// Below this, publish as one conditional PUT (body in memory).
    pub whole_put_max: u64,
    /// Part-grid floor. Must be ≥ the backend's minimum part size.
    pub part_floor: u64,
}

impl FlushConfig {
    pub fn new(export_root: PathBuf, key_prefix: String) -> Self {
        FlushConfig {
            export_root,
            key_prefix,
            floor: Duration::from_secs(60),
            quiesce: Duration::from_secs(10),
            whole_put_max: 64 * 1024 * 1024,
            part_floor: 16 * 1024 * 1024,
        }
    }
}

/// The generation registry entry — a write-through cache of the A7
/// durable rows (`tier_generation`), identity-keyed with the bucket
/// key as a mutable attribute.
#[derive(Debug, Clone)]
pub struct GenRecord {
    /// Where the current generation LIVES in the bucket. When this
    /// differs from the path-derived desired key, the file was renamed
    /// and the next flush performs the guarded bucket re-key.
    pub key: String,
    pub generation: u64,
    pub etag: String,
    pub crc64_b64: Option<String>,
    pub size: u64,
    /// A11 IA guard + foreign-recovery: false forbids BaseCopy from
    /// this object (non-Standard class, or content we cannot vouch
    /// for).
    pub copy_allowed: bool,
}

// ── the planner ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct PlanBase {
    pub size: u64,
    pub copy_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// One conditional PUT of the whole file.
    WholePut,
    /// Multipart compose with this part grid.
    Compose { part_size: u64, parts: Vec<PartSource> },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error(
        "file is {size} bytes — the tier's per-file ceiling is 5 TiB (A11); \
         this file cannot be tiered"
    )]
    TooLarge { size: u64 },
}

/// Dirty intervals → part layout. Pure; see the module docs.
pub fn plan_parts(
    size: u64,
    cap: &FileCapture,
    base: Option<PlanBase>,
    cfg: &FlushConfig,
    max_parts: usize,
) -> Result<Plan, PlanError> {
    if size > MAX_OBJECT_SIZE {
        return Err(PlanError::TooLarge { size });
    }
    if size <= cfg.whole_put_max {
        return Ok(Plan::WholePut);
    }

    // Grid: max(floor, ceil(size / max_parts)) rounded UP to a
    // power-of-two multiple of the floor (A11's fixed grid — one dirty
    // 4 KiB page re-uploads one part, so the grid is also the
    // amplification curve the economics model carries).
    let need = size.div_ceil(max_parts as u64);
    let mut part_size = cfg.part_floor.max(1);
    while part_size < need {
        part_size *= 2;
    }

    // A slot may be served by copy iff every byte of it is vouched
    // for: base present and copy-allowed, capture not whole, slot
    // fully below BOTH the base's size and the truncate watermark
    // (bytes at/beyond min_size have been shorter since gen g — a
    // clean-copy would resurrect them; finding C5's shrink-regrow
    // corruption), and no dirty interval overlaps.
    let copy_ceiling = match (&base, cap.whole) {
        (Some(b), false) if b.copy_allowed => b.size.min(cap.min_size.unwrap_or(u64::MAX)),
        _ => 0,
    };
    let overlaps_dirty = |lo: u64, hi: u64| -> bool {
        // Sorted disjoint intervals: find the first with end > lo.
        let idx = cap.intervals.partition_point(|&(_, e)| e <= lo);
        cap.intervals.get(idx).is_some_and(|&(s, _)| s < hi)
    };

    let slots = size.div_ceil(part_size);
    let mut parts: Vec<PartSource> = Vec::new();
    for slot in 0..slots {
        let lo = slot * part_size;
        let hi = (lo + part_size).min(size);
        let len = hi - lo;
        let clean = hi <= copy_ceiling && !overlaps_dirty(lo, hi);
        if clean {
            // Coalesce into the previous copy run up to the 5 GiB
            // copy-part ceiling.
            if let Some(PartSource::BaseCopy { offset, len: run }) = parts.last_mut() {
                if *offset + *run == lo && *run + len <= COPY_PART_MAX {
                    *run += len;
                    continue;
                }
            }
            parts.push(PartSource::BaseCopy { offset: lo, len });
        } else {
            parts.push(PartSource::Local { offset: lo, len });
        }
    }
    Ok(Plan::Compose { part_size, parts })
}

// ── the orchestrator ─────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Published { to_gen: u64 },
    /// Our torn earlier publish was adopted via arbitration.
    Adopted { to_gen: u64 },
    /// Bucket content already equals local truth — no upload.
    CleanMatch,
    /// Nothing captured and the bit cleared (or stayed, harmlessly).
    NothingToFlush,
    /// Fenced (412) and arbitration says re-flush next cycle.
    RetryNextCycle,
    /// Foreign interference recorded; next cycle re-publishes local
    /// truth guarded on the CURRENT object (A6: publish path is
    /// local-wins).
    ForeignDetected,
    SkippedSingleFlight,
    /// Path no longer names this identity (rename/reuse) — A7's rows
    /// (step 6) own the repair.
    PathMismatch,
    /// The epoch guard is fenced (A8): publishing is forbidden. The
    /// dirty bit stays durable; a re-claimed epoch resumes the flush.
    Fenced,
    /// The file's eviction marker is set (step 10, C2): NEVER upload
    /// bytes from a marker-set file — its local content is the 0-byte
    /// stub, not data.
    SkippedEvicted,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct TickReport {
    pub examined: usize,
    pub published: usize,
    pub clean: usize,
    pub skipped_floor: usize,
    pub skipped_quiesce: usize,
    pub failed: usize,
}

pub struct FlushOrchestrator {
    store: Arc<dyn ObjectStore>,
    backend: Arc<dyn StateBackend>,
    cfg: FlushConfig,
    /// A8: the epoch guard the heartbeat fences. Every flush re-checks
    /// it at entry AND immediately before the publish; `None` from
    /// `current()` forbids publishing.
    epoch: Arc<crate::tier::epoch::EpochGuard>,
    /// Set when the start-up import REFUSED — the bucket HAS a manifest
    /// and this hub could not read it.
    ///
    /// The export being served is then EMPTY and does not describe the
    /// bucket, so anything published from it is a lie about the tree.
    /// The barrier is the dangerous half: directories, symlinks and
    /// every mode/uid/gid live ONLY in the manifest, so one barrier
    /// over a real manifest erases them, `rpo::evaluate` then reports
    /// clean, and the idle ladder reclaims the disk that held the last
    /// copy. `server.rs` already logs "do NOT let it publish over the
    /// bucket" at the refusal; this is what makes that true.
    ///
    /// Deliberately one-way and process-local: the retry is a restart
    /// with a readable manifest, which is exactly what the intent note
    /// left behind arranges.
    publish_fenced: std::sync::atomic::AtomicBool,
    generations: DashMap<(u64, u64), GenRecord>,
    last_flush: DashMap<(u64, u64), Instant>,
    /// Step 12 (A12): the DR manifest writer's guard/seq state; the
    /// manifest itself is written at the end of every tick (the flush
    /// barrier) when its content changed.
    manifest: tokio::sync::Mutex<crate::tier::manifest::WriterState>,
    /// The most recent barrier's outcome — the only durable answer to
    /// "does the bucket currently describe this tree?". Read by the
    /// RPO predicate the lifecycle controller gates suspend and
    /// hibernate on. `None` until the first barrier runs, which is
    /// deliberately NOT clean: a hub that has not yet described itself
    /// to the bucket must not be hibernated on the strength of an
    /// empty dirty list.
    last_barrier: std::sync::Mutex<Option<crate::tier::manifest::BarrierOutcome>>,
}

impl FlushOrchestrator {
    pub fn new(
        store: Arc<dyn ObjectStore>,
        backend: Arc<dyn StateBackend>,
        cfg: FlushConfig,
        epoch: Arc<crate::tier::epoch::EpochGuard>,
    ) -> Self {
        FlushOrchestrator {
            store,
            backend,
            cfg,
            epoch,
            publish_fenced: std::sync::atomic::AtomicBool::new(false),
            generations: DashMap::new(),
            last_flush: DashMap::new(),
            manifest: tokio::sync::Mutex::new(crate::tier::manifest::WriterState::default()),
            last_barrier: std::sync::Mutex::new(None),
        }
    }

    /// The last barrier's outcome, for the status surface and the RPO
    /// predicate. `None` before the first barrier of this process.
    pub fn last_barrier(&self) -> Option<crate::tier::manifest::BarrierOutcome> {
        self.last_barrier.lock().ok().and_then(|g| g.clone())
    }

    pub fn key_for(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.cfg.export_root).ok()?;
        // `.flint/` is the tier's own control namespace (the epoch
        // object; step 12's manifests). A client file there must never
        // shadow a control object.
        //
        // Checked at EVERY depth, not just the first component. A hub
        // whose prefix is an ancestor of another share's — the case
        // `lite_operator::conflict` exists to refuse — sees the inner
        // share's `nested/.flint/epoch` as an ordinary relative path,
        // and a first-component test would map it straight back to a
        // key that overwrites that share's LIVE epoch cell. `.flint`
        // is reserved throughout the tree, so there is no legitimate
        // client file at any depth to lose.
        if rel
            .components()
            .any(|c| c.as_os_str() == crate::tier::epoch::RESERVED_DIR)
        {
            warn!(
                "tier flush: {} is under the reserved {}/ namespace — not tiered",
                path.display(),
                crate::tier::epoch::RESERVED_DIR
            );
            return None;
        }
        Some(format!("{}{}", self.cfg.key_prefix, rel.to_string_lossy()))
    }

    /// Test/observability surface.
    pub fn generation_of(&self, dev: u64, ino: u64) -> Option<GenRecord> {
        self.generations.get(&(dev, ino)).map(|e| e.clone())
    }

    /// Startup: durable registry first, then intent arbitration, then
    /// the manifest seq/guard seed (step 12 — the manifest's own seq
    /// is bucket data, so it survives even total local state loss).
    /// Returns what the seed found in the bucket, so the importer can
    /// consume the SAME read instead of issuing its own GET of the same
    /// object moments later. The three arms are load-bearing — see
    /// [`crate::tier::manifest::ManifestSeed`].
    pub async fn startup(&self) -> crate::tier::manifest::ManifestSeed {
        self.heal_generation_device().await;
        self.heal_evicted_and_dirty_device().await;
        let n = self.load_generations().await;
        let i = self.reconcile_intents().await;
        let seed =
            crate::tier::manifest::seed_full(self.store.as_ref(), &self.cfg.key_prefix).await;
        *self.manifest.lock().await = seed.writer_state_ref().into_owned();
        let out = seed;
        if n > 0 || i > 0 {
            info!("tier flush: startup loaded {} generation row(s), reconciled {} intent(s)", n, i);
        }
        out
    }

    /// Re-home generation rows whose `dev` no longer matches the mounted
    /// export, and prune the ones that no longer name a live file.
    ///
    /// Generation rows are keyed `(dev, ino)`, and `dev` is stable only
    /// by luck: it is the device number of the mounted volume, and a
    /// CSI restage can hand the volume back on a different minor. When
    /// it drifts, EVERY row is still loaded but no longer matches
    /// anything the tree walk finds, so `manifest::build` counts every
    /// file as `beyond_rpo` and DROPS it from the manifest — then the
    /// barrier publishes that manifest over the good one. Measured on a
    /// real cluster: `dev` moved 66311 → 66312 and
    /// `tenant-a/.flint/manifest` went from 7919 bytes and 37 entries
    /// to 534 bytes and 4 entries, with all 33 data objects still
    /// present in the bucket but no longer named by it. `rpoClean` then
    /// stays false forever, so hibernate is blocked permanently — which
    /// is the only reason that bucket did not lose its POSIX metadata.
    ///
    /// The prune is not optional. Re-homing alone would let a row whose
    /// file was deleted during a drifted boot (its delete missed,
    /// because deletes match on `dev` too) collide with a REUSED inode
    /// and claim someone else's S3 key. So a row survives only if its
    /// inode is still live under the export root.
    async fn heal_generation_device(&self) {
        let root = self.cfg.export_root.clone();
        let live_dev = match std::fs::metadata(&root) {
            Ok(md) => {
                use std::os::unix::fs::MetadataExt;
                md.dev()
            }
            Err(e) => {
                warn!("tier flush: cannot stat export root to check generation dev: {}", e);
                return;
            }
        };
        let rows = match self.backend.tier_list_generations().await {
            Ok(r) => r,
            Err(e) => {
                warn!("tier flush: cannot read generation rows to check dev: {}", e);
                return;
            }
        };
        let stale: Vec<_> = rows.iter().filter(|r| r.dev != live_dev).collect();
        if stale.is_empty() {
            return;
        }
        // Only now — a drift is rare — pay for a tree walk.
        let live_inos = match tokio::task::spawn_blocking(move || live_inodes(&root)).await {
            Ok(Ok(set)) => set,
            _ => {
                warn!("tier flush: generation dev drifted but the export walk failed — \
                       leaving rows alone rather than guessing");
                return;
            }
        };
        // Newest wins when two rows land on one inode.
        let mut best: std::collections::HashMap<u64, &crate::state_backend::TierGenerationRow> =
            std::collections::HashMap::new();
        for r in &stale {
            if !live_inos.contains(&r.ino) {
                continue;
            }
            best.entry(r.ino)
                .and_modify(|cur| {
                    if r.updated_unix > cur.updated_unix {
                        *cur = r;
                    }
                })
                .or_insert(r);
        }
        // INSERT BEFORE DELETE, and the order is the whole point.
        //
        // The two keys differ (`dev` differs), so the re-homed row never
        // collides with the stale one and the pair can coexist. Deleting
        // first would open a window where a crash leaves NEITHER — which
        // is precisely the bug being repaired here, made permanent for
        // those files. Insert-first makes the migration idempotent and
        // crash-safe: the worst a crash leaves is a duplicate under a
        // dead device number, which the next boot re-homes and prunes.
        let (mut rehomed, mut dropped) = (0usize, 0usize);
        let mut kept: Vec<(u64, u64)> = Vec::new();
        for (ino, r) in best {
            let row = crate::state_backend::TierGenerationRow {
                dev: live_dev,
                ino,
                key: r.key.clone(),
                generation: r.generation,
                etag: r.etag.clone(),
                crc64_b64: r.crc64_b64.clone(),
                size: r.size,
                copy_allowed: r.copy_allowed,
                updated_unix: r.updated_unix,
            };
            match self.backend.tier_upsert_generation(&row).await {
                Ok(()) => {
                    rehomed += 1;
                    kept.push((r.dev, ino));
                }
                Err(e) => warn!("tier flush: re-homing generation row ino {} failed: {}", ino, e),
            }
        }
        // Now the old rows can go: every survivor already has its
        // re-homed twin durable. A row whose re-home FAILED is left
        // alone rather than deleted — losing it is the failure this
        // whole function exists to prevent.
        for r in &stale {
            let survived = kept.iter().any(|(d, i)| *d == r.dev && *i == r.ino);
            let prunable = !live_inos.contains(&r.ino);
            if survived || prunable {
                let _ = self.backend.tier_delete_generation(r.dev, r.ino).await;
            }
        }
        dropped += stale.len().saturating_sub(rehomed);
        warn!(
            "🔧 tier flush: export device changed to {} — re-homed {} generation row(s), \
             dropped {} that no longer name a live file. Without this every file would \
             have counted beyond RPO and the next manifest barrier would have published \
             a manifest that names none of them.",
            live_dev, rehomed, dropped
        );
    }

    /// Re-home `tier_evicted` and `tier_dirty` rows across the same
    /// device drift `heal_generation_device` repairs — audit blocker 4.
    ///
    /// # Why healing only the generation half made things worse
    ///
    /// All three tier tables are keyed `(dev, ino)`, and `dev` is stable
    /// only by luck: it is the device number of the mounted volume, and
    /// a CSI restage can hand the volume back on a different minor
    /// (measured on a real cluster: 66311 → 66312). Only
    /// `tier_generation` was ever re-homed. The other two were left
    /// stranded, and the `tier_evicted` half is the destructive one:
    ///
    ///   1. A file is evicted. Its local inode is truncated to a stub
    ///      and `tier_evicted` holds the real size and the bucket key.
    ///   2. `dev` drifts across a restage.
    ///   3. `evict::is_evicted(dev, ino)` now MISSES, because the row is
    ///      filed under the dead device number. The stub is no longer
    ///      recognised as a stub — it is just an empty file.
    ///   4. READ returns `(empty, eof)` with NFS4_OK. GETATTR reports
    ///      size 0. The client is told, authoritatively, that the file
    ///      is empty. There is no error anywhere.
    ///   5. Because the GENERATION half *was* healed, the flush path
    ///      still recognises the file — and republishes that emptiness
    ///      over the intact S3 object.
    ///
    /// Step 5 is what turns a recoverable local miss into permanent
    /// loss, and it exists *because* the previous fix was partial. A
    /// half-healed database is worse than an unhealed one: unhealed, the
    /// flusher would have refused to touch the object at all.
    ///
    /// # Re-homing by inode, not by path
    ///
    /// Rows carry a `path`, but a file renamed during a drifted boot
    /// keeps its inode and changes its path — and pruning a row whose
    /// path merely moved would delete the very row that makes its stub
    /// readable, which is the data loss this function exists to
    /// prevent. So the walk is keyed by inode and the path is REPAIRED
    /// from it. A row survives only if its inode is still live under the
    /// export root; anything else is a stale row that could collide with
    /// a reused inode and claim another file's S3 key.
    ///
    /// Insert-before-delete, for the reason spelled out in
    /// `heal_generation_device`: the two keys differ, so the pair can
    /// coexist, and the worst a crash leaves is a duplicate under a dead
    /// device number that the next boot re-homes and prunes. Deleting
    /// first would leave NEITHER row — the bug, made permanent.
    async fn heal_evicted_and_dirty_device(&self) {
        let root = self.cfg.export_root.clone();
        let live_dev = match std::fs::metadata(&root) {
            Ok(md) => {
                use std::os::unix::fs::MetadataExt;
                md.dev()
            }
            Err(e) => {
                warn!("tier flush: cannot stat export root to check evicted/dirty dev: {}", e);
                return;
            }
        };

        let ev_rows = match self.backend.tier_list_evicted().await {
            Ok(r) => r,
            Err(e) => {
                warn!("tier flush: cannot read evicted rows to check dev: {}", e);
                return;
            }
        };
        let dirty_rows = match self.backend.tier_list_dirty().await {
            Ok(r) => r,
            Err(e) => {
                warn!("tier flush: cannot read dirty rows to check dev: {}", e);
                return;
            }
        };
        let ev_stale: Vec<_> = ev_rows.iter().filter(|r| r.dev != live_dev).collect();
        let dirty_stale: Vec<_> = dirty_rows.iter().filter(|r| r.dev != live_dev).collect();
        if ev_stale.is_empty() && dirty_stale.is_empty() {
            return;
        }

        // A drift is rare; only now pay for the tree walk.
        let live = match tokio::task::spawn_blocking(move || live_inode_paths(&root)).await {
            Ok(Ok(map)) => map,
            _ => {
                warn!(
                    "tier flush: evicted/dirty dev drifted but the export walk failed — \
                     leaving rows alone rather than guessing. Evicted files will read as \
                     ZERO BYTES until this succeeds; the flusher is the greater hazard \
                     and it is gated on the generation rows, which are healed separately"
                );
                return;
            }
        };

        // ── tier_evicted: the destructive half ───────────────────────
        let (mut ev_rehomed, mut ev_dropped) = (0usize, 0usize);
        for r in &ev_stale {
            let Some(live_path) = live.get(&r.ino) else {
                // The inode is gone from the export entirely. Keeping the
                // row would let it collide with a future inode reuse and
                // hand that file this one's S3 key.
                let _ = self.backend.tier_delete_evicted(r.dev, r.ino).await;
                ev_dropped += 1;
                continue;
            };
            let row = crate::state_backend::TierEvictedRow {
                dev: live_dev,
                ino: r.ino,
                key: r.key.clone(),
                generation: r.generation,
                etag: r.etag.clone(),
                crc64_b64: r.crc64_b64.clone(),
                size: r.size,
                // Repaired from the walk: a rename during a drifted boot
                // moved the stub, and the stored path would send the
                // reconciler at nothing.
                path: live_path.to_string_lossy().into_owned(),
                evicted_unix: r.evicted_unix,
                hydrating_unix: r.hydrating_unix,
            };
            match self.backend.tier_put_evicted(&row).await {
                Ok(()) => {
                    ev_rehomed += 1;
                    // Only now is it safe: the re-homed twin is durable.
                    let _ = self.backend.tier_delete_evicted(r.dev, r.ino).await;
                }
                Err(e) => warn!(
                    "tier flush: re-homing evicted row ino {} failed: {} — leaving the \
                     stale row in place; losing it would make the stub read as zero bytes",
                    r.ino, e
                ),
            }
        }

        // ── tier_dirty ───────────────────────────────────────────────
        // A stranded dirty bit is not destructive by itself, but it is
        // the bit that BLOCKS eviction of a file with unflushed changes.
        // Stranded, the file looks clean, becomes eviction-eligible, and
        // its unflushed local changes are discarded in favour of an
        // older bucket object.
        let (mut d_rehomed, mut d_dropped) = (0usize, 0usize);
        for r in &dirty_stale {
            let Some(live_path) = live.get(&r.ino) else {
                let _ = self.backend.tier_clear_dirty(r.dev, r.ino).await;
                d_dropped += 1;
                continue;
            };
            let entry = crate::state_backend::TierDirtyEntry {
                dev: live_dev,
                ino: r.ino,
                path: Some(live_path.to_string_lossy().into_owned()),
                dirtied_unix: r.dirtied_unix,
                mark_seq: r.mark_seq,
            };
            match self.backend.tier_mark_dirty(std::slice::from_ref(&entry)).await {
                Ok(()) => {
                    d_rehomed += 1;
                    let _ = self.backend.tier_clear_dirty(r.dev, r.ino).await;
                }
                Err(e) => warn!(
                    "tier flush: re-homing dirty row ino {} failed: {} — leaving the \
                     stale row; losing it would let an unflushed file be evicted",
                    r.ino, e
                ),
            }
        }

        warn!(
            "🔧 tier flush: export device changed to {} — re-homed {} evicted row(s) \
             ({} dropped) and {} dirty row(s) ({} dropped). Without the evicted half, \
             every evicted file would have read as ZERO BYTES with NFS4_OK and the next \
             flush would have republished that emptiness over the intact bucket object.",
            live_dev, ev_rehomed, ev_dropped, d_rehomed, d_dropped
        );
    }

    /// Rebuild the registry from the A7 rows — the rows are the truth
    /// (identity events delete/re-point them OUTSIDE the orchestrator,
    /// so the cache must be replaced, not merged: a stale entry for a
    /// covered file would keep its key "live", defer its tombstone,
    /// and manufacture exactly the false 412 this step exists to
    /// kill). Runs at startup and at every tick start.
    pub async fn load_generations(&self) -> usize {
        match self.backend.tier_list_generations().await {
            Ok(rows) => {
                let n = rows.len();
                self.generations.clear();
                for r in rows {
                    self.generations.insert(
                        (r.dev, r.ino),
                        GenRecord {
                            key: r.key,
                            generation: r.generation,
                            etag: r.etag,
                            crc64_b64: r.crc64_b64,
                            size: r.size,
                            copy_allowed: r.copy_allowed,
                        },
                    );
                }
                n
            }
            Err(e) => {
                warn!("tier flush: cannot load generation rows: {}", e);
                0
            }
        }
    }

    /// Write-through: registry + durable row together.
    async fn record_generation(&self, dev: u64, ino: u64, rec: GenRecord) {
        let row = crate::state_backend::TierGenerationRow {
            dev,
            ino,
            key: rec.key.clone(),
            generation: rec.generation,
            etag: rec.etag.clone(),
            crc64_b64: rec.crc64_b64.clone(),
            size: rec.size,
            copy_allowed: rec.copy_allowed,
            updated_unix: now_unix(),
        };
        if let Err(e) = self.backend.tier_upsert_generation(&row).await {
            // The registry still advances — worst case a restart
            // rediscovers by HEAD (the pre-A7 posture), never corrupt.
            warn!("tier flush: generation row upsert ({},{}): {}", dev, ino, e);
        }
        self.generations.insert((dev, ino), rec);
    }

    async fn drop_generation(&self, dev: u64, ino: u64) {
        let _ = self.backend.tier_delete_generation(dev, ino).await;
        self.generations.remove(&(dev, ino));
    }

    /// The flush barrier's first act: delete every tombstoned key
    /// (REMOVE victims, rename-over's covered files, re-key leftovers)
    /// BEFORE any publish — a renamed file's create-flavor publish at
    /// its new key depends on the covered object being gone.
    pub async fn consume_tombstones(&self) -> usize {
        let tombs = match self.backend.tier_list_tombstones().await {
            Ok(t) => t,
            Err(e) => {
                warn!("tier flush: cannot list tombstones: {}", e);
                return 0;
            }
        };
        // NEVER delete a key some generation row still lives at: a
        // re-key writes its old-key tombstone BEFORE publishing under
        // the new key, and until that publish lands (crash, 412 retry)
        // the old object is the generation's only bucket copy. The row
        // re-points at publish; the tombstone becomes consumable then.
        let live: std::collections::HashMap<String, String> =
            self.generations.iter().map(|e| (e.key.clone(), e.etag.clone())).collect();
        let mut consumed = 0;
        for t in tombs {
            if let Some(live_etag) = live.get(&t.key) {
                // Step 12's tidy: a tombstone SUPERSEDED by a
                // legitimate later publish at the same key (the live
                // row carries a different etag) names an object that
                // no longer exists — close it WITHOUT deleting
                // anything. Etag-less or same-etag tombstones keep
                // deferring (the re-key crash window).
                if t.etag.as_ref().is_some_and(|e| e != live_etag) {
                    if let Err(e) = self.backend.tier_delete_tombstone(&t.key).await {
                        warn!("tier flush: cannot close superseded tombstone {}: {}", t.key, e);
                    } else {
                        consumed += 1;
                        debug!("tier flush: superseded tombstone {} closed", t.key);
                    }
                } else {
                    debug!("tier flush: tombstone {} deferred (key still live)", t.key);
                }
                continue;
            }
            match self.store.head(&t.key).await {
                Ok(meta) => {
                    if let Some(want) = &t.etag {
                        if want != &meta.etag {
                            warn!(
                                "tier flush: tombstoned {} carries etag {} (expected {}) — \
                                 foreign interference; deleting anyway (the local tree is \
                                 the authority and the path is gone)",
                                t.key, meta.etag, want
                            );
                        }
                    }
                    if let Err(e) = self.store.delete(&t.key).await {
                        warn!("tier flush: tombstone delete {} failed: {}", t.key, e);
                        continue; // keep the tombstone; retry next tick
                    }
                }
                Err(StoreError::NotFound(_)) => {} // already gone
                Err(e) => {
                    warn!("tier flush: tombstone HEAD {} failed: {}", t.key, e);
                    continue;
                }
            }
            if let Err(e) = self.backend.tier_delete_tombstone(&t.key).await {
                warn!("tier flush: cannot close tombstone {}: {}", t.key, e);
            } else {
                consumed += 1;
                debug!("tier flush: tombstone consumed: {}", t.key);
            }
        }
        consumed
    }

    /// Startup: arbitrate every interrupted flush intent by HEAD (A6 —
    /// a crashed flush is adopt/retry/foreign, never a runbook page).
    /// Runs BEFORE the first tick.
    pub async fn reconcile_intents(&self) -> usize {
        let intents = match self.backend.list_flush_intents().await {
            Ok(v) => v,
            Err(e) => {
                warn!("tier flush: cannot list intents at startup: {}", e);
                return 0;
            }
        };
        let mut handled = 0usize;
        for intent in intents {
            let Some(key) = self.key_for(Path::new(&intent.path)) else {
                warn!("tier flush: intent {} path outside export root", intent.flush_uuid);
                continue;
            };
            let probe = IntentProbe {
                key: &key,
                to_gen: intent.to_gen,
                flush_uuid: &intent.flush_uuid,
                base_etag: intent.base_etag.as_deref(),
            };
            match arbitrate(self.store.as_ref(), &probe).await {
                Ok(Verdict::AdoptOwn(meta)) => {
                    meter::bump(Counter::ArbitrateAdoptOwn);
                    info!(
                        "tier flush: adopting torn publish of {} at gen {} (crash between \
                         Complete and commit)",
                        key, intent.to_gen
                    );
                    // Seed the registry if the path still names an
                    // identity; the whole-dirty restore re-flushes on
                    // top of the adopted generation.
                    if let Some((dev, ino)) = stat_identity(Path::new(&intent.path)) {
                        self.record_generation(
                            dev,
                            ino,
                            GenRecord {
                                key: key.clone(),
                                generation: intent.to_gen,
                                etag: meta.etag.clone(),
                                crc64_b64: meta.crc64_b64.clone(),
                                size: meta.size,
                                copy_allowed: meta.copy_source_allowed(),
                            },
                        )
                        .await;
                    }
                }
                Ok(Verdict::RetryFromBase) => {
                    meter::bump(Counter::ArbitrateRetryFromBase);
                    if let Some(mpu) = &intent.mpu_id {
                        if let Err(e) = self.store.abort_upload(&key, mpu).await {
                            warn!("tier flush: reconcile abort of {} failed: {}", mpu, e);
                        } else {
                            meter::bump(Counter::MpuAborts);
                        }
                    }
                }
                Ok(Verdict::Foreign(_)) => {
                    // No successor_check here: reconcile runs right
                    // after a fresh claim, when OUR epoch is maximal by
                    // construction — no object can carry a higher stamp.
                    meter::bump(Counter::ArbitrateForeign);
                    warn!(
                        "tier flush: intent {} for {} found FOREIGN bucket state; local \
                         truth re-publishes guarded on the current object (A6 local-wins)",
                        intent.flush_uuid, key
                    );
                }
                Err(e) => {
                    warn!("tier flush: reconcile arbitration for {} failed: {}", key, e);
                    continue; // keep the intent; retry next startup/tick
                }
            }
            if let Err(e) = self.backend.delete_flush_intent(&intent.flush_uuid).await {
                warn!("tier flush: cannot close intent {}: {}", intent.flush_uuid, e);
            }
            handled += 1;
        }
        handled
    }

    /// One scheduling pass: tombstones first (A7 — a renamed file's
    /// publish at its new key depends on the covered object being
    /// gone), then the durable dirty set.
    /// Forbid every publish for the life of this process.
    ///
    /// Called when the start-up import refused: this hub is serving an
    /// empty export over a bucket that has real content.
    pub fn fence_publishing(&self, why: &str) {
        self.publish_fenced
            .store(true, std::sync::atomic::Ordering::Release);
        warn!(
            "tier flush: publishing FENCED for this process — {why}. The export does not \
             describe the bucket, so no file and no manifest barrier will be written. \
             Fix the manifest object (or restore a versioned copy) and restart."
        );
    }

    pub fn is_publish_fenced(&self) -> bool {
        self.publish_fenced
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub async fn tick(&self) -> TickReport {
        let mut report = TickReport::default();
        // BEFORE the epoch check: this hub may legitimately hold the
        // epoch and still have nothing truthful to say about the tree.
        if self.is_publish_fenced() {
            return report;
        }
        if self.epoch.current().is_none() {
            // Fenced (A8): nothing may publish. The dirty set stays
            // durable; the fence event itself already logged loudly.
            debug!("tier flush: tick skipped — epoch fenced/not held");
            return report;
        }
        self.load_generations().await;
        self.consume_tombstones().await;
        let rows = match self.backend.tier_list_dirty().await {
            Ok(r) => r,
            Err(e) => {
                warn!("tier flush: cannot list dirty set: {}", e);
                return report;
            }
        };
        for row in rows {
            report.examined += 1;
            let (dev, ino) = (row.dev, row.ino);
            if let Some(t) = self.last_flush.get(&(dev, ino)) {
                if t.elapsed() < self.cfg.floor {
                    meter::bump(Counter::FlushesSkippedFloor);
                    report.skipped_floor += 1;
                    continue;
                }
            }
            if let Some(n) = capture::last_note(dev, ino) {
                if n.elapsed() < self.cfg.quiesce {
                    meter::bump(Counter::FlushesSkippedQuiesce);
                    report.skipped_quiesce += 1;
                    continue;
                }
            }
            match self.flush_file(&row).await {
                Outcome::Published { .. } | Outcome::Adopted { .. } => report.published += 1,
                Outcome::CleanMatch | Outcome::NothingToFlush => report.clean += 1,
                Outcome::SkippedSingleFlight => {
                    meter::bump(Counter::FlushesSkippedInflight);
                }
                Outcome::RetryNextCycle
                | Outcome::ForeignDetected
                | Outcome::PathMismatch
                | Outcome::Fenced
                | Outcome::SkippedEvicted => {}
                Outcome::Failed(_) => report.failed += 1,
            }
        }
        self.write_manifest_barrier().await;
        report
    }

    /// Step 12 (A12): the flush barrier's closing act — rewrite the DR
    /// manifest when the tree/RPO changed. Failure is non-fatal (the
    /// previous manifest stays the RPO record one barrier longer).
    pub async fn write_manifest_barrier(&self) {
        // An unreadable manifest at import ⇒ never describe the tree.
        // Guarded here as well as in `tick`, because this is the call
        // that destroys data and it is `pub`.
        if self.is_publish_fenced() {
            return;
        }
        // Fenced mid-tick ⇒ no barrier: a deposed hub must not
        // describe a bucket it no longer owns.
        let Some(epoch) = self.epoch.current() else { return };
        let gens: std::collections::HashMap<(u64, u64), GenRecord> =
            self.generations.iter().map(|e| (*e.key(), e.value().clone())).collect();
        let root = self.cfg.export_root.clone();
        let built = match tokio::task::spawn_blocking(move || {
            crate::tier::manifest::build(&root, &gens)
        })
        .await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                warn!("tier manifest: tree walk failed: {} — barrier skipped", e);
                self.record_barrier_failure();
                return;
            }
            Err(e) => {
                warn!("tier manifest: walk task failed: {} — barrier skipped", e);
                self.record_barrier_failure();
                return;
            }
        };
        let mut st = self.manifest.lock().await;
        let outcome = crate::tier::manifest::write_at_barrier(
            self.store.as_ref(),
            &self.cfg.key_prefix,
            epoch,
            &mut st,
            built,
        )
        .await;
        if let Ok(mut slot) = self.last_barrier.lock() {
            *slot = Some(outcome);
        }
    }

    /// A barrier that never reached `write_at_barrier`.
    ///
    /// Both early returns above used to leave `last_barrier` untouched,
    /// which meant `rpo::evaluate` went on reporting the PREVIOUS
    /// barrier — so `/status` said `manifestCurrent: true` while the DR
    /// record had silently stopped advancing. `BarrierOutcome::Failed`
    /// existed for exactly this event and had no writer, and
    /// `Counter::ManifestFailures` is bumped inside `write_at_barrier`,
    /// which these paths never reach. The only signal was a `warn!`,
    /// and §0 rule 4 says log lines drop under load.
    ///
    /// This is reachable without any store failure at all: `manifest::walk`
    /// propagates on the recursive `read_dir`, so a client `rmdir`
    /// between a parent's listing and its recursion aborts the whole
    /// tree walk — routine under agent-fleet churn.
    fn record_barrier_failure(&self) {
        crate::tier::meter::bump(crate::tier::meter::Counter::ManifestFailures);
        if let Ok(mut slot) = self.last_barrier.lock() {
            *slot = Some(crate::tier::manifest::BarrierOutcome::Failed);
        }
    }

    /// Flush one file. `row` is its durable dirty entry from this
    /// tick's listing (path + the observed mark sequence).
    pub async fn flush_file(&self, row: &TierDirtyEntry) -> Outcome {
        let (dev, ino) = (row.dev, row.ino);
        // A8: no epoch, no publish. Checked again right before the
        // store mutation — this one exists so a fenced hub does not
        // drain gates and read files for flushes it may not finish.
        let Some(current_epoch) = self.epoch.current() else {
            meter::bump(Counter::FlushesFenced);
            return Outcome::Fenced;
        };
        // C2: a marker-set file is UNCONDITIONALLY excluded from flush
        // — its local bytes are the evicted stub. (The bit should
        // never be set for one; if it somehow is, it stays set and
        // nothing uploads.)
        if crate::tier::evict::is_evicted(dev, ino) {
            warn!(
                "tier flush: ({},{}) has its eviction marker set with a dirty bit — \
                 refusing to upload the stub; the bit stays",
                dev, ino
            );
            return Outcome::SkippedEvicted;
        }
        let Some(path) = row.path.as_ref().map(PathBuf::from) else {
            // No path on the row (fd-only writer so far) — A7's
            // identity rows (step 6) own this; the bit keeps the file
            // whole-dirty until then.
            debug!("tier flush: ({},{}) has no path yet; deferred", dev, ino);
            return Outcome::PathMismatch;
        };
        let Some(key) = self.key_for(&path) else {
            warn!("tier flush: {} outside export root; skipped", path.display());
            return Outcome::PathMismatch;
        };
        let Some(_flight) = gate::try_begin_flush(dev, ino) else {
            return Outcome::SkippedSingleFlight;
        };

        // The path must still name this identity (renames — A7 owns
        // the durable repair; here it only costs a deferral).
        match stat_identity(&path) {
            Some(id) if id == (dev, ino) => {}
            _ => {
                debug!("tier flush: {} no longer names ({},{})", path.display(), dev, ino);
                return Outcome::PathMismatch;
            }
        }

        // Base generation: registry first; discovery by HEAD only for
        // unknown files (request economy).
        let mut base = self.generations.get(&(dev, ino)).map(|e| e.clone());
        let mut discovered_crc: Option<String> = None;
        if base.is_none() {
            match self.store.head(&key).await {
                Ok(meta) => {
                    let generation = GenerationStamps::from_meta(&meta.meta)
                        .map(|s| s.generation)
                        .unwrap_or(1);
                    discovered_crc = meta.crc64_b64.clone();
                    base = Some(GenRecord {
                        key: key.clone(),
                        generation,
                        etag: meta.etag.clone(),
                        crc64_b64: meta.crc64_b64,
                        size: meta.size,
                        // Discovered content is never vouched for as a
                        // copy source: its relationship to local bytes
                        // is unknown (and it may be non-Standard).
                        copy_allowed: false,
                    });
                }
                Err(StoreError::NotFound(_)) => {}
                Err(e) => return self.fail(dev, ino, None, format!("HEAD {}: {}", key, e)),
            }
        }

        // Take the epoch under the gate barrier (A4's atomic swap).
        let epoch = {
            let (d, i) = (dev, ino);
            tokio::task::spawn_blocking(move || gate::drain_and_take_epoch(d, i))
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
        };

        let (size, posix) = match tokio::task::spawn_blocking({
            let p = path.clone();
            move || {
                std::fs::metadata(&p).map(|m| {
                    #[cfg(unix)]
                    let posix = Some(crate::tier::store::PosixStamps::from_metadata(&m));
                    #[cfg(not(unix))]
                    let posix = None;
                    (m.len(), posix)
                })
            }
        })
        .await
        {
            Ok(Ok(s)) => s,
            _ => return self.fail(dev, ino, Some(epoch), format!("stat {}", path.display())),
        };

        // A7 re-key: the generation lives under a key the path no
        // longer derives — the file was renamed since its publish.
        let rekey_from: Option<String> = base
            .as_ref()
            .filter(|b| b.key != key)
            .map(|b| b.key.clone());

        // Restart clean-skip: when the bucket's CRC provably equals
        // local truth, adopt clean and upload nothing. Two sources:
        // HEAD discovery (unknown file), or — for a WHOLE-dirty epoch,
        // the restart shape — the durable row's stored CRC (a full
        // local read is always worth avoiding a full upload). Never
        // while a re-key is pending: clean content still has to MOVE.
        let clean_check_crc = discovered_crc.clone().or_else(|| {
            if epoch.whole {
                base.as_ref().and_then(|b| b.crc64_b64.clone())
            } else {
                None
            }
        });
        if let Some(bucket_crc) = clean_check_crc.as_ref().filter(|_| rekey_from.is_none()) {
            match file_crc(&path).await {
                Ok(local_crc) if &crc64_to_b64(local_crc) == bucket_crc => {
                    meter::bump(Counter::FlushesCleanMatch);
                    if let Some(b) = &base {
                        info!(
                            "tier flush: {} matches bucket gen {} by CRC — adopted clean, \
                             nothing uploaded",
                            key, b.generation
                        );
                    }
                    self.record_generation(dev, ino, base.clone().unwrap()).await;
                    self.try_clear_clean(dev, ino, row.mark_seq).await;
                    self.last_flush.insert((dev, ino), Instant::now());
                    return Outcome::CleanMatch;
                }
                Ok(_) => {}
                Err(e) => return self.fail(dev, ino, Some(epoch), e),
            }
        }

        if !epoch.is_dirty() && base.is_some() && rekey_from.is_none() {
            // Bit set but nothing captured and no re-key pending
            // (leftover row): try to release it; the conditional clear
            // keeps it on any race.
            self.try_clear_clean(dev, ino, row.mark_seq).await;
            return Outcome::NothingToFlush;
        }

        let plan = match plan_parts(
            size,
            &epoch,
            base.as_ref().map(|b| PlanBase { size: b.size, copy_allowed: b.copy_allowed }),
            &self.cfg,
            self.store.max_parts(),
        ) {
            Ok(p) => p,
            Err(e) => return self.fail(dev, ino, Some(epoch), e.to_string()),
        };

        let to_gen = base.as_ref().map_or(1, |b| b.generation + 1);
        let flush_uuid = uuid::Uuid::new_v4().to_string();
        // Re-key publishes CREATE at the new key (the covered object,
        // if any, was tombstone-consumed at the top of this tick);
        // clean ranges still copy from the OLD key, guarded on the
        // base's etag. In-place publishes guard If-Match as before.
        let condition = match (&base, &rekey_from) {
            (Some(b), None) => PutCondition::IfMatch(b.etag.clone()),
            _ => PutCondition::IfNoneMatchAny,
        };
        let stamps = GenerationStamps {
            generation: to_gen,
            epoch: current_epoch,
            flush_uuid: flush_uuid.clone(),
            boundary_source: None,
            // A12: mode/uid/gid/mtime ride on the object — a bucket
            // reader (and the DR import) can restore metadata without
            // the manifest.
            posix,
        };

        // Durable intent BEFORE any store mutation (A6); for a re-key,
        // ALSO the old key's tombstone — a crash after the new-key
        // publish must still delete the old object (never an orphan
        // the next import would resurrect).
        let intent = FlushIntentRecord {
            flush_uuid: flush_uuid.clone(),
            path: path.to_string_lossy().into_owned(),
            from_gen: base.as_ref().map(|b| b.generation),
            to_gen,
            mpu_id: None,
            base_etag: match (&base, &rekey_from) {
                (Some(b), None) => Some(b.etag.clone()),
                _ => None,
            },
            created_unix: now_unix(),
        };
        if let Err(e) = self.backend.put_flush_intent(&intent).await {
            return self.fail(dev, ino, Some(epoch), format!("intent: {}", e));
        }
        if let Some(old_key) = &rekey_from {
            let t = crate::state_backend::TierTombstone {
                key: old_key.clone(),
                etag: base.as_ref().map(|b| b.etag.clone()),
                created_unix: now_unix(),
            };
            if let Err(e) = self.backend.tier_put_tombstone(&t).await {
                let _ = self.backend.delete_flush_intent(&flush_uuid).await;
                return self.fail(dev, ino, Some(epoch), format!("re-key tombstone: {}", e));
            }
        }

        // A8: re-verify the epoch immediately before the publish. The
        // heartbeat fences the guard the moment a renewal fails; past
        // this check the residual window is one heartbeat interval,
        // inside which the A6 If-Match guard is the second fence (and
        // for composes, the successor's claim-time abort-sweep makes
        // our Complete fail NoSuchUpload regardless).
        if self.epoch.current() != Some(current_epoch) {
            let _ = self.backend.delete_flush_intent(&flush_uuid).await;
            capture::merge_back(dev, ino, epoch);
            meter::bump(Counter::FlushesFenced);
            warn!(
                "tier flush: ({},{}) fenced between plan and publish — nothing sent",
                dev, ino
            );
            return Outcome::Fenced;
        }

        let publish = match &plan {
            Plan::WholePut => {
                // Body read ONCE; the CRC derives from the same bytes,
                // so the server-side validation can only trip on a
                // wire fault, never on a benign local race.
                match read_whole(&path).await {
                    Ok(body) => {
                        let crc = crate::tier::store::crc64_nvme(&body);
                        let n = body.len() as u64;
                        let r = self
                            .store
                            .put_whole(&key, body, &condition, &stamps, crc)
                            .await;
                        if r.is_ok() {
                            meter::add(Counter::BytesUploaded, n);
                        }
                        r
                    }
                    Err(e) => Err(StoreError::Other(e)),
                }
            }
            Plan::Compose { parts, .. } => {
                // CRC from a streaming pass over local truth; a
                // mutation racing the part reads fails the publish
                // server-side (BadDigest) instead of landing torn.
                match file_crc(&path).await {
                    Ok(crc) => {
                        let spec = ComposeSpec {
                            key: &key,
                            local_path: &path,
                            parts: parts.clone(),
                            base_key: rekey_from.as_deref(),
                            base_etag: base.as_ref().map(|b| b.etag.clone()),
                            condition: condition.clone(),
                            stamps: stamps.clone(),
                            crc64: crc,
                        };
                        let r = self.store.compose_generation(&spec).await;
                        if r.is_ok() {
                            let (mut up, mut cp, mut upn, mut cpn) = (0u64, 0u64, 0u64, 0u64);
                            for p in parts {
                                match p {
                                    PartSource::Local { len, .. } => {
                                        up += len;
                                        upn += 1;
                                    }
                                    PartSource::BaseCopy { len, .. } => {
                                        cp += len;
                                        cpn += 1;
                                    }
                                }
                            }
                            meter::add(Counter::BytesUploaded, up);
                            meter::add(Counter::BytesCopied, cp);
                            meter::add(Counter::PartsUploaded, upn);
                            meter::add(Counter::PartsCopied, cpn);
                        }
                        r
                    }
                    Err(e) => Err(StoreError::Other(e)),
                }
            }
        };

        match publish {
            Ok(meta) => {
                meter::bump(Counter::Publishes);
                self.record_generation(
                    dev,
                    ino,
                    GenRecord {
                        key: key.clone(),
                        generation: to_gen,
                        etag: meta.etag.clone(),
                        crc64_b64: meta.crc64_b64.clone(),
                        size,
                        copy_allowed: true,
                    },
                )
                .await;
                self.finish_rekey(&rekey_from).await;
                let _ = self.backend.delete_flush_intent(&flush_uuid).await;
                self.last_flush.insert((dev, ino), Instant::now());
                self.try_clear_clean(dev, ino, row.mark_seq).await;
                debug!("tier flush: published {} gen {}", key, to_gen);
                Outcome::Published { to_gen }
            }
            // NotFound routes to arbitration too: a guarded publish
            // over a base the bucket no longer has answers 404, not
            // 412 (a foreign DELETE under a published file — the
            // chaos drill's C3 leg wedged here forever: every tick
            // retried If-Match against a missing key and failed).
            // Arbitration's HEAD then sees NotFound + a recorded base
            // ⇒ Foreign(None) ⇒ the row drops and the next cycle
            // re-CREATES local truth.
            Err(
                e @ (StoreError::PreconditionFailed(_)
                | StoreError::Conflict(_)
                | StoreError::NotFound(_)),
            ) => {
                if !matches!(e, StoreError::NotFound(_)) {
                    meter::bump(Counter::Publish412s);
                }
                let probe = IntentProbe {
                    key: &key,
                    to_gen,
                    flush_uuid: &flush_uuid,
                    base_etag: base.as_ref().map(|b| b.etag.as_str()),
                };
                match arbitrate(self.store.as_ref(), &probe).await {
                    Ok(Verdict::AdoptOwn(meta)) => {
                        meter::bump(Counter::ArbitrateAdoptOwn);
                        // Our own Complete landed; the epoch's bytes
                        // are in the object — consume it like success.
                        self.record_generation(
                            dev,
                            ino,
                            GenRecord {
                                key: key.clone(),
                                generation: to_gen,
                                etag: meta.etag.clone(),
                                crc64_b64: meta.crc64_b64.clone(),
                                size,
                                copy_allowed: true,
                            },
                        )
                        .await;
                        self.finish_rekey(&rekey_from).await;
                        let _ = self.backend.delete_flush_intent(&flush_uuid).await;
                        self.last_flush.insert((dev, ino), Instant::now());
                        self.try_clear_clean(dev, ino, row.mark_seq).await;
                        Outcome::Adopted { to_gen }
                    }
                    Ok(Verdict::RetryFromBase) => {
                        meter::bump(Counter::ArbitrateRetryFromBase);
                        capture::merge_back(dev, ino, epoch);
                        let _ = self.backend.delete_flush_intent(&flush_uuid).await;
                        Outcome::RetryNextCycle
                    }
                    Ok(Verdict::Foreign(meta)) => {
                        meter::bump(Counter::ArbitrateForeign);
                        capture::merge_back(dev, ino, epoch);
                        capture::note(dev, ino, capture::Mutation::Whole);
                        match self.successor_check(meta.as_ref()).await {
                            SuccessorCheck::Successor => {
                                // Deposed: rows and intent stay as the
                                // durable record; nothing re-publishes
                                // under a successor's reign.
                                return self.fail_keep_intent(
                                    dev,
                                    ino,
                                    "deposed: a successor's epoch stamp observed"
                                        .into(),
                                );
                            }
                            SuccessorCheck::Unverified => {
                                return self.fail_keep_intent(
                                    dev,
                                    ino,
                                    "successor check unverified; retrying".into(),
                                );
                            }
                            SuccessorCheck::ForeignHand => {}
                        }
                        match meta {
                            Some(m) => {
                                let generation = GenerationStamps::from_meta(&m.meta)
                                    .map(|s| s.generation)
                                    .unwrap_or(to_gen);
                                warn!(
                                    "tier flush: {} was overwritten in the bucket (etag {}); \
                                     local truth re-publishes over it next cycle guarded \
                                     If-Match (A6 publish path is LOCAL-WINS; deliberate \
                                     outside writes belong to import-refresh, step 12)",
                                    key, m.etag
                                );
                                self.record_generation(
                                    dev,
                                    ino,
                                    GenRecord {
                                        key: key.clone(),
                                        generation,
                                        etag: m.etag.clone(),
                                        crc64_b64: m.crc64_b64.clone(),
                                        size: m.size,
                                        // Foreign bytes are never a
                                        // copy source for local truth.
                                        copy_allowed: false,
                                    },
                                )
                                .await;
                            }
                            None => {
                                warn!(
                                    "tier flush: {} was DELETED in the bucket; local truth \
                                     re-creates it next cycle",
                                    key
                                );
                                self.drop_generation(dev, ino).await;
                            }
                        }
                        let _ = self.backend.delete_flush_intent(&flush_uuid).await;
                        Outcome::ForeignDetected
                    }
                    Err(e) => {
                        capture::merge_back(dev, ino, epoch);
                        // Intent stays: the startup reconciler owns it
                        // if we never get another cycle.
                        self.fail_keep_intent(dev, ino, format!("arbitrate {}: {}", key, e))
                    }
                }
            }
            Err(e) => {
                capture::merge_back(dev, ino, epoch);
                let _ = self.backend.delete_flush_intent(&flush_uuid).await;
                meter::bump(Counter::PublishFailures);
                warn!("tier flush: publish {} failed: {}", key, e);
                Outcome::Failed(e.to_string())
            }
        }
    }

    /// A successful re-key publish's tail: delete the old key's object
    /// and close its tombstone. Failures keep the tombstone — the next
    /// tick's sweep finishes the job (same posture as a crash here).
    async fn finish_rekey(&self, rekey_from: &Option<String>) {
        let Some(old_key) = rekey_from else { return };
        match self.store.delete(old_key).await {
            Ok(()) => {
                let _ = self.backend.tier_delete_tombstone(old_key).await;
                debug!("tier flush: re-key complete, old object {} deleted", old_key);
            }
            Err(e) => {
                warn!(
                    "tier flush: old-key delete {} failed ({}); its tombstone stays for \
                     the next sweep",
                    old_key, e
                );
            }
        }
    }

    /// The A3-safe release of the durable bit. Memo first (new
    /// mutations queue marks again), then the in-memory checks, then
    /// the observed-sequence conditional delete — any interleaved
    /// mutation either re-arms the memo path or bumps the row's
    /// sequence, and the bit survives.
    async fn try_clear_clean(&self, dev: u64, ino: u64, observed_seq: u64) -> bool {
        capture::clear_durable(dev, ino);
        let dirty = capture::snapshot(dev, ino).is_some_and(|c| c.is_dirty());
        if dirty || capture::is_queued(dev, ino) {
            capture::prime_durable(dev, ino); // bit stays set — correctly
            return false;
        }
        match self.backend.tier_clear_dirty_if_seq(dev, ino, observed_seq).await {
            Ok(true) => {
                capture::clear_quiet(dev, ino);
                gate::purge(dev, ino);
                true
            }
            Ok(false) => false, // a newer mark landed; bit survives
            Err(e) => {
                warn!("tier flush: conditional clear ({},{}): {}", dev, ino, e);
                capture::prime_durable(dev, ino);
                false
            }
        }
    }

    fn fail(&self, dev: u64, ino: u64, epoch: Option<FileCapture>, msg: String) -> Outcome {
        if let Some(e) = epoch {
            capture::merge_back(dev, ino, e);
        }
        meter::bump(Counter::PublishFailures);
        warn!("tier flush: ({},{}) {}", dev, ino, msg);
        Outcome::Failed(msg)
    }

    fn fail_keep_intent(&self, dev: u64, ino: u64, msg: String) -> Outcome {
        meter::bump(Counter::PublishFailures);
        warn!("tier flush: ({},{}) {}", dev, ino, msg);
        Outcome::Failed(msg)
    }

    /// A Foreign verdict whose stamps carry an epoch ABOVE ours is
    /// machine-readable evidence of a SUCCESSOR, not a foreign hand
    /// (FlintTierEpoch's ProbeOverwrite counterexample: a deposed-but-
    /// unfenced zombie's local-wins re-publish would land OVER the live
    /// successor's object before the first heartbeat 412 fences it).
    /// Verified against the store's epoch object before acting — a
    /// forged stamp from an outside writer must not be able to fence a
    /// healthy hub into a crash loop.
    async fn successor_check(&self, meta: Option<&ObjectMeta>) -> SuccessorCheck {
        let Some(m) = meta else { return SuccessorCheck::ForeignHand };
        let Some(stamps) = GenerationStamps::from_meta(&m.meta) else {
            return SuccessorCheck::ForeignHand;
        };
        let Some(ours) = self.epoch.current() else {
            // Already fenced: the caller must not publish anyway.
            return SuccessorCheck::Successor;
        };
        if stamps.epoch <= ours {
            return SuccessorCheck::ForeignHand;
        }
        let ekey = crate::tier::epoch::epoch_key(&self.cfg.key_prefix);
        match self.store.epoch_read(&ekey).await {
            Ok(Some(state)) if state.epoch > ours => {
                error!(
                    "tier flush: object stamped epoch {} > ours {}, and the store's \
                     epoch object confirms it (epoch {} held by {}) — a SUCCESSOR \
                     owns this prefix; fencing all publishes. The durable backlog \
                     stays local; the heartbeat exits on its next renew",
                    stamps.epoch, ours, state.epoch, state.holder_id
                );
                self.epoch.fence();
                meter::bump(Counter::FlushesFenced);
                SuccessorCheck::Successor
            }
            Ok(_) => {
                warn!(
                    "tier flush: object stamped epoch {} > ours {} but the store \
                     still shows our reign — a fabricated stamp from an outside \
                     writer; A6 local-wins proceeds",
                    stamps.epoch, ours
                );
                SuccessorCheck::ForeignHand
            }
            Err(e) => {
                // Cannot verify: neither fence on no evidence nor
                // overwrite what may be a successor's object — retry
                // the whole flush next cycle.
                warn!("tier flush: successor-check epoch read failed: {}", e);
                SuccessorCheck::Unverified
            }
        }
    }
}

/// Outcome of [`FlushOrchestrator::successor_check`].
enum SuccessorCheck {
    /// Store-confirmed successor reign: the guard is now fenced; do
    /// NOT touch rows or re-publish.
    Successor,
    /// A genuine foreign hand (or no stamp evidence): A6 local-wins.
    ForeignHand,
    /// The verifying read failed; retry next cycle.
    Unverified,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn stat_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Streaming CRC-64/NVME over the whole file (blocking pool). Shared
/// with eviction (step 10), whose precondition set re-verifies local
/// bytes against the published generation before destroying them.
pub(crate) async fn file_crc(path: &Path) -> Result<u64, String> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        use std::io::Read;
        let mut f = std::fs::File::open(&p)?;
        let mut crc = Crc64Nvme::new();
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
        }
        Ok(crc.finalize())
    })
    .await
    .map_err(|e| format!("crc join: {}", e))?
    .map_err(|e| format!("crc read: {}", e))
}

async fn read_whole(path: &Path) -> Result<Bytes, String> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::read(&p).map(Bytes::from))
        .await
        .map_err(|e| format!("read join: {}", e))?
        .map_err(|e| format!("read: {}", e))
}

/// Every inode of a regular file living under `root`, for the
/// generation-row re-homing prune.
#[cfg(unix)]
/// Like [`live_inodes`], but keeps the path each inode was found at so a
/// re-homed row can have its `path` repaired at the same time. A rename
/// during a drifted boot changes the path and keeps the inode, so the
/// stored path is exactly the field that cannot be trusted here.
///
/// Last writer wins on a hard-linked inode: any live name reaches the
/// same bytes, which is all the reconciler needs.
fn live_inode_paths(
    root: &std::path::Path,
) -> std::io::Result<std::collections::HashMap<u64, std::path::PathBuf>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = std::collections::HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let md = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                stack.push(ent.path());
            } else if md.is_file() {
                out.insert(md.ino(), ent.path());
            }
        }
    }
    Ok(out)
}

fn live_inodes(root: &std::path::Path) -> std::io::Result<std::collections::HashSet<u64>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = std::collections::HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let md = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                stack.push(ent.path());
            } else if md.is_file() {
                out.insert(md.ino());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {

    /// Every early return out of the barrier must record the failure.
    ///
    /// Both of them used to just `return`, leaving `last_barrier`
    /// holding the PREVIOUS outcome — so `rpo::evaluate` went on
    /// reporting `manifestCurrent: true` while the DR record had
    /// silently stopped advancing. `BarrierOutcome::Failed` existed for
    /// this and had no writer at all, and `ManifestFailures` is bumped
    /// inside `write_at_barrier`, which neither path reaches.
    ///
    /// Driving a walk failure end-to-end means racing an `rmdir` against
    /// a recursive `read_dir`, so this is a call-site lint instead —
    /// same shape as `every_namespace_mutation_commits_before_it_is_acked`.
    /// It asserts its own anchors resolved, so it cannot pass by not
    /// looking.
    #[test]
    fn every_skipped_barrier_records_the_failure() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tier/flush.rs"),
        )
        .expect("flush.rs must be readable");
        let prod = match src.rfind("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => &src[..],
        };

        let needle = "barrier skipped";
        let sites: Vec<usize> = prod
            .match_indices(needle)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            sites.len(), 2,
            "expected the two barrier-skip returns; the anchors are stale, \
             so this lint would have passed by not looking"
        );
        for at in sites {
            let after: String = prod[at..].lines().take(4).collect::<Vec<_>>().join("\n");
            assert!(
                after.contains("record_barrier_failure"),
                "a barrier skip that does not call `record_barrier_failure` leaves \
                 /status reporting the previous manifest as current:\n{after}"
            );
        }
    }
    use super::*;
    use crate::state_backend::memory::MemoryBackend;
    use crate::tier::capture::Mutation;
    use crate::tier::store::memory::MemoryStore;

    // ── planner units (pure) ─────────────────────────────────────────

    fn cfg_for_plan(whole_put_max: u64, part_floor: u64) -> FlushConfig {
        let mut c = FlushConfig::new(PathBuf::from("/x"), "t/".into());
        c.whole_put_max = whole_put_max;
        c.part_floor = part_floor;
        c
    }

    fn cap_of(notes: &[Mutation]) -> FileCapture {
        let mut c = FileCapture::default();
        for n in notes {
            // FileCapture::note is private; replicate through the map
            // API instead — but per-test keys would leak. Use a local
            // reconstruction: Write/Zero extend intervals via the
            // public capture surface is overkill here; build directly.
            match *n {
                Mutation::Write { offset, len } | Mutation::Zero { offset, len } => {
                    if len > 0 {
                        c.intervals.push((offset, offset + len));
                        c.intervals.sort_unstable();
                    }
                }
                Mutation::Truncate { new_size } => {
                    c.min_size = Some(c.min_size.map_or(new_size, |s| s.min(new_size)));
                    c.intervals.retain_mut(|(s, e)| {
                        if *s >= new_size {
                            return false;
                        }
                        if *e > new_size {
                            *e = new_size;
                        }
                        true
                    });
                }
                Mutation::Whole => c.whole = true,
            }
        }
        c
    }

    #[test]
    fn planner_small_and_zero_files_whole_put() {
        let cfg = cfg_for_plan(1024, 256);
        let cap = cap_of(&[Mutation::Write { offset: 0, len: 10 }]);
        assert_eq!(plan_parts(10, &cap, None, &cfg, 10_000), Ok(Plan::WholePut));
        assert_eq!(plan_parts(0, &cap, None, &cfg, 10_000), Ok(Plan::WholePut));
        assert_eq!(plan_parts(1024, &cap, None, &cfg, 10_000), Ok(Plan::WholePut));
    }

    #[test]
    fn planner_refuses_the_5tib_ceiling() {
        let cfg = cfg_for_plan(1024, 256);
        let cap = FileCapture::default();
        assert!(matches!(
            plan_parts(MAX_OBJECT_SIZE + 1, &cap, None, &cfg, 10_000),
            Err(PlanError::TooLarge { .. })
        ));
    }

    #[test]
    fn planner_grid_doubles_to_respect_max_parts() {
        let cfg = cfg_for_plan(16, 16);
        let cap = cap_of(&[Mutation::Whole]);
        // 100 bytes, max 4 parts: need=25 → grid doubles 16→32.
        match plan_parts(100, &cap, None, &cfg, 4).unwrap() {
            Plan::Compose { part_size, parts } => {
                assert_eq!(part_size, 32);
                assert_eq!(parts.len(), 4);
                assert_eq!(parts[3], PartSource::Local { offset: 96, len: 4 });
            }
            p => panic!("expected compose, got {:?}", p),
        }
    }

    #[test]
    fn planner_coalesces_clean_runs_and_isolates_dirty_slots() {
        let cfg = cfg_for_plan(64, 256);
        // 8 slots of 256; dirty only inside slot 2.
        let cap = cap_of(&[Mutation::Write { offset: 600, len: 10 }]);
        let base = Some(PlanBase { size: 2048, copy_allowed: true });
        match plan_parts(2048, &cap, base, &cfg, 10_000).unwrap() {
            Plan::Compose { parts, .. } => {
                assert_eq!(
                    parts,
                    vec![
                        PartSource::BaseCopy { offset: 0, len: 512 },
                        PartSource::Local { offset: 512, len: 256 },
                        PartSource::BaseCopy { offset: 768, len: 1280 },
                    ],
                    "adjacent clean slots must coalesce into single copy parts \
                     (A11's fan-out collapse)"
                );
            }
            p => panic!("expected compose, got {:?}", p),
        }
    }

    #[test]
    fn planner_caps_copy_runs_at_5gib() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let cfg = cfg_for_plan(64, GIB);
        let cap = FileCapture::default(); // fully clean
        let base = Some(PlanBase { size: 12 * GIB, copy_allowed: true });
        match plan_parts(12 * GIB, &cap, base, &cfg, 10_000).unwrap() {
            Plan::Compose { parts, .. } => {
                assert_eq!(
                    parts,
                    vec![
                        PartSource::BaseCopy { offset: 0, len: 5 * GIB },
                        PartSource::BaseCopy { offset: 5 * GIB, len: 5 * GIB },
                        PartSource::BaseCopy { offset: 10 * GIB, len: 2 * GIB },
                    ],
                    "copy runs must split at the 5 GiB UploadPartCopy ceiling"
                );
            }
            p => panic!("expected compose, got {:?}", p),
        }
    }

    #[test]
    fn planner_watermark_and_base_size_and_whole_force_local() {
        let cfg = cfg_for_plan(64, 256);
        // Watermark at 700: slots ≥ that may not copy even if clean.
        let cap = cap_of(&[Mutation::Truncate { new_size: 700 }]);
        let base = Some(PlanBase { size: 2048, copy_allowed: true });
        match plan_parts(2048, &cap, base, &cfg, 10_000).unwrap() {
            Plan::Compose { parts, .. } => {
                assert_eq!(parts[0], PartSource::BaseCopy { offset: 0, len: 512 });
                for p in &parts[1..] {
                    assert!(
                        matches!(p, PartSource::Local { .. }),
                        "slots crossing/beyond the truncate watermark must never \
                         clean-copy (shrink-regrow resurrection): {:?}",
                        p
                    );
                }
            }
            p => panic!("expected compose, got {:?}", p),
        }
        // whole ⇒ everything local.
        let cap = cap_of(&[Mutation::Whole]);
        match plan_parts(1024, &cap, base, &cfg, 10_000).unwrap() {
            Plan::Compose { parts, .. } => {
                assert!(parts.iter().all(|p| matches!(p, PartSource::Local { .. })));
            }
            p => panic!("{:?}", p),
        }
        // IA guard / unvouched base ⇒ everything local.
        let base_ia = Some(PlanBase { size: 2048, copy_allowed: false });
        match plan_parts(1024, &FileCapture::default(), base_ia, &cfg, 10_000).unwrap() {
            Plan::Compose { parts, .. } => {
                assert!(
                    parts.iter().all(|p| matches!(p, PartSource::Local { .. })),
                    "a non-copyable base (IA class / foreign) must never be a copy source"
                );
            }
            p => panic!("{:?}", p),
        }
        // Base shorter than the file ⇒ the tail is local (per-slot:
        // Local slots deliberately do NOT merge — each stays one
        // upload part on the grid).
        let base_short = Some(PlanBase { size: 512, copy_allowed: true });
        match plan_parts(1024, &FileCapture::default(), base_short, &cfg, 10_000).unwrap() {
            Plan::Compose { parts, .. } => {
                assert_eq!(parts[0], PartSource::BaseCopy { offset: 0, len: 512 });
                assert_eq!(parts[1], PartSource::Local { offset: 512, len: 256 });
                assert_eq!(parts[2], PartSource::Local { offset: 768, len: 256 });
            }
            p => panic!("{:?}", p),
        }
    }

    // ── e2e against the memory store ─────────────────────────────────

    struct Rig {
        _dir: tempfile::TempDir,
        root: PathBuf,
        mem: Arc<MemoryStore>,
        backend: Arc<dyn StateBackend>,
        guard: Arc<crate::tier::epoch::EpochGuard>,
        orch: FlushOrchestrator,
        /// Serialises against every other tier rig: the capture pending
        /// queue is process-global and a drain takes all of it.
        ///
        /// GENUINELY last: fields drop in declaration order, so anything
        /// declared after this would tear down with the lock already
        /// released — which is the window this guard exists to close.
        _excl: std::sync::MutexGuard<'static, ()>,
    }

    fn rig(whole_put_max: u64, part_floor: u64) -> Rig {
        let _excl = capture::test_exclusive();
        capture::force_enable();
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let mem = Arc::new(MemoryStore::new());
        let backend: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let mut cfg = FlushConfig::new(root.clone(), "t/".into());
        cfg.floor = Duration::ZERO;
        cfg.quiesce = Duration::ZERO;
        cfg.whole_put_max = whole_put_max;
        cfg.part_floor = part_floor;
        let store_dyn: Arc<dyn ObjectStore> = mem.clone();
        let guard = crate::tier::epoch::EpochGuard::held(1);
        let orch = FlushOrchestrator::new(store_dyn, backend.clone(), cfg, guard.clone());
        Rig { _dir: dir, root, mem, backend, guard, orch, _excl }
    }

    fn ident(path: &Path) -> (u64, u64) {
        stat_identity(path).unwrap()
    }

    /// Note + land the durable row, repairing parallel-test theft
    /// (process-global capture queue — the step-2 lesson): re-note the
    /// SAME mutation until the row exists in OUR backend.  No flush
    /// test evicts, so ANY eviction marker on this identity is another
    /// test's residue via ext4 inode reuse (the Linux-census lesson —
    /// a stale marker fails the flush as SkippedEvicted): drop it.
    async fn note_and_land(rigg: &Rig, path: &Path, m: Mutation) {
        let (dev, ino) = ident(path);
        crate::tier::evict::forget(dev, ino);
        capture::note_path(path, m);
        for _ in 0..50 {
            let _ = crate::tier::durable::drain_pending(&rigg.backend).await;
            let landed = rigg
                .backend
                .tier_list_dirty()
                .await
                .unwrap()
                .iter()
                .any(|r| r.dev == dev && r.ino == ino && r.path.is_some());
            if landed {
                return;
            }
            capture::clear_durable(dev, ino);
            capture::note_path(path, m);
        }
        panic!("dirty row never landed (theft-repair exhausted)");
    }

    async fn our_row(rigg: &Rig, dev: u64, ino: u64) -> Option<TierDirtyEntry> {
        rigg.backend
            .tier_list_dirty()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.dev == dev && r.ino == ino)
    }

    /// A CSI restage can hand the volume back on a different device
    /// minor. Generation rows are keyed `(dev, ino)`, so when that
    /// happens every row still loads and none of them match — and the
    /// next manifest barrier publishes a manifest naming NO files.
    /// Observed on a real cluster: 33 rows loaded, 33 counted beyond
    /// RPO, manifest cut from 7919 bytes to 534.
    /// A hub that could not read the bucket's manifest at start serves
    /// an EMPTY export. `server.rs` logs "do NOT let it publish over
    /// the bucket" — and, until this fence, nothing enforced it:
    /// `set_import_refused` wrote a status string whose only readers
    /// are two status surfaces, the flush loop spawned unconditionally,
    /// and every tick ended in `write_manifest_barrier`.
    ///
    /// One barrier from an empty tree replaces a real manifest with one
    /// naming no files. Directories, symlinks and every mode/uid/gid
    /// exist ONLY in the manifest, so they are gone; `rpo::evaluate`
    /// then reports clean and the idle ladder reclaims the disk that
    /// held the last copy.
    #[tokio::test]
    async fn a_refused_import_may_never_publish_a_barrier() {
        let r = rig(1 << 20, 1 << 20);

        // The bucket already has a real manifest. Whatever this hub
        // does next, it must not be replaced from an empty export.
        let key = "t/.flint/manifest";
        r.mem.raw_put(key, Bytes::from_static(b"REAL MANIFEST"), vec![]);

        // The import refused, so the hub is serving an empty tree.
        r.orch.fence_publishing("manifest unreadable (test)");
        assert!(r.orch.is_publish_fenced());

        // A full tick, and the barrier called directly — it is `pub`,
        // so the guard has to hold on that path too.
        let rep = r.orch.tick().await;
        r.orch.write_manifest_barrier().await;

        let (_, after) = r
            .mem
            .get_whole(key, None)
            .await
            .expect("the manifest must still exist");
        assert_eq!(
            &after[..],
            b"REAL MANIFEST",
            "a fenced hub overwrote the bucket's manifest from an empty export — \
             this is the path that erases every directory, symlink and mode"
        );
        assert_eq!(rep.published, 0, "a fenced hub published a file");
        assert_eq!(rep.examined, 0, "a fenced tick must not even walk the dirty set");
    }

    #[tokio::test]
    async fn a_drifted_export_device_re_homes_its_generation_rows() {
        let r = rig(1024, 256);
        let f = r.root.join("kept.bin");
        std::fs::write(&f, b"payload").unwrap();
        let (live_dev, ino) = ident(&f);

        // The row as a PREVIOUS boot wrote it: right inode, stale device.
        let stale = crate::state_backend::TierGenerationRow {
            dev: live_dev ^ 0x1,
            ino,
            key: "t/kept.bin".into(),
            generation: 7,
            etag: "\"deadbeef\"".into(),
            crc64_b64: None,
            size: 7,
            copy_allowed: true,
            updated_unix: 1000,
        };
        r.backend.tier_upsert_generation(&stale).await.unwrap();

        // A row for an inode that no longer exists must NOT be re-homed:
        // its inode can be reused and it would then claim another
        // file's object.
        let orphan = crate::state_backend::TierGenerationRow {
            ino: ino.wrapping_add(1_000_000),
            key: "t/vanished.bin".into(),
            ..stale.clone()
        };
        r.backend.tier_upsert_generation(&orphan).await.unwrap();

        r.orch.startup().await;

        let healed = r.orch.generation_of(live_dev, ino).expect("row must re-home to the live dev");
        assert_eq!(healed.key, "t/kept.bin");
        assert_eq!(healed.generation, 7, "the re-homed row keeps its generation");
        assert!(
            r.orch.generation_of(stale.dev, ino).is_none(),
            "the stale-dev row must be gone, not duplicated"
        );
        assert!(
            r.orch.generation_of(live_dev, orphan.ino).is_none(),
            "a row whose inode is not live must be dropped, never re-homed"
        );

        // The point of all of it: the manifest names the file again.
        let gens: std::collections::HashMap<(u64, u64), GenRecord> =
            r.orch.generations.iter().map(|e| (*e.key(), e.value().clone())).collect();
        let built = crate::tier::manifest::build(&r.root, &gens).unwrap();
        assert_eq!(built.beyond_rpo, 0, "nothing may be counted beyond RPO after healing");
        assert!(
            built.entries.iter().any(|e| e.key.as_deref() == Some("t/kept.bin")),
            "the manifest must name the file again"
        );
    }

    /// Blocker 4: the destructive half of the drift.
    ///
    /// `heal_generation_device` re-homed only `tier_generation`. The
    /// `tier_evicted` rows were left stranded under the dead device
    /// number, and that is the table that decides whether a file reads
    /// as its contents or as zero bytes. Stranded:
    ///
    ///   * the stub is no longer recognised as a stub,
    ///   * READ returns `(empty, eof)` with NFS4_OK and GETATTR says 0,
    ///   * and because the generation half WAS healed, the flusher then
    ///     republishes that emptiness over the intact bucket object.
    ///
    /// The partial fix is what makes it permanent. Unhealed, the flusher
    /// would not have recognised the file at all and the S3 object would
    /// have survived.
    #[tokio::test]
    async fn a_drifted_export_device_re_homes_its_evicted_and_dirty_rows() {
        let r = rig(1024, 256);

        // An evicted file's local presence IS a stub: zero bytes on
        // disk, with the real size living in the row.
        let stub = r.root.join("evicted.bin");
        std::fs::write(&stub, b"").unwrap();
        let (live_dev, ino) = ident(&stub);

        let dirty_file = r.root.join("dirty.bin");
        std::fs::write(&dirty_file, b"unflushed").unwrap();
        let (_, dino) = ident(&dirty_file);

        let stale_dev = live_dev ^ 0x1;

        let ev = crate::state_backend::TierEvictedRow {
            dev: stale_dev,
            ino,
            key: "t/evicted.bin".into(),
            generation: 4,
            etag: "\"cafe\"".into(),
            crc64_b64: "AAAAAAAAAAA=".into(),
            size: 4096,
            // Deliberately WRONG: a rename during the drifted boot moved
            // the stub, so the stored path is the one field that cannot
            // be trusted. Re-homing by path would strand this row;
            // re-homing by inode repairs it.
            path: "/gone/old/path/evicted.bin".into(),
            evicted_unix: 900,
            hydrating_unix: None,
        };
        r.backend.tier_put_evicted(&ev).await.unwrap();

        // An evicted row whose inode is NOT live must be dropped, not
        // re-homed: the inode can be reused, and the row would then hand
        // an unrelated file this one's bucket key.
        let ev_orphan = crate::state_backend::TierEvictedRow {
            ino: ino.wrapping_add(1_000_000),
            key: "t/vanished.bin".into(),
            ..ev.clone()
        };
        r.backend.tier_put_evicted(&ev_orphan).await.unwrap();

        r.backend
            .tier_mark_dirty(&[crate::state_backend::TierDirtyEntry {
                dev: stale_dev,
                ino: dino,
                path: None,
                dirtied_unix: 900,
                mark_seq: 5,
            }])
            .await
            .unwrap();

        // ── ANTI-VACUITY ────────────────────────────────────────────
        // Establish that the hazard is REAL in this rig before the fix
        // runs. Without this the assertions below would pass just as
        // happily against a build where the drift never happened, and
        // the test would be proving nothing.
        let before = r.backend.tier_list_evicted().await.unwrap();
        assert!(
            !before.is_empty() && before.iter().all(|row| row.dev != live_dev),
            "setup is wrong: no evicted row may be reachable under the live device \
             yet — that unreachability IS the bug, and if it is absent here the \
             post-conditions below prove nothing"
        );

        r.orch.startup().await;

        // ── evicted ─────────────────────────────────────────────────
        let after = r.backend.tier_list_evicted().await.unwrap();
        let healed = after
            .iter()
            .find(|row| row.dev == live_dev && row.ino == ino)
            .expect("the evicted row must re-home to the live device, or the stub reads as zero bytes");
        assert_eq!(
            healed.size, 4096,
            "the re-homed row must keep the file's REAL size — this is the number GETATTR serves \
             while the file is evicted, and 0 here is the data-loss report"
        );
        assert_eq!(healed.key, "t/evicted.bin", "the bucket key must survive re-homing");
        assert_eq!(healed.generation, 4);
        assert_eq!(
            healed.path,
            stub.to_string_lossy(),
            "the path must be REPAIRED from the walk, not carried over — the stored path \
             was stale and would send the reconciler at nothing"
        );
        assert!(
            !after.iter().any(|row| row.dev == stale_dev && row.ino == ino),
            "the stale-dev row must be gone, not duplicated"
        );
        assert!(
            !after.iter().any(|row| row.ino == ev_orphan.ino),
            "an evicted row whose inode is not live must be dropped — kept, it would claim \
             another file's object after an inode reuse"
        );

        // ── dirty ───────────────────────────────────────────────────
        let d_after = r.backend.tier_list_dirty().await.unwrap();
        let dh = d_after
            .iter()
            .find(|row| row.dev == live_dev && row.ino == dino)
            .expect("the dirty row must re-home, or an unflushed file becomes eviction-eligible");
        assert_eq!(dh.mark_seq, 5, "the re-homed dirty row keeps its mark sequence");
        assert_eq!(
            dh.path.as_deref(),
            Some(dirty_file.to_string_lossy().as_ref()),
            "the dirty row's path is filled in from the walk so the flusher can reach it"
        );
        assert!(
            !d_after.iter().any(|row| row.dev == stale_dev && row.ino == dino),
            "the stale-dev dirty row must be gone, not duplicated"
        );
    }

    /// The evicted/dirty re-homing runs on every boot, so it must be
    /// safe to run twice — and a crash between the insert and the delete
    /// must leave the next boot able to finish, never a hole.
    #[tokio::test]
    async fn re_homing_evicted_rows_is_idempotent() {
        let r = rig(1024, 256);
        let stub = r.root.join("evicted.bin");
        std::fs::write(&stub, b"").unwrap();
        let (live_dev, ino) = ident(&stub);

        let ev = crate::state_backend::TierEvictedRow {
            dev: live_dev ^ 0x1,
            ino,
            key: "t/evicted.bin".into(),
            generation: 2,
            etag: "\"beef\"".into(),
            crc64_b64: "AAAAAAAAAAA=".into(),
            size: 8192,
            path: stub.to_string_lossy().into_owned(),
            evicted_unix: 900,
            hydrating_unix: None,
        };
        r.backend.tier_put_evicted(&ev).await.unwrap();

        r.orch.startup().await;
        let first = r.backend.tier_list_evicted().await.unwrap();
        r.orch.startup().await;
        let second = r.backend.tier_list_evicted().await.unwrap();

        assert_eq!(first.len(), 1, "one row in, one row out");
        assert_eq!(
            first.len(),
            second.len(),
            "a second boot must not duplicate the row or drop it"
        );
        assert_eq!(second[0].dev, live_dev);
        assert_eq!(second[0].size, 8192);
    }

    /// The re-homing is a MIGRATION, so it has to be safe to run twice —
    /// it runs on every boot, and a crash mid-way must leave the next
    /// boot able to finish the job rather than a mess.
    ///
    /// Insert-before-delete is what buys that: the two keys differ on
    /// `dev`, so a crash between them leaves a duplicate under a dead
    /// device number, never a hole. Deleting first would leave NEITHER
    /// row — the exact bug this repairs, made permanent.
    #[tokio::test]
    async fn re_homing_generation_rows_is_idempotent() {
        let r = rig(1024, 256);
        let f = r.root.join("kept.bin");
        std::fs::write(&f, b"payload").unwrap();
        let (live_dev, ino) = ident(&f);

        let stale = crate::state_backend::TierGenerationRow {
            dev: live_dev ^ 0x1,
            ino,
            key: "t/kept.bin".into(),
            generation: 3,
            etag: "\"abc\"".into(),
            crc64_b64: None,
            size: 7,
            copy_allowed: true,
            updated_unix: 1000,
        };
        r.backend.tier_upsert_generation(&stale).await.unwrap();

        r.orch.startup().await;
        let after_one = r.backend.tier_list_generations().await.unwrap();
        assert_eq!(after_one.len(), 1, "one row in, one row out: {after_one:?}");
        assert_eq!(after_one[0].dev, live_dev);
        assert_eq!(after_one[0].generation, 3);

        // Second boot: nothing stale left, so it must be a no-op rather
        // than duplicating or dropping anything.
        r.orch.startup().await;
        let after_two = r.backend.tier_list_generations().await.unwrap();
        assert_eq!(after_two.len(), 1, "a second pass changed the rows: {after_two:?}");
        assert_eq!(after_two[0].dev, live_dev);
        assert_eq!(after_two[0].generation, 3);
        assert!(r.orch.generation_of(live_dev, ino).is_some());
    }

    #[tokio::test]
    async fn first_publish_then_idle_and_bit_released() {
        let r = rig(1024, 256);
        let f = r.root.join("hello.bin");
        std::fs::write(&f, b"first generation contents").unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Write { offset: 0, len: 25 }).await;

        r.orch.tick().await;
        let g = r.orch.generation_of(dev, ino).expect("must be registered");
        assert_eq!(g.generation, 1);
        let (_, bytes) = r.mem.get_whole("t/hello.bin", Some(&g.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), b"first generation contents");
        assert!(
            our_row(&r, dev, ino).await.is_none(),
            "a fully-flushed file's durable bit must be released"
        );
        assert!(
            !capture::is_durable(dev, ino),
            "the durable memo must be released with the row"
        );
        // Nothing dirty → a second tick must not publish again.
        r.orch.tick().await;
        assert_eq!(r.orch.generation_of(dev, ino).unwrap().generation, 1);
    }

    #[tokio::test]
    async fn dirty_range_composes_over_clean_copy_and_content_matches() {
        let r = rig(256, 256);
        let f = r.root.join("data.bin");
        let mut content = vec![0xAAu8; 4096];
        std::fs::write(&f, &content).unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        let g1 = r.orch.generation_of(dev, ino).unwrap();
        assert_eq!(g1.generation, 1);

        // Dirty one slot in the middle; gen 2 composes over gen 1.
        content[512..768].fill(0x55);
        {
            use std::os::unix::fs::FileExt;
            let fh = std::fs::OpenOptions::new().write(true).open(&f).unwrap();
            fh.write_at(&content[512..768], 512).unwrap();
        }
        note_and_land(&r, &f, Mutation::Write { offset: 512, len: 256 }).await;
        r.orch.tick().await;
        let g2 = r.orch.generation_of(dev, ino).unwrap();
        assert_eq!(g2.generation, 2, "If-Match continuity gen1→gen2");
        let (m2, bytes) = r.mem.get_whole("t/data.bin", Some(&g2.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), content.as_slice(), "composed bytes == local truth");
        assert!(m2.etag.contains('-'), "gen2 must be a multipart compose: {}", m2.etag);
        // The plan for that state is 1 local slot between 2 coalesced
        // copy runs (the deterministic economics-shape assertion).
        let cap = cap_of(&[Mutation::Write { offset: 512, len: 256 }]);
        match plan_parts(
            4096,
            &cap,
            Some(PlanBase { size: 4096, copy_allowed: true }),
            &FlushConfig { ..r.orch.cfg.clone() },
            10_000,
        )
        .unwrap()
        {
            Plan::Compose { parts, .. } => assert_eq!(parts.len(), 3),
            p => panic!("{:?}", p),
        }
    }

    #[tokio::test]
    async fn floor_and_quiesce_defer_the_flush() {
        let mut r = rig(1024, 256);
        let f = r.root.join("hot.bin");
        std::fs::write(&f, b"hot file").unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Write { offset: 0, len: 8 }).await;

        // Quiesce: the note is fresh; a huge quiesce window defers.
        r.orch.cfg.quiesce = Duration::from_secs(3600);
        r.orch.tick().await;
        assert!(r.orch.generation_of(dev, ino).is_none(), "quiescence must defer");
        assert!(our_row(&r, dev, ino).await.is_some());

        // Publish once with the window off.
        r.orch.cfg.quiesce = Duration::ZERO;
        r.orch.tick().await;
        assert_eq!(r.orch.generation_of(dev, ino).unwrap().generation, 1);

        // Floor: re-dirty; a huge floor defers the SECOND flush.
        note_and_land(&r, &f, Mutation::Write { offset: 0, len: 8 }).await;
        r.orch.cfg.floor = Duration::from_secs(3600);
        r.orch.tick().await;
        assert_eq!(
            r.orch.generation_of(dev, ino).unwrap().generation,
            1,
            "the per-file floor must defer re-publish (fsync-churn cap)"
        );
        r.orch.cfg.floor = Duration::ZERO;
        r.orch.tick().await;
        assert_eq!(r.orch.generation_of(dev, ino).unwrap().generation, 2);
    }

    #[tokio::test]
    async fn restart_whole_dirty_clean_file_skips_upload_by_crc() {
        let r = rig(1024, 256);
        let f = r.root.join("clean.bin");
        std::fs::write(&f, b"stable contents").unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Write { offset: 0, len: 15 }).await;
        r.orch.tick().await;
        let etag1 = r.orch.generation_of(dev, ino).unwrap().etag;

        // "Restart": a fresh orchestrator (empty registry), the bit
        // restored whole-dirty — the A3 fallback shape.
        let cfg2 = FlushConfig {
            floor: Duration::ZERO,
            quiesce: Duration::ZERO,
            ..r.orch.cfg.clone()
        };
        let orch2 = FlushOrchestrator::new(
            r.mem.clone(),
            r.backend.clone(),
            cfg2,
            crate::tier::epoch::EpochGuard::held(1),
        );
        r.backend
            .tier_mark_dirty(&[TierDirtyEntry {
                dev,
                ino,
                path: Some(f.to_string_lossy().into_owned()),
                dirtied_unix: 1,
                mark_seq: 999_999,
            }])
            .await
            .unwrap();
        capture::prime_durable(dev, ino);
        capture::note(dev, ino, Mutation::Whole);

        let before = crate::tier::meter::snapshot();
        orch2.tick().await;
        let after = crate::tier::meter::snapshot();
        assert!(
            after.flushes_clean_match > before.flushes_clean_match,
            "identical content must be adopted by CRC, not re-uploaded"
        );
        let g = orch2.generation_of(dev, ino).unwrap();
        assert_eq!(g.etag, etag1, "the bucket object must be untouched");
        assert!(
            our_row(&r, dev, ino).await.is_none(),
            "the restored bit must clear once content is proven clean"
        );
    }

    #[tokio::test]
    async fn foreign_overwrite_recovers_local_wins_all_local() {
        let r = rig(1024, 256);
        let f = r.root.join("contested.bin");
        std::fs::write(&f, b"local truth v1").unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Write { offset: 0, len: 14 }).await;
        r.orch.tick().await;
        assert_eq!(r.orch.generation_of(dev, ino).unwrap().generation, 1);

        // An outside writer replaces the object.
        r.mem.raw_put("t/contested.bin", Bytes::from_static(b"foreign bytes"), vec![]);

        std::fs::write(&f, b"local truth v2").unwrap();
        note_and_land(&r, &f, Mutation::Whole).await;
        let row = our_row(&r, dev, ino).await.unwrap();
        let outcome = r.orch.flush_file(&row).await;
        assert_eq!(outcome, Outcome::ForeignDetected, "the guarded publish must fence");
        assert!(
            !r.orch.generation_of(dev, ino).unwrap().copy_allowed,
            "foreign bytes must never become a copy source"
        );

        // Next cycle: local truth re-publishes over it, guarded.
        r.orch.tick().await;
        let g = r.orch.generation_of(dev, ino).unwrap();
        let (_, bytes) = r.mem.get_whole("t/contested.bin", Some(&g.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), b"local truth v2", "publish path is LOCAL-WINS (A6)");
    }

    #[tokio::test]
    async fn reconcile_adopts_the_torn_intent_and_flushing_continues() {
        let r = rig(1024, 256);
        let f = r.root.join("torn.bin");
        std::fs::write(&f, b"torn publish contents").unwrap();
        let (dev, ino) = ident(&f);

        // The crash shape: intent recorded, object landed with our
        // stamps, process died before the commit.
        let uuid = "uuid-torn-1";
        r.backend
            .put_flush_intent(&FlushIntentRecord {
                flush_uuid: uuid.into(),
                path: f.to_string_lossy().into_owned(),
                from_gen: None,
                to_gen: 1,
                mpu_id: None,
                base_etag: None,
                created_unix: 1,
            })
            .await
            .unwrap();
        r.mem.raw_put(
            "t/torn.bin",
            Bytes::from_static(b"torn publish contents"),
            GenerationStamps { generation: 1, epoch: 0, flush_uuid: uuid.into(), boundary_source: None, posix: None }.to_meta(),
        );

        assert_eq!(r.orch.reconcile_intents().await, 1);
        assert!(r.backend.list_flush_intents().await.unwrap().is_empty());
        let g = r.orch.generation_of(dev, ino).expect("adopted generation must seed");
        assert_eq!(g.generation, 1);

        // And the pipeline continues on top of the adopted base.
        std::fs::write(&f, b"post-crash update!!!!").unwrap();
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        let g2 = r.orch.generation_of(dev, ino).unwrap();
        assert_eq!(g2.generation, 2);
        let (_, bytes) = r.mem.get_whole("t/torn.bin", Some(&g2.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), b"post-crash update!!!!");
    }

    /// Land an identity event in OUR backend (the event queue is
    /// process-global like capture's — theft-repair by re-noting; a
    /// double-apply is idempotent, the covered row being already gone).
    async fn rename_and_land(
        rigg: &Rig,
        moved: (u64, u64),
        new_path: &Path,
        covered: Option<(u64, u64)>,
    ) {
        for _ in 0..50 {
            crate::tier::identity::note_rename(Some(moved), new_path, covered);
            let _ = crate::tier::durable::drain_pending(&rigg.backend).await;
            let moved_ok = rigg
                .backend
                .tier_list_dirty()
                .await
                .unwrap()
                .iter()
                .any(|r| {
                    r.dev == moved.0
                        && r.ino == moved.1
                        && r.path.as_deref() == Some(&*new_path.to_string_lossy())
                });
            let covered_ok = match covered {
                None => true,
                Some(c) => {
                    // Either the covered row was tombstoned by our
                    // apply, or it never had a generation row.
                    rigg.backend
                        .tier_list_generations()
                        .await
                        .unwrap()
                        .iter()
                        .all(|g| !(g.dev == c.0 && g.ino == c.1))
                }
            };
            if moved_ok && covered_ok {
                return;
            }
        }
        panic!("identity event never landed (theft-repair exhausted)");
    }

    /// THE STEP-6 DRILL: git's tmp-write+rename idiom, the tier's
    /// proof workload. Every finalize must publish cleanly — zero
    /// false 412s, zero Foreign verdicts, no orphan tmp objects, no
    /// resurrection of covered files. (Pre-A7 this wedged on EVERY
    /// iteration: the covered object at the final key made the fresh
    /// file's create-flavor publish 412 into a Foreign verdict.)
    #[tokio::test]
    async fn git_storm_tmp_write_rename_never_false_412s() {
        let r = rig(1024, 256);
        let final_path = r.root.join("obj.pack");
        let mut prev_ident: Option<(u64, u64)> = None;
        let mut last_content = Vec::new();
        let mut last_ident = (0, 0);
        for i in 0..12u32 {
            let tmp = r.root.join(format!("obj.pack.tmp{}", i));
            let content = format!("packfile generation {} payload", i).into_bytes();
            std::fs::write(&tmp, &content).unwrap();
            note_and_land(&r, &tmp, Mutation::Whole).await;
            std::fs::rename(&tmp, &final_path).unwrap();
            let moved = ident(&final_path);
            rename_and_land(&r, moved, &final_path, prev_ident).await;

            // The tick sequence, run deterministically so the OUTCOME
            // is assertable per iteration.
            r.orch.load_generations().await;
            r.orch.consume_tombstones().await;
            let row = our_row(&r, moved.0, moved.1).await.expect("moved bit must be set");
            let out = r.orch.flush_file(&row).await;
            assert!(
                matches!(out, Outcome::Published { .. } | Outcome::CleanMatch),
                "iteration {}: {:?} — the false-412 wedge A7 exists to kill",
                i, out
            );
            prev_ident = Some(moved);
            last_ident = moved;
            last_content = content;
        }
        // End state: exactly ONE object, the final content, no
        // tombstones, no orphan tmp keys.
        let listed = r.mem.list("t/").await.unwrap();
        assert_eq!(
            listed.len(),
            1,
            "orphan objects after the storm: {:?}",
            listed.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
        assert_eq!(listed[0].key, "t/obj.pack");
        let g = r.orch.generation_of(last_ident.0, last_ident.1).unwrap();
        let (_, bytes) = r.mem.get_whole("t/obj.pack", Some(&g.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), last_content.as_slice());
        assert!(
            r.backend.tier_list_tombstones().await.unwrap().is_empty(),
            "every tombstone must be consumed"
        );
    }

    /// Rename AFTER publish: the flusher re-keys — the new key gets
    /// the generation via server-side copy (no re-upload of clean
    /// bytes), the old object is deleted, and nothing 412s.
    #[tokio::test]
    async fn rename_after_publish_rekeys_the_object() {
        let r = rig(256, 256);
        let a = r.root.join("a.bin");
        let content = vec![0xCDu8; 4096];
        std::fs::write(&a, &content).unwrap();
        let idn = ident(&a);
        note_and_land(&r, &a, Mutation::Whole).await;
        r.orch.tick().await;
        let g1 = r.orch.generation_of(idn.0, idn.1).unwrap();
        assert_eq!((g1.generation, g1.key.as_str()), (1, "t/a.bin"));

        let b = r.root.join("b.bin");
        std::fs::rename(&a, &b).unwrap();
        rename_and_land(&r, idn, &b, None).await;

        r.orch.tick().await;
        let g2 = r.orch.generation_of(idn.0, idn.1).unwrap();
        assert_eq!(
            (g2.generation, g2.key.as_str()),
            (2, "t/b.bin"),
            "the row must re-point at the new key"
        );
        let (m, bytes) = r.mem.get_whole("t/b.bin", Some(&g2.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), content.as_slice());
        assert!(
            m.etag.contains('-'),
            "a clean re-key must move by server-side copy (multipart), not re-upload: {}",
            m.etag
        );
        assert!(
            matches!(r.mem.head("t/a.bin").await, Err(StoreError::NotFound(_))),
            "the old key's object must be deleted"
        );
        assert!(r.backend.tier_list_tombstones().await.unwrap().is_empty());
    }

    /// Chaos C3's find: a foreign DELETE of a published file's object
    /// makes the guarded re-publish answer 404 (not 412) — that must
    /// route to arbitration (Foreign(None) ⇒ drop the row) and the
    /// next cycle re-CREATES local truth, never wedge forever.
    #[tokio::test]
    async fn foreign_delete_of_published_object_recreates_next_cycle() {
        let r = rig(1024, 256);
        let f = r.root.join("phoenix.bin");
        std::fs::write(&f, b"local truth v1").unwrap();
        let idn = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        assert!(r.mem.head("t/phoenix.bin").await.is_ok());

        // The foreign hand deletes the object; local appends.
        r.mem.delete("t/phoenix.bin").await.unwrap();
        std::fs::write(&f, b"local truth v1 + v2").unwrap();
        note_and_land(&r, &f, Mutation::Whole).await;

        // First cycle: 404 ⇒ arbitrated Foreign(None) ⇒ row dropped.
        // Second cycle: no base ⇒ create-flavor publish wins.
        r.orch.tick().await;
        r.orch.tick().await;
        let g = r
            .orch
            .generation_of(idn.0, idn.1)
            .expect("the file must re-publish after a foreign delete");
        let (_, bytes) = r.mem.get_whole("t/phoenix.bin", Some(&g.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), b"local truth v1 + v2", "local truth re-created");
    }

    /// FlintTierEpoch's ProbeOverwrite counterexample, closed: a
    /// deposed-but-unfenced hub (guard still at epoch 1, store epoch
    /// object superseded to 2) 412s against the successor's object and
    /// must FENCE on the epoch stamp — never local-wins over the live
    /// successor.
    #[tokio::test]
    async fn foreign_object_with_successor_epoch_stamp_fences_not_republishes() {
        let r = rig(1024, 256);
        let f = r.root.join("contested.bin");
        std::fs::write(&f, b"zombie truth v1").unwrap();
        let idn = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;

        // The store's epoch object: our reign (1), then a successor's (2).
        let ekey = crate::tier::epoch::epoch_key("t/");
        r.mem.epoch_acquire(&ekey, "zombie", None).await.unwrap();
        let state = r.mem.epoch_read(&ekey).await.unwrap().unwrap();
        r.mem.epoch_acquire(&ekey, "successor", Some(&state)).await.unwrap();

        // The successor re-publishes the key, stamped with ITS epoch.
        let cur = r.mem.head("t/contested.bin").await.unwrap();
        let body = Bytes::from_static(b"the successor's truth");
        let stamps = GenerationStamps {
            generation: 2,
            epoch: 2,
            flush_uuid: "succ-uuid".into(),
            boundary_source: None,
            posix: None,
        };
        r.mem
            .put_whole(
                "t/contested.bin",
                body.clone(),
                &PutCondition::IfMatch(cur.etag),
                &stamps,
                crate::tier::store::crc64_nvme(&body),
            )
            .await
            .unwrap();

        // The zombie's next flush 412s, sees the higher stamp, verifies
        // against the epoch object, and FENCES.
        std::fs::write(&f, b"zombie truth v2 (must never land)").unwrap();
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        assert!(r.guard.current().is_none(), "the guard must be fenced");
        let (_, bytes) = r.mem.get_whole("t/contested.bin", None).await.unwrap();
        assert_eq!(
            bytes.as_ref(),
            b"the successor's truth",
            "the successor's object must survive untouched"
        );
        assert!(
            our_row(&r, idn.0, idn.1).await.is_some(),
            "the durable backlog stays for recovery"
        );
        // Fenced ticks publish nothing.
        r.orch.tick().await;
        let (_, bytes) = r.mem.get_whole("t/contested.bin", None).await.unwrap();
        assert_eq!(bytes.as_ref(), b"the successor's truth");
    }

    /// The forged-stamp guard: an outside writer stamping an absurd
    /// epoch must NOT fence a healthy hub (the store still shows our
    /// reign) — A6 local-wins re-publishes as ever.
    #[tokio::test]
    async fn fabricated_epoch_stamp_does_not_fence_a_healthy_hub() {
        let r = rig(1024, 256);
        let f = r.root.join("forged.bin");
        std::fs::write(&f, b"local truth v1").unwrap();
        let idn = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;

        // Epoch object: ours, epoch 1 — matching the guard.
        let ekey = crate::tier::epoch::epoch_key("t/");
        r.mem.epoch_acquire(&ekey, "us", None).await.unwrap();

        // A foreign hand overwrites with a fabricated epoch-99 stamp.
        let cur = r.mem.head("t/forged.bin").await.unwrap();
        let body = Bytes::from_static(b"impostor bytes");
        let stamps = GenerationStamps {
            generation: 9,
            epoch: 99,
            flush_uuid: "forged-uuid".into(),
            boundary_source: None,
            posix: None,
        };
        r.mem
            .put_whole(
                "t/forged.bin",
                body.clone(),
                &PutCondition::IfMatch(cur.etag),
                &stamps,
                crate::tier::store::crc64_nvme(&body),
            )
            .await
            .unwrap();

        std::fs::write(&f, b"local truth v2").unwrap();
        note_and_land(&r, &f, Mutation::Whole).await;
        // Cycle 1: 412 ⇒ Foreign ⇒ stamp 99 > 1 but the store shows our
        // reign ⇒ fabrication ⇒ foreign state recorded.  Cycle 2:
        // local-wins re-publish guarded on the impostor's etag.
        r.orch.tick().await;
        assert!(r.guard.current().is_some(), "a forged stamp must not fence us");
        r.orch.tick().await;
        let g = r.orch.generation_of(idn.0, idn.1).expect("re-published");
        let (_, bytes) = r.mem.get_whole("t/forged.bin", Some(&g.etag)).await.unwrap();
        assert_eq!(bytes.as_ref(), b"local truth v2", "local truth re-won the key");
    }

    /// Step 12's tidy: a tombstone whose key was legitimately
    /// re-published (live row, DIFFERENT etag) names an object that no
    /// longer exists — it closes WITHOUT deleting anything. A
    /// same-etag tombstone (the re-key crash window) keeps deferring.
    #[tokio::test]
    async fn superseded_tombstone_closes_without_deleting_the_live_object() {
        let r = rig(1024, 256);
        let f = r.root.join("reborn.bin");
        std::fs::write(&f, b"the second life").unwrap();
        let idn = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        let g = r.orch.generation_of(idn.0, idn.1).unwrap();

        // A stale tombstone from a superseded life of this key.
        r.backend
            .tier_put_tombstone(&crate::state_backend::TierTombstone {
                key: g.key.clone(),
                etag: Some("stale-superseded-etag".into()),
                created_unix: 1,
            })
            .await
            .unwrap();
        // And one that still matches the LIVE etag — the re-key crash
        // window shape; it must keep deferring.
        r.backend
            .tier_put_tombstone(&crate::state_backend::TierTombstone {
                key: "t/other-live.bin".into(),
                etag: Some("held".into()),
                created_unix: 1,
            })
            .await
            .unwrap();
        r.orch.generations.insert(
            (999_999, 777_777),
            GenRecord {
                key: "t/other-live.bin".into(),
                generation: 1,
                etag: "held".into(),
                crc64_b64: None,
                size: 1,
                copy_allowed: false,
            },
        );

        r.orch.consume_tombstones().await;
        let left = r.backend.tier_list_tombstones().await.unwrap();
        assert_eq!(left.len(), 1, "{:?}", left);
        assert_eq!(left[0].key, "t/other-live.bin", "same-etag tombstone defers");
        assert!(r.mem.head(&g.key).await.is_ok(), "the live object is untouched");
        r.orch.generations.remove(&(999_999, 777_777));
    }

    /// REMOVE after publish: the tombstone flows through the barrier
    /// and the bucket object is deleted — never resurrected.
    #[tokio::test]
    async fn remove_after_publish_deletes_the_bucket_object() {
        let r = rig(1024, 256);
        let f = r.root.join("victim.bin");
        std::fs::write(&f, b"soon to be deleted").unwrap();
        let idn = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        assert!(r.mem.head("t/victim.bin").await.is_ok());

        std::fs::remove_file(&f).unwrap();
        for _ in 0..50 {
            crate::tier::identity::note_remove(idn);
            let _ = crate::tier::durable::drain_pending(&r.backend).await;
            if r.backend
                .tier_list_tombstones()
                .await
                .unwrap()
                .iter()
                .any(|t| t.key == "t/victim.bin")
            {
                break;
            }
        }
        assert!(
            r.backend.tier_list_generations().await.unwrap().iter().all(|g| g.ino != idn.1),
            "the generation row must die with the file"
        );

        r.orch.tick().await;
        assert!(
            matches!(r.mem.head("t/victim.bin").await, Err(StoreError::NotFound(_))),
            "the bucket object must be deleted at the barrier"
        );
        assert!(r.backend.tier_list_tombstones().await.unwrap().is_empty());
        assert!(our_row(&r, idn.0, idn.1).await.is_none());
    }

    #[tokio::test]
    async fn shrink_regrow_never_resurrects_old_bytes() {
        let r = rig(256, 256);
        let f = r.root.join("shrink.bin");
        std::fs::write(&f, vec![0xEEu8; 4096]).unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        assert_eq!(r.orch.generation_of(dev, ino).unwrap().generation, 1);

        // Shrink to 1024, regrow to 4096 (kernel zero-fills) — the
        // C5 corruption shape: a naive clean-copy would resurrect
        // gen 1's 0xEE tail into gen 2.
        {
            let fh = std::fs::OpenOptions::new().write(true).open(&f).unwrap();
            fh.set_len(1024).unwrap();
            fh.set_len(4096).unwrap();
        }
        note_and_land(&r, &f, Mutation::Truncate { new_size: 1024 }).await;
        capture::note_path(&f, Mutation::Zero { offset: 1024, len: 3072 });
        r.orch.tick().await;
        let g2 = r.orch.generation_of(dev, ino).unwrap();
        assert_eq!(g2.generation, 2);
        let (_, bytes) = r.mem.get_whole("t/shrink.bin", Some(&g2.etag)).await.unwrap();
        assert_eq!(&bytes[..1024], vec![0xEEu8; 1024].as_slice());
        assert_eq!(
            &bytes[1024..],
            vec![0u8; 3072].as_slice(),
            "the regrown gap must be zeros — never gen 1's resurrected bytes"
        );
    }

    // ── step 7: the epoch fence at the flusher (A8) ──────────────────

    #[tokio::test]
    async fn fenced_orchestrator_publishes_nothing_and_keeps_the_bit() {
        let r = rig(1024, 256);
        let f = r.root.join("fenced.bin");
        std::fs::write(&f, b"must never reach the bucket").unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;

        r.guard.fence();
        let report = r.orch.tick().await;
        assert_eq!(report.examined, 0, "a fenced tick must not even walk the dirty set");
        assert!(
            matches!(r.mem.head("t/fenced.bin").await, Err(StoreError::NotFound(_))),
            "nothing may publish through a fenced guard"
        );
        assert!(
            our_row(&r, dev, ino).await.is_some(),
            "the durable bit survives the fence — a re-claimed epoch resumes the flush"
        );
        // Direct flush_file is refused the same way.
        let row = our_row(&r, dev, ino).await.unwrap();
        assert!(matches!(r.orch.flush_file(&row).await, Outcome::Fenced));
    }

    #[tokio::test]
    async fn stamps_carry_the_held_epoch() {
        let r = rig(1024, 256);
        r.guard.set_held(7);
        let f = r.root.join("stamped.bin");
        std::fs::write(&f, b"epoch-stamped").unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.orch.tick().await;
        assert_eq!(r.orch.generation_of(dev, ino).unwrap().generation, 1);
        let meta = r.mem.head("t/stamped.bin").await.unwrap();
        assert_eq!(
            meta.meta.get(GenerationStamps::META_EPOCH).map(String::as_str),
            Some("7"),
            "every publish is stamped with the LIVE epoch (A8)"
        );
    }

    #[tokio::test]
    async fn reserved_namespace_files_are_never_tiered() {
        let r = rig(1024, 256);
        let dir = r.root.join(crate::tier::epoch::RESERVED_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("epoch");
        std::fs::write(&f, b"a client file shadowing the control object").unwrap();
        assert_eq!(
            r.orch.key_for(&f),
            None,
            "a client file under .flint/ must not shadow a tier control object"
        );
        assert!(r.orch.key_for(&r.root.join("normal.bin")).is_some());

        // At ANY depth, not just the first component. This is the
        // ancestor-hub case: a share on `t/` that has somehow ended up
        // holding a nested share's control objects must not map them
        // back to keys that overwrite that share's LIVE epoch cell.
        let nested = r.root.join("nested").join(crate::tier::epoch::RESERVED_DIR);
        std::fs::create_dir_all(&nested).unwrap();
        let inner = nested.join("epoch");
        std::fs::write(&inner, b"another share's live epoch cell").unwrap();
        assert_eq!(
            r.orch.key_for(&inner),
            None,
            "a nested .flint/ is another share's control namespace — never tier it"
        );
        assert_eq!(
            r.orch.key_for(&nested.join("manifest")),
            None,
            "same for the nested manifest"
        );
        // Anti-vacuity: the guard is the RESERVED name, not the depth.
        let sibling = r.root.join("nested").join("ordinary.bin");
        std::fs::write(&sibling, b"client data").unwrap();
        assert!(
            r.orch.key_for(&sibling).is_some(),
            "an ordinary nested file is still tiered"
        );
    }

    /// The A8 drill, first half: kill the hub mid-flush; the restart
    /// must resume WITHOUT operator CAS (self-recognition), its claim
    /// sweeping the crashed flush's orphan assembly, and the re-flush
    /// publishes under the NEW epoch.
    #[tokio::test]
    async fn kill_mid_flush_restart_resumes_without_operator_cas() {
        let r = rig(256, 256);
        let store_dyn: Arc<dyn ObjectStore> = r.mem.clone();
        // Absurd lease so any accidental foreign-wait path would hang
        // past the timeout instead of passing by luck.
        let mut ecfg = crate::tier::epoch::EpochConfig::new("t/", "hub-drill7".into());
        ecfg.heartbeat = Duration::from_secs(3600);
        ecfg.lease_misses = 1000;

        let l1 = crate::tier::epoch::claim(&store_dyn, &ecfg, "t/").await.unwrap();
        assert_eq!(l1.epoch, 1);

        let f = r.root.join("midflush.bin");
        std::fs::write(&f, vec![0xABu8; 4096]).unwrap();
        let (dev, ino) = ident(&f);
        note_and_land(&r, &f, Mutation::Whole).await;
        r.mem.inject_crash_before_complete();
        r.orch.tick().await;
        assert_eq!(
            r.mem.list_uploads("t/").await.unwrap().len(),
            1,
            "the crashed flush must leave its in-flight assembly"
        );

        // "Crash": no release, no heartbeat. "Restart": same holder_id.
        let l2 = tokio::time::timeout(
            Duration::from_secs(5),
            crate::tier::epoch::claim(&store_dyn, &ecfg, "t/"),
        )
        .await
        .expect("restart must resume by self-recognition, not wait out a lease")
        .unwrap();
        assert_eq!(l2.epoch, 2);
        assert!(
            r.mem.list_uploads("t/").await.unwrap().is_empty(),
            "the resumed claim must sweep the crashed flush's assembly"
        );

        let cfg2 = FlushConfig {
            floor: Duration::ZERO,
            quiesce: Duration::ZERO,
            ..r.orch.cfg.clone()
        };
        let orch2 = FlushOrchestrator::new(
            store_dyn,
            r.backend.clone(),
            cfg2,
            crate::tier::epoch::EpochGuard::held(l2.epoch),
        );
        orch2.startup().await;
        orch2.tick().await;

        let g = orch2.generation_of(dev, ino).expect("the resumed flush must publish");
        assert_eq!(g.generation, 1);
        let meta = r.mem.head("t/midflush.bin").await.unwrap();
        assert_eq!(meta.size, 4096);
        assert_eq!(
            meta.meta.get(GenerationStamps::META_EPOCH).map(String::as_str),
            Some("2"),
            "the resumed publish must carry the RESUMED epoch"
        );
    }
}
