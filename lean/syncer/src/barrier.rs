//! The publish barrier (plan §2.1, seven steps) — the machine
//! `lean/formal/LeanSubtree.tla` checks. Order is load-bearing:
//! consume → scan → intent/window → uploads → manifest CAS (merge) →
//! GC deletes LAST → baseline rewrite.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bytes::Bytes;

use flint_store::{crc64_nvme, crc64_to_b64, GenerationStamps, PosixStamps, PutCondition, StoreError};

use super::inbox;
use super::manifest::{self, LeanEntry};
use super::scan;
use super::state::{BaselineEntry, Carrier, ConflictRecord, IntentJournal};
use super::{now_unix, LeanError, LeanResult, Syncer};

/// The last gateway request this workspace acted on, per verb. A
/// watermark rather than a consumed queue — see `note_verb_requests`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VerbWatermark {
    #[serde(default)]
    pub boundary_unix: u64,
    #[serde(default)]
    pub sync_unix: u64,
}

#[derive(Debug, Default)]
pub struct BarrierReport {
    pub seq: Option<u64>,
    pub uploaded: Vec<String>,
    pub deleted: Vec<String>,
    /// Paths a DECLARED removal (delete/rename design) cited out this
    /// barrier — a subset of `deleted` once the GC has run.
    pub removed: Vec<String>,
    /// Declared removals this barrier REFUSED (locally dirty, not a
    /// file, uncontainable); the cell carries the reason.
    pub removals_refused: usize,
    pub parked: Vec<String>,
    pub consumed: usize,
    pub foreign_queued: usize,
    /// Paths whose transfer drifted or was swept mid-compose: nothing
    /// published, nothing advanced; the next scan re-queues them.
    pub deferred: Vec<String>,
    /// Paths whose object the collector LEFT BEHIND because this store
    /// does not enforce `If-Match` on DELETE (`conformance.rs`). The
    /// delete still reached the boundary; only the object survives.
    ///
    /// NOT added to `deleted`, and so NOT dropped from the merge base:
    /// the rule step 7 follows is "drop the entry once the key no longer
    /// holds our bytes", which is why the absent and replaced-absent
    /// arms drop it and the skipped-etag arm does not. A leaked key
    /// still holds them. The model names that rule
    /// `BaselineKeepsUncollected` and it is what ships, so a leak
    /// follows the arm that already exists rather than inventing a
    /// third; the cost is that the collector re-offers the path (one
    /// HEAD) at every later barrier, exactly as a skipped one is.
    pub leaked: Vec<String>,
    /// Paths this barrier published OVER a version it never integrated;
    /// each has a conflict record naming the preserved copy (R7).
    pub surfaced: Vec<String>,
    /// Retired handles the batch delete took.
    pub collected: usize,
    /// Orphan handles the sweep took, when it was due.
    pub swept: usize,
    pub no_change: bool,
    /// Bytes this barrier actually published — the input to the
    /// sentinel work meter (boundary-verbs plan D3.1). Metering work
    /// rather than calls is what keeps a sentinel storm from
    /// un-coalescing a hot large file's republish: a counted budget
    /// charges a 2 GiB checkpoint the same one unit as a 4 KiB file.
    pub published_bytes: u64,
    /// The manifest ETag this barrier installed (the ack's CAS token).
    pub manifest_etag: Option<String>,
    /// What the manifest HEAD/install told us the bucket is at, for the
    /// news ticker (D5) — free, off requests the barrier already made.
    pub observed_seq: Option<u64>,
    pub observed_etag: Option<String>,
    /// Paths this barrier saw absent for the FIRST time: their deletes
    /// are withheld to the next scan (the rename-vs-walk guard). On a
    /// declared boundary a non-empty set means the confirmation lstat
    /// found them back on disk — the race the guard exists for.
    pub first_absence: Vec<String>,
    /// First-absence paths a DECLARED boundary confirmed gone by direct
    /// lstat and published as ordinary deletes (`confirm_absences`).
    pub absences_confirmed: usize,
    /// This barrier finished a rescope a crash had left half-applied
    /// before it did anything else. Reported so a run that looks like
    /// an ordinary barrier can be told from one that had to converge a
    /// workspace first.
    pub rescope_replayed: bool,
}

/// Upload waves per chunk. The chunk is `fanout * this`, so a wave
/// still saturates fan-out and the sync point between chunks is rare.
const UPLOAD_CHUNK_WAVES: usize = 16;


