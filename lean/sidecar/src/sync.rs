//! The sync verb (v1, HITL — decided §9 Q4). Harness-invoked, never
//! background.
//!
//! Sync BEGINS with a full scan: "locally dirty" means dirty per THAT
//! scan against the baseline — never per the last barrier's snapshot
//! (otherwise sync honors a remote delete over the agent's un-scanned
//! latest work: the review's steady-state destruction finding). Policy:
//! locally-dirty wins; remote deletions apply only to locally-clean
//! paths; adds/changes fetch through the store; every skipped apply is
//! a surfaced conflict, never silent.

use std::collections::BTreeMap;

use serde::Serialize;

use flint_store::{GenerationStamps, PosixStamps, StoreError};

use super::barrier::{mtime_of, write_file_atomic};
use super::state::{BaselineEntry, ConflictRecord};
use super::{inbox, manifest, now_unix, scan, LeanResult, Sidecar};

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub applied: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<String>,
    pub seq: u64,
}

impl Sidecar {
    pub async fn sync(&mut self) -> LeanResult<SyncReport> {
        let mut report = SyncReport::default();
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

        // 3. Apply adds/changes (remote differs from OUR merge base).
        for (path, etag) in &remote {
            let base = baseline.inst_base.get(path);
            let unchanged_remotely = base.map(|b| b == etag).unwrap_or(false);
            if unchanged_remotely {
                continue;
            }
            if baseline.entries.get(path).map(|b| &b.etag == etag).unwrap_or(false) {
                continue; // already integrated (e.g. our own publish)
            }
            if locally_dirty(path) {
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
            let (meta, body) = match self.store.get_whole(&key, Some(etag)).await {
                Ok(ok) => ok,
                Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                    continue; // superseded mid-sync; the next sync sees the newer truth
                }
                Err(e) => return Err(e.into()),
            };
            let local = self.cfg.root.join(path);
            let mode = PosixStamps::from_meta(&meta.meta).map(|p| p.mode);
            write_file_atomic(&local, &body, mode)?;
            let st = std::fs::metadata(&local)?;
            let stamps = GenerationStamps::from_meta(&meta.meta);
            baseline.entries.insert(
                path.clone(),
                BaselineEntry {
                    etag: meta.etag.clone(),
                    generation: stamps.map(|s| s.generation).unwrap_or(0),
                    size: st.len(),
                    mtime_unix: mtime_of(&st),
                },
            );
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
            let local = self.cfg.root.join(&path);
            if !local.exists() {
                baseline.entries.remove(&path);
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
            report.deleted.push(path.clone());
        }

        // 5. Advance the merge base to the manifest we synced against;
        //    the baseline advanced per-path above.
        baseline.seq = theirs.seq;
        baseline.inst_base =
            theirs.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        let rescan = scan::scan(&self.cfg.root)?;
        baseline.prev_scan = rescan.keys().cloned().collect();
        self.state.save_baseline(&baseline)?;
        report.seq = theirs.seq;
        Ok(report)
    }
}
