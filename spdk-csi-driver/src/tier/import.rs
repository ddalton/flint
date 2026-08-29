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
use crate::tier::store::{ObjectStore, PosixStamps};
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
    /// Durable "a foreign-key sweep is owed" note, written before the
    /// manifest lane returns and removed only when a sweep completes.
    ///
    /// Without it the sweep is lost forever the first time a hub is
    /// suspended or restarted in the middle of one: the next start
    /// finds no import intent (the manifest lane finished and cleared
    /// it) and non-fresh state (the manifest lane placed rows), so
    /// `maybe_import_on_start` returns None and the remaining foreign
    /// keys are never ingested. On a 200k-object bucket that is a
    /// silent partial restore.
    pub sweep_note_path: Option<&'a Path>,
}

/// Why a placement failed, and the whole reason the reports carry two
/// counters instead of one.
///
/// A per-key defect fails identically on every retry, so discarding the
/// work is correct. A RETRYABLE failure is a property of the
/// environment at that instant — a full volume, an exhausted inode
/// table, a throttled bucket, a backend that blinked — and it is not
/// confined to the key that hit it: every key after it failed for the
/// same reason. Treating those as per-key is what turns a transient
/// condition into a permanent one, because the durable "work is owed"
/// note is cleared and nothing ever looks at the bucket again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// The key/entry itself is unusable. Retrying changes nothing.
    PerKey,
    /// The environment refused. A later pass may well succeed.
    Retryable,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
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
    /// Of `failed`, those that failed on a retryable condition rather
    /// than a per-key defect. See [`FailKind`]. Nonzero means the tree
    /// this report describes is INCOMPLETE THROUGH NO FAULT OF THE
    /// DATA, and must not be believed.
    pub failed_retryable: usize,
}

impl ImportReport {
    pub fn did_anything(&self) -> bool {
        *self != ImportReport::default()
    }
    /// Count a failure and its kind together, so no call site can
    /// record one without the other.
    pub fn fail(&mut self, kind: FailKind) {
        self.failed += 1;
        if kind == FailKind::Retryable {
            self.failed_retryable += 1;
        }
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

/// What `maybe_import_on_start` decided, and what the caller still owes.
pub struct ImportOutcome {
    pub report: Option<ImportReport>,
    /// True when a foreign-key sweep must run once the listener is up.
    pub sweep_owed: bool,
    /// Set when the import was REFUSED on a manifest the bucket has but
    /// we could not read. Surfaced as a loud condition, and the intent
    /// note is deliberately left in place so the next start retries.
    pub refused: Option<String>,
}

/// The start_tier driver: run the manifest lane when a crashed import
/// must be resumed (intent note present), or when the state is FRESH and
/// the bucket holds content (DR restore / bucket adopt).
///
/// `seed` is what the orchestrator's startup already read. Its three
/// arms decide three different things:
///
/// - **Present** — restore, and no prefix LIST is needed to know there
///   is content: the manifest says so. (One GET total for a DR boot,
///   down from two GETs and two LISTs.)
/// - **Absent** — no manifest object exists. Only then is a LIST needed,
///   to tell an empty bucket from one holding foreign data to adopt.
/// - **Unreadable** — the object EXISTS and we could not use it. On
///   fresh state this must REFUSE: importing anyway would serve a tree
///   with every directory, symlink, mode and owner missing, because
///   only the manifest carries them, and the sweep would then publish
///   that impoverished tree back over the real one. Serving nothing is
///   recoverable; serving a wrong tree that overwrites the right one is
///   not.
pub async fn maybe_import_on_start(
    backend: &Arc<dyn StateBackend>,
    store: &Arc<dyn ObjectStore>,
    seed: &manifest::ManifestSeed,
    cfg: ImportConfig<'_>,
) -> ImportOutcome {
    let none = |sweep_owed: bool| ImportOutcome { report: None, sweep_owed, refused: None };
    let resume = cfg.intent_path.map(|p| p.exists()).unwrap_or(false);
    // A sweep interrupted by a restart is owed regardless of freshness —
    // by then the manifest lane has placed rows, so the state is not
    // fresh and every other gate below would decline.
    let sweep_pending = cfg.sweep_note_path.map(|p| p.exists()).unwrap_or(false);

    let doc = match seed {
        manifest::ManifestSeed::Present(m, _) => Some(m.as_ref()),
        manifest::ManifestSeed::Absent => None,
        manifest::ManifestSeed::Unreadable(why, _) => {
            if state_is_fresh(backend).await {
                // Leave the intent note: this is a retryable condition,
                // and clearing it would turn a transient bucket error
                // into a permanent one.
                return ImportOutcome {
                    report: None,
                    sweep_owed: sweep_pending,
                    refused: Some(why.clone()),
                };
            }
            warn!(
                "tier import: the bucket's manifest is unusable ({}) — the local tree is \
                 already established, so this is logged and skipped rather than refused",
                why
            );
            None
        }
    };

    if !resume {
        if !state_is_fresh(backend).await {
            return none(sweep_pending);
        }
        if doc.is_none() {
            // No manifest ⇒ the only way to tell an empty bucket from
            // one holding foreign data is to look.
            let control = format!("{}{}/", cfg.key_prefix, crate::tier::epoch::RESERVED_DIR);
            match store.list(cfg.key_prefix).await {
                Ok(objs) if objs.iter().any(|o| !o.key.starts_with(&control)) => {}
                Ok(_) => return none(false), // only our own control objects
                Err(e) => {
                    warn!("tier import: bucket list failed ({}) — import skipped", e);
                    return none(sweep_pending);
                }
            }
        }
        info!("tier import: fresh state + bucket content — restoring from the bucket");
    } else {
        warn!("tier import: resuming a crashed import (intent note present)");
    }
    let report = import_refresh(backend, doc, cfg).await;
    ImportOutcome { report: Some(report), sweep_owed: true, refused: None }
}

/// The import-refresh verb — the MANIFEST lane, pre-listener.
///
/// The foreign-key sweep used to run here too. It moved to
/// [`sweep_foreign`], which runs AFTER the listener binds, because the
/// two lanes have opposite cost profiles: the manifest lane is one GET
/// and rebuilds the whole namespace, while the sweep is a full prefix
/// LIST plus a HEAD per unknown object. Making every client wait out
/// the second to get the first is minutes of unavailability buying
/// nothing — the tree the manifest describes is already complete and
/// correct, and foreign keys are by definition objects no flint hub
/// published.
///
/// `manifest` is the document the orchestrator's startup seed already
/// read; this function never GETs it again.
pub async fn import_refresh(
    backend: &Arc<dyn StateBackend>,
    manifest: Option<&Manifest>,
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
            rep.fail(FailKind::Retryable);
            return rep;
        }
    };
    let known_keys: HashSet<String> = match backend.tier_list_generations().await {
        Ok(rows) => rows.into_iter().map(|r| r.key).collect(),
        Err(e) => {
            warn!("tier import: cannot list generations ({}) — import refused", e);
            rep.fail(FailKind::Retryable);
            return rep;
        }
    };

    // ── manifest lane ────────────────────────────────────────────────
    let mut created_dirs: Vec<(PathBuf, i64)> = Vec::new();
    let mut pending = Pending::default();
    match manifest {
        Some(m) => {
            {
                let mut m = m.clone();
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
                        rep.fail(FailKind::PerKey);
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
                                    rep.fail(FailKind::Retryable);
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
                                    rep.fail(FailKind::Retryable);
                                }
                            }
                        }
                        EntryKind::File => {
                            let key = e
                                .key
                                .clone()
                                .unwrap_or_else(|| format!("{}{}", cfg.key_prefix, e.path));
                            let (Some(etag), Some(generation), Some(size)) =
                                (e.etag.clone(), e.generation, e.size)
                            else {
                                warn!("tier import: manifest file {} lacks etag/gen/size", e.path);
                                rep.fail(FailKind::PerKey);
                                continue;
                            };
                            if !admissible(&mut rep, &tombstoned, &known_keys, &local, &key) {
                                continue;
                            }
                            let staged = stage_stub(
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
                            );
                            let (st, erow, grow) = match staged {
                                Ok(v) => v,
                                Err(kind) => {
                                    rep.fail(kind);
                                    continue;
                                }
                            };
                            pending.staged.push(st);
                            pending.rows.push((erow, grow));
                            if pending.len() >= INGEST_CHUNK {
                                pending
                                    .flush(backend, Placement::Quiescent, &mut rep)
                                    .await;
                            }
                        }
                    }
                }
            }
        }
        None => {
            debug!("tier import: no manifest — foreign-key sweep only");
        }
    }

    pending.flush(backend, Placement::Quiescent, &mut rep).await;

    // Directory mtimes LAST, deepest first — creating children touched
    // every parent we made.
    created_dirs.sort_by(|a, z| z.0.cmp(&a.0));
    for (d, mtime) in &created_dirs {
        let t = filetime::FileTime::from_unix_time(*mtime, 0);
        let _ = filetime::set_file_mtime(d, t);
    }

    // The sweep is OWED from here on. Written before the intent note is
    // cleared, so a crash in the gap resumes both rather than neither.
    if let Some(p) = cfg.sweep_note_path {
        if let Err(e) = std::fs::write(p, b"sweep\n") {
            warn!(
                "tier import: cannot write sweep note {}: {} — a foreign-key sweep \
                 interrupted by a restart will be lost",
                p.display(),
                e
            );
        }
    }
    // Same rule as the sweep note, one lane earlier. Clearing the
    // intent note on a retryably-incomplete walk is strictly worse than
    // clearing the sweep note: the sweep can re-adopt a missing FILE as
    // a foreign key, but directories, symlinks and mode/uid/gid live
    // only in the manifest, so nothing else in the system can restore
    // them. Keeping the note re-runs this lane on the next start, which
    // is the one path that can.
    if rep.failed_retryable == 0 {
        if let Some(p) = cfg.intent_path {
            let _ = std::fs::remove_file(p);
        }
    } else {
        warn!(
            "tier import: {} entry/entries failed on a retryable condition — the intent note \
             is KEPT so the next start re-runs the manifest lane",
            rep.failed_retryable
        );
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

/// When the stub becomes visible, which decides what must be true
/// before it does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placement {
    /// Pre-listener. No client exists yet, and `evict::reconcile` runs
    /// before the listener binds and loads every marker from the rows.
    /// A plain rename is safe because nothing else is writing the tree.
    Quiescent,
    /// Behind a live listener. The name is visible the instant it is
    /// linked, so the marker must already be installed and the link must
    /// not replace anything a client just created.
    Live,
}