impl Syncer {
    /// Verify the cell still names us at OUR epoch; anything else is a
    /// fence. Read-verify before the manifest CAS (the per-request
    /// validation the gateway will also enforce).
    async fn verify_not_deposed(&self) -> LeanResult<()> {
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| LeanError::State("no lease".into()))?;
        match self.store.epoch_read(&self.cfg.epoch_key()).await? {
            Some(state) if state.epoch == lease.epoch && state.holder_id == lease.holder_id => {
                Ok(())
            }
            Some(state) => Err(LeanError::Fenced(format!(
                "cell at epoch {} holder {} (we are epoch {})",
                state.epoch, state.holder_id, lease.epoch
            ))),
            None => Err(LeanError::Fenced("epoch cell vanished".into())),
        }
    }

    /// Step 1 (P1-lite, 2026-09-25): what the tree is OWED, derived from
    /// the document and taken in one pass. A path is owed where the
    /// document differs from the baseline — which is also the merge base —
    /// and the tree is clean there; only a path the tree HOLDS or its
    /// scope covers (what the scope declined stays declined). Nothing is
    /// stored between deriving it and taking it, so nothing can be
    /// overtaken: the queue this replaces needed a prune for that (L-123).
    /// A DIRTY path is the agent's newer work and is not owed: it
    /// publishes, and the merge records what it publishes over (R7, the
    /// delete override). Unless its bytes already ARE the document's —
    /// a restart between this writer's CAS and step 7 leaves exactly
    /// that — and then the baseline simply follows (content convergence).
    /// Returns how many paths it took, and the pointer's (seq, etag) as it
    /// read them — the barrier's fast path needs exactly that, and must not
    /// pay a second GET for it (an idle tick is the cell and the pointer).
    pub async fn consume_owed(&mut self) -> LeanResult<(usize, Option<(u64, String)>)> {
        let mut baseline = self.state.load_baseline()?;
        // THE CHEAP PATH (scan trigger). Only three things make a path
        // newly owed: the document moved (the pointer is not the one last
        // derived against — a peer, a UI save, or this tree's own commit
        // over a document that had moved), the baseline moved (only this
        // tree's own commit, which moves the pointer too), or the agent
        // backed out of a path the last derive skipped as its work. So: one
        // small GET, a stat per skipped path, never the entries.
        if let Some(d) = baseline.derived_etag.clone() {
            if let Some(p) = manifest::load_pointer(self.store.as_ref(), &self.cfg).await? {
                let still_theirs = baseline.skipped.iter().all(|path| {
                    check_contained(&self.cfg.root, path).is_ok()
                        && local_dirty(&self.cfg.root.join(path), baseline.entries.get(path))
                });
                if d == p.etag && still_theirs {
                    return Ok((0, Some((p.pointer.seq, p.etag))));
                }
            }
        }
        let Some(current) = manifest::load(self.store.as_ref(), &self.cfg).await? else {
            return Ok((0, None));
        };
        let doc = &current.manifest;
        // H10: whatever this barrier does next — the fast path included —
        // the ack names this document or a later one.
        self.note_carrier_uncited(doc)?;
        let scope = self.state.load_scope()?.map(|v| super::sync::Scope::new(&v));
        let same = |a: &str, b: &str| a.trim_matches('"') == b.trim_matches('"');
        let mut taken = 0usize;
        // Something owed could not be taken now (a fetch that lost to a
        // newer document, an unreadable path, a failed write): the next
        // consume derives again even if the pointer does not move.
        let mut left = false;
        // What is not owed because the agent is working on it: owed the
        // moment it backs out, which the cheap path checks.
        let mut skipped: BTreeSet<String> = BTreeSet::new();

        // Deletions first: a document that turned the file `build` into
        // the directory `build/log.txt` owes both, and the file must go
        // before the directory can be made.
        let gone: Vec<String> =
            baseline.entries.keys().filter(|p| !doc.entries.contains_key(*p)).cloned().collect();
        for path in gone {
            if check_contained(&self.cfg.root, &path).is_err() {
                // Never materializable here, so nothing to remove.
                baseline.entries.remove(&path);
                baseline.prev_scan.remove(&path);
                continue;
            }
            let local = self.cfg.root.join(&path);
            let be = baseline.entries.get(&path).cloned();
            let record = |kind: String| ConflictRecord {
                path: path.clone(),
                foreign_etag: String::new(),
                preserved_key: None,
                kind,
                at_unix: now_unix(),
            };
            match std::fs::symlink_metadata(&local) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.trace("tombstone", serde_json::json!({"path": path, "action": "absent"}));
                }
                Err(e) => {
                    // Unreadable is not absent.
                    self.trace("tombstone", serde_json::json!({"path": path, "action": "deferred"}));
                    self.state.append_conflict(&record(format!(
                        "consume-foreign-delete-deferred: cannot stat the path: {e}"
                    )))?;
                    left = true;
                    continue;
                }
                Ok(m) if !m.is_file() || local_dirty(&local, be.as_ref()) => {
                    // The agent's newer work: not owed. It publishes over
                    // the delete, with a record (`commit-recreated-deleted`).
                    // Owed again if the agent backs out.
                    self.trace("tombstone", serde_json::json!({"path": path, "action": "kept-dirty"}));
                    skipped.insert(path.clone());
                    continue;
                }
                Ok(_) => {
                    if let Err(e) = std::fs::remove_file(&local) {
                        self.state.append_conflict(&record(format!(
                            "consume-foreign-delete-failed (will retry): {e}"
                        )))?;
                        left = true;
                        continue;
                    }
                    // CONFIRM: only NotFound is absence.
                    if !matches!(
                        std::fs::symlink_metadata(&local),
                        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
                    ) {
                        self.state.append_conflict(&record(
                            "consume-foreign-delete-unconfirmed (will retry)".into(),
                        ))?;
                        left = true;
                        continue;
                    }
                    self.trace("tombstone", serde_json::json!({"path": path, "action": "removed"}));
                    taken += 1;
                }
            }
            baseline.entries.remove(&path);
            baseline.prev_scan.remove(&path);
        }

        for (path, e) in &doc.entries {
            let be = baseline.entries.get(path).cloned();
            if be.as_ref().is_some_and(|b| same(&b.etag, &e.etag)) {
                continue;
            }
            let held = be.is_some() || scope.as_ref().is_none_or(|s| s.covers(path));
            if !held {
                continue;
            }
            if let Err(err) = check_contained(&self.cfg.root, path) {
                // A path this tree can never safely materialise: surfaced,
                // and not left owed — nothing here will ever take it.
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: e.etag.clone(),
                    preserved_key: None,
                    kind: format!("consume-refused-containment: {err}"),
                    at_unix: now_unix(),
                })?;
                self.trace("consume", serde_json::json!({"path": path, "etag": e.etag, "from": "manifest", "action": "refused"}));
                continue;
            }
            let local = self.cfg.root.join(path);
            if local_dirty(&local, be.as_ref()) {
                // CONTENT CONVERGENCE: the tree's bytes are the document's.
                let converged = std::fs::symlink_metadata(&local).is_ok_and(|m| m.is_file() && m.len() == e.size)
                    && file_crc(&self.cfg.root, path).is_ok_and(|c| same(&crc64_to_b64(c), &e.crc64_b64));
                if converged {
                    let st = std::fs::metadata(&local)?;
                    baseline.entries.insert(
                        path.clone(),
                        BaselineEntry {
                            etag: e.etag.clone(),
                            key: Some(e.key.clone()),
                            generation: e.generation,
                            size: st.len(),
                            mtime_unix: mtime_of(&st),
                            mtime_nanos: Some(mtime_nanos_of(&st)),
                            crc64_b64: Some(e.crc64_b64.clone()),
                        },
                    );
                    baseline.prev_scan.insert(path.clone());
                    self.trace("consume", serde_json::json!({"path": path, "etag": e.etag, "from": "manifest", "action": "converged"}));
                    taken += 1;
                } else {
                    // The agent's work, not owed — until it backs out.
                    skipped.insert(path.clone());
                }
                continue;
            }
            let (meta, body) = match self.store.get_whole(&e.key, Some(&e.etag)).await {
                Ok(got) => got,
                // A newer document replaced it (and its collector took the
                // handle): owed again against that one.
                Err(StoreError::NotFound(_)) | Err(StoreError::PreconditionFailed(_)) => {
                    self.trace("consume", serde_json::json!({"path": path, "etag": e.etag, "from": "manifest", "action": "missing"}));
                    left = true;
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            // VERIFIED before it is written, as checkout's fetch is: against
            // the CRC the document cites, else the backend's attestation.
            let got = crc64_to_b64(crc64_nvme(&body));
            let want = Some(e.crc64_b64.clone()).filter(|c| !c.is_empty()).or_else(|| meta.crc64_b64.clone());
            if let Some(want) = want {
                if want != got {
                    self.state.append_conflict(&ConflictRecord {
                        path: path.clone(),
                        foreign_etag: e.etag.clone(),
                        preserved_key: None,
                        kind: format!(
                            "consume-refused-checksum: etag {} is cited with CRC-64 {want} but the \
                             bytes fetched under it hash to {got}",
                            e.etag
                        ),
                        at_unix: now_unix(),
                    })?;
                    self.trace("consume", serde_json::json!({"path": path, "etag": e.etag, "from": "manifest", "action": "refused-checksum"}));
                    left = true;
                    continue;
                }
            }
            let mode = PosixStamps::from_meta(&meta.meta).map(|p| p.mode);
            consume_window("before-write", path);
            // H7: the licence to overwrite is re-checked with the temp
            // written, immediately before the rename; a write since is the
            // agent's, and it stays (and publishes).
            let still_clean = || !local_dirty(&local, be.as_ref());
            let st = match contained_path(&self.cfg.root, path)
                .and_then(|target| write_file_atomic_if(&target, &body, mode, &still_clean))
            {
                Ok(Some(st)) => st,
                // The agent wrote first: as a dirty path above.
                Ok(None) => {
                    skipped.insert(path.clone());
                    continue;
                }
                Err(err) => {
                    // TRANSIENT (a full disk is not a planted symlink): the
                    // path stays owed and the record repeats.
                    self.state.append_conflict(&ConflictRecord {
                        path: path.clone(),
                        foreign_etag: e.etag.clone(),
                        preserved_key: None,
                        kind: format!("consume-write-failed (will retry): {err}"),
                        at_unix: now_unix(),
                    })?;
                    self.trace("consume", serde_json::json!({"path": path, "etag": e.etag, "from": "manifest", "action": "deferred"}));
                    left = true;
                    continue;
                }
            };
            consume_window("after-rename", path);
            // The baseline records the inode this consume WROTE (H7).
            let stamps = GenerationStamps::from_meta(&meta.meta);
            baseline.entries.insert(
                path.clone(),
                BaselineEntry {
                    etag: meta.etag.clone(),
                    key: Some(e.key.clone()),
                    generation: stamps.map(|s| s.generation).unwrap_or(e.generation),
                    size: st.len(),
                    mtime_unix: mtime_of(&st),
                    mtime_nanos: Some(mtime_nanos_of(&st)),
                    crc64_b64: Some(got),
                },
            );
            baseline.prev_scan.insert(path.clone());
            self.trace("consume", serde_json::json!({"path": path, "etag": e.etag, "from": "manifest", "action": "adopted"}));
            taken += 1;
        }
        baseline.derived_etag = (!left).then(|| current.etag.clone());
        baseline.skipped = skipped;
        if !left {
            // Everything owed is taken: the tree is integrated with this
            // document (its dirty paths are the agent's, about to publish).
            baseline.seq = doc.seq;
            baseline.manifest_etag = Some(current.etag.clone());
        }
        // The files this baseline vouches for reached the disk before the
        // baseline does (audit 2026-09-03, finding 9).
        self.state.sync_tree()?;
        self.state.save_baseline(&baseline)?;
        // The pointer as this load saw it (a legacy single object has none,
        // and the fast path asks the bucket itself).
        let seen = current.pointer.as_ref().map(|_| (doc.seq, current.etag.clone()));
        Ok((taken, seen))
    }

    /// The cell's verb requests alone (§2.5), as a barrier's step 1 reads
    /// them: no consume, no fetch.
    pub async fn read_cell_requests(&mut self) -> LeanResult<()> {
        let doc = inbox::load(self.store.as_ref(), &self.cfg).await?.doc;
        self.note_verb_requests(&doc)
    }

    /// Act on the inbox document's two request fields (§2.5, D14).
    ///
    /// The asymmetry is deliberate and is about blast radius, not
    /// principle: a boundary request is PERFORMED (it publishes what is
    /// already on disk and mutates nothing local), a sync request is
    /// CARRIED into the ticker as advisory news and the agent decides.
    ///
    /// Both are watermarked by `requested_unix` rather than consumed by
    /// a clearing CAS. The field is idempotent state, so a repeat is a
    /// no-op and a lost clear cannot double-publish; and not writing
    /// means the doors cost no request at all, which is what §2.5
    /// promises.
    fn note_verb_requests(&mut self, doc: &inbox::InboxDoc) -> LeanResult<()> {
        let mut mark = self.load_verb_watermark()?;
        if let Some(r) = &doc.sync_request {
            if r.requested_unix > mark.sync_unix {
                self.carry_sync_request(r.requested_unix, &r.requestor)?;
                mark.sync_unix = r.requested_unix;
            }
        }
        if let Some(r) = &doc.boundary_request {
            if r.requested_unix > mark.boundary_unix {
                self.request_boundary(
                    &format!("gw:{}:{}", r.requested_unix, r.requestor),
                    Some(format!("boundary requested by {}", r.requestor)),
                )?;
                mark.boundary_unix = r.requested_unix;
            }
        }
        self.save_verb_watermark(&mark)
    }

    fn verb_watermark_path(&self) -> std::path::PathBuf {
        self.cfg.state_dir().join("verb-requests.json")
    }

    pub(crate) fn load_verb_watermark(&self) -> LeanResult<VerbWatermark> {
        let p = self.verb_watermark_path();
        if !p.exists() {
            return Ok(VerbWatermark::default());
        }
        Ok(std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default())
    }

    fn save_verb_watermark(&self, m: &VerbWatermark) -> LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(m)
            .map_err(|e| LeanError::State(format!("verb watermark: {e}")))?;
        super::control::write_atomic(&self.verb_watermark_path(), &bytes)
    }

    /// The consume's dirty case: the agent's local version wins; the
    /// FOREIGN bytes are preserved first (a conflict record keeps both
    /// recoverable), then the recognized ETag advances so the eventual
    /// publish supersedes them KNOWINGLY.
    /// Preserve the bytes at `key` (a handle, or an ingress object) under
    /// `conflicts/<uuid>/<path>`, by a server-side copy guarded on `etag`:
    /// no bytes cross this process, and a 10 GB checkpoint is preserved
    /// in one request. The copy is what a conflict record names, and what
    /// outlives the handle once the collector or the orphan sweep takes it.
    async fn preserve_conflict_copy(&self, path: &str, key: &str, etag: &str) -> LeanResult<String> {
        let dst = self.cfg.conflict_key(&uuid::Uuid::new_v4().to_string(), path);
        let stamps = GenerationStamps {
            generation: 0,
            epoch: self
                .lease
                .as_ref()
                .map(|l| l.epoch)
                .or_else(|| self.state.load_incarnation().ok().flatten().map(|i| i.epoch))
                .unwrap_or(0),
            flush_uuid: "conflict-preserve".into(),
            boundary_source: None,
            posix: None,
        };
        self.store
            .copy_object(key, Some(etag), &dst, &PutCondition::IfNoneMatchAny, &stamps)
            .await?;
        Ok(dst)
    }

    /// Take the SECOND absence observation now, by direct `lstat`.
    ///
    /// `scan::classify` withholds a path's delete until absence has
    /// survived two consecutive scans — the rename-vs-walk race guard
    /// (`lib.rs:37`). The hazard that rule names is the WALK missing a
    /// file renamed under it, not deletion itself. On the cadence path
    /// the second walk arrives at the next floor tick and nobody has
    /// been promised otherwise. On a DECLARED boundary it cannot wait:
    /// the ack would claim a coherent point (D1 — "everything
    /// ordered-before T") while the manifest still cites a file the
    /// agent removed before it touched the sentinel, and it would say
    /// so with `report.deleted: 0` and `status: "ok"`.
    ///
    /// A direct `lstat` is exactly what the rename-vs-walk guard asks
    /// for and is immune to the walk race by construction, so the
    /// second observation costs one syscall per transiently-absent
    /// path — never a second full pass. The cadence path is unchanged
    /// and still waits for the second walk.
    pub(crate) fn confirm_absences(&self, classified: &mut scan::Classified) -> LeanResult<usize> {
        if classified.first_absence.is_empty() {
            return Ok(0);
        }
        let mut confirmed = 0;
        for path in std::mem::take(&mut classified.first_absence) {
            // ONLY NotFound is absence. Every other errno — EACCES on a
            // parent, EIO, EMFILE, ELOOP — used to read as "the agent
            // deleted it", and this is the oracle that PUBLISHES the
            // delete and then DELETES THE OBJECT. An unreadable file is
            // the one case where guessing is unrecoverable, so it fails
            // CLOSED, exactly as the walk does at `scan.rs`'s
            // `symlink_metadata(&path)?`. Two call sites, one syscall:
            // they must not have opposite error policies.
            let gone = match std::fs::symlink_metadata(self.cfg.root.join(&path)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => {
                    classified.first_absence.insert(path.clone());
                    return Err(LeanError::State(format!(
                        "cannot confirm the absence of {path:?}: {e} — refusing to publish \
                         a deletion on an unreadable path"
                    )));
                }
                Ok(_) => false,
            };
            if gone {
                classified.deletes.insert(path);
                confirmed += 1;
            } else {
                // Back on disk: the rename-vs-walk race, caught exactly
                // as the rule intends. Still only a first absence.
                classified.first_absence.insert(path);
            }
        }
        Ok(confirmed)
    }

    /// Steps 2–7. `barrier` = one full publish cycle (the cadence arm).
    pub async fn run_barrier(&mut self) -> LeanResult<BarrierReport> {
        self.barrier_inner(false, "manual").await
    }

    /// The floor tick's barrier. Stamped `cadence` so the bucket can
    /// answer "which clock installed this boundary?" without the ack —
    /// the ack is a LOCAL file, and the fleet-visible question is asked
    /// by operators and by the gateway's `/status`, neither of which
    /// can see it.
    pub async fn cadence_barrier(&mut self) -> LeanResult<BarrierReport> {
        self.barrier_inner(false, "cadence").await
    }

    /// A barrier whose result somebody is going to ACK: a sentinel
    /// honor (D1) or the preStop drain (D10). Identical to
    /// `run_barrier` except that it confirms first-absence paths rather
    /// than acking a boundary that withholds them.
    pub async fn declared_barrier(&mut self) -> LeanResult<BarrierReport> {
        self.barrier_inner(true, "sentinel").await
    }

    /// `declared_barrier` under an explicit provenance stamp — the
    /// drain and the budget-deferred honor are the same barrier
    /// reporting a different clock.
    pub async fn declared_barrier_as(&mut self, source: &str) -> LeanResult<BarrierReport> {
        self.barrier_inner(true, source).await
    }

    async fn barrier_inner(&mut self, declared: bool, source: &str) -> LeanResult<BarrierReport> {
        // Every publishing path ends here — cadence, sentinel, drain, the
        // one-shot verb — so a reader is refused here before the first
        // request, whichever of them a caller reached. The callers that
        // owe an answer (the publish sentinel, the drain) give it before
        // this; this is the backstop for the ones that do not.
        self.refuse_if_read("barrier")?;
        // Review 2026-09-18, H10: a barrier that begins while a publish
        // declaration stands CARRIES it, whoever called — the floor's
        // cadence barrier after a failed honor as much as the honor itself
        // (the model's `DSet` and `InstallSource` read the live pending,
        // not the caller). So it confirms first absences as a declared
        // barrier does, stamps the deferred clock, and journals itself
        // with its CAS as the install the ack names (`IntentJournal::
        // carrier`). A torn record has no id and carries nothing.
        let carries: Option<String> = self
            .load_pending(super::sentinel::Verb::Publish)
            .ok()
            .flatten()
            .map(|p| p.id)
            .filter(|id| !id.is_empty());
        let declared = declared || carries.is_some();
        let source = if carries.is_some() && source == "cadence" { "sentinel-deferred" } else { source };
        // No lease is held until the commit section (design 2026-09-13
        // §4): the consume, the scan and the uploads below run with the
        // cell at rest, guarded by each object's own etag. What the
        // uploads are STAMPED with is the last epoch this incarnation
        // held — informational, like every epoch stamp on an object; the
        // manifest entries carry the commit section's real epoch.
        let epoch_hint = self.state.load_incarnation()?.map(|i| i.epoch).unwrap_or(0);
        let mut report = BarrierReport::default();
        let barrier_started = std::time::Instant::now();
        self.trace("barrier_start", serde_json::json!({"source": source, "declared": declared}));

        // STEP 0: finish any rescope a crash left half-applied, before
        // the scan reads the tree (scoped-read design §4.3). This is the
        // gate that makes the narrow verb safe at all: a half-applied
        // narrow is a tree whose files and whose citations disagree, and
        // `classify` reads that disagreement as an upload or as a
        // DELETE depending on which half landed. Replay is idempotent,
        // so the cost on the normal path is one `stat` of a file that
        // is not there.
        if let Some(r) = self.replay_scope_intent().await? {
            report.rescope_replayed = true;
            eprintln!(
                "flint-sync: finished an interrupted rescope before the barrier \
                 (uncited {}, unlinked {}, materialised {})",
                r.uncited, r.unlinked, r.materialized
            );
        }

        // Step 1: the cell's verb requests (§2.5's doors ride this GET), then
        // what the tree is owed. A UI save, delete or rename COMMITS (P2), so
        // it reaches the tree as a peer's change does: derived, not queued.
        let inbox_doc = inbox::load(self.store.as_ref(), &self.cfg).await?.doc;
        // A failure here must never fail the barrier: the request is
        // idempotent state and the next tick re-reads it.
        if let Err(e) = self.note_verb_requests(&inbox_doc) {
            eprintln!("flint-sync: verb request not consumed (retrying next tick): {e}");
        }
        let (consumed, seen_pointer) = self.consume_owed().await?;
        report.consumed = consumed;
        // The agent runs alongside: what it does to the tree between the
        // removal pass above and the scan below is the model's interleaving
        // of a local edit between Consume and Scan (the rename world's
        // tenth box run reached its counterexample through it). A test
        // window, like the consume's (H7); nothing in it calls the store.
        consume_window("before-scan", "");
        // Step 2: scan-diff against the persisted baseline.
        let mut baseline = self.state.load_baseline()?;
        let scanned = scan::scan(&self.cfg.root)?;
        let mut classified = scan::classify(&scanned, &baseline);
        if declared {
            report.absences_confirmed = self.confirm_absences(&mut classified)?;
        }
        report.first_absence = classified.first_absence.iter().cloned().collect();

        // Skip-on-no-diff: nothing local, and the bucket manifest where the
        // consume left it.
        if classified.uploads.is_empty()
            && classified.deletes.is_empty()
            && classified.first_absence.is_empty()
        {
            // Read the POINTER, never the entries: the 0b rig measured
            // the idle tick at 1M entries as 27 s / 1.3 GiB — dominated
            // by fetching and parsing a 264 MiB document just to read
            // `seq`. The pointer is a few hundred bytes and carries the
            // seq in its body, so a GET of it costs one round trip and
            // answers strictly more than a HEAD of the old object did.
            //
            // Legacy workspaces (no pointer yet) keep the HEAD they had.
            let pointer = match seen_pointer {
                // The consume just read it: no second GET.
                Some((seq, etag)) => Some((seq, etag)),
                None => manifest::load_pointer(self.store.as_ref(), &self.cfg).await?.map(|l| (l.pointer.seq, l.etag)),
            };
            let unchanged = match pointer {
                Some((seq, etag)) => {
                    // The news ticker rides this request for free — D5's
                    // "zero added bucket requests" is still literal.
                    report.observed_seq = Some(seq);
                    report.observed_etag = Some(etag.clone());
                    baseline.manifest_etag.as_deref() == Some(etag.as_str())
                }
                None => match self.store.head(&self.cfg.manifest_key()).await {
                    Ok(meta) => {
                        report.observed_seq =
                            GenerationStamps::from_meta(&meta.meta).map(|s| s.generation);
                        report.observed_etag = Some(meta.etag.clone());
                        baseline.manifest_etag.as_deref() == Some(meta.etag.as_str())
                    }
                    Err(StoreError::NotFound(_)) => baseline.manifest_etag.is_none(),
                    Err(e) => return Err(e.into()),
                },
            };
            if unchanged {
                report.no_change = true;
                report.seq = Some(baseline.seq);
                // prev_scan still advances (the two-scan rule's clock) —
                // but the baseline document (hundreds of MB at 1M
                // entries) is rewritten only when the scan SET moved.
                // Compare without building the set: both sides iterate
                // sorted, and the allocation this used to make was the
                // size of the very document it was trying not to write.
                if scanned.len() != baseline.prev_scan.len()
                    || !scanned.keys().eq(baseline.prev_scan.iter())
                {
                    baseline.prev_scan = scanned.keys().cloned().collect();
                    self.state.save_baseline(&baseline)?;
                }
                self.trace_barrier_end(&report, barrier_started);
                return Ok(report);
            }
        }

        // Step 3: intent journal, then the window (the commitment
        // point: the same CAS drops the consumed entries).
        let flush_uuid = uuid::Uuid::new_v4().to_string();
        self.trace("scan", serde_json::json!({"flush": flush_uuid, "uploads": classified.uploads.len(),
            "deletes": classified.deletes.len(), "first_absence": classified.first_absence.len()}));
        let mut intent = self.state.load_intent()?;
        intent = IntentJournal { flush_uuid: flush_uuid.clone(), carrier: intent.carrier.clone() };
        self.state.save_intent(&intent)?;
        // No window (P2): a UI save commits without the lease, and the
        // uploads below race it the way two writers race each other —
        // the commit's CAS decides, and the loser is preserved and
        // recorded.

        // Step 4: guarded uploads, fanned out under a bounded window
        // (each key's guard chain is independent; the 412 policy and
        // conflict records are applied to the collected results below,
        // in deterministic path order).
        let mut upserts: BTreeMap<String, LeanEntry> = BTreeMap::new();
        let mut parked: BTreeSet<String> = BTreeSet::new();
        // Citations whose etag was observed with no lease held (adopted
        // uploads, citation repairs): re-read inside the commit section.
        let mut observed: BTreeSet<String> = BTreeSet::new();
        let mut new_baseline_entries: BTreeMap<String, BaselineEntry> = BTreeMap::new();
        let outcomes: Vec<(String, LeanResult<UploadOutcome>)> = {
            use futures::stream::{self, StreamExt};
            // CHUNKED into waves, so a wave's outcomes are collected
            // before the next is fanned out. No lease is held here
            // (design 2026-09-13 §4), so nothing renews or fences
            // between chunks any more — the between-chunk fence that
            // stopped a deposed straggler's PUTs went with the straggler:
            // a PUT outside the lease is one legitimate writer's PUT,
            // guarded by If-Match, and a collision is preserved and
            // superseded, never silently overwritten.
            let mut outcomes: Vec<(String, LeanResult<UploadOutcome>)> = Vec::new();
            // NOT cfg.fanout. That knob was raised to 128 for the READ
            // path, and the read-side measurement (2.5x on 20k small
            // files) says nothing about this one, so uploads keep the
            // value they were measured at until someone measures them.
            // Memory is no longer the reason they are separate: this
            // path is byte-bounded too now, by the store's gate
            // (`with_upload_inflight_max_bytes`), which charges every
            // whole body below and every multipart part inside the
            // store from before its read until its PUT returns.
            let fanout = self.cfg.upload_fanout.max(1);
            let chunk = fanout.saturating_mul(UPLOAD_CHUNK_WAVES).max(1);
            let all: Vec<&String> = classified.uploads.iter().collect();
            for group in all.chunks(chunk) {
                {
                    let this: &Syncer = &*self;
                    let mut part: Vec<(String, LeanResult<UploadOutcome>)> =
                        stream::iter(group.iter().map(|path| {
                            let path = *path;
                            let scanned_entry = &scanned[path];
                            let base = baseline.entries.get(path);
                            let flush_uuid = &flush_uuid;
                            async move {
                                let r = this
                                    .upload_one(path, scanned_entry, base, epoch_hint, flush_uuid)
                                    .await;
                                (path.clone(), r)
                            }
                        }))
                        .buffer_unordered(fanout)
                        .collect()
                        .await;
                    outcomes.append(&mut part);
                }
            }
            outcomes
        };
        let mut outcomes = outcomes;
        outcomes.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, outcome) in outcomes {
            let path = &path;
            match outcome? {
                UploadOutcome::Published { entry, baseline_entry, adopted } => {
                    self.trace("upload", serde_json::json!({"flush": flush_uuid, "path": path, "etag": entry.etag,
                        "key": entry.key, "outcome": if adopted { "adopted" } else { "put" }}));
                    if adopted {
                        observed.insert(path.clone());
                    }
                    report.published_bytes += entry.size;
                    upserts.insert(path.clone(), entry);
                    new_baseline_entries.insert(path.clone(), baseline_entry);
                    report.uploaded.push(path.clone());
                }
                UploadOutcome::Deferred => {
                    self.trace("upload", serde_json::json!({"flush": flush_uuid, "path": path, "outcome": "deferred"}));
                    report.deferred.push(path.clone());
                }
            }
        }

        // A PULL-ONLY boundary: nothing uploaded or deleted, so the merge
        // can only add nothing — and then the commit section writes nothing
        // to the bucket (no CAS, no GC) and what remains is local: note what
        // theirs owes this tree and take its seq. That needs no
        // fence and no window; claiming for it queued every such writer
        // behind the publishing ones (65 of 191 claims in the writers drill).
        // A merge that does add something (a mirror flag to restamp, say)
        // falls through to the commit section.
        if classified.uploads.is_empty()
            && classified.deletes.is_empty()
            && upserts.is_empty()
            && observed.is_empty()
        {
            let current = manifest::load(self.store.as_ref(), &self.cfg).await?;
            let m = self.merge_onto(
                current.as_ref(),
                &merge_base(&baseline),
                &upserts,
                &classified.deletes,
                &parked,
                &flush_uuid,
            );
            if let Some(handle) = m.expected.as_ref().filter(|_| m.adds_nothing()) {
                // What theirs changed at a path this tree holds is owed: the
                // next consume derives it, because theirs is not the document
                // it last derived against (or is, and there is nothing).
                self.note_carrier_uncited(&m.theirs)?;
                report.seq = Some(m.theirs.seq);
                report.no_change = true;
                report.manifest_etag = Some(handle.etag.clone());
                report.observed_seq = Some(m.theirs.seq);
                report.observed_etag = Some(handle.etag.clone());
                report.foreign_queued = m.foreign.len();
                baseline.seq = m.theirs.seq;
                baseline.manifest_etag = Some(handle.etag.clone());
                baseline.prev_scan = scanned.keys().cloned().collect();
                self.state.save_baseline(&baseline)?;
                self.state.clear_intent_keys()?;
                self.trace_barrier_end(&report, barrier_started);
                return Ok(report);
            }
        }

        // THE COMMIT SECTION (design 2026-09-13 §4). Everything above ran
        // with no lease: the uploads are durable and etag-guarded, and
        // nothing is cited yet. Claim the cell now — for the merge, the
        // CAS, the deletes and the baseline — and hand it to the next
        // waiter when done. This is the only stretch two writers of one
        // workspace serialise on, and it is small: one pointer CAS and
        // the guarded deletes, milliseconds to seconds, never the upload
        // of a checkpoint.
        let held = super::lease::claim(self).await?;
        let epoch = held.epoch;
        eprintln!("flint-sync: publish fence held (epoch {epoch})");
        if self.cfg.drill_hold_commit_secs > 0 {
            eprintln!(
                "flint-sync: DRILL: holding the fence for {}s inside the commit section",
                self.cfg.drill_hold_commit_secs
            );
            tokio::time::sleep(std::time::Duration::from_secs(self.cfg.drill_hold_commit_secs)).await;
        }
        let commit: LeanResult<()> = async {
            // The entries carry the epoch the commit HOLDS, not the hint
            // the uploads were stamped with.
            for e in upserts.values_mut() {
                e.epoch = epoch;
            }
            // Step 5: the manifest CAS (three-way merge; bounded retries).
            // The cell is read after each manifest load below, not here: a
            // read here preceded the HEAD fan-out and the window, and a
            // holder deposed after it CASed onto its successor's document
            // (review 2026-09-18, H2).
            // EVERY citation this barrier adds was read or written with no
            // lease held. Another writer's commit may since have uncited
            // the path and collected the object: its GC deletes by the etag
            // it integrated. That etag names an ADOPTED object (the model's
            // LeanBarrierLeaseAdoptBlind) — and, since an S3 whole-PUT etag
            // is the MD5 of the bytes, it also names this barrier's own PUT
            // of IDENTICAL bytes (finding 13, the writers drill's A3: a
            // same-content rewrite re-uploaded, the peer's GC deleted it,
            // the commit cited a hole). Collection runs only inside a
            // commit section, so a re-read HERE, holding the fence, cannot
            // be overtaken before this CAS — held, that is, until the read
            // of the cell after the load: a deposal before it is seen
            // there, and after it the successor's rotation moves the
            // pointer the CAS expects (H2). Whatever is gone or replaced is
            // withheld: parked, recorded, and left dirty for the next
            // barrier to publish with a PUT of its own.
            let heads: Vec<(String, LeanResult<bool>)> = {
                use futures::stream::{self, StreamExt};
                let this: &Syncer = &*self;
                stream::iter(upserts.iter().map(|(path, cited)| async move {
                    let still = match this.store.head(&cited.key).await {
                        Ok(meta) => Ok(meta.etag == cited.etag),
                        Err(StoreError::NotFound(_)) => Ok(false),
                        Err(e) => Err(e.into()),
                    };
                    (path.clone(), still)
                }))
                .buffer_unordered(self.cfg.upload_fanout.max(1))
                .collect()
                .await
            };
            let mut heads = heads;
            heads.sort_by(|a, b| a.0.cmp(&b.0));
            for (path, still) in heads {
                let still_there = still?;
                let was_observed = observed.contains(&path);
                let cited = &upserts[&path];
                self.trace("observed", serde_json::json!({"flush": flush_uuid, "path": path, "etag": cited.etag, "still": still_there, "own_put": !was_observed}));
                if still_there {
                    continue;
                }
                let gone = upserts.remove(&path).expect("looked up above");
                new_baseline_entries.remove(&path);
                report.uploaded.retain(|p| p != &path);
                report.published_bytes = report.published_bytes.saturating_sub(gone.size);
                // ADOPTED bytes that are the baseline's own (a file touched
                // without a change, re-uploaded by a restarted barrier under
                // the same flush) that went before the commit: nothing new
                // is lost, so the path is not parked, and the next consume
                // takes the manifest's version. (Once this was the citation
                // repair's arm; storm drill S0, 2026-09-15, showed parking
                // it kept a writer on withheld bytes for good.)
                let integrated = was_observed
                    && baseline.entries.get(&path).is_some_and(|be| be.etag == gone.etag);
                if integrated {
                    self.state.append_conflict(&ConflictRecord {
                        path: path.clone(),
                        foreign_etag: gone.etag,
                        preserved_key: None,
                        kind: "repair-withheld: the object this barrier re-cited was replaced or \
                               collected before its commit; nothing cited, and the next consume \
                               installs the manifest's version"
                            .into(),
                        at_unix: now_unix(),
                    })?;
                    continue;
                }
                parked.insert(path.clone());
                let kind = if was_observed {
                    "adopt-withheld: the object this barrier found already at its key was \
                     replaced or collected before its commit; nothing cited, the path stays \
                     dirty and publishes next barrier"
                } else {
                    "upload-withheld: the object this barrier PUT was replaced or collected \
                     before its commit (a peer's GC recognizes the etag of identical bytes); \
                     nothing cited, the path stays dirty and publishes next barrier"
                };
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: gone.etag,
                    preserved_key: None,
                    kind: kind.into(),
                    at_unix: now_unix(),
                })?;
                report.parked.push(path.clone());
            }
            let mut foreign_entries: Vec<(String, LeanEntry)> = vec![];
            // Set when the merge added nothing to the document: nothing is
            // installed, and theirs becomes the merge base as it stands.
            let mut installed_nothing = false;
            let mut attempt = 0;
            // The last value carried out: the adoptions this install
            // declined for a move, whose entries stay in the cell.
            let (theirs_at_cas, installed, installed_etag, overridden, recreated, over_theirs, replaced_etag) = loop {
                attempt += 1;
                if attempt > CAS_ATTEMPTS {
                    return Err(LeanError::State(format!(
                        "manifest CAS lost {CAS_ATTEMPTS} merge races — refusing this barrier"
                    )));
                }
                let current = manifest::load(self.store.as_ref(), &self.cfg).await?;
                // Review 2026-09-18, H2: THE fence — the cell read AFTER the
                // load this CAS merges onto. The read at the top of step 5
                // comes before the HEAD fan-out and the window, and a holder
                // deposed in between loaded its successor's document,
                // rotation included, and CASed onto it: the rotation fences
                // only a load that PRECEDES it. Read here, the order closes
                // both ways: deposed before this load, and this read sees
                // it; deposed after, and the successor's rotation has moved
                // the pointer this CAS expects.
                self.verify_not_deposed().await?;
                // The baseline IS the merge base (P1-lite).
                let base = merge_base(&baseline);
                let MergeOnto { theirs, expected, merged, foreign, overridden, recreated, over_theirs } = self.merge_onto(
                    current.as_ref(),
                    &base,
                    &upserts,
                    &classified.deletes,
                    &parked,
                    &flush_uuid,
                );
                // A DOCUMENT MUST BE MATERIALISABLE. No filesystem can hold
                // a file `a` and a file `a/x`, so no document may cite
                // both — a fresh checkout would have to create a directory
                // where its own manifest says a file goes. The agent makes
                // exactly that shape by replacing a published file with a
                // directory (or the reverse): the scan sees the old path
                // absent and the new one new, the two-scan guard withholds a
                // FIRST absence, and the upload lands beside the citation it
                // was meant to replace (review 2026-09-18, M5's agent-side
                // twin). Judged here because this is where the document's
                // final shape is known — theirs, my upserts and the deletes
                // that actually applied. Mine is the side that yields: the
                // path stays dirty and publishes at the next barrier, by
                // which time the withheld absence is confirmed and its
                // citation gone. This re-merges without spending a CAS
                // attempt; the clash is decided, not raced.
                let clash = path_clashes(&merged.entries, &upserts);
                if !clash.is_empty() {
                    for path in clash {
                        let gone = upserts.remove(&path).expect("named from upserts above");
                        new_baseline_entries.remove(&path);
                        report.uploaded.retain(|p| p != &path);
                        report.published_bytes =
                            report.published_bytes.saturating_sub(gone.size);
                        parked.insert(path.clone());
                        report.parked.push(path.clone());
                        self.state.append_conflict(&ConflictRecord {
                            path: path.clone(),
                            foreign_etag: gone.etag,
                            preserved_key: None,
                            kind: "upload-withheld-path-clash: the document still cites a path \
                                   this one would sit inside (or around) — a published file \
                                   replaced by a directory, or the reverse. No workspace can \
                                   hold both, so nothing is cited here until that citation \
                                   goes; the path stays dirty and publishes next barrier"
                                .into(),
                            at_unix: now_unix(),
                        })?;
                        self.trace(
                            "path_clash",
                            serde_json::json!({"flush": flush_uuid, "path": path}),
                        );
                    }
                    attempt -= 1;
                    continue;
                }
                // Nothing of ours changes the document — a barrier that only
                // found the manifest moved by another writer. Installing it
                // anyway was an empty generation, and the peer's next tick
                // then found the manifest moved and did the same: two idle
                // writers traded generations (and cell claims) for as long
                // as both ran. Theirs becomes the merge base as it stands.
                if let Some(handle) = expected.as_ref() {
                    if merged.entries == theirs.entries && merged.sole_writer == theirs.sole_writer {
                        foreign_entries = foreign;
                        installed_nothing = true;
                        break (theirs.clone(), theirs, handle.etag.clone(), vec![], vec![], vec![], Some(handle.etag.clone()));
                    }
                }
                match manifest::cas_write_retiring(
                    self.store.as_ref(),
                    &self.cfg,
                    &merged,
                    Some(&theirs),
                    expected.as_ref(),
                    epoch,
                    &flush_uuid,
                    Some(source),
                )
                .await
                {
                    Ok(meta) => {
                        // Before the deletes and before step 7: this is the
                        // only record that survives a crash in that window.
                        self.trace("cas", serde_json::json!({"flush": flush_uuid, "seq": merged.seq,
                            "expected": expected.as_ref().map(|h| h.etag.clone()), "etag": meta.etag, "result": "ok"}));
                        // H10: what this install carried of the standing
                        // declaration survives a restart, for the ack.
                        if let Some(id) = carries.as_ref() {
                            let mut c = intent
                                .carrier
                                .take()
                                .filter(|c| &c.pending_id == id)
                                .unwrap_or_else(|| Carrier { pending_id: id.clone(), ..Default::default() });
                            c.paths.extend(classified.uploads.iter().chain(&classified.deletes).cloned());
                            c.deletes.extend(classified.deletes.iter().cloned());
                            // `honor_publish`'s dropped set, decided here:
                            // parks and deferrals are settled before the
                            // CAS, and an outranked delete is one the merged
                            // document still cites.
                            c.dropped.extend(report.parked.iter().chain(&report.deferred).cloned());
                            c.dropped.extend(
                                classified.deletes.iter().filter(|p| merged.entries.contains_key(p.as_str())).cloned(),
                            );
                            intent.carrier = Some(c);
                        }
                        self.state.save_intent(&intent)?;
                        foreign_entries = foreign;
                        let replaced = expected.as_ref().map(|h| h.etag.clone());
                        break (theirs, merged, meta.etag, overridden, recreated, over_theirs, replaced);
                    }
                    Err(LeanError::Store(StoreError::PreconditionFailed(_)))
                    | Err(LeanError::Store(StoreError::Conflict(_))) => {
                        self.trace("cas", serde_json::json!({"flush": flush_uuid, "seq": merged.seq,
                            "expected": expected.as_ref().map(|h| h.etag.clone()), "result": "lost"}));
                        // A rotation is exactly this 412, and re-merging past
                        // it would be the straggler install: the next
                        // attempt's read after its load is the fence.
                        // Back off first: under P2 a UI save never waits on
                        // the lease, so the race is usually lost to saves,
                        // and the gap between two of them is the way in (G2).
                        tokio::time::sleep(cas_backoff(attempt)).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            };
            report.seq = Some(installed.seq);
            report.no_change = installed_nothing;
            // A fused install IS a coherent point, and cadence/hybrid have
            // exactly one source. The gauges must not report "no boundary
            // ever" on a workspace that publishes every minute. A barrier
            // that installed nothing marks no boundary of its own.
            if !installed_nothing {
                self.note_boundary("cadence", installed.seq)?;
            }
            report.manifest_etag = Some(installed_etag.clone());
            report.observed_seq = Some(installed.seq);
            report.observed_etag = Some(installed_etag.clone());
            report.foreign_queued = foreign_entries.len();

            // Step 6: RETIREMENT and collection (design 2026-09-19, R3 and
            // R7). What the document this CAS merged onto cited and the
            // installed document does not — a modified path's old handle,
            // a deleted path's — is retired, minus every handle the
            // installed document still cites at another path (a rename
            // cites the source's handle at the destination) and every
            // handle an inbox entry names (a rename whose destination is
            // still only in the cell). A retired handle can never be cited
            // again: nothing discovers handles from paths, and the merge
            // carries forward only what the base cites. So the collector
            // deletes it UNCONDITIONALLY, in one batch — no HEAD, no
            // If-Match, no fence, no renew (a straggler deposed after its
            // CAS has an exact retired set too; the model's
            // `LeanImmutableStragglerGC`). The HEAD-then-If-Match dance,
            // the collector that gave way on a store without a conditional
            // DELETE (review H1's leak) and the C2 renew-before-delete
            // clock all went with the slot.
            //
            // And what this barrier published OVER without ever integrating
            // it — a peer's publish, a UI write consumed elsewhere — is
            // preserved by a server-side copy and a record BEFORE the batch
            // takes the handle. The slot's 412 caught that at the PUT and
            // `Upload412Preserves` did the same; a fresh handle has no slot
            // to fail on, so the CAS, the one place the citation is read,
            // takes the decision (R7; the model's first HitlOverAny run
            // under handles cited an acked write over blind without it).
            let still_cited: BTreeSet<&str> = installed.entries.values().map(|e| e.key.as_str()).collect();
            let mut retired: Vec<String> = vec![];
            for (path, was) in &theirs_at_cas.entries {
                let dropped = installed.entries.get(path).map(|now| now.key != was.key).unwrap_or(true);
                if dropped && !still_cited.contains(was.key.as_str()) {
                    retired.push(was.key.clone());
                }
            }
            for (path, was) in &overridden {
                let integrated = baseline
                    .entries
                    .get(path)
                    .is_some_and(|be| be.key.as_deref() == Some(was.key.as_str()));
                if integrated || parked.contains(path) {
                    continue;
                }
                let preserved = self.preserve_conflict_copy(path, &was.key, &was.etag).await?;
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: was.etag.clone(),
                    preserved_key: Some(preserved),
                    kind: "commit-surfaced-foreign: this barrier published over a version it never \
                           integrated (another writer's publish, or a UI write consumed elsewhere); \
                           that version is preserved under this record and superseded knowingly"
                        .into(),
                    at_unix: now_unix(),
                })?;
                report.surfaced.push(path.clone());
                self.trace("surface", serde_json::json!({"flush": flush_uuid, "path": path, "etag": was.etag, "key": was.key}));
            }
            for (path, deleted) in &recreated {
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: deleted.clone(),
                    preserved_key: None,
                    kind: "commit-recreated-deleted: this barrier published over a DELETE it never \
                           integrated (another writer's, or the UI's); mine wins, the path is cited \
                           again with this tree's bytes, and the deleted version is named here"
                        .into(),
                    at_unix: now_unix(),
                })?;
                report.surfaced.push(path.clone());
                self.trace("surface", serde_json::json!({"flush": flush_uuid, "path": path, "etag": deleted, "deleted": true}));
            }
            // M3: what this barrier's DELETE removed that it never
            // integrated — theirs, changed since the baseline — is preserved
            // before the batch below takes the handle, and named.
            for (path, was) in &over_theirs {
                let preserved = self.preserve_conflict_copy(path, &was.key, &was.etag).await?;
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: was.etag.clone(),
                    preserved_key: Some(preserved),
                    kind: "commit-deleted-over-theirs: this barrier deleted a path whose version it \
                           never integrated (another writer's publish, or a UI save); the delete \
                           stands, and that version is preserved under this record"
                        .into(),
                    at_unix: now_unix(),
                })?;
                report.surfaced.push(path.clone());
                self.trace("surface", serde_json::json!({"flush": flush_uuid, "path": path, "etag": was.etag, "key": was.key, "over_delete": true}));
            }
            for path in &classified.deletes {
                if installed.entries.contains_key(path) {
                    // Never expected: since M3 a delete applies over theirs,
                    // and only uploads are parked, so every delete leaves the
                    // document. Were one still cited it is not in this
                    // boundary — PARKED, which the ack names as not carried —
                    // and the next scan classifies it again. Traced like
                    // every outcome: a replay owes a GC step for every path
                    // in the delete set (W4 phase 2, R3-S2).
                    self.trace("gc", serde_json::json!({"flush": flush_uuid, "path": path, "result": "still-cited"}));
                    report.parked.push(path.clone());
                } else {
                    let key = theirs_at_cas.entries.get(path).map(|e| e.key.clone());
                    self.trace("gc", serde_json::json!({"flush": flush_uuid, "path": path, "key": key, "result": "retired"}));
                    report.deleted.push(path.clone());
                }
            }
            if self.cfg.drill_hold_gc_secs > 0 && !retired.is_empty() {
                eprintln!(
                    "flint-sync: DRILL: holding the GC for {}s before the batch delete",
                    self.cfg.drill_hold_gc_secs
                );
                self.trace("drill_hold", serde_json::json!({"flush": flush_uuid, "where": "gc",
                    "secs": self.cfg.drill_hold_gc_secs}));
                tokio::time::sleep(std::time::Duration::from_secs(self.cfg.drill_hold_gc_secs)).await;
            }
            // RETIRE-AGE G (M1): what this commit retired was LOGGED with its
            // CAS (`manifest::cas_write_retiring`), as the gateway's commits
            // log theirs. It is collected once the retirement is
            // `retire_grace_secs` old — at once when that is 0 — by whichever
            // writer's commit section comes next; until then the sweeps spare
            // it, so a reader that loaded the document before this CAS can
            // still fetch it. No ledger, no sweep: a leak, never a deletion
            // a lagging reader meets.
            let ledger = match self.retire_ledger(now_unix()).await {
                Ok(l) => Some(l),
                Err(e) => {
                    eprintln!("flint-sync: could not read the retire logs ({e}); no sweep this time");
                    None
                }
            };
            if let Some(l) = &ledger {
                match self.reap_retired(l, &installed, &flush_uuid).await {
                    Ok(n) => report.collected = n,
                    Err(e) => eprintln!("flint-sync: retired handles not collected (next time): {e}"),
                }
            }
            if !retired.is_empty() {
                self.trace("gc", serde_json::json!({"flush": flush_uuid, "retired": retired.len(),
                    "collected": report.collected}));
            }
            // The orphan sweep (R4): a handle nothing cites, no young retire
            // log names, older than its grace — left by a writer that never
            // committed. Inside the commit section, under the cell, so the
            // commit's re-read of its own uploads above and this CAS are
            // race-free against it; never this barrier's own handles.
            if let Some(l) = &ledger {
                match self.sweep_orphans_if_due(&installed, &flush_uuid, now_unix(), &l.spared).await {
                    Ok(n) => report.swept = n,
                    Err(e) => eprintln!("flint-sync: orphan sweep (retrying next time it is due): {e}"),
                }
            }

            // Step 7: the baseline follows what this barrier published. What
            // theirs changed is owed, and the next consume derives it: the
            // pointer now names a document it never derived against. UNLESS
            // this CAS replaced exactly the document the consume derived
            // against — then the installed one is that plus this tree's own
            // changes, nothing new is owed, and the next tick stays cheap
            // (a busy agent's publishes would otherwise each cost a full
            // derive). The paths published or deleted here are no longer the
            // agent's pending work.
            let same = |a: &str, b: &str| a.trim_matches('"') == b.trim_matches('"');
            // No document replaced ("") is the empty workspace's derive.
            let over_derived = matches!(&baseline.derived_etag,
                Some(d) if same(replaced_etag.as_deref().unwrap_or(""), d));
            if over_derived {
                baseline.derived_etag = Some(installed_etag.clone());
                for path in new_baseline_entries.keys().chain(&report.deleted) {
                    baseline.skipped.remove(path);
                }
            }
            for (path, be) in new_baseline_entries {
                baseline.entries.insert(path, be);
            }
            for path in &report.deleted {
                baseline.entries.remove(path);
            }
            self.note_carrier_uncited(&installed)?;
            baseline.seq = installed.seq;
            baseline.manifest_etag = Some(installed_etag);
            baseline.prev_scan = scanned.keys().cloned().collect();
            self.state.save_baseline(&baseline)?;
            self.state.clear_intent_keys()?;
            // The commit section's end (the model's `Finish`). No window is
            // written: under P2 a UI save never waits on one (G1).
            self.trace("window_clear", serde_json::json!({"flush": flush_uuid}));
            // Reap superseded generations. Immutable metadata that is never
            // collected is a leak that grows by a whole manifest per
            // publish, and this also collects the orphan a crash between the
            // entries PUT and the pointer CAS leaves behind. Best effort by
            // design: a publish that succeeded is not un-done by a failure
            // to tidy up after it.
            let swept_generations = match manifest::sweep_generations(self.store.as_ref(), &self.cfg).await {
                Ok(0) => 0,
                Ok(n) => {
                    eprintln!("flint-sync: reaped {n} superseded manifest generation(s)");
                    n
                }
                Err(e) => {
                    eprintln!("flint-sync: generation sweep: {e}");
                    0
                }
            };
            self.trace("sweep", serde_json::json!({"flush": flush_uuid, "what": "generations", "removed": swept_generations}));
            // And the CHUNK reaper, same reason, same best-effort terms.
            // `sweep_chunks` returns 0 without a request on a workspace that
            // is not chunked, so this costs nothing until the layout moves.
            //
            // It was written to four model-established rules, guarded by six
            // mutation configs, and called from nothing but its own tests —
            // so with chunking on, every publish left its superseded chunks
            // in the bucket forever. The rules ran in the model and in the
            // suite and never once in production. `the_barrier_reaps...`
            // below is the test that asks whether this line exists at all.
            let swept_chunks = match ledger.as_ref() {
                None => Ok(0),
                Some(l) => manifest::sweep_chunks_sparing(self.store.as_ref(), &self.cfg, &l.spared).await,
            };
            let swept_chunks = match swept_chunks {
                Ok(0) => 0,
                Ok(n) => {
                    eprintln!("flint-sync: reaped {n} unreferenced manifest chunk(s)");
                    n
                }
                Err(e) => {
                    eprintln!("flint-sync: chunk sweep: {e}");
                    0
                }
            };
            self.trace("sweep", serde_json::json!({"flush": flush_uuid, "what": "chunks", "removed": swept_chunks}));
            Ok(())
        }
        .await;
        // Hand the cell on whatever happened — unless it is no longer
        // ours to hand on: a fence means a successor holds it, and the
        // barrier is simply abandoned (its manifest never installed; its
        // uploads stand, and the next barrier adopts them by flush_uuid).
        match &commit {
            Err(LeanError::Fenced(m)) => {
                self.trace("fence", serde_json::json!({"where": "commit", "detail": m}));
                self.lease = None
            }
            _ => {
                if let Err(e) = super::lease::release(self).await {
                    eprintln!(
                        "flint-sync: the publish fence could not be released ({e}); a waiter \
                         deposes it after the quiet threshold"
                    );
                }
            }
        }
        commit?;
        self.trace_barrier_end(&report, barrier_started);
        Ok(report)
    }

    fn trace_barrier_end(&self, report: &BarrierReport, started: std::time::Instant) {
        if self.cfg.event_trace.is_none() {
            return;
        }
        self.trace(
            "barrier_end",
            serde_json::json!({
                "seq": report.seq, "uploaded": report.uploaded.len(), "deleted": report.deleted.len(),
                "parked": report.parked.len(),
                "leaked": report.leaked.len(),
                "consumed": report.consumed, "no_change": report.no_change,
                "ms": started.elapsed().as_millis() as u64, "requests": self.trace_requests(),
            }),
        );
    }

    /// The > whole_put_max path: contiguous `PartSource::Local` chunks
    /// through `compose_generation` (streaming multipart; the store
    /// aborts its partial assembly on every failure path). The CRC is
    /// computed by a streaming pass first; a writer racing the compose
    /// fails server-side validation and the path DEFERS to the next
    /// barrier — publish-possibly-torn is put_whole's documented
    /// dilemma, not this path's.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn upload_compose(
        &self,
        path: &str,
        key: &str,
        // The ONE descriptor `upload_one` opened by a walk that follows
        // no link at any depth. The store reads its parts from this and
        // never reopens the path (review 2026-09-18, H5's residual).
        file: std::sync::Arc<std::fs::File>,
        size: u64,
        scanned: &scan::ScanEntry,
        stamps: GenerationStamps,
        generation: u64,
        epoch: u64,
    ) -> LeanResult<UploadOutcome> {
        // NO PRE-PASS. The store accumulates the full-object CRC from
        // the parts as it reads them for upload, so this file is read
        // ONCE (the 2026-09-10 door drill measured the second read at
        // roughly 11 s of the `mixed` workload's 24.3 s).
        // Part grid: within [min_part_size, ...], at most max_parts,
        // contiguous from 0.
        let min_part = self.store.min_part_size().max(1);
        let max_parts = self.store.max_parts().max(1) as u64;
        let mut chunk = self.cfg.whole_put_max.max(min_part);
        let need = size.div_ceil(chunk);
        if need > max_parts {
            chunk = size.div_ceil(max_parts).div_ceil(min_part) * min_part;
        }
        let mut parts = vec![];
        let mut off = 0u64;
        while off < size {
            let len = chunk.min(size - off);
            parts.push(flint_store::PartSource::Local { offset: off, len });
            off += len;
        }


        // A fresh handle: the Complete lands on a key nobody else writes
        // (design 2026-09-19, R1). The one thing that can already sit
        // there is this barrier's OWN Complete whose response was lost —
        // a torn response — and the recogniser is the bytes' checksum:
        // the same, cite them (re-read under the fence first, like every
        // citation); different, nothing minted this key but us, so the
        // barrier refuses rather than guess. The 412 arms this replaced
        // — adopt-own through the crash journal, preserve-a-foreign-
        // version, park — arbitrated a slot that no longer exists.
        let spec = flint_store::ComposeSpec {
            progress: None,
            key,
            local: Some(file),
            parts: parts.clone(),
            base_key: None,
            base_etag: None,
            condition: PutCondition::IfNoneMatchAny,
            stamps: stamps.clone(),
            crc64: None,
        };
        match self.store.compose_generation(&spec).await {
            Ok(meta) => {
                // The store computed it; refuse to cite bytes nothing
                // vouched for rather than invent a number for the
                // manifest. A backend that returns no checksum has not
                // validated the publish server-side either.
                let crc = meta
                    .crc64_b64
                    .as_deref()
                    .and_then(flint_store::crc64_from_b64)
                    .ok_or_else(|| {
                        LeanError::State(format!(
                            "compose of {key} returned no full-object checksum — refusing to \
                             cite bytes nothing vouched for"
                        ))
                    })?;
                Ok(UploadOutcome::published(
                    path, key.to_string(), meta.etag, crc, size, scanned, generation, epoch,
                ))
            }
            Err(StoreError::ChecksumMismatch(_)) | Err(StoreError::NoSuchUpload(_)) => {
                // Drift mid-compose, or the operator sweep aborted us:
                // nothing published; the next barrier re-queues.
                Ok(UploadOutcome::Deferred)
            }
            Err(StoreError::PreconditionFailed(_)) => {
                // The torn-response recogniser compares OUR bytes' checksum
                // against what landed, and without the pre-pass we do not
                // have one — so the file is read HERE, on the rare recovery
                // path, instead of on every publish.
                let crc = file_crc(&self.cfg.root, path)?;
                let head = self.store.head(key).await?;
                if head.crc64_b64.as_deref() == Some(crc64_to_b64(crc).as_str()) {
                    let g = GenerationStamps::from_meta(&head.meta).map(|s| s.generation).unwrap_or(generation);
                    return Ok(UploadOutcome::adopted(
                        path, key.to_string(), head.etag, crc, head.size, scanned, g, epoch,
                    ));
                }
                Err(LeanError::State(format!(
                    "handle {key} already holds different bytes (etag {}): a handle is minted once \
                     per flush, so something other than this barrier wrote it; refusing",
                    head.etag
                )))
            }
            Err(e) => Err(e.into()),
        }
    }


    async fn upload_one(
        &self,
        path: &str,
        scanned: &scan::ScanEntry,
        base: Option<&BaselineEntry>,
        epoch: u64,
        flush_uuid: &str,
    ) -> LeanResult<UploadOutcome> {
        let key = self.cfg.handle_key(path, flush_uuid);
        let local_path = self.cfg.root.join(path);
        let generation = base.map(|b| b.generation + 1).unwrap_or(1);
        // Review 2026-09-12, atomicity-6: the scan skipped symlinks; this
        // read followed them. A regular file swapped for a symlink between
        // the two published the link's TARGET — a file outside the
        // workspace, the syncer's own /proc/self/environ included. Every
        // stat here is an lstat and the read opens O_NOFOLLOW; a path that
        // is no longer a regular file is refused, recorded, and deferred
        // (the next scan skips it, and the two-scan rule retires it).
        //
        // Review 2026-09-18, H5: and not only the last component. A
        // DIRECTORY on the path swapped for a link (`mv d d.bak; ln -s
        // /proc/self d`) made `d/environ` a regular file outside the
        // workspace; the lstat and the O_NOFOLLOW open guarded the last
        // component only. The file is opened by a walk that follows no
        // link at any depth, and everything below — the stamps, the
        // size, the bytes — comes from that one descriptor.
        let file = match super::safefs::open_beneath_nofollow(&self.cfg.root, path) {
            Ok(f) => f,
            Err(e)
                if e.kind() == std::io::ErrorKind::InvalidInput
                    || matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) =>
            {
                self.state.append_conflict(&ConflictRecord {
                    path: path.to_string(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: format!(
                        "upload-refused-not-regular: no longer a regular file inside the workspace after \
                         the scan (a symlink or special file replaced it or a directory above it: {e}); \
                         nothing published"
                    ),
                    at_unix: now_unix(),
                })?;
                return Ok(UploadOutcome::Deferred);
            }
            Err(e) => return Err(LeanError::State(format!("stat {}: {e}", local_path.display()))),
        };
        let lmeta = file
            .metadata()
            .map_err(|e| LeanError::State(format!("stat {}: {e}", local_path.display())))?;
        let posix = Some(PosixStamps::from_metadata(&lmeta));
        let stamps = GenerationStamps {
            generation,
            epoch,
            flush_uuid: flush_uuid.to_string(),
            boundary_source: None,
            posix,
        };
        let size = lmeta.len();
        if size > self.cfg.whole_put_max {
            // Streaming multipart compose: put_whole is never fed past
            // whole_put_max (unbounded memory + S3's 5 GiB wall).
            return self
                .upload_compose(path, &key, std::sync::Arc::new(file), size, scanned, stamps, generation, epoch)
                .await;
        }
        // The upload byte gate — the store's own, shared with the compose
        // arm above, which charges its parts inside the store. This body
        // is charged from before it is read (the read is the allocation)
        // until its PUT, retried or not, has returned: the permit lives
        // to the end of this function. `None` = a store with no gate.
        let _upload_permit = match self.store.upload_gate() {
            Some(g) => Some(g.acquire(size).await),
            None => None,
        };
        // `Bytes::from(Vec<u8>)` TAKES OWNERSHIP without copying, so the
        // old `Bytes::from(body.clone())` was a full memcpy of every
        // published file body — bought solely to leave `body` intact for
        // the 412 retry below, which is reached almost never. Build the
        // `Bytes` once; the retry gets a refcount clone.
        let body = Bytes::from({
            use std::io::Read;
            let mut v = Vec::with_capacity(size as usize);
            (&file).read_to_end(&mut v)?;
            v
        });
        let uploaded_len = body.len() as u64;
        let crc = crc64_nvme(&body);
        // A fresh handle (design 2026-09-19, R1): no If-Match, because
        // nothing else ever writes this key. The one thing that can sit
        // there is this barrier's OWN PUT whose response was lost, and
        // the bytes' checksum recognises it: the same, cite it (re-read
        // under the fence first, like every citation); different, nothing
        // minted this key but us, so refuse rather than guess. The 412
        // policy this replaced — adopt-own, preserve-then-supersede a
        // foreign version, park, the 404-on-If-Match create arm —
        // arbitrated a slot that no longer exists; a foreign version is
        // now met at the CAS, the one place a citation is read (R7).
        match self
            .store
            .put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps, crc)
            .await
        {
            Ok(meta) => Ok(UploadOutcome::published(
                path, key, meta.etag, crc, uploaded_len, scanned, generation, epoch,
            )),
            Err(StoreError::PreconditionFailed(_)) => {
                let head = self.store.head(&key).await?;
                if head.crc64_b64.as_deref() == Some(crc64_to_b64(crc).as_str()) {
                    let g = GenerationStamps::from_meta(&head.meta).map(|s| s.generation).unwrap_or(generation);
                    return Ok(UploadOutcome::adopted(
                        path, key, head.etag, crc, head.size, scanned, g, epoch,
                    ));
                }
                Err(LeanError::State(format!(
                    "handle {key} already holds different bytes (etag {}): a handle is minted once \
                     per flush, so something other than this barrier wrote it; refusing",
                    head.etag
                )))
            }
            Err(e) => Err(e.into()),
        }
    }
}

