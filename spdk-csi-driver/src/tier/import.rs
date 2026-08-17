//! Import-refresh — L2 step 12 (design review A7's consumer + A12's
//! restore driver).
//!
//! ONE verb, two lanes, run BEFORE the listener binds (nothing here is
//! concurrent with clients):
//!
//! - **manifest lane** — rebuild the tree the DR manifest describes:
//!   directories and symlinks materialize directly (mode/uid/gid/mtime
//!   applied), regular files materialize as EVICTED STUBS — a 0-byte
//!   file whose durable marker points at the bucket object — so a
//!   restore moves no content bytes; everything hydrates on first
//!   touch through the step-11 machinery. This is the DR path: CAS +
//!   manifest-driven restore + consumer remount.
//! - **sweep lane** — bucket objects under the prefix that the
//!   manifest does not know (foreign additions, pre-existing bucket
//!   data being adopted) ingest the same way, metadata from their A12
//!   posix stamps when present.
//!
//! Invariants:
//!
//! - **A tombstoned key is NEVER re-ingested** (A7): its object is a
//!   deleted/renamed-away file whose bucket delete has not flushed;
//!   importing it would resurrect it on every refresh.
//! - **Local wins**: an existing local path is never touched — import
//!   ADDS what is missing, it never overwrites what is present.
//! - **Crash-safe by temp+rename**: the stub is assembled at a
//!   `.flint-import.*` temp name (chmod/chown/mtime, durable marker +
//!   generation rows keyed by its identity, marker xattr), then
//!   renamed into place. A durable intent note next to state.db marks
//!   the import; on restart with the note present, stray temps are
//!   swept and rows whose path never materialized are deleted before
//!   the (idempotent) import re-runs. The evicted-row reconciler's
//!   "orphaned" arm is exactly this residue — step 12 owns it, here.

use crate::state_backend::{StateBackend, TierEvictedRow, TierGenerationRow};
use crate::tier::manifest::{self, EntryKind, Manifest, IMPORT_TMP_PREFIX};
use crate::tier::meter::{self, Counter};
use crate::tier::store::{ObjectStore, PosixStamps, StoreError};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

pub struct ImportConfig<'a> {
    pub export_root: &'a Path,
    pub key_prefix: &'a str,
    /// Durable "an import is running" note (next to state.db, like the
    /// ballast). None (memory backend) loses crash-resume, not
    /// correctness — a memory-state hub is fresh every boot anyway.
    pub intent_path: Option<&'a Path>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub dirs_restored: usize,
    pub symlinks_restored: usize,
    pub stubs_created: usize,
    /// The A7 guard at work: keys refused because their tombstone has
    /// not flushed.
    pub skipped_tombstoned: usize,
    /// Local-wins: the path already exists locally.
    pub skipped_local_exists: usize,
    /// A live generation row already covers the key (idempotent
    /// resume, or a normally-tracked file).
    pub skipped_known: usize,
    /// Crashed-import residue removed at resume.
    pub swept_temps: usize,
    pub swept_rows: usize,
    pub failed: usize,
}

impl ImportReport {
    pub fn did_anything(&self) -> bool {
        *self != ImportReport::default()
    }
}

/// Fresh = the tier has NO durable state at all — the DR/adopt boot
/// shape. Errors count as NOT fresh: never import on unknown state.
pub async fn state_is_fresh(backend: &Arc<dyn StateBackend>) -> bool {
    matches!(backend.tier_list_generations().await, Ok(v) if v.is_empty())
        && matches!(backend.tier_list_dirty().await, Ok(v) if v.is_empty())
        && matches!(backend.tier_list_evicted().await, Ok(v) if v.is_empty())
        && matches!(backend.tier_list_tombstones().await, Ok(v) if v.is_empty())
}