/// A stub built on disk but not yet durable and not yet named.
struct Staged {
    tmp: PathBuf,
    local: PathBuf,
    dev: u64,
    ino: u64,
    stub: Stub,
}

/// Phase 1: create the temp, stamp it with the final metadata, and
/// build the two durable rows. Nothing is written to the database and
/// no name exists yet, so abandoning a staged stub costs one unlink.
fn stage_stub(
    local: &Path,
    stub: Stub,
) -> Result<(Staged, TierEvictedRow, TierGenerationRow), FailKind> {
    // A path with no parent is a defect in the key; the create below
    // failing is the volume talking. Only the first is per-key.
    let parent = local.parent().ok_or(FailKind::PerKey)?;
    let tmp = parent.join(format!("{}{}", IMPORT_TMP_PREFIX, uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
        // ENOSPC here is the inode table as often as it is the bytes:
        // a stub is zero-length, so a volume with gigabytes free still
        // refuses once `df -i` reads 100%.
        warn!("tier import: temp create {}: {}", tmp.display(), e);
        return Err(FailKind::Retryable);
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
        return Err(FailKind::Retryable);
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
        // The FINAL path from the start: a crash before the link
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
    Ok((
        Staged { tmp, local: local.to_path_buf(), dev, ino, stub },
        erow,
        grow,
    ))
}

/// Phase 2: rows are durable, so install the marker (live placement
/// only), stamp the xattr, and give the inode its real name.
///
/// The order inside this function is the whole safety argument for
/// running a sweep behind a live listener — see
/// [`crate::tier::evict::insert_marker_for_import`].
async fn place_stub(
    backend: &Arc<dyn StateBackend>,
    st: Staged,
    placement: Placement,
    rep: &mut ImportReport,
) {
    let Staged { tmp, local, dev, ino, stub } = st;
    // From here every failure path must also drop the marker. A marker
    // surviving on a freed inode NUMBER is its own data-loss vector once
    // the kernel reuses it: an unrelated new file would read as evicted
    // and hydrate S3 bytes over itself.
    let mut rollback = Rollback { backend, dev, ino, marker: false, tmp: Some(tmp.clone()) };

    if placement == Placement::Live {
        crate::tier::evict::insert_marker_for_import(
            dev,
            ino,
            crate::tier::evict::EvictedMeta {
                size: stub.size,
                key: stub.key.clone(),
                generation: stub.generation,
                etag: stub.etag.clone(),
                crc64_b64: stub.crc64_b64.clone(),
            },
        );
        rollback.marker = true;
    }

    let _ = crate::tier::evict::set_xattr(
        &tmp,
        crate::tier::evict::EVICTED_XATTR,
        format!("{}:{}", stub.generation, stub.etag).as_bytes(),
    );

    let placed = match placement {
        // Nothing else is writing the tree.
        Placement::Quiescent => std::fs::rename(&tmp, &local),
        // A client may have created this very name while the HEAD was
        // in flight. `rename` REPLACES, which would silently clobber it
        // — and clobber it with a STUB, so the client's bytes would be
        // gone and every later read would serve bucket content that
        // never held them. hard_link fails with EEXIST instead, and
        // local wins, which is the import's standing rule.
        Placement::Live => std::fs::hard_link(&tmp, &local),
    };
    match placed {
        Ok(()) => {}
        Err(e) if placement == Placement::Live && e.kind() == std::io::ErrorKind::AlreadyExists => {
            debug!("tier import: {} appeared locally during the sweep — local wins", local.display());
            rollback.run().await;
            rep.skipped_local_exists += 1;
            return;
        }
        Err(e) => {
            warn!("tier import: place {}: {}", local.display(), e);
            rollback.run().await;
            rep.fail(FailKind::Retryable);
            return;
        }
    }
    if placement == Placement::Live {
        // The link succeeded, so the inode has two names; drop the temp
        // one. The marker and rows key on (dev,ino), which the
        // surviving name still carries.
        let _ = std::fs::remove_file(&tmp);
    }
    rollback.disarm();
    meter::bump(Counter::ImportStubs);
    rep.stubs_created += 1;
    debug!("tier import: {} ← {} ({} bytes, gen {})", local.display(), stub.key, stub.size, stub.generation);
}

/// Stubs per durable batch. Big enough that the writer-thread barrier
/// amortizes, small enough that an interrupted sweep loses at most this
/// much staged work and that the temps do not pile up.
const INGEST_CHUNK: usize = 256;

/// Stubs staged since the last flush, plus their rows.
#[derive(Default)]
struct Pending {
    staged: Vec<Staged>,
    rows: Vec<(TierEvictedRow, TierGenerationRow)>,
}

impl Pending {
    fn len(&self) -> usize {
        self.staged.len()
    }

    /// Commit the batch: ONE database write for every row, then place
    /// each stub. Both per-row backend calls are barriers on the sqlite
    /// writer thread, so a large import would otherwise pay two
    /// uncoalescable round trips per file — while, in the sweep's case,
    /// competing with live clients for that same writer.
    async fn flush(
        &mut self,
        backend: &Arc<dyn StateBackend>,
        placement: Placement,
        rep: &mut ImportReport,
    ) {
        if self.staged.is_empty() {
            return;
        }
        let staged = std::mem::take(&mut self.staged);
        let rows = std::mem::take(&mut self.rows);
        if let Err(e) = backend.tier_ingest_batch(&rows).await {
            // Partial failure is failure: a stub with one of its two
            // rows is exactly the inconsistency the reconciler exists
            // to clean up, so drop the whole chunk rather than leave
            // one behind. No marker was installed and no name exists,
            // so this really is just unlinks.
            warn!("tier import: row batch of {} failed ({}) — chunk abandoned", rows.len(), e);
            for st in &staged {
                let _ = backend.tier_delete_evicted(st.dev, st.ino).await;
                let _ = backend.tier_delete_generation(st.dev, st.ino).await;
                let _ = std::fs::remove_file(&st.tmp);
            }
            rep.failed += staged.len();
            return;
        }
        for st in staged {
            place_stub(backend, st, placement, rep).await;
        }
    }
}

/// Everything that must be true before an object is worth staging.
/// Returns false (and counts the skip) when it is not.
fn admissible(
    rep: &mut ImportReport,
    tombstoned: &HashSet<String>,
    known_keys: &HashSet<String>,
    local: &Path,
    key: &str,
) -> bool {
    // A7: NEVER resurrect a tombstoned key.
    if tombstoned.contains(key) {
        meter::bump(Counter::ImportSkippedTombstoned);
        rep.skipped_tombstoned += 1;
        debug!("tier import: {} tombstoned — NOT resurrected", key);
        return false;
    }
    if known_keys.contains(key) {
        rep.skipped_known += 1;
        return false;
    }
    if local.symlink_metadata().is_ok() {
        rep.skipped_local_exists += 1;
        return false;
    }
    true
}

/// Undo a half-built stub. Holds the marker flag because dropping the
/// marker is the step that is easy to forget and expensive to get
/// wrong.
struct Rollback<'a> {
    backend: &'a Arc<dyn StateBackend>,
    dev: u64,
    ino: u64,
    marker: bool,
    tmp: Option<PathBuf>,
}

impl Rollback<'_> {
    async fn run(&mut self) {
        if self.marker {
            crate::tier::evict::forget(self.dev, self.ino);
            self.marker = false;
        }
        let _ = self.backend.tier_delete_evicted(self.dev, self.ino).await;
        let _ = self.backend.tier_delete_generation(self.dev, self.ino).await;
        if let Some(t) = self.tmp.take() {
            let _ = std::fs::remove_file(t);
        }
    }

    fn disarm(&mut self) {
        self.marker = false;
        self.tmp = None;
    }
}

