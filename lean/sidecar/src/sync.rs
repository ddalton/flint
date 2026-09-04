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
//! A scoped sync advances `inst_base` only for the paths it actually
//! applied or verified in scope, and leaves `baseline.seq` /
//! `baseline.manifest_etag` UNTOUCHED. `inst_base` is the three-way
//! merge base; if a scoped sync advanced the whole merge base to
//! bucket-current, every out-of-scope foreign change would look
//! already-integrated to the next merge and would be silently lost from
//! the inbox flow forever. With D4 those changes remain "foreign" and
//! flow through the normal merge → inbox → consume path at the next
//! barrier, untouched from today.

use std::collections::BTreeMap;

use serde::Serialize;

use flint_store::{crc64_nvme, crc64_to_b64, GenerationStamps, PosixStamps, StoreError};

use super::barrier::{mtime_of, write_file_atomic_in};
use super::state::{BaselineEntry, ConflictRecord};
use super::{inbox, manifest, now_unix, scan, LeanResult, Sidecar};

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub applied: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<String>,
    pub seq: u64,
    /// Remote changes seen but deferred to the inbox flow because they
    /// fell outside the requested scope (D4). Zero for a whole-tree
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

impl Sidecar {
    /// Whole-tree sync — exactly as shipped, including advancing
    /// `seq`/`manifest_etag`.
    pub async fn sync(&mut self) -> LeanResult<SyncReport> {
        self.sync_scoped(None).await
    }

