//! Checkout + the restart matrix (plan §2.1).
//!
//! | State on wake            | Action                                  |
//! |--------------------------|-----------------------------------------|
//! | No marker, empty tree    | full checkout                           |
//! | No marker, partial tree  | resume (local-wins skips present paths) |
//! | Marker present           | NEVER re-materialize: reload baseline,  |
//! |                          | rescan rebuilds dirt, lease self-       |
//! |                          | recognizes via the persisted id         |
//!
//! Re-checkout over a live tree is forbidden: local-wins protects only
//! PRESENT paths, so it would resurrect the agent's unpublished deletes
//! (`LeanRematerialize.cfg` rediscovers exactly that). The marker is
//! written LAST — it is the agent-start gate.

use std::collections::BTreeSet;

use flint_store::StoreError;

use super::barrier::{mtime_of, write_file_atomic};
use super::manifest;
use super::state::BaselineEntry;
use super::{LeanError, LeanResult, Sidecar};

#[derive(Debug, Default)]
pub struct CheckoutReport {
    pub materialized: usize,
    pub skipped_present: usize,
    pub bytes: u64,
    /// Restart-matrix row taken.
    pub resumed_live_tree: bool,
}

impl Sidecar {
    /// Materialize the workspace from the manifest. Idempotent across
    /// crashes (resume skips present paths); refuses over budget
    /// BEFORE the first byte.
    pub async fn checkout(&mut self) -> LeanResult<CheckoutReport> {
        let mut report = CheckoutReport::default();
        if self.state.marker_present() {
            // The live-tree row: never re-materialize.
            report.resumed_live_tree = true;
            return Ok(report);
        }

        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        let mut baseline = self.state.load_baseline()?;
        let (m, metag) = match loaded {
            Some(l) => (l.manifest, Some(l.etag)),
            None => (Default::default(), None),
        };

        // Budgets: refuse before materializing anything.
        let total_bytes: u64 = m.entries.values().map(|e| e.size).sum();
        if self.cfg.max_bytes > 0 && total_bytes > self.cfg.max_bytes {
            return Err(LeanError::Budget(format!(
                "checkout is {} bytes; budget {}",
                total_bytes, self.cfg.max_bytes
            )));
        }
        if self.cfg.max_files > 0 && m.entries.len() as u64 > self.cfg.max_files {
            return Err(LeanError::Budget(format!(
                "checkout is {} files; budget {}",
                m.entries.len(),
                self.cfg.max_files
            )));
        }

        let mut present: BTreeSet<String> = BTreeSet::new();
        for (path, entry) in &m.entries {
            let local = self.cfg.root.join(path);
            if local.exists() {
                // Resume: local-wins on present paths.
                report.skipped_present += 1;
                present.insert(path.clone());
                let st = std::fs::metadata(&local)?;
                baseline.entries.insert(
                    path.clone(),
                    BaselineEntry {
                        etag: entry.etag.clone(),
                        generation: entry.generation,
                        size: st.len(),
                        mtime_unix: mtime_of(&st),
                    },
                );
                continue;
            }
            let (meta, body) =
                match self.store.get_whole(&entry.key, Some(&entry.etag)).await {
                    Ok(ok) => ok,
                    Err(StoreError::PreconditionFailed(_)) => {
                        // S3-wins: the object moved past the manifest
                        // (a HITL write not yet re-cited). Adopt the
                        // CURRENT version — its inbox entry will
                        // reconcile the manifest at the next barrier.
                        self.store.get_whole(&entry.key, None).await?
                    }
                    Err(StoreError::NotFound(_)) => {
                        return Err(LeanError::State(format!(
                            "manifest cites {} but the object is gone — refusing a silent hole \
                             (mixed-writer bucket?)",
                            entry.key
                        )));
                    }
                    Err(e) => return Err(e.into()),
                };
            write_file_atomic(&local, &body, Some(entry.mode))?;
            let st = std::fs::metadata(&local)?;
            baseline.entries.insert(
                path.clone(),
                BaselineEntry {
                    etag: meta.etag.clone(),
                    generation: entry.generation,
                    size: st.len(),
                    mtime_unix: mtime_of(&st),
                },
            );
            present.insert(path.clone());
            report.materialized += 1;
            report.bytes += body.len() as u64;
        }

        baseline.seq = m.seq;
        baseline.manifest_etag = metag;
        baseline.inst_base = m.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        baseline.prev_scan = present;
        self.state.save_baseline(&baseline)?;
        // The marker is written LAST: the agent-start gate.
        self.state.write_marker()?;
        Ok(report)
    }
}
