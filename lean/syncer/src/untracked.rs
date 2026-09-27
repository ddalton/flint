//! Finding 10: an upload whose commit never came.
//!
//! Uploads hold no lease. A writer lost for good between an upload and its
//! commit — its pod replaced, its node gone — leaves bytes at a fresh
//! handle, and nothing tracks them: no manifest cites them, and the writer
//! that would have cited them is gone.
//! Nothing reads them either — every checkout and consume fetches the
//! handle the manifest cites (the S3-wins arm that once served such bytes
//! is gone) — so the work they hold is lost unless something cites it.
//!
//! The sweep tracks such an object the way the gateway takes a UI save
//! (P2, 2026-09-25): it COMMITS a copy, and every writer's tree takes it as
//! any peer's change (adopted where the path is clean, preserved beside the
//! agent's edit where it is not). The formal model checks the rule: tracked at
//! ANY time, a live writer's upload still in flight included, every
//! invariant holds (`LeanBarrierLeaseOrphanTracked`), so the grace below is
//! churn control rather than a safety margin; without it, a world in which
//! every syncer has gone quiet still holds such bytes
//! (`LeanBarrierLeaseOrphanDiverges`).

use std::collections::HashMap;

use flint_store::{crc64_nvme, crc64_to_b64, GenerationStamps, PutCondition, StoreError};

use super::{manifest, now_unix, LeanResult, Syncer};

/// How long an object nothing cites may sit before a sweep takes it —
/// the ingress sweep's and the orphan sweep's grace, seconds. Churn control,
/// not a safety margin: the commit re-reads what it cites (R4a).
pub const UNTRACKED_GRACE_SECS: u64 = 600;

/// The flush id of an ingress object's adopted copy: named after the
/// ingress etag, so the copy of one ingress version has one handle and
/// a second sweep finds it rather than minting another.
pub fn ingress_flush(etag: &str) -> String {
    let clean: String = etag.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("ingress-{clean}")
}