/// The start_tier driver: run the verb when a crashed import must be
/// resumed (intent note present), or when the state is FRESH and the
/// bucket holds non-control content (DR restore / bucket adopt).
pub async fn maybe_import_on_start(
    backend: &Arc<dyn StateBackend>,
    store: &Arc<dyn ObjectStore>,
    cfg: ImportConfig<'_>,
) -> Option<ImportReport> {
    let resume = cfg.intent_path.map(|p| p.exists()).unwrap_or(false);
    if !resume {
        if !state_is_fresh(backend).await {
            return None;
        }
        let control = format!("{}{}/", cfg.key_prefix, crate::tier::epoch::RESERVED_DIR);
        match store.list(cfg.key_prefix).await {
            Ok(objs) if objs.iter().any(|o| !o.key.starts_with(&control)) => {}
            Ok(_) => return None, // only our own control objects
            Err(e) => {
                warn!("tier import: bucket list failed ({}) — import skipped", e);
                return None;
            }
        }
        info!("tier import: fresh state + bucket content — restoring from the bucket");
    } else {
        warn!("tier import: resuming a crashed import (intent note present)");
    }
    Some(import_refresh(backend, store, cfg).await)
}

/// The import-refresh verb. Pre-listener only (see module docs).
pub async fn import_refresh(
    backend: &Arc<dyn StateBackend>,
    store: &Arc<dyn ObjectStore>,
    cfg: ImportConfig<'_>,
) -> ImportReport {
    let mut rep = ImportReport::default();
    let resume = cfg.intent_path.map(|p| p.exists()).unwrap_or(false);
    if let Some(p) = cfg.intent_path {
        if let Err(e) = std::fs::write(p, b"import\n") {
            warn!("tier import: cannot write intent note {}: {}", p.display(), e);
        }
    }
    if resume {
        sweep_crashed_import(backend, cfg.export_root, &mut rep).await;
    }

    // Context the guards consult (loaded once; nothing else mutates
    // rows pre-listener).
    let tombstoned: HashSet<String> = match backend.tier_list_tombstones().await {
        Ok(t) => t.into_iter().map(|t| t.key).collect(),
        Err(e) => {
            // Cannot see the tombstones ⇒ cannot honor A7: bail.
            warn!("tier import: cannot list tombstones ({}) — import refused", e);
            rep.failed += 1;
            return rep;
        }
    };
    let known_keys: HashSet<String> = match backend.tier_list_generations().await {
        Ok(rows) => rows.into_iter().map(|r| r.key).collect(),
        Err(e) => {
            warn!("tier import: cannot list generations ({}) — import refused", e);
            rep.failed += 1;
            return rep;
        }
    };

    // ── manifest lane ────────────────────────────────────────────────
    let mkey = manifest::manifest_key(cfg.key_prefix);
    let mut manifest_file_keys: HashSet<String> = HashSet::new();
    let mut created_dirs: Vec<(PathBuf, i64)> = Vec::new();
    match store.get_whole(&mkey, None).await {
        Ok((_, bytes)) => match Manifest::parse(&bytes) {
            Ok(mut m) => {
                info!(
                    "tier import: manifest seq {} — {} entries, {} beyond RPO at loss time",
                    m.seq,
                    m.entries.len(),
                    m.beyond_rpo
                );
                m.entries.sort_by(|a, z| a.path.cmp(&z.path)); // parents first
                for e in &m.entries {
                    let Some(rel) = safe_rel_path(&e.path) else {
                        warn!("tier import: manifest path {:?} refused", e.path);
                        rep.failed += 1;
                        continue;
                    };
                    let local = cfg.export_root.join(&rel);
                    match e.kind {
                        EntryKind::Dir => {
                            if local.symlink_metadata().is_ok() {
                                continue; // local wins
                            }
                            match std::fs::create_dir_all(&local) {
                                Ok(()) => {
                                    apply_posix(&local, e.mode, e.uid, e.gid, None);
                                    created_dirs.push((local, e.mtime_unix));
                                    rep.dirs_restored += 1;
                                }
                                Err(err) => {
                                    warn!("tier import: mkdir {}: {}", local.display(), err);
                                    rep.failed += 1;
                                }
                            }
                        }
                        EntryKind::Symlink => {
                            if local.symlink_metadata().is_ok() {
                                continue; // local wins
                            }
                            let target = e.target.clone().unwrap_or_default();
                            #[cfg(unix)]
                            let made = std::os::unix::fs::symlink(&target, &local);
                            #[cfg(not(unix))]
                            let made = Err::<(), std::io::Error>(std::io::Error::other(
                                "symlinks unsupported",
                            ));
                            match made {
                                Ok(()) => {
                                    lchown_best_effort(&local, e.uid, e.gid);
                                    let t = filetime::FileTime::from_unix_time(e.mtime_unix, 0);
                                    let _ = filetime::set_symlink_file_times(&local, t, t);
                                    rep.symlinks_restored += 1;
                                }
                                Err(err) => {
                                    warn!("tier import: symlink {}: {}", local.display(), err);
                                    rep.failed += 1;
                                }
                            }
                        }
                        EntryKind::File => {
                            let key = e
                                .key
                                .clone()
                                .unwrap_or_else(|| format!("{}{}", cfg.key_prefix, e.path));
                            manifest_file_keys.insert(key.clone());
                            let (Some(etag), Some(generation), Some(size)) =
                                (e.etag.clone(), e.generation, e.size)
                            else {
                                warn!("tier import: manifest file {} lacks etag/gen/size", e.path);
                                rep.failed += 1;
                                continue;
                            };
                            ingest_file(
                                backend,
                                &mut rep,
                                &tombstoned,
                                &known_keys,
                                &local,
                                Stub {
                                    key,
                                    generation,
                                    etag,
                                    crc64_b64: e.crc64_b64.clone().unwrap_or_default(),
                                    size,
                                    mode: e.mode,
                                    uid: e.uid,
                                    gid: e.gid,
                                    mtime_unix: e.mtime_unix,
                                },
                            )
                            .await;
                        }
                    }
                }
            }
            Err(e) => {
                warn!("tier import: manifest unparseable ({}) — sweep lane only", e);
            }
        },
        Err(StoreError::NotFound(_)) => {
            debug!("tier import: no manifest — sweep lane only");
        }
        Err(e) => {
            warn!("tier import: manifest read failed ({}) — sweep lane only", e);
        }
    }

    // ── sweep lane: bucket objects the manifest does not know ────────
    let control = format!("{}{}/", cfg.key_prefix, crate::tier::epoch::RESERVED_DIR);
    match store.list(cfg.key_prefix).await {
        Ok(objs) => {
            for o in objs {
                if o.key.starts_with(&control) || manifest_file_keys.contains(&o.key) {
                    continue;
                }
                let Some(rel) = o
                    .key
                    .strip_prefix(cfg.key_prefix)
                    .and_then(safe_rel_path)
                else {
                    warn!("tier import: bucket key {:?} refused (unsafe path)", o.key);
                    continue;
                };
                let local = cfg.export_root.join(&rel);
                // The cheap guards first — a HEAD costs money.
                if tombstoned.contains(&o.key) {
                    meter::bump(Counter::ImportSkippedTombstoned);
                    rep.skipped_tombstoned += 1;
                    debug!("tier import: {} tombstoned — NOT resurrected", o.key);
                    continue;
                }
                if known_keys.contains(&o.key) {
                    rep.skipped_known += 1;
                    continue;
                }
                if local.symlink_metadata().is_ok() {
                    rep.skipped_local_exists += 1;
                    continue;
                }
                let head = match store.head(&o.key).await {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("tier import: HEAD {}: {}", o.key, e);
                        rep.failed += 1;
                        continue;
                    }
                };
                let posix = PosixStamps::from_meta(&head.meta);
                let generation = crate::tier::store::GenerationStamps::from_meta(&head.meta)
                    .map(|s| s.generation)
                    .unwrap_or(1);
                if let Some(parent) = local.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        warn!("tier import: mkdir -p {}: {}", parent.display(), e);
                        rep.failed += 1;
                        continue;
                    }
                }
                ingest_file(
                    backend,
                    &mut rep,
                    &tombstoned,
                    &known_keys,
                    &local,
                    Stub {
                        key: o.key.clone(),
                        generation,
                        etag: head.etag.clone(),
                        crc64_b64: head.crc64_b64.clone().unwrap_or_default(),
                        size: head.size,
                        mode: posix.map_or(0o100644, |p| p.mode),
                        uid: posix.map_or_else(process_uid, |p| p.uid),
                        gid: posix.map_or_else(process_gid, |p| p.gid),
                        mtime_unix: posix.map_or_else(
                            || head.last_modified_unix.unwrap_or(0) as i64,
                            |p| p.mtime_unix,
                        ),
                    },
                )
                .await;
            }
        }
        Err(e) => {
            warn!("tier import: bucket list failed ({}) — sweep lane skipped", e);
            rep.failed += 1;
        }
    }

    // Directory mtimes LAST, deepest first — creating children touched
    // every parent we made.
    created_dirs.sort_by(|a, z| z.0.cmp(&a.0));
    for (d, mtime) in &created_dirs {
        let t = filetime::FileTime::from_unix_time(*mtime, 0);
        let _ = filetime::set_file_mtime(d, t);
    }

    if let Some(p) = cfg.intent_path {
        let _ = std::fs::remove_file(p);
    }
    if rep.did_anything() {
        info!(
            "tier import: {} dir(s), {} symlink(s), {} stub(s); skipped {} tombstoned, \
             {} local-wins, {} known; {} failed",
            rep.dirs_restored,
            rep.symlinks_restored,
            rep.stubs_created,
            rep.skipped_tombstoned,
            rep.skipped_local_exists,
            rep.skipped_known,
            rep.failed
        );
    }
    rep
}

