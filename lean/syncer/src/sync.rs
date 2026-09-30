//! The sync verb (v1, HITL — decided §9 Q4), plus the scoped form the
//! `.flint/sync` sentinel invokes (boundary-verbs plan §2.2, D4).
//! Harness- or sentinel-invoked, never background.
//!
//! Sync BEGINS with a full scan: "locally dirty" means dirty per THAT
//! scan against the baseline — never per the last barrier's snapshot
//! (otherwise sync honors a remote delete over the agent's un-scanned
//! latest work: the review's steady-state destruction finding). Policy:
//! locally-dirty wins; remote deletions apply only to locally-clean
//! paths; adds/changes fetch through the store; every skipped apply is
//! a surfaced conflict, never silent.
//!
//! **The scope rule (D4) is a correctness rule, not an optimization.**
//! A scoped sync moves the baseline — which is also the merge base since
//! P1-lite — only for the paths it applied or verified in scope, and
//! leaves `baseline.seq` / `baseline.manifest_etag` UNTOUCHED. What it
//! leaves out stays OWED: the next barrier's consume derives it and takes
//! it, untouched from today. And a path the WORKSPACE'S scope declined
//! (a scoped checkout) is never fetched here either: the baseline does
//! not hold it, and only the held set and the scope's cover are owed.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use flint_store::{crc64_nvme, crc64_to_b64, GenerationStamps, PosixStamps, StoreError};

use super::barrier::{mtime_nanos_of, mtime_of};
use super::state::{BaselineEntry, ConflictRecord};
use super::{manifest, now_unix, scan, LeanError, LeanResult, Syncer};

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub applied: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<String>,
    pub seq: u64,
    /// Remote changes seen but left owed because they fell outside the
    /// requested scope (D4): the next barrier's consume takes those the
    /// tree holds. Zero for a whole-tree
    /// sync.
    #[serde(default)]
    pub out_of_scope_foreign: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
}

/// A normalized scope: path prefixes and exact paths, matched on
/// COMPONENT boundaries (`"in"` never matches `internal/`).
#[derive(Debug, Clone)]
pub struct Scope {
    entries: Vec<String>,
}