impl Syncer {
    /// The INGRESS sweep (immutable-handles design 2026-09-19, R5): track
    /// every object at a BARE path key under `files/` — an outside
    /// writer's `aws s3 cp`, the one write nothing in this crate makes any
    /// more — that has sat there for at least `untracked_grace_secs` and
    /// that no entry has adopted. The object is copied server-side to a
    /// handle of its own, named after the ingress etag so a second sweep
    /// finds its copy already there, and ONE manifest CAS cites the copy
    /// (`manifest::commit_edit`, as the gateway commits a save) — only while
    /// the path still cites what the sweep saw; a citation that moved since
    /// leaves the object to the next sweep. The ingress
    /// object itself is never deleted and never needs a conditional
    /// DELETE: a later outside overwrite is a new ingress with a new etag,
    /// adopted the same way.
    ///
    /// Before handles this sweep tracked a lost writer's upload at the
    /// cited key (finding 10). Under handles such an upload is an orphan
    /// HANDLE, and the orphan sweep collects it (R4) — never adopts it.
    ///
    /// One manifest read and a LIST of the files prefix, then one commit per
    /// object adopted. Returns the paths adopted.
    pub async fn track_untracked(&self, now: u64) -> LeanResult<Vec<String>> {
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        let cited: HashMap<&str, &str> = loaded
            .as_ref()
            .map(|l| l.manifest.entries.iter().map(|(p, e)| (p.as_str(), e.key.as_str())).collect())
            .unwrap_or_default();
        let listed = self.store.list(&self.cfg.files_prefix()).await?;
        let mut tracked = vec![];
        for obj in listed {
            // A handle is a writer's or the gateway's, never ingress.
            if self.cfg.handle_parts(&obj.key).is_some() {
                continue;
            }
            let Some(path) = obj.key.strip_prefix(&self.cfg.files_prefix()) else { continue };
            if path.is_empty() || path.ends_with('/') {
                continue;
            }
            // No modification time, no judgement of age: leave it.
            let Some(modified) = obj.last_modified_unix else { continue };
            if now.saturating_sub(modified) < self.cfg.untracked_grace_secs {
                continue;
            }
            let handle = self.cfg.handle_key(path, &ingress_flush(&obj.etag));
            // Adopted already: its copy is cited.
            if cited.get(path).is_some_and(|k| *k == handle.as_str()) {
                continue;
            }
            let stamps = GenerationStamps {
                generation: 0,
                epoch: 0,
                flush_uuid: ingress_flush(&obj.etag),
                boundary_source: None,
                posix: None,
            };
            let copied = match self
                .store
                .copy_object(&obj.key, Some(&obj.etag), &handle, &PutCondition::IfNoneMatchAny, &stamps)
                .await
            {
                Ok(m) => m,
                // The copy is already there (a sweep that died before its
                // append): use it as it stands.
                Err(StoreError::PreconditionFailed(_)) => match self.store.head(&handle).await {
                    Ok(m) => m,
                    // The ingress object moved between the LIST and the
                    // copy: a new etag, the next sweep's business.
                    Err(StoreError::NotFound(_)) => continue,
                    Err(e) => return Err(e.into()),
                },
                Err(StoreError::NotFound(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            // Cited entries carry a CRC every reader verifies against: the
            // backend's attestation, else the bytes' own, read once.
            let crc64_b64 = match copied.crc64_b64.clone() {
                Some(c) => c,
                None => {
                    let (_, bytes) = self.store.get_whole(&handle, Some(&copied.etag)).await?;
                    crc64_to_b64(crc64_nvme(&bytes))
                }
            };
            let seen = cited.get(path).map(|k| k.to_string());
            let committed = manifest::commit_edit(self.store.as_ref(), &self.cfg, &ingress_flush(&obj.etag), |_, doc| {
                let was = doc.entries.get(path);
                if was.map(|e| e.key.clone()) != seen {
                    return Err(()); // the citation moved since the sweep looked
                }
                let entry = manifest::LeanEntry {
                    key: handle.clone(),
                    etag: copied.etag.clone(),
                    crc64_b64: crc64_b64.clone(),
                    size: copied.size,
                    mode: was.map(|e| e.mode).unwrap_or(0o644),
                    mtime_unix: now_unix() as i64,
                    generation: was.map(|e| e.generation).unwrap_or(0) + 1,
                    epoch: 0,
                };
                doc.tombstones.remove(path);
                doc.entries.insert(path.to_string(), entry);
                Ok(())
            })
            .await;
            match committed {
                Ok(()) => {
                    self.trace("untracked", serde_json::json!({"path": path, "etag": obj.etag, "handle": handle,
                        "cited": seen}));
                    tracked.push(path.to_string());
                }
                Err(manifest::EditError::Refused(())) | Err(manifest::EditError::Contended) => continue,
                Err(manifest::EditError::Store(e)) => return Err(e),
            }
        }
        Ok(tracked)
    }

    /// The floor's hook: sweep when `untracked_sweep_secs` have passed since
    /// the last sweep (0 = never). A writer's first tick starts the clock
    /// instead of sweeping — its own checkout has just read every citation.
    /// Best-effort: a sweep that fails is retried on the next tick and never
    /// fails the floor.
    pub async fn sweep_untracked_if_due(&self, now: u64) {
        if self.cfg.untracked_sweep_secs == 0 || self.cfg.access.is_read() {
            return;
        }
        let last = match self.state.load_untracked_sweep_at() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("flint-sync: untracked sweep clock unreadable (skipping): {e}");
                return;
            }
        };
        if last == 0 {
            let _ = self.state.save_untracked_sweep_at(now);
            return;
        }
        if now.saturating_sub(last) < self.cfg.untracked_sweep_secs {
            return;
        }
        match self.track_untracked(now).await {
            Ok(paths) => {
                if !paths.is_empty() {
                    eprintln!(
                        "flint-sync: tracked {} upload(s) no commit cited, left by a lost writer: {paths:?}",
                        paths.len()
                    );
                }
                let _ = self.state.save_untracked_sweep_at(now);
            }
            Err(e) => eprintln!("flint-sync: untracked sweep failed (retrying next tick): {e}"),
        }
    }

    /// The ORPHAN sweep (immutable-handles design 2026-09-19, R4): a
    /// handle the installed document does not cite, no inbox entry names,
    /// and whose writer never committed — its pod replaced between an
    /// upload and its CAS — is garbage, and is collected, never adopted.
    ///
    /// Runs INSIDE a commit section, under the cell: a live writer's
    /// uploads precede its claim and carry no lease, so the only thing
    /// that keeps a sweep from taking one is that the sweeper holds the
    /// cell while that writer's commit re-reads its own uploads before its
    /// CAS (`LeanImmutableSweepLeaseFree` is the sweep without the cell;
    /// `LeanImmutableCasCitesBlind` the commit without the re-read). The
    /// grace is churn control, not a safety margin: a live writer whose
    /// upload sat longer than it is withheld at its commit and re-uploads.
    /// This barrier's own handles are never candidates. Bare keys — the
    /// ingress namespace — are not handles and are left to
    /// `track_untracked`.
    ///
    /// One LIST of the files prefix per sweep, on the untracked sweep's
    /// cadence (`untracked_sweep_secs`); best effort by design.
    pub(crate) async fn sweep_orphans_if_due(
        &self,
        installed: &manifest::LeanManifest,
        own_flush: &str,
        now: u64,
        spared: &std::collections::HashSet<String>,
    ) -> LeanResult<usize> {
        if self.cfg.untracked_sweep_secs == 0 {
            return Ok(0);
        }
        let last = self.state.load_orphan_sweep_at()?;
        if last == 0 {
            // A writer's first commit starts the clock instead of sweeping.
            self.state.save_orphan_sweep_at(now)?;
            return Ok(0);
        }
        if now.saturating_sub(last) < self.cfg.untracked_sweep_secs {
            return Ok(0);
        }
        let n = self.sweep_orphans(installed, own_flush, now, spared).await?;
        self.state.save_orphan_sweep_at(now)?;
        Ok(n)
    }

    /// The sweep itself, whenever the caller decides it is due.
    pub(crate) async fn sweep_orphans(
        &self,
        installed: &manifest::LeanManifest,
        own_flush: &str,
        now: u64,
        spared: &std::collections::HashSet<String>,
    ) -> LeanResult<usize> {
        let cited: std::collections::HashSet<&str> =
            installed.entries.values().map(|e| e.key.as_str()).collect();
        let listed = self.store.list(&self.cfg.files_prefix()).await?;
        let mut orphans: Vec<String> = vec![];
        for obj in listed {
            let Some((_, flush)) = self.cfg.handle_parts(&obj.key) else { continue };
            // Retired less than G ago (M1): a lagging reader may still fetch it.
            if flush == own_flush || cited.contains(obj.key.as_str()) || spared.contains(&obj.key) {
                continue;
            }
            // No modification time, no judgement of age: leave it.
            let Some(modified) = obj.last_modified_unix else { continue };
            if now.saturating_sub(modified) < self.cfg.untracked_grace_secs {
                continue;
            }
            orphans.push(obj.key);
        }
        if orphans.is_empty() {
            return Ok(0);
        }
        let report = self.store.delete_many(&orphans).await?;
        // The handles taken, named: a trace check maps each to the version
        // it held (`TraceCore.tla`'s Sweep takes one handle per step).
        let taken: Vec<&String> = orphans.iter().filter(|k| !report.failed.iter().any(|(f, _)| f == *k)).collect();
        self.trace("sweep", serde_json::json!({"what": "orphans", "candidates": orphans.len(),
            "removed": report.deleted, "refused": report.failed.len(), "keys": taken}));
        if report.deleted > 0 {
            eprintln!(
                "flint-sync: collected {} orphan handle(s) no commit cited, left by a lost writer",
                report.deleted
            );
        }
        Ok(report.deleted)
    }
}

/// The retire logs as a writer's sweeps read them (M1, `manifest::RetireLog`).
#[derive(Default)]
pub(crate) struct RetireLedger {
    /// Every handle and chunk KEY a log younger than the retire age names:
    /// the orphan sweep and the chunk reaper spare these.
    pub spared: std::collections::HashSet<String>,
    /// The logs at least that old, by key.
    pub due: Vec<(String, manifest::RetireLog)>,
}

impl Syncer {
    /// List the retire logs and read the ones not read before. One LIST per
    /// commit section; a GET per log, once.
    pub(crate) async fn retire_ledger(&self, now: u64) -> LeanResult<RetireLedger> {
        let listed = self.store.list(&manifest::retired_prefix(&self.cfg)).await?;
        let mut cache = self.state.load_retire_cache();
        let present: std::collections::BTreeSet<&str> = listed.iter().map(|o| o.key.as_str()).collect();
        cache.retain(|k, _| present.contains(k.as_str()));
        let mut ledger = RetireLedger::default();
        for o in &listed {
            let Some(at) = manifest::retire_log_at(&self.cfg, &o.key) else { continue };
            let log = match cache.get(&o.key) {
                Some(l) => l.clone(),
                None => match self.store.get_whole(&o.key, None).await {
                    // A log nobody can parse spares nothing and is reaped
                    // when due: its handles fall to the write-age rule.
                    Ok((_, body)) => serde_json::from_slice(&body).unwrap_or_default(),
                    Err(StoreError::NotFound(_)) => continue,
                    Err(e) => return Err(e.into()),
                },
            };
            cache.insert(o.key.clone(), log.clone());
            if now.saturating_sub(at) >= self.cfg.retire_grace_secs {
                ledger.due.push((o.key.clone(), log));
            } else {
                ledger.spared.extend(log.handles.iter().cloned());
                ledger.spared.extend(log.chunks.iter().map(|a| self.cfg.chunk_key(a)));
            }
        }
        self.state.save_retire_cache(&cache)?;
        Ok(ledger)
    }