/// What the foreign-key sweep did.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReport {
    pub scanned: usize,
    pub stubs_created: usize,
    pub skipped_tombstoned: usize,
    pub skipped_known: usize,
    pub skipped_local_exists: usize,
    pub failed: usize,
    /// Of `failed`, those that failed on a retryable condition. See
    /// [`FailKind`]. Nonzero keeps the note and holds `completed` low.
    pub failed_retryable: usize,
    /// The sweep ran to the end of the listing and the note was
    /// cleared. False = it was interrupted or refused, and is still
    /// owed.
    pub completed: bool,
}

impl SweepReport {
    /// See [`ImportReport::fail`].
    pub fn fail(&mut self, kind: FailKind) {
        self.failed += 1;
        if kind == FailKind::Retryable {
            self.failed_retryable += 1;
        }
    }
}

/// Objects under the prefix that no flint hub published — foreign
/// additions, or pre-existing bucket data being adopted — ingested as
/// evicted stubs.
///
/// **This runs BEHIND A LIVE LISTENER**, which is the whole reason it is
/// separate from the manifest lane. A prefix LIST plus a HEAD per
/// unknown object is minutes on a large bucket, and making every client
/// wait that out buys nothing: the manifest already rebuilt the real
/// tree. But running live changes what "safe" means for every step, so:
///
/// - stubs install their eviction marker BEFORE their name is linked
///   (`Placement::Live` — see [`crate::tier::evict::insert_marker_for_import`]);
/// - placement is no-replace, so a name a client created while the HEAD
///   was in flight wins;
/// - the tombstone set is re-read every chunk, because a client can
///   delete a file mid-sweep and re-ingesting its key would resurrect
///   it;
/// - the durable note is cleared only on a completed pass, so a
///   suspend twelve minutes into a 200k-object sweep resumes rather
///   than losing the remainder forever.
pub async fn sweep_foreign(
    backend: Arc<dyn StateBackend>,
    store: Arc<dyn ObjectStore>,
    export_root: PathBuf,
    key_prefix: String,
    note_path: Option<PathBuf>,
) -> SweepReport {
    /// Keys per guard re-read. Small enough that a client's delete is
    /// honoured within a chunk; the same size as the durable batch, so
    /// the guards are re-read on the same rhythm the rows are written.
    const GUARD_RECHECK: usize = INGEST_CHUNK;

    let mut rep = SweepReport::default();
    let mut pending = Pending::default();
    let control = format!("{}{}/", key_prefix, crate::tier::epoch::RESERVED_DIR);

    // RESUMING a sweep that was interrupted. Rows are written a chunk
    // at a time, before the stubs in that chunk are placed, so an
    // interruption in that window leaves rows for names that never
    // appeared. Left alone they are WORSE than nothing: the resumed
    // sweep loads them into `known` and skips every one of their keys
    // as already-imported, so those objects never enter the tree and
    // the note is cleared as though the work were done. Clear the
    // residue first — the same cleanup a crashed manifest-lane import
    // does, for the same reason.
    if note_path.as_deref().map(|p| p.exists()).unwrap_or(false) {
        let mut cleanup = ImportReport::default();
        sweep_crashed_import(&backend, &export_root, &mut cleanup).await;
        if cleanup.swept_temps > 0 || cleanup.swept_rows > 0 {
            warn!(
                "tier sweep: resuming — swept {} stray temp(s) and {} row(s) whose \
                 stub never materialized",
                cleanup.swept_temps, cleanup.swept_rows
            );
        }
    }

    let objs = match store.list(&key_prefix).await {
        Ok(o) => o,
        Err(e) => {
            warn!(
                "tier sweep: bucket list failed ({}) — foreign keys remain unimported; \
                 the note is kept so the next start retries",
                e
            );
            rep.fail(FailKind::Retryable);
            return rep;
        }
    };

    // The manifest lane has already placed generation rows for every
    // key it restored, so `known_keys` alone distinguishes foreign
    // objects from ours — no separate manifest key set is needed.
    let mut known: HashSet<String> = match backend.tier_list_generations().await {
        Ok(rows) => rows.into_iter().map(|r| r.key).collect(),
        Err(e) => {
            warn!("tier sweep: cannot list generations ({}) — sweep refused", e);
            rep.fail(FailKind::Retryable);
            return rep;
        }
    };
    let mut tombstoned: HashSet<String> = match backend.tier_list_tombstones().await {
        Ok(t) => t.into_iter().map(|t| t.key).collect(),
        Err(e) => {
            warn!("tier sweep: cannot list tombstones ({}) — sweep refused (A7)", e);
            rep.fail(FailKind::Retryable);
            return rep;
        }
    };

    let candidates: Vec<_> = objs.into_iter().filter(|o| !o.key.starts_with(&control)).collect();
    if candidates.is_empty() {
        clear_note(note_path.as_deref());
        rep.completed = true;
        return rep;
    }
    info!("tier sweep: {} object(s) under the prefix to consider", candidates.len());

    for (i, o) in candidates.iter().enumerate() {
        if i % GUARD_RECHECK == 0 && i > 0 {
            // Re-read the guards. A client deleting a file mid-sweep
            // writes a tombstone, and ingesting its key afterwards
            // would resurrect a file the user just removed; a client
            // WRITING one produces a generation row, and re-ingesting
            // would stub over live bytes.
            if let Ok(t) = backend.tier_list_tombstones().await {
                tombstoned = t.into_iter().map(|t| t.key).collect();
            }
            if let Ok(rows) = backend.tier_list_generations().await {
                known = rows.into_iter().map(|r| r.key).collect();
            }
        }
        rep.scanned += 1;

        let Some(rel) = o.key.strip_prefix(key_prefix.as_str()).and_then(safe_rel_path) else {
            warn!("tier sweep: bucket key {:?} refused (unsafe path)", o.key);
            rep.fail(FailKind::PerKey);
            continue;
        };
        let local = export_root.join(&rel);

        // The cheap guards first — a HEAD costs money.
        let mut one = ImportReport::default();
        if !admissible(&mut one, &tombstoned, &known, &local, &o.key) {
            rep.skipped_tombstoned += one.skipped_tombstoned;
            rep.skipped_known += one.skipped_known;
            rep.skipped_local_exists += one.skipped_local_exists;
            continue;
        }
        let head = match store.head(&o.key).await {
            Ok(h) => h,
            Err(e) => {
                warn!("tier sweep: HEAD {}: {}", o.key, e);
                rep.fail(FailKind::Retryable);
                continue;
            }
        };
        let posix = PosixStamps::from_meta(&head.meta);
        let generation = crate::tier::store::GenerationStamps::from_meta(&head.meta)
            .map(|s| s.generation)
            .unwrap_or(1);
        if let Some(parent) = local.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!("tier sweep: mkdir -p {}: {}", parent.display(), e);
                rep.fail(FailKind::Retryable);
                continue;
            }
        }

        let staged = stage_stub(
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
        );
        let (st, erow, grow) = match staged {
            Ok(v) => v,
            Err(kind) => {
                rep.fail(kind);
                continue;
            }
        };
        pending.staged.push(st);
        pending.rows.push((erow, grow));
        // Keep the in-loop guard current without another query — the
        // key is spoken for from the moment it is staged.
        known.insert(o.key.clone());
        if pending.len() >= INGEST_CHUNK {
            flush_sweep(&backend, &mut pending, &mut rep).await;
        }
    }
    flush_sweep(&backend, &mut pending, &mut rep).await;

    // The note is cleared only when the whole listing was walked AND
    // nothing failed retryably.
    //
    // The original rule here cleared it unconditionally, reasoning that
    // "a sweep that failed objects still completed its pass — those
    // keys are individually broken, and re-running would fail them
    // again". That is true of an unsafe key and false of everything
    // else that reaches this line: a HEAD that was throttled, a mkdir
    // that hit ENOSPC, a stub the inode table refused. Those are
    // tree-wide and transient — every key after the volume filled
    // failed for the same reason — and clearing the note made the
    // condition permanent. The next start finds non-fresh state (so no
    // import) and no note (so no sweep), and the objects stay in the
    // bucket, invisible, even after the operator grows the volume.
    if rep.failed_retryable == 0 {
        clear_note(note_path.as_deref());
        rep.completed = true;
    } else {
        warn!(
            "tier sweep: {} object(s) failed on a retryable condition (out of space/inodes, \
             a throttled bucket, a backend error) — the export does NOT describe the bucket. \
             The sweep note is KEPT so the next start retries; check `df -i` as well as `df`, \
             grow the volume if either is full, then restart.",
            rep.failed_retryable
        );
    }
    info!(
        "tier sweep: {} scanned, {} stub(s) created, {} tombstoned, {} known, \
         {} local-wins, {} failed",
        rep.scanned,
        rep.stubs_created,
        rep.skipped_tombstoned,
        rep.skipped_known,
        rep.skipped_local_exists,
        rep.failed
    );
    rep
}

