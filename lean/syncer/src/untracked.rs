//! Finding 10: an upload whose commit never came.
//!
//! Uploads hold no lease. A writer lost for good between an upload and its
//! commit — its pod replaced, its node gone — leaves bytes at a key the
//! manifest cites under a different etag, and nothing tracks them: no
//! manifest cites them, no inbox entry names them, and the writer that
//! would have cited them is gone. A fresh checkout reads them (the S3-wins
//! arm) while a live writer keeps the cited version, so the two disagree
//! for as long as nobody rewrites the path. A NEW writer heals it — its
//! checkout adopts the bytes and its next commit's citation repair cites
//! them (measured 2026-09-15) — but a workspace whose writers all outlive
//! the loss never checks out again.
//!
//! The sweep tracks such an object the way the gateway tracks a UI write:
//! an inbox entry, which every writer's consume integrates (adopted where
//! the path is clean, preserved beside the agent's edit where it is not)
//! and the next commit cites. The formal model checks the rule: tracked at
//! ANY time, a live writer's upload still in flight included, every
//! invariant holds (`LeanBarrierLeaseOrphanTracked`), so the grace below is
//! churn control rather than a safety margin; without it, a world in which
//! every syncer has gone quiet still holds such bytes
//! (`LeanBarrierLeaseOrphanDiverges`).

use std::collections::HashMap;

use super::inbox::{self, InboxEntry};
use super::{manifest, LeanError, LeanResult, Syncer};

/// The author an inbox entry from the sweep carries.
pub const UNTRACKED_AUTHOR: &str = "untracked-sweep";

impl Syncer {
    /// Track every object at a CITED key whose etag the manifest does not
    /// cite, that no inbox entry names, and that has sat so for at least
    /// `untracked_grace_secs`. Returns the paths tracked. An object at a
    /// key no manifest cites is not this: no checkout serves it (a delete
    /// whose GC never ran leaves one), and it is garbage, not a
    /// disagreement.
    ///
    /// One manifest read, one inbox read and a LIST of the files prefix.
    /// An open barrier window stops the sweep; the next one resumes it.
    pub async fn track_untracked(&self, now: u64) -> LeanResult<Vec<String>> {
        let Some(loaded) = manifest::load(self.store.as_ref(), &self.cfg).await? else {
            return Ok(vec![]);
        };
        let cited: HashMap<&str, (&str, &str)> = loaded
            .manifest
            .entries
            .iter()
            .map(|(p, e)| (e.key.as_str(), (p.as_str(), e.etag.as_str())))
            .collect();
        let doc = inbox::load(self.store.as_ref(), &self.cfg).await?.doc;
        let same = |a: &str, b: &str| a.trim_matches('"') == b.trim_matches('"');
        let listed = self.store.list(&format!("{}/files/", self.cfg.prefix)).await?;
        let mut tracked = vec![];
        for obj in listed {
            let Some(&(path, cited_etag)) = cited.get(obj.key.as_str()) else { continue };
            if same(cited_etag, &obj.etag) {
                continue;
            }
            if doc.entries.iter().any(|e| e.path == path && same(&e.etag, &obj.etag)) {
                continue;
            }
            // No modification time, no judgement of age: leave it.
            let Some(modified) = obj.last_modified_unix else { continue };
            if now.saturating_sub(modified) < self.cfg.untracked_grace_secs {
                continue;
            }
            let entry = InboxEntry {
                path: path.to_string(),
                etag: obj.etag.clone(),
                author: UNTRACKED_AUTHOR.into(),
                added_unix: now,
                // Nobody vouches for these bytes but the backend: the
                // consume checks them against its attestation, as it does
                // any entry without a writer's CRC.
                crc64_b64: None,
                cited: Some(cited_etag.to_string()),
            };
            match inbox::gateway_append(self.store.as_ref(), &self.cfg, entry).await {
                Ok(()) => {
                    self.trace("untracked", serde_json::json!({"path": path, "etag": obj.etag, "cited": cited_etag}));
                    tracked.push(path.to_string());
                }
                Err(LeanError::State(m)) if m.contains("window open") => break,
                Err(e) => return Err(e),
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
}