    /// Collect the handles the due logs name — a retired handle is never
    /// cited again (a rename keeps its handle cited, so never retires it),
    /// and `installed` is checked anyway — then the logs themselves. A due
    /// log's CHUNKS are only no longer spared: the chunk reaper, with its
    /// fence, decides them. Returns the handles collected.
    pub(crate) async fn reap_retired(
        &self,
        ledger: &RetireLedger,
        installed: &manifest::LeanManifest,
        own_flush: &str,
    ) -> LeanResult<usize> {
        let cited: std::collections::HashSet<&str> = installed.entries.values().map(|e| e.key.as_str()).collect();
        let mut handles: Vec<String> = ledger
            .due
            .iter()
            .flat_map(|(_, l)| l.handles.iter().cloned())
            .filter(|k| !cited.contains(k.as_str()) && !ledger.spared.contains(k))
            .collect();
        handles.sort();
        handles.dedup();
        let mut collected = 0;
        if !handles.is_empty() {
            let report = self.store.delete_many(&handles).await?;
            collected = report.deleted;
            let taken: Vec<&String> = handles.iter().filter(|k| !report.failed.iter().any(|(f, _)| f == *k)).collect();
            // Named, as the orphan sweep's are: a trace check maps each key.
            self.trace("sweep", serde_json::json!({"flush": own_flush, "what": "retired", "removed": report.deleted,
                "refused": report.failed.len(), "keys": taken}));
            if !report.failed.is_empty() {
                // Keep the logs: their handles are retried next time.
                return Ok(collected);
            }
        }
        let mut cache = self.state.load_retire_cache();
        for (key, _) in &ledger.due {
            match self.store.delete(key).await {
                Ok(()) | Err(StoreError::NotFound(_)) => {
                    cache.remove(key);
                }
                Err(e) => eprintln!("flint-sync: could not drop the retire log {key} ({e}); next time"),
            }
        }
        self.state.save_retire_cache(&cache)?;
        Ok(collected)
    }
}