/// One evicted-stub materialization: temp file with final metadata →
/// durable rows keyed by its identity → marker xattr → rename into
/// place. The consult-map marker is NOT inserted here — the caller
/// runs `evict::reconcile` after the import (start_tier's existing
/// order), which loads every marker from the rows.
struct Stub {
    key: String,
    generation: u64,
    etag: String,
    /// Empty = unknown (a foreign object without our checksum);
    /// hydration then adopts the computed CRC (step 11).
    crc64_b64: String,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_unix: i64,
}

async fn ingest_file(
    backend: &Arc<dyn StateBackend>,
    rep: &mut ImportReport,
    tombstoned: &HashSet<String>,
    known_keys: &HashSet<String>,
    local: &Path,
    stub: Stub,
) {
    // A7: NEVER resurrect a tombstoned key.
    if tombstoned.contains(&stub.key) {
        meter::bump(Counter::ImportSkippedTombstoned);
        rep.skipped_tombstoned += 1;
        debug!("tier import: {} tombstoned — NOT resurrected", stub.key);
        return;
    }
    if known_keys.contains(&stub.key) {
        rep.skipped_known += 1;
        return;
    }
    if local.symlink_metadata().is_ok() {
        rep.skipped_local_exists += 1;
        return;
    }
    let Some(parent) = local.parent() else {
        rep.failed += 1;
        return;
    };
    let tmp = parent.join(format!("{}{}", IMPORT_TMP_PREFIX, uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
        warn!("tier import: temp create {}: {}", tmp.display(), e);
        rep.failed += 1;
        return;
    }
    apply_posix(&tmp, stub.mode, stub.uid, stub.gid, Some(stub.mtime_unix));
    #[cfg(unix)]
    let identity = tmp.symlink_metadata().ok().map(|m| {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    });
    #[cfg(not(unix))]
    let identity: Option<(u64, u64)> = None;
    let Some((dev, ino)) = identity else {
        let _ = std::fs::remove_file(&tmp);
        rep.failed += 1;
        return;
    };
    let now = now_unix();
    let erow = TierEvictedRow {
        dev,
        ino,
        key: stub.key.clone(),
        generation: stub.generation,
        etag: stub.etag.clone(),
        crc64_b64: stub.crc64_b64.clone(),
        size: stub.size,
        // The FINAL path from the start: a crash before the rename
        // leaves a row whose path never materialized — exactly what
        // the resume sweep deletes.
        path: local.to_string_lossy().into_owned(),
        evicted_unix: now,
        hydrating_unix: None,
    };
    let grow = TierGenerationRow {
        dev,
        ino,
        key: stub.key.clone(),
        generation: stub.generation,
        etag: stub.etag.clone(),
        crc64_b64: if stub.crc64_b64.is_empty() { None } else { Some(stub.crc64_b64.clone()) },
        size: stub.size,
        // Unknown provenance/storage class: never vouched for as a
        // server-side copy source (A11).
        copy_allowed: false,
        updated_unix: now,
    };
    let rows = async {
        backend.tier_put_evicted(&erow).await.map_err(|e| format!("evicted row: {}", e))?;
        backend.tier_upsert_generation(&grow).await.map_err(|e| format!("gen row: {}", e))
    }
    .await;
    if let Err(e) = rows {
        warn!("tier import: rows for {}: {} — stub abandoned", stub.key, e);
        let _ = backend.tier_delete_evicted(dev, ino).await;
        let _ = backend.tier_delete_generation(dev, ino).await;
        let _ = std::fs::remove_file(&tmp);
        rep.failed += 1;
        return;
    }
    let _ = crate::tier::evict::set_xattr(
        &tmp,
        crate::tier::evict::EVICTED_XATTR,
        format!("{}:{}", stub.generation, stub.etag).as_bytes(),
    );
    if let Err(e) = std::fs::rename(&tmp, local) {
        warn!("tier import: rename into {}: {}", local.display(), e);
        let _ = backend.tier_delete_evicted(dev, ino).await;
        let _ = backend.tier_delete_generation(dev, ino).await;
        let _ = std::fs::remove_file(&tmp);
        rep.failed += 1;
        return;
    }
    meter::bump(Counter::ImportStubs);
    rep.stubs_created += 1;
    debug!("tier import: {} ← {} ({} bytes, gen {})", local.display(), stub.key, stub.size, stub.generation);
}

/// Resume hygiene: delete stray `.flint-import.*` temps, then rows
/// whose path never materialized (identity no longer resolves) — the
/// crashed-import residue the reconciler's orphan arm defers to us.
async fn sweep_crashed_import(
    backend: &Arc<dyn StateBackend>,
    export_root: &Path,
    rep: &mut ImportReport,
) {
    fn sweep_temps(dir: &Path, rep: &mut ImportReport) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy().into_owned();
            let p = ent.path();
            let Ok(md) = p.symlink_metadata() else { continue };
            if md.is_dir() {
                if name != crate::tier::epoch::RESERVED_DIR {
                    sweep_temps(&p, rep);
                }
            } else if name.starts_with(IMPORT_TMP_PREFIX) && std::fs::remove_file(&p).is_ok() {
                rep.swept_temps += 1;
            }
        }
    }
    sweep_temps(export_root, rep);
    if let Ok(rows) = backend.tier_list_evicted().await {
        for row in rows {
            #[cfg(unix)]
            let resolves = std::fs::symlink_metadata(&row.path)
                .map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.dev(), m.ino()) == (row.dev, row.ino)
                })
                .unwrap_or(false);
            #[cfg(not(unix))]
            let resolves = false;
            if !resolves {
                warn!(
                    "tier import resume: row for {} never materialized — swept",
                    row.path
                );
                let _ = backend.tier_delete_evicted(row.dev, row.ino).await;
                let _ = backend.tier_delete_generation(row.dev, row.ino).await;
                rep.swept_rows += 1;
            }
        }
    }
}