enum UploadOutcome {
    /// `adopted`: the etag was OBSERVED at the handle (bytes already
    /// there, no PUT of ours produced them — a torn response), so the
    /// commit section re-reads it before citing, as it does every
    /// citation.
    Published { entry: LeanEntry, baseline_entry: BaselineEntry, adopted: bool },
    /// The source drifted mid-transfer (checksum refused server-side)
    /// or the assembly was swept: publish nothing, advance nothing —
    /// the next scan re-queues the path.
    Deferred,
}

impl UploadOutcome {
    #[allow(clippy::too_many_arguments)]
    fn published(
        path: &str,
        key: String,
        etag: String,
        crc: u64,
        uploaded_len: u64,
        scanned: &scan::ScanEntry,
        generation: u64,
        epoch: u64,
    ) -> UploadOutcome {
        let _ = path;
        UploadOutcome::Published {
            adopted: false,
            entry: LeanEntry {
                key: key.clone(),
                etag: etag.clone(),
                crc64_b64: crc64_to_b64(crc),
                // The length the object HAS (review 2026-09-12,
                // atomicity-1): the scanned size was cited while the
                // body was read fresh, so a file that grew between the
                // two was cited short and every fresh checkout of it
                // failed its CRC fold — the successor never started.
                size: uploaded_len,
                mode: scanned.mode,
                mtime_unix: scanned.mtime_unix,
                generation,
                epoch,
            },
            baseline_entry: BaselineEntry {
                etag,
                key: Some(key),
                generation,
                // The PRE-read stat: if the agent wrote during our read,
                // the next scan sees the drift and re-queues (the
                // re-stat/re-queue valve).
                size: scanned.size,
                mtime_unix: scanned.mtime_unix,
                mtime_nanos: Some(scanned.mtime_nanos),
                crc64_b64: Some(crc64_to_b64(crc)),
            },
        }
    }
}