/// Drain a staged chunk into the sweep's own report shape.
async fn flush_sweep(
    backend: &Arc<dyn StateBackend>,
    pending: &mut Pending,
    rep: &mut SweepReport,
) {
    let mut one = ImportReport::default();
    pending.flush(backend, Placement::Live, &mut one).await;
    rep.stubs_created += one.stubs_created;
    rep.skipped_local_exists += one.skipped_local_exists;
    rep.failed += one.failed;
    rep.failed_retryable += one.failed_retryable;
}

fn clear_note(p: Option<&Path>) {
    if let Some(p) = p {
        let _ = std::fs::remove_file(p);
    }
}

/// Adopt an existing local tree that predates the tier.
///
/// The dangerous boot shape: a PVC that already holds files, bound to a
/// bucket that has never seen them. Nothing marks those files dirty —
/// capture only notes mutations, and these happened before the tier
/// existed — so the flusher publishes nothing, the manifest lists
/// nothing, and the RPO predicate reports a perfectly clean volume
/// whose every byte exists only on the PVC. Hibernating that share
/// deletes the entire project.
///
/// The fix is to treat pre-existing files as what they are: unpublished
/// work. One walk at startup, whole-dirty for every regular file with
/// no generation row, and the ordinary flush cycle uploads them.
///
/// Deliberately NOT run on every tier enable. The caller gates this on
/// the one shape that can produce it — fresh tier state, no manifest in
/// the bucket, non-empty export root — because marking a large tree
/// whole-dirty is expensive and, anywhere else, wrong.
pub async fn adopt_local_tree(
    backend: &Arc<dyn StateBackend>,
    export_root: &Path,
) -> AdoptReport {
    let mut rep = AdoptReport::default();

    fn walk(dir: &Path, rep: &mut AdoptReport, marked: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            rep.unreadable_dirs += 1;
            return;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let Ok(md) = p.symlink_metadata() else { continue };
            if md.is_dir() {
                // The tier's own control namespace is never client data.
                if crate::tier::epoch::is_reserved_component(ent.file_name()) {
                    continue;
                }
                walk(&p, rep, marked);
            } else if md.is_file() {
                // Symlinks and specials carry no bucket object; the
                // manifest reconstructs them from the tree walk.
                marked.push(p);
            }
        }
    }

    let root = export_root.to_path_buf();
    let (mut marked, walked) = match tokio::task::spawn_blocking(move || {
        let mut r = AdoptReport::default();
        let mut marked = Vec::new();
        walk(&root, &mut r, &mut marked);
        (marked, r)
    })
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!("tier adopt: tree walk task failed: {} — nothing marked", e);
            return rep;
        }
    };
    rep.unreadable_dirs = walked.unreadable_dirs;

    for path in marked.drain(..) {
        // Inside a write ticket, so the note cannot land in an epoch
        // the flusher has already taken (gate.rs's straggler rule).
        match crate::tier::gate::enter_path(&path) {
            Ok(_ticket) => {
                crate::tier::capture::note_path(&path, crate::tier::capture::Mutation::Whole);
                rep.marked_dirty += 1;
            }
            Err(crate::tier::gate::Excluded) => rep.skipped_excluded += 1,
        }
    }

    // Push the RAM marks into the durable dirty set immediately: an
    // adopt that only lived in memory would be undone by a crash
    // before the first flush, silently restoring the original hazard.
    if let Err(e) = crate::tier::durable::drain_pending(backend).await {
        warn!(
            "tier adopt: durable dirty-bit write failed: {} — {} file(s) are dirty in \
             RAM only and would be lost to a crash before the first flush",
            e, rep.marked_dirty
        );
    }
    rep
}

