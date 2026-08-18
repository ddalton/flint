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
const CHUNK: u64 = 8 * 1024 * 1024;
/// Marker-poll cadence while parking an RPC.
const PARK_POLL_MS: u64 = 50;
/// Retry backoff cap for failed restores.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

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
}

impl Default for HydrateConfig {
    fn default() -> Self {
        HydrateConfig { hold: Duration::from_secs(15), concurrency: 4 }
    }
}

pub(crate) struct Inflight {
    pub(crate) write_pri: AtomicBool,
}

pub struct Hydrator {
    backend: Arc<dyn StateBackend>,
    store: Arc<dyn ObjectStore>,
    cfg: HydrateConfig,
    handle: tokio::runtime::Handle,
    shared: Arc<tokio::sync::Semaphore>,
    write_reserved: Arc<tokio::sync::Semaphore>,
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
        cfg: HydrateConfig { hold: Duration::from_secs(2), concurrency },
        handle: tokio::runtime::Handle::current(),
        shared: Arc::new(tokio::sync::Semaphore::new(concurrency.max(1))),
        write_reserved: Arc::new(tokio::sync::Semaphore::new(1)),
        inflight: DashMap::new(),
    })
}

/// Request hydration of an evicted file. Sync and cheap — callable
/// from the blocking closures at the marker-consult sites. Idempotent:
/// an in-flight restore absorbs the request (a WRITE trigger upgrades
/// its priority).
pub fn request(dev: u64, ino: u64, path: &Path, trigger: Trigger) {
    let Some(h) = current() else { return };
    if !evict::is_evicted(dev, ino) {
        return;
    }
    use dashmap::mapref::entry::Entry;
    let fresh = match h.inflight.entry((dev, ino)) {
        Entry::Occupied(e) => {
            if trigger == Trigger::Write {
                e.get().write_pri.store(true, Ordering::Relaxed);
            }
            None
        }
        Entry::Vacant(v) => {
            let inflight = Arc::new(Inflight {
                write_pri: AtomicBool::new(trigger == Trigger::Write),
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

// ── the restore task ─────────────────────────────────────────────────

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
        // Permit: write-priority may ALSO take the reserved slot.
        let _permit = if inflight.write_pri.load(Ordering::Relaxed) {
            tokio::select! {
                p = Arc::clone(&h.write_reserved).acquire_owned() => p.ok(),
                p = Arc::clone(&h.shared).acquire_owned() => p.ok(),
            }
        } else {
            match tokio::time::timeout(
                Duration::from_millis(500),
                Arc::clone(&h.shared).acquire_owned(),
            )
            .await
            {
                Ok(p) => p.ok(),
                Err(_) => continue, // re-check write_pri
            }
        };

        // A10 admission: wait while the object cannot fit in
        // headroom-minus-reserve (the watermark pass may be freeing
        // space right now). Path-scoped to the configured export root.
        if let Some(meta) = evict::marker_meta(dev, ino) {
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
        }

        let began = std::time::Instant::now();
        match restore_once(&h, dev, ino, &path).await {
            Ok(bytes) => {
                meter::bump(Counter::HydrationsCompleted);
                meter::add(Counter::HydrationBytes, bytes);
                meter::add(Counter::HydrationMillis, began.elapsed().as_millis() as u64);
                info!("tier hydrate: {} restored ({} bytes)", path.display(), bytes);
                break;
            }
            Err(e) => {
                meter::bump(Counter::HydrationFailures);
                attempt += 1;
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
    h.inflight.remove(&(dev, ino));
}

/// One restore attempt. On ANY failure the file is truncated back to
/// the stub and the hydrating flag cleared — partial bytes never
/// survive an error path.
pub(crate) async fn restore_once(
    h: &Arc<Hydrator>,
    dev: u64,
    ino: u64,
    path: &Path,
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

    let result = stream_restore(h, dev, ino, path, &mut meta).await;
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

/// Ranged, If-Match-guarded streaming restore into the stub inode.
/// Handles the S3-wins adopt on 412 by updating `meta` + the durable
/// marker and signalling a retry (the caller loop restarts).
async fn stream_restore(
    h: &Arc<Hydrator>,
    dev: u64,
    ino: u64,
    path: &Path,
    meta: &mut EvictedMeta,
) -> Result<u64, String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("open stub: {}", e))?;
    let file = Arc::new(file);
    let mut crc = Crc64Nvme::new();
    let mut off = 0u64;
    while off < meta.size {
        let len = CHUNK.min(meta.size - off);
        let chunk = match h.store.get_range(&meta.key, off, len, &meta.etag).await {
            Ok(b) => b,
            Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                // A6: hydration-GET 412 (or a deleted-and-replaced
                // key) is S3-WINS. Adopt the bucket's CURRENT object
                // and restart the restore on it.
                return match adopt_foreign(h, dev, ino, meta).await {
                    Ok(()) => Err("foreign overwrite adopted — restarting restore".into()),
                    Err(e) => Err(e),
                };
            }
            Err(e) => return Err(format!("get_range: {}", e)),
        };
        if chunk.is_empty() {
            return Err("short object: empty range before expected end".into());
        }
        crc.update(&chunk);
        let f = Arc::clone(&file);
        let at = off;
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::FileExt;
            f.write_all_at(&chunk, at)
        })
        .await
        .map_err(|e| format!("pwrite join: {}", e))?
        .map_err(|e| format!("pwrite: {}", e))?;
        off += len;
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
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
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
        Arc::new(Hydrator {
            backend: r.backend.clone(),
            store: r.mem.clone(),
            cfg: HydrateConfig { hold: Duration::from_secs(2), concurrency },
            handle: tokio::runtime::Handle::current(),
            shared: Arc::new(tokio::sync::Semaphore::new(concurrency.max(1))),
            write_reserved: Arc::new(tokio::sync::Semaphore::new(1)),
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
    async fn failed_restore_truncates_back_and_retry_succeeds() {
        let r = rig();
        let content = vec![0x5Au8; 1500];
        let (dev, ino, _key) = evicted_file(&r, "h2.bin", content.clone()).await;
        let f = r.root.join("h2.bin");

        let h = local_hydrator(&r, 2);
        r.mem.inject_get_range_failure();
        let err = restore_once(&h, dev, ino, &f).await.unwrap_err();
        assert!(err.contains("injected"), "{}", err);
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

        let inflight = Arc::new(Inflight { write_pri: AtomicBool::new(true) });
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
}