impl UploadOutcome {
    /// `published`, for bytes this barrier found already at the key and
    /// cites without a PUT of its own.
    #[allow(clippy::too_many_arguments)]
    fn adopted(
        path: &str,
        key: String,
        etag: String,
        crc: u64,
        uploaded_len: u64,
        scanned: &scan::ScanEntry,
        generation: u64,
        epoch: u64,
    ) -> UploadOutcome {
        match UploadOutcome::published(path, key, etag, crc, uploaded_len, scanned, generation, epoch) {
            UploadOutcome::Published { entry, baseline_entry, .. } => {
                UploadOutcome::Published { entry, baseline_entry, adopted: true }
            }
            other => other,
        }
    }
}

/// The paths THIS barrier must withhold because the document would cite a
/// file and something under it (a published file replaced by a directory,
/// or the reverse). Whichever side is this barrier's upsert yields; if both
/// are, so does the parent.
///
/// Every key under `p/` sits in one contiguous run of the sorted map, so a
/// range lookup finds them: O(N log N). The first version scanned every
/// key for every key, and a 200k-path commit spent 80 s here (bench
/// 2026-09-25). The run does NOT start right after `p`: `p-b` and `p.txt`
/// sort between `p` and `p/`.
pub(super) fn path_clashes<V, U>(
    cited: &BTreeMap<String, V>,
    upserts: &BTreeMap<String, U>,
) -> Vec<String> {
    let mut mine: Vec<String> = vec![];
    for p in cited.keys() {
        let pfx = format!("{p}/");
        let mut under = cited.range::<str, _>((std::ops::Bound::Included(pfx.as_str()), std::ops::Bound::Unbounded))
            .map(|(k, _)| k)
            .take_while(|q| q.starts_with(&pfx))
            .peekable();
        if under.peek().is_none() {
            continue;
        }
        if upserts.contains_key(p) {
            mine.push(p.clone());
        } else if let Some(c) = under.find(|q| upserts.contains_key(*q)) {
            mine.push(c.clone());
        }
    }
    mine.sort();
    mine.dedup();
    mine
}