/// What [`adopt_local_tree`] did.
#[derive(Debug, Default, PartialEq, Eq, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptReport {
    pub marked_dirty: usize,
    pub skipped_excluded: usize,
    pub unreadable_dirs: usize,
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
                if !crate::tier::epoch::is_reserved_component(&name) {
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
///
/// The reserved namespace is rejected at EVERY depth, not just the
/// first component. A hub whose prefix is an ancestor of another
/// share's — the case `lite_operator::conflict` exists to refuse, and
/// which the store-side epoch cannot fence because a nested prefix
/// mints a DIFFERENT epoch object — lists the inner share's
/// `nested/.flint/epoch` and `nested/.flint/manifest` as ordinary
/// keys. Admitting them materializes another share's LIVE control
/// objects as client files in this export, where a subsequent client
/// write republishes over them. `.flint` is reserved throughout the
/// tree, so nothing legitimate is refused.
fn safe_rel_path(p: &str) -> Option<PathBuf> {
    if p.is_empty() {
        return None;
    }
    let pb = PathBuf::from(p);
    for c in pb.components() {
        match c {
            Component::Normal(seg) => {
                if crate::tier::epoch::is_reserved_component(seg) {
                    return None;
                }
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
        // CHOWN FIRST, THEN CHMOD. chown(2) clears S_ISUID/S_ISGID for
        // an unprivileged caller — and it does so even for a no-op
        // chown to the file's existing owner, which is the ordinary
        // case for a non-root hub restoring its own files. Doing it in
        // the other order silently strips the setuid and setgid bits
        // off EVERY restored file, so a DR restore quietly disarms
        // every `sudo`, `mount`, `ping` and shared-group directory in
        // the tree. The manifest carried the right mode all along; the
        // restore threw it away one line later.
        //
        // Best-effort (a non-root hub cannot chown to a foreign uid;
        // the manifest still records the truth), but the failure is
        // now VISIBLE rather than a discarded return.
        if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
                let e = std::io::Error::last_os_error();
                debug!("tier import: chown {} to {}:{} failed: {}", path.display(), uid, gid, e);
            }
        }
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777));
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
        /// Serialises against every other tier rig: the capture pending
        /// queue is process-global and a drain takes all of it.
        ///
        /// GENUINELY last: fields drop in declaration order, so anything
        /// declared after this would tear down with the lock already
        /// released — which is the window this guard exists to close.
        _excl: std::sync::MutexGuard<'static, ()>,
    }

    fn rig() -> Rig {
        let _excl = capture::test_exclusive();
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
        Rig { _dir: dir, root, mem, backend, orch, _excl }
    }

    /// The PVC-predates-its-bucket hazard. Files that existed before
    /// the tier was switched on are invisible to capture, so nothing
    /// publishes them, the manifest cannot list them, and the RPO
    /// predicate reports a volume the bucket cannot actually rebuild.
    /// A hibernate on that verdict deletes the whole project.
    #[tokio::test]
    async fn adopting_a_pre_tier_tree_marks_it_for_publication() {
        let r = rig();
        std::fs::create_dir_all(r.root.join("src/nested")).unwrap();
        std::fs::write(r.root.join("README.md"), b"docs").unwrap();
        std::fs::write(r.root.join("src/nested/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(r.root.join("empty.txt"), b"").unwrap();
        // The tier's own control namespace is not client data.
        std::fs::create_dir_all(r.root.join(crate::tier::epoch::RESERVED_DIR)).unwrap();
        std::fs::write(r.root.join(crate::tier::epoch::RESERVED_DIR).join("epoch"), b"x").unwrap();

        // Precondition: this is exactly the shape the caller gates on.
        assert!(state_is_fresh(&r.backend).await, "no rows yet — a fresh tier over an old PVC");

        let rep = adopt_local_tree(&r.backend, &r.root).await;
        assert_eq!(rep.marked_dirty, 3, "every regular file, including the empty one");

        // Assert on the paths THIS tree owns, not on a count. The
        // capture pending-set is process-global and `drain_pending`
        // flushes all of it into whichever backend calls first, so a
        // concurrent test's notes land here too — a count would be
        // flaky, and naming the files is the stronger claim anyway.
        let dirty = r.backend.tier_list_dirty().await.unwrap();
        let mine: Vec<&str> = dirty
            .iter()
            .filter_map(|d| d.path.as_deref())
            .filter(|p| p.starts_with(&*r.root.to_string_lossy()))
            .collect();
        for want in ["README.md", "src/nested/main.rs", "empty.txt"] {
            assert!(
                mine.iter().any(|p| p.ends_with(want)),
                "{want} must be DURABLY marked, not RAM-only: {mine:?}"
            );
        }
        assert_eq!(mine.len(), 3, "and nothing else under this root: {mine:?}");
        assert!(
            !mine.iter().any(|p| p.contains(crate::tier::epoch::RESERVED_DIR)),
            "the reserved control namespace must never be published as client data"
        );
    }

    /// A bucket listing is DATA. The dangerous shape is an ancestor
    /// hub: a share on `tenant-x/` lists a nested share's
    /// `tenant-x/nested/.flint/epoch`, strips its own prefix, and is
    /// left holding `nested/.flint/epoch` — which a first-component
    /// test admits. Materializing it makes another share's live
    /// control objects client-visible here, and a client write then
    /// republishes over them. The store-side epoch cannot catch this:
    /// a nested prefix mints a DIFFERENT epoch object and the two hubs
    /// never contend.
    #[test]
    fn the_reserved_namespace_is_refused_at_every_depth() {
        for bad in [
            ".flint/epoch",
            ".flint/manifest",
            "nested/.flint/epoch",
            "nested/.flint/manifest",
            "a/b/c/.flint/epoch",
        ] {
            assert_eq!(safe_rel_path(bad), None, "{bad} is a control object, never client data");
        }
        // Anti-vacuity: the guard is the reserved NAME, not the depth,
        // and not a substring of it.
        for ok in ["README.md", "nested/main.rs", "a/b/c/d.bin", "flint/x", ".flintish/x"] {
            assert!(safe_rel_path(ok).is_some(), "{ok} is ordinary client data");
        }
        // The pre-existing guards still hold.
        for bad in ["", "/abs", "../escape", "./here", "a/../b"] {
            assert_eq!(safe_rel_path(bad), None, "{bad} must stay refused");
        }
    }

    /// What `start_tier` does, in the order it does it: seed the
    /// manifest ONCE, run the manifest lane pre-listener, then run the
    /// foreign-key sweep as if the listener were up. Tests that assert
    /// on the two lanes separately call the pieces directly.
    async fn import_then_sweep(
        backend: &Arc<dyn StateBackend>,
        store: &Arc<dyn ObjectStore>,
        cfg: ImportConfig<'_>,
    ) -> (Option<ImportReport>, SweepReport) {
        let seed = manifest::seed_full(store.as_ref(), cfg.key_prefix).await;
        let root = cfg.export_root.to_path_buf();
        let prefix = cfg.key_prefix.to_string();
        let note = cfg.sweep_note_path.map(|p| p.to_path_buf());
        let outcome = maybe_import_on_start(backend, store, &seed, cfg).await;
        let sweep = if outcome.sweep_owed {
            sweep_foreign(Arc::clone(backend), Arc::clone(store), root, prefix, note).await
        } else {
            SweepReport::default()
        };
        (outcome.report, sweep)
    }

    /// The sweep as `serve` spawns it: post-listener, live placement.
    async fn sweep(r: &Rig) -> SweepReport {
        sweep_foreign(
            r.backend.clone(),
            r.mem.clone() as Arc<dyn ObjectStore>,
            r.root.clone(),
            "t/".to_string(),
            None,
        )
        .await
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

        let sweep_note = root_b.join("..").join("flint-sweep-pending-drill");
        let (rep, sw) = import_then_sweep(
            &backend_b,
            &store_b,
            ImportConfig {
                export_root: &root_b,
                key_prefix: "t/",
                intent_path: Some(&intent),
                sweep_note_path: Some(&sweep_note),
            },
        )
        .await;
        let rep = rep.expect("fresh state + bucket content must import");
        assert!(sw.completed, "the sweep must run and clear its note");
        assert!(!sweep_note.exists(), "a completed sweep clears its note");
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
        let seed2 = manifest::seed_full(store_b.as_ref(), "t/").await;
        let doc2 = match &seed2 {
            manifest::ManifestSeed::Present(m, _) => Some(m.as_ref()),
            _ => None,
        };
        let rep2 = import_refresh(
            &backend_b,
            doc2,
            ImportConfig {
                export_root: &root_b,
                key_prefix: "t/",
                intent_path: None,
                sweep_note_path: None,
            },
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

        let rep = sweep(&r).await;
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
        let rep2 = sweep(&r).await;
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

        let rep = sweep(&r).await;
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
        // The sweep runs behind a live listener, so it installs the
        // marker ITSELF, before the name is linked — no `reconcile` is
        // involved, and a client GETATTRing this path the instant it
        // appears must already see the logical size.
        let (dev, ino) = ident(&p);
        assert_eq!(
            evict::logical_size(dev, ino),
            Some(900),
            "a swept-in stub must be evicted the moment it is visible"
        );
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

        let (rep, sw) = import_then_sweep(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: Some(&intent),
                sweep_note_path: None,
            },
        )
        .await;
        let rep = rep.expect("intent note must force a resume even on non-fresh state");
        assert_eq!(rep.swept_temps, 1, "{:?}", rep);
        assert_eq!(rep.swept_rows, 1);
        // The key is foreign (no manifest describes it), so the STUB is
        // the sweep's work, not the manifest lane's.
        assert_eq!(sw.stubs_created, 1, "{:?}", sw);
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

    /// **The most dangerous thing the sweep can get wrong.**
    ///
    /// A swept-in stub whose durable rows exist but whose eviction
    /// MARKER does not is indistinguishable, to every read path, from
    /// an ordinary empty file: GETATTR says size 0, `cat` returns EOF
    /// with no error. And the first small WRITE publishes over the real
    /// object under an If-Match that SUCCEEDS — the generation row's
    /// etag is genuinely the bucket's current one — so a 10 GiB object
    /// becomes 4 KiB and every copy of the original is gone.
    ///
    /// Pre-listener that ordering was safe because `evict::reconcile`
    /// loaded the markers before any client existed. Running behind a
    /// live listener, it is not: the name is visible the instant it is
    /// linked. So the marker must be installed BEFORE the link, and
    /// this test asserts it from the only vantage point that matters —
    /// the marker map, with no `reconcile` anywhere in sight.
    #[tokio::test]
    async fn a_swept_in_stub_is_evicted_before_its_name_exists() {
        let r = rig();
        let big: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        r.mem.raw_put("t/precious.bin", Bytes::from(big.clone()), vec![]);

        let rep = sweep(&r).await;
        assert_eq!(rep.stubs_created, 1, "{:?}", rep);

        let p = r.root.join("precious.bin");
        let (dev, ino) = ident(&p);
        // NO evict::reconcile() call. If the marker were only written
        // to the rows, this is where a live client would see a 0-byte
        // file and destroy 4 KiB of bucket data on its first write.
        assert!(
            evict::is_evicted(dev, ino),
            "the stub must be evicted the instant its name is visible"
        );
        assert_eq!(
            evict::logical_size(dev, ino),
            Some(4096),
            "and report the BUCKET's size, not the stub's zero"
        );
        assert_eq!(std::fs::symlink_metadata(&p).unwrap().len(), 0, "on disk it is a stub");
    }

    /// A client can create a file while the sweep's HEAD is in flight.
    /// The old code renamed over the target, which would replace the
    /// client's bytes with a STUB — the bytes gone, and every later
    /// read served from a bucket object that never held them. Local
    /// wins, so the link must be no-replace.
    #[tokio::test]
    async fn a_name_that_appears_mid_sweep_is_not_clobbered() {
        let r = rig();
        r.mem.raw_put("t/racy.bin", Bytes::from_static(b"bucket version"), vec![]);

        // Stand in for the client that won the race: the name exists
        // before the sweep reaches it.
        let p = r.root.join("racy.bin");
        std::fs::write(&p, b"the client wrote this").unwrap();

        let rep = sweep(&r).await;
        assert_eq!(rep.skipped_local_exists, 1, "{:?}", rep);
        assert_eq!(rep.stubs_created, 0);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"the client wrote this",
            "the client's bytes must survive the sweep"
        );
        let (dev, ino) = ident(&p);
        assert!(!evict::is_evicted(dev, ino), "and it must not be marked evicted");
    }

    /// A failure after the rows are durable must drop the marker too.
    /// A marker left behind on a freed inode NUMBER is its own
    /// data-loss vector: the kernel reuses inode numbers, and an
    /// unrelated new file landing on that number would read as evicted
    /// and hydrate someone else's S3 bytes over itself.
    #[tokio::test]
    async fn a_failed_placement_leaves_no_marker_behind() {
        let r = rig();
        r.mem.raw_put("t/blocked/obj.bin", Bytes::from_static(b"x"), vec![]);
        // A regular FILE where the sweep needs a directory: the
        // `create_dir_all` for the parent fails, and the ingest never
        // starts. Nothing may be left in the marker map.
        std::fs::write(r.root.join("blocked"), b"i am not a directory").unwrap();

        let before = evict::marker_stats().0;
        let rep = sweep(&r).await;
        assert_eq!(rep.failed, 1, "{:?}", rep);
        assert_eq!(rep.stubs_created, 0);
        assert_eq!(
            evict::marker_stats().0,
            before,
            "a failed ingest must leave the marker map exactly as it found it"
        );
        // And no rows either — a row whose path never materialized is
        // what the resume sweep exists to clean up, so not creating one
        // is better than creating one.
        let rows = r.backend.tier_list_evicted().await.unwrap();
        assert!(
            !rows.iter().any(|x| x.key == "t/blocked/obj.bin"),
            "no evicted row for an ingest that never placed a file"
        );
    }

    /// The sweep must not storm live readers. MARKER_CYCLE exists to
    /// catch byte-DESTROYING events mid-read; a sweep insert destroys
    /// nothing, because the name did not exist a moment ago and no read
    /// can be in flight against it. Bumping would invalidate every
    /// concurrent reader in the volume — 200k times on a large bucket —
    /// and livelock the COPY/CLONE windows.
    ///
    /// MARKER_CYCLE is process-global and the suite runs in parallel, so
    /// a single "unchanged" reading can be falsified by an unrelated
    /// test evicting something in the same window. The observation is
    /// still sound, taken the right way round: if the sweep does NOT
    /// bump, at least one attempt sees a clean delta; if it DOES bump,
    /// no attempt ever can.
    #[tokio::test]
    async fn the_sweep_does_not_invalidate_concurrent_readers() {
        let mut clean = false;
        for attempt in 0..8 {
            let r = rig();
            for i in 0..5 {
                r.mem.raw_put(
                    &format!("t/bulk{attempt}-{i}.bin"),
                    Bytes::from_static(b"data"),
                    vec![],
                );
            }
            let before = evict::marker_cycle();
            let rep = sweep(&r).await;
            assert_eq!(rep.stubs_created, 5, "{:?}", rep);
            if evict::marker_cycle() == before {
                clean = true;
                break;
            }
        }
        assert!(
            clean,
            "every attempt saw MARKER_CYCLE move across a sweep — the sweep is bumping it, \
             which storms every concurrent reader in the volume with spurious DELAYs \
             (200k times on a large bucket) for inserts that destroy nothing"
        );
    }

    /// The sweep's durable note. Without it, a hub suspended twelve
    /// minutes into a 200k-object sweep loses every remaining key
    /// forever: the next start finds no import intent (the manifest
    /// lane cleared it) and non-fresh state (the manifest lane placed
    /// rows), so nothing runs the sweep again.
    #[tokio::test]
    async fn an_owed_sweep_survives_a_restart() {
        let r = rig();
        r.mem.raw_put("t/late.bin", Bytes::from_static(b"arrived late"), vec![]);
        let note = r.root.join("..").join("flint-sweep-note-test");
        let intent = r.root.join("..").join("flint-import-intent-note-test");
        let _ = std::fs::remove_file(&note);

        // The manifest lane runs and owes a sweep.
        let seed = manifest::seed_full(&*(r.mem.clone() as Arc<dyn ObjectStore>), "t/").await;
        let outcome = maybe_import_on_start(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            &seed,
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: Some(&intent),
                sweep_note_path: Some(&note),
            },
        )
        .await;
        assert!(outcome.sweep_owed);
        assert!(note.exists(), "the owed sweep must be recorded DURABLY");
        assert!(!intent.exists(), "and the import intent cleared behind it");

        // The hub SERVED before it was suspended, and that is the whole
        // point of this test: on the next start the fresh-state lane is
        // shut, so the NOTE is the only thing that can still owe the
        // sweep. A tombstone is the cheapest durable trace of a hub that
        // has run.
        //
        // This used to be asserted as `!state_is_fresh(..) || true`,
        // which is always true and therefore asserted nothing. It was
        // hiding a real hole: nothing here had ever made the state
        // non-fresh, so BOTH starts took the fresh-state lane, and
        // `outcome2.sweep_owed` below was carried by that lane rather
        // than by the note. The test could not have failed if the note
        // had never been read at all.
        r.backend
            .tier_put_tombstone(&crate::state_backend::TierTombstone {
                key: "t/served-before.bin".to_string(),
                etag: None,
                created_unix: 1,
            })
            .await
            .unwrap();
        assert!(
            !state_is_fresh(&r.backend).await,
            "the hub has served, so the FRESH-state import lane must be shut"
        );

        // CONTROL, and it is what makes the assertion after it mean
        // something: same non-fresh state, same bucket, NO note. Nothing
        // may owe a sweep. If this ever starts owing one, the leg below
        // is measuring the other lane again.
        let seed_ctl = manifest::seed_full(&*(r.mem.clone() as Arc<dyn ObjectStore>), "t/").await;
        let control = maybe_import_on_start(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            &seed_ctl,
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: Some(&intent),
                sweep_note_path: None,
            },
        )
        .await;
        assert!(
            !control.sweep_owed,
            "non-fresh state with no note must owe NOTHING — otherwise the \
             note is not what carries the sweep across the restart"
        );
        let seed2 = manifest::seed_full(&*(r.mem.clone() as Arc<dyn ObjectStore>), "t/").await;
        let outcome2 = maybe_import_on_start(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            &seed2,
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: Some(&intent),
                sweep_note_path: Some(&note),
            },
        )
        .await;
        assert!(
            outcome2.sweep_owed,
            "the note alone must be enough to owe a sweep on the next start"
        );

        let rep = sweep_foreign(
            r.backend.clone(),
            r.mem.clone() as Arc<dyn ObjectStore>,
            r.root.clone(),
            "t/".to_string(),
            Some(note.clone()),
        )
        .await;
        assert_eq!(rep.stubs_created, 1, "{:?}", rep);
        assert!(rep.completed);
        assert!(!note.exists(), "a completed sweep clears its note");
        let _ = std::fs::remove_file(&intent);
    }

    /// The note must survive a TREE-WIDE transient refusal, and the
    /// namespace must come back when the condition lifts.
    ///
    /// The old rule cleared the note on any pass that walked the whole
    /// listing, reasoning that failures are per-key defects which would
    /// fail again anyway. A throttled bucket is the counterexample:
    /// every key after the throttle starts fails for one reason, and
    /// clearing the note made that transient condition PERMANENT — the
    /// next start finds non-fresh state (no import lane) and no note
    /// (no sweep lane), so those objects are never looked at again.
    ///
    /// The second half is the anti-vacuity control and the operator's
    /// actual recovery: same rig, same objects, condition lifted. If
    /// the note were being kept for some reason other than the refusal,
    /// this arm would keep it too and the test would fail.
    #[tokio::test]
    async fn the_sweep_note_survives_a_retryable_refusal_and_the_namespace_returns() {
        let r = rig();
        for i in 0..4 {
            r.mem.raw_put(&format!("t/obj{i}.bin"), Bytes::from_static(b"x"), vec![]);
        }
        let note = r.root.join("..").join("flint-sweep-note-retryable");
        std::fs::write(&note, b"sweep\n").unwrap();

        // Sustained throttle: every HEAD this pass issues is refused.
        // Exactly four, so the budget is spent by the end of the pass —
        // the recovery arm below then runs against a bucket that has
        // genuinely stopped throttling, not a flag someone flipped.
        r.mem.inject_head_failures(4);
        let rep = sweep_foreign(
            r.backend.clone(),
            r.mem.clone() as Arc<dyn ObjectStore>,
            r.root.clone(),
            "t/".to_string(),
            Some(note.clone()),
        )
        .await;

        assert_eq!(rep.scanned, 4, "the sweep must have LOOKED at every key: {:?}", rep);
        assert_eq!(rep.stubs_created, 0, "nothing can have landed: {:?}", rep);
        assert_eq!(rep.failed_retryable, 4, "all four are environmental: {:?}", rep);
        assert!(!rep.completed, "a refused pass is not a completed one");
        assert!(
            note.exists(),
            "THE BUG: clearing the note here is what made a throttle permanent"
        );
        for i in 0..4 {
            assert!(
                !r.root.join(format!("obj{i}.bin")).exists(),
                "the tree really is short — otherwise nothing was lost to begin with"
            );
        }

        // The condition lifts (the operator grew the volume, or the
        // bucket stopped throttling) and the next pass recovers.
        let rep2 = sweep_foreign(
            r.backend.clone(),
            r.mem.clone() as Arc<dyn ObjectStore>,
            r.root.clone(),
            "t/".to_string(),
            Some(note.clone()),
        )
        .await;
        assert_eq!(rep2.stubs_created, 4, "the namespace must return: {:?}", rep2);
        assert_eq!(rep2.failed_retryable, 0, "{:?}", rep2);
        assert!(rep2.completed);
        assert!(!note.exists(), "and NOW the note clears");
    }

    /// The discriminating counterpart: the gate must not degrade into
    /// "always keep the note". A key the sweep can never place — an
    /// unsafe path — is a permanent property of the bucket, so holding
    /// the note for it would wedge every future start into re-running a
    /// sweep that cannot progress.
    #[tokio::test]
    async fn a_per_key_defect_still_clears_the_sweep_note() {
        let r = rig();
        r.mem.raw_put("t/../escape.bin", Bytes::from_static(b"nope"), vec![]);
        r.mem.raw_put("t/fine.bin", Bytes::from_static(b"yes"), vec![]);
        let note = r.root.join("..").join("flint-sweep-note-perkey");
        std::fs::write(&note, b"sweep\n").unwrap();

        let rep = sweep_foreign(
            r.backend.clone(),
            r.mem.clone() as Arc<dyn ObjectStore>,
            r.root.clone(),
            "t/".to_string(),
            Some(note.clone()),
        )
        .await;

        assert_eq!(rep.failed, 1, "the unsafe key must have been refused: {:?}", rep);
        assert_eq!(
            rep.failed_retryable, 0,
            "and refused as PER-KEY — if this is retryable the gate never releases: {:?}",
            rep
        );
        assert_eq!(rep.stubs_created, 1, "the good key still lands: {:?}", rep);
        assert!(rep.completed);
        assert!(!note.exists(), "a pass whose only failures are per-key is done");
    }

    /// F2a — the same rule one lane earlier, where the stakes are
    /// higher: the sweep can re-adopt a missing FILE as a foreign key,
    /// but directories, symlinks and mode/uid/gid live only in the
    /// manifest. If the intent note clears on a retryably-short walk,
    /// nothing in the system can ever restore them.
    #[tokio::test]
    async fn a_retryably_incomplete_manifest_lane_keeps_its_intent_note() {
        let r = rig();
        let intent = r.root.join("..").join("flint-import-intent-retryable");
        let note = r.root.join("..").join("flint-sweep-note-manifest");
        let _ = std::fs::remove_file(&intent);
        let _ = std::fs::remove_file(&note);

        // Refuse the environment: a read-only export root fails every
        // mkdir with EACCES. Root ignores the mode bits, so probe for
        // the capability rather than assuming it.
        use std::os::unix::fs::PermissionsExt;
        let ro = || std::fs::Permissions::from_mode(0o555);
        let rw = || std::fs::Permissions::from_mode(0o755);
        let sub = r.root.join("probe");
        std::fs::set_permissions(&r.root, ro()).unwrap();
        let writable_anyway = std::fs::create_dir(&sub).is_ok();
        if writable_anyway {
            let _ = std::fs::remove_dir(&sub);
            std::fs::set_permissions(&r.root, rw()).unwrap();
            eprintln!("skipped: running as root, a read-only directory refuses nothing");
            return;
        }

        let m = manifest::Manifest::parse(
            br#"{"version":1,"seq":1,"epoch":1,"written_unix":0,"beyond_rpo":0,
                 "skipped_special":0,"entries":[
                 {"path":"d","type":"dir","mode":16877,"uid":0,"gid":0,"mtime_unix":0}]}"#,
        )
        .expect("fixture manifest must parse");
        let rep = import_refresh(
            &r.backend,
            Some(&m),
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: Some(&intent),
                sweep_note_path: Some(&note),
            },
        )
        .await;
        std::fs::set_permissions(&r.root, rw()).unwrap();

        assert_eq!(rep.dirs_restored, 0, "the mkdir must really have failed: {:?}", rep);
        assert_eq!(rep.failed_retryable, 1, "and failed ENVIRONMENTALLY: {:?}", rep);
        assert!(
            intent.exists(),
            "THE BUG: clearing the intent note here loses the directory forever — \
             no sweep can restore a dir, because a dir is not an object"
        );

        // Control: the same lane, the same manifest, a writable root.
        // The note must clear — otherwise the assertion above is
        // measuring something other than the refusal.
        //
        // `rig()` holds `capture::test_exclusive()`, so the first rig
        // MUST be released before the second is built — two live rigs
        // in one test is a self-deadlock, not a failure.
        drop(r);
        let r2 = rig();
        let intent2 = r2.root.join("..").join("flint-import-intent-control");
        let note2 = r2.root.join("..").join("flint-sweep-note-control");
        let _ = std::fs::remove_file(&intent2);
        let rep2 = import_refresh(
            &r2.backend,
            Some(&m),
            ImportConfig {
                export_root: &r2.root,
                key_prefix: "t/",
                intent_path: Some(&intent2),
                sweep_note_path: Some(&note2),
            },
        )
        .await;
        assert_eq!(rep2.dirs_restored, 1, "{:?}", rep2);
        assert_eq!(rep2.failed_retryable, 0, "{:?}", rep2);
        assert!(!intent2.exists(), "a clean lane clears its intent note");
        let _ = std::fs::remove_file(&intent);
        let _ = std::fs::remove_file(&note);
        let _ = std::fs::remove_file(&note2);
    }

    /// A bucket that HAS a manifest we cannot read is not the same as a
    /// bucket with none. Importing anyway would serve a tree with every
    /// directory, symlink, mode and owner missing — only the manifest
    /// carries them — and the hub would then publish that impoverished
    /// tree back over the real one. Refuse, loudly, and leave the intent
    /// note so the next start retries.
    #[tokio::test]
    async fn an_unreadable_manifest_refuses_the_import_rather_than_serving_a_wrong_tree() {
        let r = rig();
        r.mem.raw_put("t/data.bin", Bytes::from_static(b"real content"), vec![]);
        // A manifest object that exists and is garbage.
        r.mem.raw_put(&manifest::manifest_key("t/"), Bytes::from_static(b"{ not json"), vec![]);

        let seed = manifest::seed_full(&*(r.mem.clone() as Arc<dyn ObjectStore>), "t/").await;
        assert!(matches!(seed, manifest::ManifestSeed::Unreadable(_, _)));

        let intent = r.root.join("..").join("flint-intent-unreadable-test");
        std::fs::write(&intent, b"import\n").unwrap();
        let outcome = maybe_import_on_start(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            &seed,
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: Some(&intent),
                sweep_note_path: None,
            },
        )
        .await;
        assert!(outcome.refused.is_some(), "an unreadable manifest must REFUSE on fresh state");
        assert!(outcome.report.is_none(), "and import nothing");
        assert!(
            intent.exists(),
            "the intent note must SURVIVE a refusal — clearing it would turn a \
             transient bucket error into a permanent data loss"
        );
        assert!(!r.root.join("data.bin").exists(), "nothing was restored");
        let _ = std::fs::remove_file(&intent);
    }

    /// A bucket with NO manifest is the adopt path and must still work
    /// — that is a legitimate, common shape (pre-existing bucket data).
    #[tokio::test]
    async fn an_absent_manifest_is_the_adopt_path_not_a_refusal() {
        let r = rig();
        r.mem.raw_put("t/legacy.bin", Bytes::from_static(b"pre-existing"), vec![]);

        let seed = manifest::seed_full(&*(r.mem.clone() as Arc<dyn ObjectStore>), "t/").await;
        assert!(matches!(seed, manifest::ManifestSeed::Absent));
        let outcome = maybe_import_on_start(
            &r.backend,
            &(r.mem.clone() as Arc<dyn ObjectStore>),
            &seed,
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: None,
                sweep_note_path: None,
            },
        )
        .await;
        assert!(outcome.refused.is_none());
        assert!(outcome.sweep_owed, "the foreign object must still be swept in");
    }

    /// The batch must be equivalent to the per-row writes it replaces,
    /// including across the chunk boundary — a sweep of 600 objects
    /// crosses it twice, and an off-by-one there loses a stub silently.
    #[tokio::test]
    async fn a_sweep_larger_than_one_chunk_ingests_every_object() {
        let r = rig();
        let n = INGEST_CHUNK * 2 + 7;
        for i in 0..n {
            r.mem.raw_put(&format!("t/many/{i:05}.bin"), Bytes::from_static(b"x"), vec![]);
        }

        let rep = sweep(&r).await;
        assert_eq!(rep.stubs_created, n, "{:?}", rep);
        assert_eq!(rep.failed, 0);
        assert!(rep.completed);

        // Every stub is on disk, durable, AND in the marker map — the
        // batch must not have skipped the marker for the tail chunk.
        let rows = r.backend.tier_list_evicted().await.unwrap();
        assert_eq!(rows.len(), n, "one durable row per object");
        for i in [0usize, INGEST_CHUNK - 1, INGEST_CHUNK, n - 1] {
            let p = r.root.join(format!("many/{i:05}.bin"));
            assert!(p.exists(), "{} missing", p.display());
            let (dev, ino) = ident(&p);
            assert!(evict::is_evicted(dev, ino), "{} has no marker", p.display());
        }
        // And no import temps survive.
        let leftovers: Vec<_> = std::fs::read_dir(r.root.join("many"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|f| f.starts_with(IMPORT_TMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "import temps survived: {leftovers:?}");
    }

    /// An interrupted sweep must not turn its own half-written rows
    /// into a permanent skip.
    ///
    /// Rows go down a chunk at a time, BEFORE the stubs in that chunk
    /// are placed, so a crash in that window leaves rows naming files
    /// that do not exist. Those rows are worse than nothing: the
    /// resumed sweep loads them into its known-keys guard and skips
    /// every one of their objects as already-imported — then clears the
    /// note, as though the work were finished. The objects stay in the
    /// bucket and never appear in the tree again.
    #[tokio::test]
    async fn an_interrupted_sweeps_residue_does_not_hide_its_objects_forever() {
        let r = rig();
        r.mem.raw_put("t/unlucky.bin", Bytes::from_static(b"still in the bucket"), vec![]);
        let note = r.root.join("..").join("flint-sweep-residue-test");
        std::fs::write(&note, b"sweep\n").unwrap();

        // The residue an interrupted chunk leaves: rows for a stub that
        // was never linked, plus its temp.
        std::fs::write(r.root.join(".flint-import.interrupted"), b"").unwrap();
        let never = r.root.join("unlucky.bin");
        r.backend
            .tier_put_evicted(&crate::state_backend::TierEvictedRow {
                dev: 777_777,
                ino: 666_666,
                key: "t/unlucky.bin".into(),
                generation: 1,
                etag: "e".into(),
                crc64_b64: String::new(),
                size: 19,
                path: never.to_string_lossy().into_owned(),
                evicted_unix: 1,
                hydrating_unix: None,
            })
            .await
            .unwrap();
        r.backend
            .tier_upsert_generation(&crate::state_backend::TierGenerationRow {
                dev: 777_777,
                ino: 666_666,
                key: "t/unlucky.bin".into(),
                generation: 1,
                etag: "e".into(),
                crc64_b64: None,
                size: 19,
                copy_allowed: false,
                updated_unix: 1,
            })
            .await
            .unwrap();

        let rep = sweep_foreign(
            r.backend.clone(),
            r.mem.clone() as Arc<dyn ObjectStore>,
            r.root.clone(),
            "t/".to_string(),
            Some(note.clone()),
        )
        .await;

        assert_eq!(
            rep.stubs_created, 1,
            "the object must be ingested on the resumed pass, not skipped as known: {rep:?}"
        );
        assert_eq!(rep.skipped_known, 0, "{rep:?}");
        assert!(never.exists(), "and its name must exist in the tree");
        assert!(
            !r.root.join(".flint-import.interrupted").exists(),
            "the stray temp must be swept"
        );
        // Exactly one row, keyed on the REAL identity.
        let (dev, ino) = ident(&never);
        let rows = r.backend.tier_list_evicted().await.unwrap();
        let mine: Vec<_> = rows.iter().filter(|x| x.key == "t/unlucky.bin").collect();
        assert_eq!(mine.len(), 1, "the phantom row must be gone: {mine:?}");
        assert_eq!((mine[0].dev, mine[0].ino), (dev, ino));
        assert!(evict::is_evicted(dev, ino));
        assert!(!note.exists());
    }

    /// Two manifest entries may cite ONE bucket key. That is not a
    /// corrupt manifest — it is what `manifest::walk` PRODUCES for a
    /// hard-linked pair, because it looks the key up by (dev, ino) and
    /// both names resolve to the same generation row.
    ///
    /// `known_keys` is a snapshot taken once before the entry loop
    /// (import.rs:263) and never refreshed, so `admissible` cannot see
    /// the row the first entry just wrote: both entries materialize as
    /// stubs pointing at the same object. The first write through
    /// EITHER name then re-keys — the stored key is not what the path
    /// derives — and `finish_rekey` deletes the old object
    /// UNCONDITIONALLY, without asking whether another live generation
    /// row still cites it. The other stub is left pointing at nothing.
    ///
    /// RED at 740e45db: the shared object is deleted.
    #[tokio::test]
    async fn a_rekey_must_not_delete_a_key_another_live_row_still_cites() {
        let r = rig();

        // Seed the shared object the way a previous epoch would have.
        let body = Bytes::from_static(b"one object, two names");
        let stamps = crate::tier::store::GenerationStamps {
            generation: 1,
            epoch: 1,
            flush_uuid: "fixture".into(),
            boundary_source: None,
            posix: None,
        };
        let meta = r
            .mem
            .put_whole(
                "t/shared.bin",
                body.clone(),
                &crate::tier::store::PutCondition::IfNoneMatchAny,
                &stamps,
                crate::tier::store::crc64_nvme(&body),
            )
            .await
            .unwrap();

        // Built as a struct, not as a JSON fixture: a real ETag
        // carries literal double quotes, so hand-formatted JSON here
        // fails to parse and the test dies on its fixture instead of
        // on the behaviour it names.
        let entry = |path: &str| manifest::Entry {
            path: path.to_string(),
            kind: manifest::EntryKind::File,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            mtime_unix: 0,
            key: Some("t/shared.bin".to_string()),
            generation: Some(1),
            etag: Some(meta.etag.clone()),
            crc64_b64: meta.crc64_b64.clone(),
            size: Some(body.len() as u64),
            target: None,
        };
        let m = manifest::Manifest {
            version: manifest::MANIFEST_VERSION,
            seq: 1,
            epoch: 1,
            written_unix: 0,
            beyond_rpo: 0,
            skipped_special: 0,
            entries: vec![entry("a.bin"), entry("b.bin")],
        };

        let rep = import_refresh(
            &r.backend,
            Some(&m),
            ImportConfig {
                export_root: &r.root,
                key_prefix: "t/",
                intent_path: None,
                sweep_note_path: None,
            },
        )
        .await;

        // Anti-vacuity: if the second entry had been skipped, there
        // would be no second citation and nothing below would be
        // testing the thing it names.
        assert!(r.root.join("a.bin").exists(), "a.bin must materialize: {rep:?}");
        assert!(
            r.root.join("b.bin").exists(),
            "b.bin must materialize TOO — if `known_keys` had refreshed, this test would \
             be measuring nothing: {rep:?}"
        );
        let (b_dev, b_ino) = ident(&r.root.join("b.bin"));
        let cited = r
            .backend
            .tier_list_generations()
            .await
            .unwrap()
            .into_iter()
            .find(|g| g.dev == b_dev && g.ino == b_ino)
            .expect("b.bin must carry a generation row")
            .key;
        assert_eq!(cited, "t/shared.bin", "b.bin must cite the shared object");

        // Write through a.bin. Its path derives "t/a.bin", which is not
        // the stored key, so this publish is a re-key.
        std::fs::write(r.root.join("a.bin"), b"rewritten through the first name").unwrap();
        evict::forget(ident(&r.root.join("a.bin")).0, ident(&r.root.join("a.bin")).1);
        note_and_land(&r, &r.root.join("a.bin"), Mutation::Whole).await;
        r.orch.tick().await;
        assert!(
            r.mem.head("t/a.bin").await.is_ok(),
            "precondition: the re-key must actually have published under the new key"
        );

        assert!(
            r.mem.head(&cited).await.is_ok(),
            "THE BUG: the re-key deleted {cited}, which b.bin's live generation row still \
             cites — b.bin is now a stub pointing at nothing"
        );
    }

    /// The server's own filehandle-MAC secret lives at
    /// `<export>/.flint-nfs/fh.key` (`nfs::v4::fh_kernel::META_DIR`).
    /// The tier reserves exactly one name — `.flint`
    /// (`epoch::RESERVED_DIR`) — and knows nothing about this one, so
    /// the adopt walk marks fh.key dirty like any client file. From
    /// there it is uploaded to the bucket, listed in the DR manifest,
    /// and becomes eviction-eligible; truncating it makes EVERY
    /// filehandle in the volume permanently STALE.
    ///
    /// The client-facing door already hides this directory
    /// (fileops.rs filters META_DIR out of READDIR). Only the tier's
    /// own walks do not.
    ///
    /// RED at 740e45db: fh.key is marked dirty.
    #[tokio::test]
    async fn adopt_must_not_tier_the_servers_own_meta_dir() {
        let r = rig();
        let meta_dir = r.root.join(crate::nfs::v4::fh_kernel::META_DIR);
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(meta_dir.join("fh.key"), b"the filehandle MAC secret").unwrap();
        std::fs::write(meta_dir.join("state.db"), b"the server's own state").unwrap();
        // One real client file, so "marked nothing at all" cannot pass
        // this test by accident.
        std::fs::write(r.root.join("client.txt"), b"real data").unwrap();

        let rep = adopt_local_tree(&r.backend, &r.root).await;

        let root_str = r.root.to_string_lossy().into_owned();
        let dirty = r.backend.tier_list_dirty().await.unwrap();
        let mine: Vec<String> = dirty
            .iter()
            .filter_map(|d| d.path.clone())
            .filter(|p| p.starts_with(&root_str))
            .collect();

        assert!(
            mine.iter().any(|p| p.ends_with("client.txt")),
            "anti-vacuity: the ordinary client file MUST be adopted, or this test would \
             pass on an adopt that did nothing: {mine:?}"
        );
        assert!(
            !mine.iter().any(|p| p.contains(crate::nfs::v4::fh_kernel::META_DIR)),
            "THE BUG: the server's own {} is tiered — fh.key reaches the bucket and \
             becomes eviction-eligible, and truncating it STALEs every filehandle in the \
             volume: {mine:?}",
            crate::nfs::v4::fh_kernel::META_DIR
        );
        assert_eq!(rep.marked_dirty, 1, "exactly the one client file: {rep:?}");
    }

    /// `chown(2)` clears S_ISUID/S_ISGID for an unprivileged caller —
    /// including a no-op chown to the file's existing owner, which is
    /// what a non-root hub does to every file it restores. Doing the
    /// chmod first therefore stripped setuid and setgid off the whole
    /// restored tree, silently disarming every `sudo`, `mount` and
    /// shared-group directory in it. The manifest carried the right
    /// mode all along; the restore threw it away one line later.
    #[tokio::test]
    async fn dr_restore_must_not_strip_setuid() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("sudo");
        std::fs::write(&f, b"#!/bin/sh\n").unwrap();

        let (uid, gid) = {
            let md = std::fs::metadata(&f).unwrap();
            (md.uid(), md.gid())
        };

        // ANTI-VACUITY: prove this platform actually strips on chown,
        // or the assertion below passes for a reason that has nothing
        // to do with the fix.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o4755)).unwrap();
        if let Ok(c) = std::ffi::CString::new(f.as_os_str().as_encoded_bytes()) {
            unsafe { libc::chown(c.as_ptr(), uid, gid) };
        }
        if std::fs::metadata(&f).unwrap().mode() & 0o4000 != 0 {
            eprintln!("skipped: this platform does not clear setuid on chown");
            return;
        }

        apply_posix(&f, 0o104755, uid, gid, Some(1_700_000_000));

        assert_eq!(
            std::fs::metadata(&f).unwrap().mode() & 0o7777,
            0o4755,
            "THE BUG: the restore chmod'd before it chown'd and setuid was cleared"
        );
    }
}
