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
    crc64_to_b64, ComposeSpec, Crc64Nvme, GenerationStamps, ObjectStore, PartSource,
    PutCondition, StoreError,
};
use bytes::Bytes;
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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

/// The in-memory generation registry entry (durable in step 6).
#[derive(Debug, Clone)]
pub struct GenRecord {
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
    generations: DashMap<(u64, u64), GenRecord>,
    last_flush: DashMap<(u64, u64), Instant>,
}

impl FlushOrchestrator {
    pub fn new(
        store: Arc<dyn ObjectStore>,
        backend: Arc<dyn StateBackend>,
        cfg: FlushConfig,
    ) -> Self {
        FlushOrchestrator {
            store,
            backend,
            cfg,
            generations: DashMap::new(),
            last_flush: DashMap::new(),
        }
    }

    pub fn key_for(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.cfg.export_root).ok()?;
        Some(format!("{}{}", self.cfg.key_prefix, rel.to_string_lossy()))
    }

    /// Test/observability surface.
    pub fn generation_of(&self, dev: u64, ino: u64) -> Option<GenRecord> {
        self.generations.get(&(dev, ino)).map(|e| e.clone())
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
                        self.generations.insert(
                            (dev, ino),
                            GenRecord {
                                generation: intent.to_gen,
                                etag: meta.etag.clone(),
                                crc64_b64: meta.crc64_b64.clone(),
                                size: meta.size,
                                copy_allowed: meta.copy_source_allowed(),
                            },
                        );
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

    /// One scheduling pass over the durable dirty set.
    pub async fn tick(&self) -> TickReport {
        let mut report = TickReport::default();
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
                | Outcome::PathMismatch => {}
                Outcome::Failed(_) => report.failed += 1,
            }
        }
        report
    }

    /// Flush one file. `row` is its durable dirty entry from this
    /// tick's listing (path + the observed mark sequence).
    pub async fn flush_file(&self, row: &TierDirtyEntry) -> Outcome {
        let (dev, ino) = (row.dev, row.ino);
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

        let size = match tokio::task::spawn_blocking({
            let p = path.clone();
            move || std::fs::metadata(&p).map(|m| m.len())
        })
        .await
        {
            Ok(Ok(s)) => s,
            _ => return self.fail(dev, ino, Some(epoch), format!("stat {}", path.display())),
        };

        // Restart clean-skip: discovered bucket CRC equals local truth
        // ⇒ adopt clean, upload nothing. (Worth a full local read;
        // never worth a full upload.)
        if let Some(bucket_crc) = &discovered_crc {
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
                    self.generations.insert((dev, ino), base.clone().unwrap());
                    self.try_clear_clean(dev, ino, row.mark_seq).await;
                    self.last_flush.insert((dev, ino), Instant::now());
                    return Outcome::CleanMatch;
                }
                Ok(_) => {}
                Err(e) => return self.fail(dev, ino, Some(epoch), e),
            }
        }

        if !epoch.is_dirty() && base.is_some() {
            // Bit set but nothing captured (leftover row): try to
            // release it; the conditional clear keeps it on any race.
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
        let condition = match &base {
            Some(b) => PutCondition::IfMatch(b.etag.clone()),
            None => PutCondition::IfNoneMatchAny,
        };
        let stamps = GenerationStamps {
            generation: to_gen,
            epoch: 0, // pre-epoch-machinery (step 7)
            flush_uuid: flush_uuid.clone(),
        };

        // Durable intent BEFORE any store mutation (A6).
        let intent = FlushIntentRecord {
            flush_uuid: flush_uuid.clone(),
            path: path.to_string_lossy().into_owned(),
            from_gen: base.as_ref().map(|b| b.generation),
            to_gen,
            mpu_id: None,
            base_etag: base.as_ref().map(|b| b.etag.clone()),
            created_unix: now_unix(),
        };
        if let Err(e) = self.backend.put_flush_intent(&intent).await {
            return self.fail(dev, ino, Some(epoch), format!("intent: {}", e));
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
                self.generations.insert(
                    (dev, ino),
                    GenRecord {
                        generation: to_gen,
                        etag: meta.etag.clone(),
                        crc64_b64: meta.crc64_b64.clone(),
                        size,
                        copy_allowed: true,
                    },
                );
                let _ = self.backend.delete_flush_intent(&flush_uuid).await;
                self.last_flush.insert((dev, ino), Instant::now());
                self.try_clear_clean(dev, ino, row.mark_seq).await;
                debug!("tier flush: published {} gen {}", key, to_gen);
                Outcome::Published { to_gen }
            }
            Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
                meter::bump(Counter::Publish412s);
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
                        self.generations.insert(
                            (dev, ino),
                            GenRecord {
                                generation: to_gen,
                                etag: meta.etag.clone(),
                                crc64_b64: meta.crc64_b64.clone(),
                                size,
                                copy_allowed: true,
                            },
                        );
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
                                self.generations.insert(
                                    (dev, ino),
                                    GenRecord {
                                        generation,
                                        etag: m.etag.clone(),
                                        crc64_b64: m.crc64_b64.clone(),
                                        size: m.size,
                                        // Foreign bytes are never a
                                        // copy source for local truth.
                                        copy_allowed: false,
                                    },
                                );
                            }
                            None => {
                                warn!(
                                    "tier flush: {} was DELETED in the bucket; local truth \
                                     re-creates it next cycle",
                                    key
                                );
                                self.generations.remove(&(dev, ino));
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

/// Streaming CRC-64/NVME over the whole file (blocking pool).
async fn file_crc(path: &Path) -> Result<u64, String> {
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

#[cfg(test)]
mod tests {
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
        orch: FlushOrchestrator,
    }

    fn rig(whole_put_max: u64, part_floor: u64) -> Rig {
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
        let orch = FlushOrchestrator::new(store_dyn, backend.clone(), cfg);
        Rig { _dir: dir, root, mem, backend, orch }
    }

    fn ident(path: &Path) -> (u64, u64) {
        stat_identity(path).unwrap()
    }

    /// Note + land the durable row, repairing parallel-test theft
    /// (process-global capture queue — the step-2 lesson): re-note the
    /// SAME mutation until the row exists in OUR backend.
    async fn note_and_land(rigg: &Rig, path: &Path, m: Mutation) {
        let (dev, ino) = ident(path);
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
        let orch2 = FlushOrchestrator::new(r.mem.clone(), r.backend.clone(), cfg2);
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
            GenerationStamps { generation: 1, epoch: 0, flush_uuid: uuid.into() }.to_meta(),
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
}