pub(super) fn local_dirty(local: &Path, base: Option<&BaselineEntry>) -> bool {
    match (std::fs::metadata(local), base) {
        (Err(_), None) => false,                   // both absent: clean
        (Err(_), Some(_)) => true,                 // locally deleted vs baseline
        (Ok(_), None) => true,                     // local exists, never published
        (Ok(m), Some(b)) => {
            scan::stat_changed(b.size, b.mtime_unix, b.mtime_nanos, m.len(), mtime_of(&m), mtime_nanos_of(&m))
        }
    }
}

fn file_crc(root: &Path, rel: &str) -> LeanResult<u64> {
    use std::io::Read;
    let mut crc = flint_store::Crc64Nvme::new();
    // No link anywhere on the path (H5): this hashes what a 412 recovery
    // is about to call "already there".
    let mut f = super::safefs::open_beneath_nofollow(root, rel)?;
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        crc.update(&buf[..n]);
    }
    Ok(crc.finalize())
}

pub(super) fn mtime_nanos_of(m: &std::fs::Metadata) -> u32 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
}

pub(super) fn mtime_of(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Validate `rel` under `root` WITHOUT creating anything — for callers
/// that must decide whether a path is writable before doing any work
/// (inbox consume refuses a refused path outright rather than routing it
/// through conflict-preserve, which would cost a GET+PUT and leave the
/// path in the baseline).
pub(super) fn check_contained(root: &Path, rel: &str) -> LeanResult<()> {
    resolve_contained(root, rel, false).map(|_| ())
}

/// Resolve `rel` under `root`, refusing escapes, creating missing parent
/// directories. Walks the parent chain component by component: a
/// component that EXISTS as a symlink is a refusal (never followed).
pub(super) fn contained_path(root: &Path, rel: &str) -> LeanResult<std::path::PathBuf> {
    resolve_contained(root, rel, true).map(|(p, _)| p)
}

/// As `contained_path`, and also what the walk already learned about
/// the FINAL component: its `lstat` if it exists, `None` if it does
/// not. The walk stats every component to refuse a symlink; a caller
/// that then asked `exists()` and `metadata()` paid two more stats for
/// an answer it was already holding (checkout's resume check did, on
/// every one of 20,000 files).
pub(super) fn contained_path_stat(
    root: &Path,
    rel: &str,
) -> LeanResult<(std::path::PathBuf, Option<std::fs::Metadata>)> {
    resolve_contained(root, rel, true)
}

fn resolve_contained(
    root: &Path,
    rel: &str,
    create_dirs: bool,
) -> LeanResult<(std::path::PathBuf, Option<std::fs::Metadata>)> {
    use std::path::Component;
    let refuse = |why: &str| {
        Err(LeanError::State(format!(
            "refusing write to {rel:?}: {why} (containment)"
        )))
    };
    if rel.is_empty() {
        return refuse("empty path");
    }
    if super::scan::is_control_path(rel) {
        // The reserved namespace is the syncer's own; a citation or
        // inbox entry naming it is surfaced, never materialized (D0.3).
        return refuse("reserved control namespace");
    }
    if rel.split('/').any(|seg| seg.ends_with(super::scan::TMP_SUFFIX)) {
        // Review 2026-09-18, C1: the walk skips this suffix at every
        // depth (the consume's own temp sibling, atomicity-7), so a file
        // materialised under it is absent from every scan — cited once
        // by the repair as a first absence, then classified a delete and
        // its object collected. Surfaced and never materialised, like the
        // control namespace; the gateway refuses the name at the door.
        return refuse("a name reserved for the syncer's temporary files");
    }
    let relp = Path::new(rel);
    let mut cur = root.to_path_buf();
    let mut comps: Vec<&std::ffi::OsStr> = vec![];
    for c in relp.components() {
        match c {
            Component::Normal(n) => comps.push(n),
            Component::CurDir => {}
            Component::ParentDir => return refuse("`..` component"),
            Component::RootDir | Component::Prefix(_) => return refuse("absolute path"),
        }
    }
    if comps.is_empty() {
        return refuse("no path components");
    }
    let last = comps.len() - 1;
    let mut final_meta = None;
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return refuse("path traverses a symlink");
            }
            Ok(m) if i < last && !m.is_dir() => {
                return refuse("parent component is not a directory");
            }
            Ok(m) => {
                if i == last {
                    final_meta = Some(m);
                }
            }
            Err(_) if i < last => {
                if create_dirs {
                    if let Err(e) = std::fs::create_dir(&cur) {
                        // A SIBLING may have created it between the stat
                        // above and this mkdir: fetches run on their own
                        // tasks, so the first files of one directory
                        // race here, and the loser's EEXIST is not a
                        // containment refusal. It is fine only if what
                        // exists now is a plain directory — a symlink
                        // that appeared in the window is refused exactly
                        // as one the stat found.
                        match std::fs::symlink_metadata(&cur) {
                            Ok(m) if m.file_type().is_symlink() => {
                                return refuse("path traverses a symlink");
                            }
                            Ok(m) if m.is_dir() => {}
                            _ => {
                                return Err(LeanError::State(format!(
                                    "mkdir {}: {e}",
                                    cur.display()
                                )))
                            }
                        }
                    }
                }
            }
            Err(_) => {}
        }
    }
    Ok((cur, final_meta))
}