impl Scope {
    pub fn new(raw: &[String]) -> Scope {
        let mut entries = vec![];
        for e in raw.iter().take(super::sentinel::MAX_SCOPE_ENTRIES) {
            if e.len() > super::sentinel::MAX_SCOPE_ENTRY_LEN {
                continue;
            }
            let norm = e.trim_matches('/').replace('\\', "/");
            if norm.is_empty() || norm.split('/').any(|c| c == ".." || c == ".") {
                continue;
            }
            entries.push(norm);
        }
        Scope { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Component-boundary match: an entry covers itself exactly and
    /// everything beneath it, never a sibling sharing a name prefix.
    pub fn covers(&self, path: &str) -> bool {
        self.entries.iter().any(|e| {
            path == e.as_str()
                || path.strip_prefix(e.as_str()).map(|r| r.starts_with('/')).unwrap_or(false)
        })
    }
}

impl Syncer {
    /// Whole-tree sync: advances `seq` and the cheap path's record once it
    /// has taken everything owed; `manifest_etag` stays where the last
    /// barrier left it (`reader.rs` relies on that).
    pub async fn sync(&mut self) -> LeanResult<SyncReport> {
        self.sync_scoped(None).await
    }

    pub async fn sync_scoped(&mut self, scope: Option<Vec<String>>) -> LeanResult<SyncReport> {
        // An all-rejected scope is REFUSED, never widened. `Scope::new`
        // silently drops entries that are too long, past
        // MAX_SCOPE_ENTRIES, or contain `.`/`..` — so a caller naming
        // three malformed paths used to get `None`, and `in_scope`'s
        // `.unwrap_or(true)` turned that into the WHOLE TREE. A
        // whole-tree sync re-derives against the remote manifest and
        // DELETES local files for remotely-deleted paths, so the failure
        // mode of a typo was maximum privilege. An error must not
        // return a legal value.
        let scope = match scope {
            None => None,
            Some(raw) => {
                let s = Scope::new(&raw);
                if s.is_empty() {
                    return Err(LeanError::State(format!(
                        "sync scope named {} entr{} and NONE survived validation — \
                         refusing, because an empty scope widens to the WHOLE TREE",
                        raw.len(),
                        if raw.len() == 1 { "y" } else { "ies" }
                    )));
                }
                Some(s)
            }
        };
        let in_scope = |path: &str| scope.as_ref().map(|s| s.covers(path)).unwrap_or(true);
        let mut report = SyncReport::default();
        report.scope = scope.as_ref().map(|s| s.entries().to_vec());
        let mut baseline = self.state.load_baseline()?;

        // 1. The scan comes FIRST; dirt is judged against it alone.
        let scanned = scan::scan(&self.cfg.root)?;
        let classified = scan::classify(&scanned, &baseline);
        let locally_dirty = |path: &str| {
            classified.uploads.contains(path)
                || classified.deletes.contains(path)
                || classified.first_absence.contains(path)
        };

        // 2. Remote truth: the committed document (since P2 the gateway
        //    commits each UI verb, so there is no inbox overlay).
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        // The agent's paths this sync leaves untaken (dirty), for the
        // consume's cheap path: owed again if the agent backs out.
        let mut dirty_skips: BTreeSet<String> = BTreeSet::new();
        let (theirs, metag) = match loaded {
            Some(l) => (l.manifest, Some(l.etag)),
            // No document at all. For a tree that holds nothing that is a
            // new workspace, and there is nothing to do. For one that
            // holds paths it is a wiped or re-pointed prefix, and reading
            // it as an EMPTY document would delete every clean file here —
            // perhaps the last copy. The consume takes nothing in this
            // case; the sync refuses (review 2026-09-29, S1).
            None if baseline.entries.is_empty() => (Default::default(), None),
            None => {
                return Err(LeanError::State(format!(
                    "sync: the workspace has no document (no pointer, no manifest) but this tree's \
                     baseline holds {} paths — refusing to read a missing document as an empty \
                     one, which would delete every clean file here",
                    baseline.entries.len()
                )))
            }
        };
        // Something owed could not be taken now (a fetch that lost to a
        // newer document, a failed write): the next consume derives again
        // even if the pointer does not move — exactly the consume's `left`
        // (review 2026-09-29, S2/S3).
        let mut left = false;
        // Path -> (etag, HANDLE): the entry's own key, which is what the
        // fetch below reads (design 2026-09-19: a handle is fetched by
        // name; the bare path is nobody's to read).
        let remote: BTreeMap<String, (String, String)> =
            theirs.entries.iter().map(|(p, e)| (p.clone(), (e.etag.clone(), e.key.clone()))).collect();
        // The writer's CRC for each remote etag: the manifest's.
        let remote_crc: BTreeMap<String, Option<String>> = theirs
            .entries
            .iter()
            .map(|(p, e)| (p.clone(), Some(e.crc64_b64.clone())))
            .collect();

        // The workspace's own scope (a scoped checkout): a path it neither
        // holds nor covers is not this tree's to fetch.
        let held_scope = self.state.load_scope()?.map(|v| Scope::new(&v));
        let held = |b: &super::state::Baseline, p: &str| {
            b.entries.contains_key(p) || held_scope.as_ref().is_none_or(|s| s.covers(p))
        };

        // 3. Remote deletions: in the baseline, gone from the manifest —
        //    apply only on locally-clean paths.
        //
        //    BEFORE the adds, and that order is a correctness rule, not
        //    a tidiness one. A remote generation that turned the file
        //    `build` into the directory `build/log.txt` sends both a
        //    deletion and an add; with the adds first, the add is
        //    refused for containment (`sync-refused-containment`) while
        //    the file is still there, and the deletion then takes the
        //    file away — one sync leaves the workspace without either,
        //    and sync is harness-invoked, never background, so "the next
        //    sync fixes it" may be never. The two passes act on disjoint
        //    path sets (this one takes what is NOT in `remote`, that one
        //    what IS), so nothing else moves with the order.
        let base_paths: Vec<String> = baseline.entries.keys().cloned().collect();
        for path in base_paths {
            if remote.contains_key(&path) {
                continue;
            }
            if !in_scope(&path) {
                report.out_of_scope_foreign += 1;
                continue;
            }
            let local = self.cfg.root.join(&path);
            if !local.exists() {
                baseline.entries.remove(&path);
                continue;
            }
            super::barrier::consume_window("before-delete", &path);
            // H7: judged by the step-1 scan AND by a fresh stat now — a
            // write since the scan is the agent's, and it stays.
            if locally_dirty(&path) || super::barrier::local_dirty(&local, baseline.entries.get(&path)) {
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: "sync-remote-delete-vs-dirty".into(),
                    at_unix: now_unix(),
                })?;
                report.conflicts.push(path.clone());
                dirty_skips.insert(path.clone());
                continue;
            }
            std::fs::remove_file(&local)?;
            baseline.entries.remove(&path);
            report.deleted.push(path.clone());
        }

        // 4. Apply adds/changes (remote differs from the baseline).
        for (path, (etag, key)) in &remote {
            if baseline.entries.get(path).map(|b| &b.etag == etag).unwrap_or(false) {
                continue;
            }
            if !held(&baseline, path) {
                continue;
            }
            if !in_scope(path) {
                // D4: NOT integrated — it stays owed, and the next
                // barrier's consume takes it.
                report.out_of_scope_foreign += 1;
                continue;
            }
            if locally_dirty(path) {
                // The phantom-conflict rule (§2.2): `sync` saves the
                // baseline only at the end, so a crash mid-apply
                // followed by a re-honor makes already-applied paths
                // scan dirty against the stale baseline. Declaring a
                // conflict there would report a conflict for a path
                // whose local bytes ARE the remote bytes, and the path
                // would then re-publish as a spurious generation bump.
                // Compare content identity first.
                // The remote's crc, read off the object we would apply
                // (the backend's attestation) before the manifest's.
                // One HEAD, only on the dirty-vs-remote path.
                let remote_meta = match self.store.head(key).await {
                    Ok(m) if m.etag == *etag => Some(m),
                    _ => None,
                };
                // The remote's CRC: the backend's attestation when it
                // offers one, else the writer's for this etag — client-
                // computed, so it exists on a backend that attests none
                // (Ozone). Without either the bytes cannot be judged
                // identical, and the path is a conflict as before.
                let want: Option<String> = remote_meta.as_ref().and_then(|m| {
                    m.crc64_b64.clone().or_else(|| remote_crc.get(path).cloned().flatten())
                });
                let local_path = self.cfg.root.join(path);
                let local_crc =
                    std::fs::read(&local_path).ok().map(|b| crc64_to_b64(crc64_nvme(&b)));
                let identical = want.is_some() && want == local_crc;
                if identical {
                    let st = std::fs::metadata(&local_path)?;
                    let stamps =
                        remote_meta.as_ref().and_then(|m| GenerationStamps::from_meta(&m.meta));
                    baseline.entries.insert(
                        path.clone(),
                        BaselineEntry {
                            etag: etag.clone(),
                            key: Some(key.clone()),
                            generation: stamps
                                .map(|s| s.generation)
                                .or_else(|| theirs.entries.get(path).map(|e| e.generation))
                                .unwrap_or(0),
                            size: st.len(),
                            mtime_unix: mtime_of(&st),
                            mtime_nanos: Some(mtime_nanos_of(&st)),
                            crc64_b64: local_crc,
                        },
                    );
                        report.applied.push(path.clone());
                    continue;
                }
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: etag.clone(),
                    preserved_key: None, // remote version stays in the bucket
                    kind: "sync-dirty".into(),
                    at_unix: now_unix(),
                })?;
                report.conflicts.push(path.clone());
                dirty_skips.insert(path.clone());
                continue;
            }
            let fetched = self.store.get_whole(key, Some(etag)).await;
            let (meta, body) = match fetched {
                Ok(ok) => ok,
                Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                    // Superseded mid-sync, or not readable yet: owed still.
                    left = true;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let mode = PosixStamps::from_meta(&meta.meta).map(|p| p.mode);
            // VERIFIED before it is written, exactly as checkout's fresh
            // fetch is: against the writer's CRC for this etag (the
            // manifest's), else the backend's attestation when it offers
            // one. What the baseline records is OURS, over the bytes
            // written.
            let got = crc64_to_b64(crc64_nvme(&body));
            let want =
                remote_crc.get(path).cloned().flatten().or_else(|| meta.crc64_b64.clone());
            if let Some(want) = want {
                if want != got {
                    return Err(LeanError::State(format!(
                        "sync: {path} (etag {etag}) is cited with CRC-64 {want}, but the bytes \
                         fetched under that etag hash to {got} — the object is corrupt or the \
                         store returned the wrong bytes; refusing to apply it (nothing written)"
                    )));
                }
            }
            // A path this tree can never safely materialise is refused and
            // NOT left owed (as the consume does); a write that fails for
            // any other reason below is transient and stays owed.
            if let Err(e) = super::barrier::check_contained(&self.cfg.root, path) {
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: etag.clone(),
                    preserved_key: None,
                    kind: format!("sync-refused-containment: {e}"),
                    at_unix: now_unix(),
                })?;
                report.conflicts.push(path.clone());
                continue;
            }
            super::barrier::consume_window("before-write", path);
            // Review 2026-09-18, H7: dirt was judged by the scan at step 1,
            // and a whole-tree sync fetches for minutes after it; a path
            // the agent writes since then is modified, and its version
            // wins. So the licence is checked once more against a fresh
            // stat, with the temp written, immediately before the rename.
            let local = self.cfg.root.join(path);
            let base = baseline.entries.get(path).cloned();
            let still_clean = || !super::barrier::local_dirty(&local, base.as_ref());
            let written = super::barrier::contained_path(&self.cfg.root, path)
                .and_then(|target| super::barrier::write_file_atomic_if(&target, &body, mode, &still_clean));
            let st = match written {
                Ok(Some(st)) => st,
                Ok(None) => {
                    self.state.append_conflict(&ConflictRecord {
                        path: path.clone(),
                        foreign_etag: etag.clone(),
                        preserved_key: None, // remote version stays in the bucket
                        kind: "sync-dirty".into(),
                        at_unix: now_unix(),
                    })?;
                    report.conflicts.push(path.clone());
                    dirty_skips.insert(path.clone());
                    continue;
                }
                Err(e) => {
                    // TRANSIENT (a full disk, a directory it cannot write
                    // to): surfaced, never a wedge, and still owed.
                    self.state.append_conflict(&ConflictRecord {
                        path: path.clone(),
                        foreign_etag: etag.clone(),
                        preserved_key: None,
                        kind: format!("sync-write-failed (will retry): {e}"),
                        at_unix: now_unix(),
                    })?;
                    report.conflicts.push(path.clone());
                    left = true;
                    continue;
                }
            };
            // The baseline records the inode this sync wrote (its fstat),
            // never a fresh stat of the path an agent may have written
            // since (H7).
            super::barrier::consume_window("after-rename", path);
            let stamps = GenerationStamps::from_meta(&meta.meta);
            baseline.entries.insert(
                path.clone(),
                BaselineEntry {
                    etag: meta.etag.clone(),
                    key: Some(key.clone()),
                    generation: stamps.map(|s| s.generation).unwrap_or(0),
                    size: st.len(),
                    mtime_unix: mtime_of(&st),
                    mtime_nanos: Some(mtime_nanos_of(&st)),
                    crc64_b64: Some(got),
                },
            );
            report.applied.push(path.clone());
        }

        // 5. What this sync leaves owed. Whole-tree, the sync derived against
        //    this document: what it left is the agent's (dirty) and is owed
        //    the moment the agent backs out — the consume's cheap path record,
        //    as a consume would write it. Scoped, it derived only part: the
        //    next consume derives again (D4 keeps manifest_etag where it was).
        match &scope {
            None if !left => {
                baseline.seq = theirs.seq;
                baseline.derived_etag = Some(metag.clone().unwrap_or_default());
                baseline.skipped = dirty_skips;
                report.seq = theirs.seq;
            }
            // Something is still owed: nothing is recorded as derived, and
            // the tree is not integrated with this document yet.
            None => {
                baseline.derived_etag = None;
                baseline.skipped.clear();
                report.seq = baseline.seq;
            }
            Some(_) => {
                baseline.derived_etag = None;
                baseline.skipped.clear();
                report.seq = baseline.seq;
            }
        }
        let rescan = scan::scan(&self.cfg.root)?;
        baseline.prev_scan = rescan.keys().cloned().collect();
        // Materialised files before the baseline that vouches for them.
        self.state.sync_tree()?;
        self.state.save_baseline(&baseline)?;
        self.trace("sync", serde_json::json!({"scoped": scope.is_some(), "applied": report.applied.len(),
            "deleted": report.deleted.len(), "conflicts": report.conflicts.len(), "seq": report.seq}));
        Ok(report)
    }
}