/// Reject absolute paths, `..`, `.` and the reserved namespace — a
/// manifest and bucket listing are DATA, not trusted input.
fn safe_rel_path(p: &str) -> Option<PathBuf> {
    if p.is_empty() {
        return None;
    }
    let pb = PathBuf::from(p);
    let mut first = true;
    for c in pb.components() {
        match c {
            Component::Normal(seg) => {
                if first && seg == crate::tier::epoch::RESERVED_DIR {
                    return None;
                }
                first = false;
            }
            _ => return None,
        }
    }
    Some(pb)
}

fn apply_posix(path: &Path, mode: u32, uid: u32, gid: u32, mtime: Option<i64>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777));
        // Best-effort (non-root hubs cannot chown; the manifest still
        // records the truth).
        if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            unsafe { libc::chown(c.as_ptr(), uid, gid) };
        }
    }
    #[cfg(not(unix))]
    let _ = (mode, uid, gid);
    if let Some(m) = mtime {
        let t = filetime::FileTime::from_unix_time(m, 0);
        let _ = filetime::set_file_mtime(path, t);
    }
}

fn lchown_best_effort(path: &Path, uid: u32, gid: u32) {
    #[cfg(unix)]
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    }
    #[cfg(not(unix))]
    let _ = (path, uid, gid);
}