/// Write `bytes` to `path` through an exclusive temp sibling and a
/// rename. The parent already exists — containment created it — and
/// one that vanished since is recreated on the retry path in `safefs`,
/// so the common path pays no `mkdir` and no `stat` for it. Returns
/// the written file's metadata.
pub(super) fn write_file_atomic(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> LeanResult<std::fs::Metadata> {
    super::safefs::check_parent(path)?;
    // NOT with_extension(): that REPLACES the final extension, so
    // "a.txt" and "a.md" would collide on one tmp name.
    let tmp = path.with_file_name(format!(
        "{}.flint-sync-tmp",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    ));
    // The temp sibling is computed AFTER containment ran, so the walk
    // never saw it: it needs its own refusal, not a plain write.
    // FAST (no per-file fsync): materialisations are made durable by
    // `sync_tree` before the marker or baseline that vouches for them.
    super::safefs::write_via_tmp_fast(path, &tmp, bytes, mode)
}

/// `write_file_atomic`, renamed over `path` only if `proceed()` holds once
/// the temp is written (`safefs::write_via_tmp_fast_if`).
pub(super) fn write_file_atomic_if(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    proceed: &dyn Fn() -> bool,
) -> LeanResult<Option<std::fs::Metadata>> {
    super::safefs::check_parent(path)?;
    let tmp = path.with_file_name(format!(
        "{}.flint-sync-tmp",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    ));
    super::safefs::write_via_tmp_fast_if(path, &tmp, bytes, mode, proceed)
}

// Test-only: run a closure inside a write window for one path —
// `"before-write"` (the licence to overwrite checked, the temp not yet
// written), `"after-rename"` (before the baseline records the write), or
// `"before-delete"` (`sync`'s remote-delete arm). The consume and `sync`
// both open them. Nothing in either window calls the store, so the hooked double
// cannot reach them (review 2026-09-18, H7).
#[cfg(test)]
thread_local! {
    pub(super) static CONSUME_WINDOW_HOOK: std::cell::RefCell<Option<(String, &'static str, Box<dyn Fn()>)>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(super) fn consume_window(stage: &'static str, path: &str) {
    let hook = CONSUME_WINDOW_HOOK.with(|h| {
        let mut h = h.borrow_mut();
        if matches!(&*h, Some((p, s, _)) if p == path && *s == stage) {
            h.take().map(|(_, _, f)| f)
        } else {
            None
        }
    });
    if let Some(f) = hook {
        f();
    }
}

#[cfg(not(test))]
#[inline(always)]
pub(super) fn consume_window(_: &'static str, _: &str) {}

/// How many merge races a commit may lose before its barrier gives up (the
/// next tick tries again). Under P2 the UI saves without the lease (G1), so
/// the races are lost to saves, and backing off finds the gaps between them.
const CAS_ATTEMPTS: u32 = 12;

/// The wait after the `attempt`-th lost race: 10 ms, doubling, capped at
/// 640 ms, plus up to half again of jitter from the clock's nanoseconds, so
/// a writer and a stream of saves do not fall into lockstep.
fn cas_backoff(attempt: u32) -> std::time::Duration {
    let base = 10u64 << attempt.saturating_sub(1).min(6);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    std::time::Duration::from_millis(base + nanos % (base / 2 + 1))
}


/// The merge base, P1-lite: the baseline's own citations. A tree holds no
/// version the document never cited (P2), so what it integrated IS what it
/// merges from; one base serves the merge and every reader of its outcome
/// (M5, L-124).
fn merge_base(baseline: &super::state::Baseline) -> BTreeMap<String, String> {
    baseline.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect()
}

/// One three-way merge of a barrier's changes onto the bucket's manifest.
struct MergeOnto {
    theirs: manifest::LeanManifest,
    /// The CAS handle over `theirs`; `None` when the bucket has no manifest.
    expected: Option<manifest::ManifestHandle>,
    merged: manifest::LeanManifest,
    /// Other writers' changes since the merge base that the tree lacks.
    foreign: Vec<(String, LeanEntry)>,
    /// Theirs' version at a path this barrier uploaded, changed since the
    /// merge base: what mine publishes OVER. The caller surfaces those it
    /// never integrated (design 2026-09-19, R7).
    overridden: Vec<(String, LeanEntry)>,
    /// Theirs' DELETES this barrier's uploads publish over (`merge`).
    recreated: Vec<(String, String)>,
    /// Theirs' version at a path this barrier DELETES, changed since the
    /// merge base (M3): the caller preserves and records each.
    over_theirs: Vec<(String, LeanEntry)>,
}

impl MergeOnto {
    /// The merge adds nothing to the document: installing it would be an
    /// empty generation.
    fn adds_nothing(&self) -> bool {
        self.merged.entries == self.theirs.entries && self.merged.sole_writer == self.theirs.sole_writer
    }
}

impl Syncer {
    /// H10: the carried uploads the document this barrier ended at does not
    /// cite (a peer deleted them since). The ack names that document, so it
    /// reads this; no request is made for it.
    fn note_carrier_uncited(&self, doc: &manifest::LeanManifest) -> LeanResult<()> {
        let mut intent = self.state.load_intent()?;
        let Some(c) = intent.carrier.as_mut() else { return Ok(()) };
        let uncited: BTreeSet<String> = c
            .paths
            .iter()
            .filter(|p| !c.deletes.contains(*p) && !doc.entries.contains_key(p.as_str()))
            .cloned()
            .collect();
        if uncited != c.uncited {
            c.uncited = uncited;
            self.state.save_intent(&intent)?;
        }
        Ok(())
    }

    fn merge_onto(
        &self,
        current: Option<&manifest::LoadedManifest>,
        // `merge_base`'s answer, which the caller also gives every other
        // reader of this merge's outcome.
        base: &BTreeMap<String, String>,
        upserts: &BTreeMap<String, LeanEntry>,
        deletes: &BTreeSet<String>,
        parked: &BTreeSet<String>,
        flush_uuid: &str,
    ) -> MergeOnto {
        let (theirs, expected) = match current {
            // The handle carries the LAYOUT as well as the tag: a
            // workspace still on the legacy single object CASes a
            // pointer that must not exist yet, not one it read.
            Some(l) => (l.manifest.clone(), Some(l.handle())),
            None => (Default::default(), None),
        };
        let manifest::Merged { doc: mut merged, foreign, overridden, deletes: outcomes, recreated, over_theirs } =
            manifest::merge(base, &theirs, upserts, deletes, parked);
        // The deletes theirs outranked, named: the trace's record of the
        // merge's per-path decision (`TraceCore.tla` checks it against
        // `Install`'s `Foreign(s, p)`).
        // Since M3 no delete is outranked; the trace keeps the field (empty)
        // and names the deletes that went over theirs.
        let outranked: Vec<&String> = vec![];
        let deleted_over: Vec<&String> =
            outcomes.iter().filter(|(_, o)| **o == manifest::DeleteOutcome::OverTheirs).map(|(p, _)| p).collect();
        // Deletions another writer made since this workspace's
        // merge base: in the base, gone from theirs, and not this
        // barrier's own upsert, delete or park. `merge` has no use
        // for them — theirs already lacks the path — but the TREE
        // does: they reach it through the local queue, as the
        // foreign upserts do.
        let gone: Vec<(String, String)> = base
            .iter()
            .filter(|(p, _)| {
                !theirs.entries.contains_key(*p)
                    && !upserts.contains_key(*p)
                    && !deletes.contains(*p)
                    && !parked.contains(*p)
            })
            .map(|(p, retired)| (p.clone(), retired.clone()))
            .collect();
        // `merge` clears it; the installing pass owns it. A mirror
        // is a property of how this workspace is DEPLOYED, so it
        // comes from config on every publish rather than being
        // inherited from whatever wrote last.
        merged.sole_writer = self.cfg.sole_writer;
        self.trace("merge", serde_json::json!({"flush": flush_uuid, "theirs_seq": theirs.seq, "upserts": upserts.len(),
            "deletes": deletes.len(), "foreign": foreign.len(), "gone": gone.len(), "overridden": overridden.len(),
            "outranked": outranked, "over_theirs": deleted_over, "adds_nothing": merged.entries == theirs.entries}));
        MergeOnto { theirs, expected, merged, foreign, overridden, recreated, over_theirs }
    }
}