    pub async fn sync_scoped(&mut self, scope: Option<Vec<String>>) -> LeanResult<SyncReport> {
        let scope = scope.map(|s| Scope::new(&s)).filter(|s| !s.is_empty());
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

        // 2. Remote truth: manifest + inbox overlay (an inbox entry is
        //    a write the manifest has not re-cited yet).
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        let (theirs, _metag) = match loaded {
            Some(l) => (l.manifest, Some(l.etag)),
            None => (Default::default(), None),
        };
        let ib = inbox::load(self.store.as_ref(), &self.cfg).await?;
        let mut remote: BTreeMap<String, String> =
            theirs.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        for e in &ib.doc.entries {
            remote.insert(e.path.clone(), e.etag.clone());
        }

        /// Paths whose `inst_base` this sync is entitled to advance:
        /// under D4, exactly those it applied or verified in scope.
        struct Advanced(std::collections::BTreeSet<String>);
        let mut advanced = Advanced(Default::default());

        // 3. Apply adds/changes (remote differs from OUR merge base).
        for (path, etag) in &remote {
            let base = baseline.inst_base.get(path);
            let unchanged_remotely = base.map(|b| b == etag).unwrap_or(false);
            if unchanged_remotely {
                advanced.0.insert(path.clone());
                continue;
            }
            if !in_scope(path) {
                // D4: NOT integrated, NOT advanced — it stays foreign
                // and reaches this workspace through the next barrier's
                // merge → inbox → consume path.
                report.out_of_scope_foreign += 1;
                continue;
            }
            if baseline.entries.get(path).map(|b| &b.etag == etag).unwrap_or(false) {
                advanced.0.insert(path.clone()); // already integrated (e.g. our own publish)
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
                // The remote entry's crc must come from the object we
                // would actually apply, not from the manifest: when
                // remote truth is an INBOX overlay, the manifest entry
                // is a generation behind and its crc would never match.
                // One HEAD, only on the dirty-vs-remote path.
                let remote_meta = match self.store.head(&self.cfg.file_key(path)).await {
                    Ok(m) if m.etag == *etag => Some(m),
                    _ => None,
                };
                let local_path = self.cfg.root.join(path);
                let identical = match (&remote_meta, std::fs::read(&local_path)) {
                    (Some(m), Ok(bytes)) => {
                        m.crc64_b64.as_deref() == Some(crc64_to_b64(crc64_nvme(&bytes)).as_str())
                    }
                    _ => false,
                };
                if identical {
                    let st = std::fs::metadata(&local_path)?;
                    let stamps =
                        remote_meta.as_ref().and_then(|m| GenerationStamps::from_meta(&m.meta));
                    baseline.entries.insert(
                        path.clone(),
                        BaselineEntry {
                            etag: etag.clone(),
                            generation: stamps
                                .map(|s| s.generation)
                                .or_else(|| theirs.entries.get(path).map(|e| e.generation))
                                .unwrap_or(0),
                            size: st.len(),
                            mtime_unix: mtime_of(&st),
                            version_id: None,
                        },
                    );
                    advanced.0.insert(path.clone());
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
                continue;
            }
            let key = self.cfg.file_key(path);
            // D13: under a gated citation this reader resolves the
            // CITED version, never the current one — the same rule
            // checkout follows, for the same reason. An inbox-overlaid
            // path has no cited version yet and takes the etag path.
            let pinned_version = if theirs.pinned_reads {
                theirs
                    .entries
                    .get(path)
                    .filter(|e| &e.etag == etag)
                    .and_then(|e| e.version_id.clone())
            } else {
                None
            };
            let fetched = match &pinned_version {
                Some(vid) => self.store.get_version(&key, vid).await,
                None => self.store.get_whole(&key, Some(etag)).await,
            };
            let (meta, body) = match fetched {
                Ok(ok) => ok,
                Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                    continue; // superseded mid-sync; the next sync sees the newer truth
                }
                Err(e) => return Err(e.into()),
            };
            let mode = PosixStamps::from_meta(&meta.meta).map(|p| p.mode);
            if let Err(e) = write_file_atomic_in(&self.cfg.root, path, &body, mode) {
                // Containment refusal: surfaced, never a wedge.
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
            let local = self.cfg.root.join(path);
            let st = std::fs::metadata(&local)?;
            let stamps = GenerationStamps::from_meta(&meta.meta);
            baseline.entries.insert(
                path.clone(),
                BaselineEntry {
                    etag: meta.etag.clone(),
                    generation: stamps.map(|s| s.generation).unwrap_or(0),
                    size: st.len(),
                    mtime_unix: mtime_of(&st),
                    version_id: None,
                },
            );
            advanced.0.insert(path.clone());
            report.applied.push(path.clone());
        }

        // 4. Remote deletions: in our merge base, gone from the
        //    manifest, and not overlaid by an inbox entry — apply only
        //    on locally-clean paths.
        let base_paths: Vec<String> = baseline.inst_base.keys().cloned().collect();
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
                advanced.0.insert(path.clone());
                continue;
            }
            if locally_dirty(&path) {
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: "sync-remote-delete-vs-dirty".into(),
                    at_unix: now_unix(),
                })?;
                report.conflicts.push(path.clone());
                continue;
            }
            std::fs::remove_file(&local)?;
            baseline.entries.remove(&path);
            advanced.0.insert(path.clone());
            report.deleted.push(path.clone());
        }

        // 5. Advance the merge base.
        //
        //    Whole-tree: to the manifest we synced against, exactly as
        //    shipped. Scoped (D4): ONLY for the paths this sync applied
        //    or verified, and seq/manifest_etag stay put — otherwise
        //    every out-of-scope foreign change reads as
        //    already-integrated at the next merge and is lost.
        let theirs_base: BTreeMap<String, String> =
            theirs.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        match &scope {
            None => {
                baseline.seq = theirs.seq;
                baseline.inst_base = theirs_base;
                report.seq = theirs.seq;
            }
            Some(_) => {
                for path in &advanced.0 {
                    match theirs_base.get(path) {
                        Some(etag) => {
                            baseline.inst_base.insert(path.clone(), etag.clone());
                        }
                        None => {
                            baseline.inst_base.remove(path);
                        }
                    }
                }
                report.seq = baseline.seq;
            }
        }
        let rescan = scan::scan(&self.cfg.root)?;
        baseline.prev_scan = rescan.keys().cloned().collect();
        // Materialised files before the baseline that vouches for them.
        self.state.sync_tree()?;
        self.state.save_baseline(&baseline)?;
        Ok(report)
    }
}