fn process_uid() -> u32 {
    #[cfg(unix)]
    unsafe {
        libc::geteuid()
    }
    #[cfg(not(unix))]
    0
}

fn process_gid() -> u32 {
    #[cfg(unix)]
    unsafe {
        libc::getegid()
    }
    #[cfg(not(unix))]
    0
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
    use crate::tier::evict;
    use crate::tier::flush::{FlushConfig, FlushOrchestrator};
    use crate::tier::store::memory::MemoryStore;
    use bytes::Bytes;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

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
        cfg.whole_put_max = 1024 * 1024;
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

    fn set_mode_mtime(path: &Path, mode: u32, mtime: i64) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(mtime, 0)).unwrap();
    }

    /// THE step-12 closing drill: publish a tree, DESTROY the PVC
    /// (fresh export dir + fresh state backend — only the bucket
    /// survives), rebuild by import, and assert POSIX fidelity against
    /// the manifest, with the RPO readable from the bucket alone.
    #[tokio::test]
    async fn dr_drill_full_rebuild_from_the_bucket_alone() {
        let r = rig();
        // ── the tree ─────────────────────────────────────────────────
        let model: Vec<u8> = (0..1500u32).map(|i| (i % 249) as u8).collect();
        std::fs::create_dir(r.root.join("data")).unwrap();
        std::fs::write(r.root.join("data/model.bin"), &model).unwrap();
        std::fs::write(r.root.join("notes.txt"), b"remember the milk").unwrap();
        std::os::unix::fs::symlink("data/model.bin", r.root.join("latest")).unwrap();
        set_mode_mtime(&r.root.join("data/model.bin"), 0o640, 1_700_000_100);
        set_mode_mtime(&r.root.join("notes.txt"), 0o644, 1_700_000_200);
        set_mode_mtime(&r.root.join("data"), 0o750, 1_700_000_300);

        for f in ["data/model.bin", "notes.txt"] {
            let p = r.root.join(f);
            let (dev, ino) = ident(&p);
            capture::forget(dev, ino);
            note_and_land(&r, &p, Mutation::Whole).await;
        }
        let report = r.orch.tick().await;
        assert_eq!(report.published, 2, "both files must publish");

        // ── the RPO is readable from the BUCKET ALONE ────────────────
        let (_, mbytes) = r
            .mem
            .get_whole(&manifest::manifest_key("t/"), None)
            .await
            .expect("the barrier must have written a manifest");
        let m = manifest::Manifest::parse(&mbytes).unwrap();
        assert!(m.seq >= 1);
        assert_eq!(m.beyond_rpo, 0);
        let paths: Vec<&str> = m.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["data", "data/model.bin", "latest", "notes.txt"]);
        let fm = m.entries.iter().find(|e| e.path == "data/model.bin").unwrap();
        assert_eq!(fm.size, Some(1500));
        assert!(fm.etag.is_some() && fm.crc64_b64.is_some() && fm.generation.is_some());
        assert_eq!(fm.mode & 0o7777, 0o640);
        assert_eq!(fm.mtime_unix, 1_700_000_100);
        // The published objects carry the A12 posix stamps too.
        let head = r.mem.head(fm.key.as_deref().unwrap()).await.unwrap();
        let ps = PosixStamps::from_meta(&head.meta).expect("posix stamps ride the object");
        assert_eq!(ps.mode & 0o7777, 0o640);
        assert_eq!(ps.mtime_unix, 1_700_000_100);

        // ── DESTROY the PVC: export + state both gone ────────────────
        let dir_b = tempfile::TempDir::new().unwrap();
        let root_b = dir_b.path().to_path_buf();
        let backend_b: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let store_b: Arc<dyn ObjectStore> = r.mem.clone();
        let intent = root_b.join("..").join("flint-import-intent-drill");

        let rep = maybe_import_on_start(
            &backend_b,
            &store_b,
            ImportConfig { export_root: &root_b, key_prefix: "t/", intent_path: Some(&intent) },
        )
        .await
        .expect("fresh state + bucket content must import");
        assert_eq!(
            (rep.dirs_restored, rep.symlinks_restored, rep.stubs_created, rep.failed),
            (1, 1, 2, 0),
            "{:?}",
            rep
        );
        assert!(!intent.exists(), "intent note must clear on completion");

        // Markers load exactly the way start_tier does it.
        let er = evict::reconcile(&backend_b).await;
        assert_eq!(er.loaded, 2, "{:?}", er);

        // ── POSIX fidelity vs the manifest ───────────────────────────
        let md = std::fs::symlink_metadata(root_b.join("data")).unwrap();
        assert!(md.is_dir());
        assert_eq!(md.permissions().mode() & 0o7777, 0o750);
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(md.mtime(), 1_700_000_300, "dir mtime applied AFTER children");
        }
        let lt = std::fs::read_link(root_b.join("latest")).unwrap();
        assert_eq!(lt.to_string_lossy(), "data/model.bin");
        for (f, mode, mtime, size) in [
            ("data/model.bin", 0o640u32, 1_700_000_100i64, 1500u64),
            ("notes.txt", 0o644, 1_700_000_200, 17),
        ] {
            let p = root_b.join(f);
            let md = std::fs::symlink_metadata(&p).unwrap();
            use std::os::unix::fs::MetadataExt;
            assert_eq!(md.len(), 0, "{} is a stub", f);
            assert_eq!(md.permissions().mode() & 0o7777, mode);
            assert_eq!(md.mtime(), mtime);
            let (dev, ino) = (md.dev(), md.ino());
            assert!(evict::is_evicted(dev, ino));
            assert_eq!(evict::logical_size(dev, ino), Some(size), "GETATTR serves logical size");
        }

        // ── content comes back through hydration, byte-identical ─────
        let p = root_b.join("data/model.bin");
        let (dev, ino) = ident(&p);
        let h = crate::tier::hydrate::local_for_tests(backend_b.clone(), store_b.clone(), 2);
        let n = crate::tier::hydrate::restore_once(&h, dev, ino, &p)
            .await
            .expect("hydration must restore the stub");
        assert_eq!(n, 1500);
        assert_eq!(std::fs::read(&p).unwrap(), model, "restored byte-identical");
        assert_eq!(ident(&p), (dev, ino), "restored IN PLACE");
        {
            use std::os::unix::fs::MetadataExt;
            let md = std::fs::symlink_metadata(&p).unwrap();
            assert_eq!(md.mtime(), 1_700_000_100, "hydration is not a modification");
        }

        // ── idempotency: a second refresh adds nothing ───────────────
        let rep2 = import_refresh(
            &backend_b,
            &store_b,
            ImportConfig { export_root: &root_b, key_prefix: "t/", intent_path: None },
        )
        .await;
        assert_eq!(rep2.stubs_created, 0);
        assert_eq!(rep2.dirs_restored, 0);
        assert_eq!(rep2.symlinks_restored, 0);
        assert!(rep2.skipped_known >= 1, "{:?}", rep2);
    }

    /// A7's guard: a tombstoned key is NEVER re-ingested — before the
    /// barrier consumes the tombstone the object still exists in the
    /// bucket, and importing it would resurrect a removed file.
    #[tokio::test]
    async fn import_never_resurrects_a_tombstoned_key() {
        let r = rig();
        r.mem.raw_put("t/ghost.bin", Bytes::from_static(b"i was removed"), vec![]);
        r.backend
            .tier_put_tombstone(&crate::state_backend::TierTombstone {
                key: "t/ghost.bin".into(),
                etag: None,
                created_unix: 1,
            })
            .await
            .unwrap();

        let rep = import_refresh(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            ImportConfig { export_root: &r.root, key_prefix: "t/", intent_path: None },
        )
        .await;
        assert_eq!(rep.skipped_tombstoned, 1, "{:?}", rep);
        assert_eq!(rep.stubs_created, 0);
        assert!(!r.root.join("ghost.bin").exists(), "the ghost must NOT come back");

        // The barrier consumes the tombstone (object deleted) — the
        // next refresh finds nothing to ingest at that key.
        r.orch.consume_tombstones().await;
        assert!(matches!(
            r.mem.head("t/ghost.bin").await,
            Err(crate::tier::store::StoreError::NotFound(_))
        ));
        let rep2 = import_refresh(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            ImportConfig { export_root: &r.root, key_prefix: "t/", intent_path: None },
        )
        .await;
        assert_eq!(rep2.skipped_tombstoned, 0);
        assert_eq!(rep2.stubs_created, 0);
        assert!(!r.root.join("ghost.bin").exists());
    }

    /// Local wins on existing paths; a stamp-less foreign object
    /// ingests with default metadata and hydrates through the
    /// empty-CRC adopt lane.
    #[tokio::test]
    async fn import_local_wins_and_foreign_objects_ingest_with_defaults() {
        let r = rig();
        std::fs::write(r.root.join("existing.txt"), b"local truth").unwrap();
        r.mem.raw_put("t/existing.txt", Bytes::from_static(b"bucket impostor"), vec![]);
        let foreign: Vec<u8> = (0..900u32).map(|i| (i % 97) as u8).collect();
        r.mem.raw_put("t/pre/loaded.bin", Bytes::from(foreign.clone()), vec![]);

        let rep = import_refresh(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            ImportConfig { export_root: &r.root, key_prefix: "t/", intent_path: None },
        )
        .await;
        assert_eq!(rep.skipped_local_exists, 1, "{:?}", rep);
        assert_eq!(rep.stubs_created, 1);
        assert_eq!(
            std::fs::read(r.root.join("existing.txt")).unwrap(),
            b"local truth",
            "local file untouched"
        );

        let p = r.root.join("pre/loaded.bin");
        let md = std::fs::symlink_metadata(&p).unwrap();
        assert_eq!(md.len(), 0);
        assert_eq!(md.permissions().mode() & 0o7777, 0o644, "stamp-less default mode");
        let er = evict::reconcile(&r.backend).await;
        assert!(er.loaded >= 1, "{:?}", er);
        let (dev, ino) = ident(&p);
        assert_eq!(evict::logical_size(dev, ino), Some(900));
        // Empty recorded CRC (unknown provenance) — hydration adopts
        // the computed one.
        let h = crate::tier::hydrate::local_for_tests(
            r.backend.clone(),
            r.mem.clone() as Arc<dyn ObjectStore>,
            2,
        );
        let n = crate::tier::hydrate::restore_once(&h, dev, ino, &p).await.unwrap();
        assert_eq!(n, 900);
        assert_eq!(std::fs::read(&p).unwrap(), foreign);
    }

    /// Crash-resume: stray temps are swept, rows whose path never
    /// materialized are deleted, and the interrupted key imports
    /// cleanly on the re-run.
    #[tokio::test]
    async fn crashed_import_resume_sweeps_residue_and_completes() {
        let r = rig();
        let content: Vec<u8> = (0..640u32).map(|i| (i % 61) as u8).collect();
        let meta = r.mem.raw_put("t/redo.bin", Bytes::from(content), vec![]);

        // The crash residue: a stray temp, rows for a path that never
        // materialized (bogus identity), and the intent note.
        std::fs::write(r.root.join(".flint-import.deadbeef"), b"").unwrap();
        let never = r.root.join("redo.bin");
        r.backend
            .tier_put_evicted(&crate::state_backend::TierEvictedRow {
                dev: 999_999,
                ino: 888_888,
                key: "t/redo.bin".into(),
                generation: 1,
                etag: meta.etag.clone(),
                crc64_b64: String::new(),
                size: 640,
                path: never.to_string_lossy().into_owned(),
                evicted_unix: 1,
                hydrating_unix: None,
            })
            .await
            .unwrap();
        r.backend
            .tier_upsert_generation(&crate::state_backend::TierGenerationRow {
                dev: 999_999,
                ino: 888_888,
                key: "t/redo.bin".into(),
                generation: 1,
                etag: meta.etag.clone(),
                crc64_b64: None,
                size: 640,
                copy_allowed: false,
                updated_unix: 1,
            })
            .await
            .unwrap();
        let intent = r.root.join("..").join("flint-import-intent-crash");
        std::fs::write(&intent, b"import\n").unwrap();

        let rep = maybe_import_on_start(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            ImportConfig { export_root: &r.root, key_prefix: "t/", intent_path: Some(&intent) },
        )
        .await
        .expect("intent note must force a resume even on non-fresh state");
        assert_eq!(rep.swept_temps, 1, "{:?}", rep);
        assert_eq!(rep.swept_rows, 1);
        assert_eq!(rep.stubs_created, 1);
        assert!(!intent.exists());
        assert!(!r.root.join(".flint-import.deadbeef").exists());

        // The re-imported stub is fully consistent: rows match the
        // REAL identity now.
        let (dev, ino) = ident(&never);
        let rows = r.backend.tier_list_evicted().await.unwrap();
        let row = rows.iter().find(|x| x.key == "t/redo.bin").unwrap();
        assert_eq!((row.dev, row.ino), (dev, ino));
        assert_eq!(row.size, 640);
    }
}
