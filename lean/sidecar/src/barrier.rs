//! The publish barrier (plan §2.1, seven steps) — the machine
//! `lean/formal/LeanSubtree.tla` checks. Order is load-bearing:
//! consume → scan → intent/window → uploads → manifest CAS (merge) →
//! GC deletes LAST → baseline rewrite.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bytes::Bytes;

use flint_store::{
    crc64_nvme, crc64_to_b64, GenerationStamps, PosixStamps, PutCondition, StoreError,
};

use super::inbox::{self, InboxEntry};
use super::manifest::{self, LeanEntry};
use super::scan;
use super::state::{BaselineEntry, ConflictRecord, IntentJournal};
use super::{now_unix, LeanError, LeanResult, Sidecar};

#[derive(Debug, Default)]
pub struct BarrierReport {
    pub seq: Option<u64>,
    pub uploaded: Vec<String>,
    pub deleted: Vec<String>,
    pub parked: Vec<String>,
    pub consumed: usize,
    pub foreign_queued: usize,
    pub no_change: bool,
}

impl Sidecar {
    fn lease_epoch(&self) -> LeanResult<u64> {
        self.lease
            .as_ref()
            .map(|l| l.epoch)
            .ok_or_else(|| LeanError::State("barrier without a held lease".into()))
    }

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

    /// Step 1: consume the inbox. A barrier NEVER runs against an
    /// unconsumed inbox — this is what makes HITL uploads structurally
    /// un-amputatable. Returns the entries integrated (dropped from the
    /// cell at the window-open CAS).
    pub async fn consume_inbox(&mut self) -> LeanResult<Vec<InboxEntry>> {
        let loaded = inbox::load(self.store.as_ref(), &self.cfg).await?;
        let mut consumed = vec![];
        let mut baseline = self.state.load_baseline()?;
        for entry in &loaded.doc.entries {
            let key = self.cfg.file_key(&entry.path);
            // Already integrated (a crashed earlier consume): idempotent.
            if baseline.entries.get(&entry.path).map(|b| b.etag == entry.etag).unwrap_or(false) {
                consumed.push(entry.clone());
                continue;
            }
            let head = match self.store.head(&key).await {
                Ok(m) => m,
                Err(StoreError::NotFound(_)) => {
                    // The object vanished under a tracked entry: surface
                    // it — the bytes are NOT recoverable from here.
                    self.state.append_conflict(&ConflictRecord {
                        path: entry.path.clone(),
                        foreign_etag: entry.etag.clone(),
                        preserved_key: None,
                        kind: "consume-object-missing".into(),
                        at_unix: now_unix(),
                    })?;
                    consumed.push(entry.clone());
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if head.etag != entry.etag {
                // Superseded by a newer write (its own inbox entry
                // follows, or it is the sidecar's): drop.
                consumed.push(entry.clone());
                continue;
            }
            let local_path = self.cfg.root.join(&entry.path);
            let dirty = local_dirty(&local_path, baseline.entries.get(&entry.path));
            if dirty {
                // Locally-dirty wins; preserve the FOREIGN bytes first
                // (a conflict record must keep both versions
                // recoverable), then advance the recognized ETag so our
                // eventual publish supersedes it KNOWINGLY.
                let preserved = self.preserve_conflict_copy(&entry.path, &head.etag).await?;
                self.state.append_conflict(&ConflictRecord {
                    path: entry.path.clone(),
                    foreign_etag: entry.etag.clone(),
                    preserved_key: Some(preserved),
                    kind: "consume-dirty".into(),
                    at_unix: now_unix(),
                })?;
                let stamps = GenerationStamps::from_meta(&head.meta);
                baseline.entries.insert(
                    entry.path.clone(),
                    BaselineEntry {
                        etag: head.etag.clone(),
                        generation: stamps.map(|s| s.generation).unwrap_or(0),
                        // Deliberately NOT the local file's stat: the
                        // path must still scan as dirty so the local
                        // version publishes.
                        size: u64::MAX,
                        mtime_unix: 0,
                    },
                );
            } else {
                // Clean: adopt the foreign content into the tree.
                let (meta, body) =
                    self.store.get_whole(&key, Some(&entry.etag)).await.map_err(|e| match e {
                        StoreError::PreconditionFailed(m) => {
                            StoreError::Other(format!("consume raced a newer write: {m}"))
                        }
                        other => other,
                    })?;
                let mode = PosixStamps::from_meta(&meta.meta).map(|p| p.mode);
                write_file_atomic(&local_path, &body, mode)?;
                let st = std::fs::metadata(&local_path)?;
                let stamps = GenerationStamps::from_meta(&meta.meta);
                baseline.entries.insert(
                    entry.path.clone(),
                    BaselineEntry {
                        etag: meta.etag.clone(),
                        generation: stamps.map(|s| s.generation).unwrap_or(0),
                        size: st.len(),
                        mtime_unix: mtime_of(&st),
                    },
                );
                baseline.prev_scan.insert(entry.path.clone());
            }
            consumed.push(entry.clone());
        }
        self.state.save_baseline(&baseline)?;
        Ok(consumed)
    }

    async fn preserve_conflict_copy(&self, path: &str, etag: &str) -> LeanResult<String> {
        let key = self.cfg.file_key(path);
        let dst = self.cfg.conflict_key(&uuid::Uuid::new_v4().to_string(), path);
        // v1: client-side copy (GET + guarded PUT). The server-side
        // CopyObject lever is the designed v2 optimization for large
        // files.
        let (meta, body) = self.store.get_whole(&key, Some(etag)).await?;
        let crc = crc64_nvme(&body);
        let stamps = GenerationStamps {
            generation: 0,
            epoch: self.lease.as_ref().map(|l| l.epoch).unwrap_or(0),
            flush_uuid: "conflict-preserve".into(),
            posix: PosixStamps::from_meta(&meta.meta),
        };
        self.store
            .put_whole(&dst, body, &PutCondition::IfNoneMatchAny, &stamps, crc)
            .await?;
        Ok(dst)
    }

    /// Steps 2–7. `barrier` = one full publish cycle.
    pub async fn run_barrier(&mut self) -> LeanResult<BarrierReport> {
        let epoch = self.lease_epoch()?;
        let mut report = BarrierReport::default();

        // Cooperative deposal check BEFORE any write (a thawed
        // straggler fences here instead of landing data PUTs). This
        // narrows the window; the per-request epoch validation that
        // CLOSES it is the gateway's (P5) — the model's LeanNoEpochCheck
        // mutation is the proof rotation alone does not cover the data
        // path.
        self.verify_not_deposed().await?;

        // Step 1.
        let consumed = self.consume_inbox().await?;
        report.consumed = consumed.len();

        // Step 2: scan-diff against the persisted baseline.
        let mut baseline = self.state.load_baseline()?;
        let scanned = scan::scan(&self.cfg.root)?;
        let classified = scan::classify(&scanned, &baseline);

        // Skip-on-no-diff: nothing local, nothing consumed, no pending
        // citation repair, and the bucket manifest where we left it.
        let repairs_pending = baseline
            .entries
            .iter()
            .any(|(p, be)| be.size != u64::MAX && baseline.inst_base.get(p) != Some(&be.etag));
        if classified.uploads.is_empty()
            && classified.deletes.is_empty()
            && consumed.is_empty()
            && classified.first_absence.is_empty()
            && !repairs_pending
        {
            // HEAD, never GET: the 0b rig measured the idle tick at 1M
            // entries as 27 s / 1.3 GiB — dominated by fetching and
            // parsing a 264 MiB manifest just to read `seq`. The
            // document ETag against the persisted one answers the same
            // question for the price of one HEAD.
            let unchanged = match self.store.head(&self.cfg.manifest_key()).await {
                Ok(meta) => baseline.manifest_etag.as_deref() == Some(meta.etag.as_str()),
                Err(StoreError::NotFound(_)) => baseline.manifest_etag.is_none(),
                Err(e) => return Err(e.into()),
            };
            if unchanged {
                report.no_change = true;
                report.seq = Some(baseline.seq);
                // prev_scan still advances (the two-scan rule's clock) —
                // but the baseline document (hundreds of MB at 1M
                // entries) is rewritten only when the scan SET moved.
                let now_present: std::collections::BTreeSet<String> =
                    scanned.keys().cloned().collect();
                if now_present != baseline.prev_scan {
                    baseline.prev_scan = now_present;
                    self.state.save_baseline(&baseline)?;
                }
                return Ok(report);
            }
        }

        // Step 3: intent journal, then the window (the commitment
        // point: the same CAS drops the consumed entries).
        let flush_uuid = uuid::Uuid::new_v4().to_string();
        let mut intent = self.state.load_intent()?;
        let prior_uuids = {
            let mut v = intent.recent_uuids.clone();
            if !intent.flush_uuid.is_empty() {
                v.push(intent.flush_uuid.clone());
            }
            v
        };
        intent = IntentJournal {
            flush_uuid: flush_uuid.clone(),
            keys: classified.uploads.iter().map(|p| self.cfg.file_key(p)).collect(),
            recent_uuids: prior_uuids.clone(),
        };
        self.state.save_intent(&intent)?;
        let deadline = now_unix() + self.cfg.window_slack_secs;
        inbox::open_window(self.store.as_ref(), &self.cfg, epoch, deadline).await?;
        if !consumed.is_empty() {
            // Drop consumed entries now that they are durably in the
            // baseline (idempotent if this crashes: re-consume skips).
            inbox::drop_entries(self.store.as_ref(), &self.cfg, epoch, &consumed).await?;
        }

        // Step 4: guarded uploads, fanned out under a bounded window
        // (each key's guard chain is independent; the 412 policy and
        // conflict records are applied to the collected results below,
        // in deterministic path order).
        let mut upserts: BTreeMap<String, LeanEntry> = BTreeMap::new();
        let mut parked: BTreeSet<String> = BTreeSet::new();
        let mut new_baseline_entries: BTreeMap<String, BaselineEntry> = BTreeMap::new();
        let outcomes: Vec<(String, LeanResult<UploadOutcome>)> = {
            use futures::stream::{self, StreamExt};
            let this: &Sidecar = &*self;
            stream::iter(classified.uploads.iter().map(|path| {
                let scanned_entry = &scanned[path];
                let base = baseline.entries.get(path);
                let flush_uuid = &flush_uuid;
                let prior_uuids = &prior_uuids;
                async move {
                    let r = this
                        .upload_one(path, scanned_entry, base, epoch, flush_uuid, prior_uuids)
                        .await;
                    (path.clone(), r)
                }
            }))
            .buffer_unordered(self.cfg.fanout.max(1))
            .collect()
            .await
        };
        let mut outcomes = outcomes;
        outcomes.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, outcome) in outcomes {
            let path = &path;
            match outcome? {
                UploadOutcome::Published { entry, baseline_entry } => {
                    upserts.insert(path.clone(), entry);
                    new_baseline_entries.insert(path.clone(), baseline_entry);
                    report.uploaded.push(path.clone());
                }
                UploadOutcome::Parked { foreign_etag } => {
                    parked.insert(path.clone());
                    self.state.append_conflict(&ConflictRecord {
                        path: path.clone(),
                        foreign_etag,
                        preserved_key: None, // parking preserves in place
                        kind: "upload-412-parked".into(),
                        at_unix: now_unix(),
                    })?;
                    report.parked.push(path.clone());
                }
            }
        }

        // Citation repairs: paths whose integrated object (the
        // baseline) differs from the manifest's citation — consumed
        // HITL adoptions and checkout's S3-wins arm. No bytes move; the
        // manifest re-cites what this sidecar already integrated.
        // Without this, an adopted upload is clean-vs-baseline, never
        // enters the upload set, and the manifest silently drops it —
        // the battery's amputation leg caught exactly that.
        let repair_candidates: Vec<String> = baseline
            .entries
            .iter()
            .filter(|(path, be)| {
                !classified.uploads.contains(*path)
                    && !classified.deletes.contains(*path)
                    && be.size != u64::MAX // consume-dirty sentinel: publishes via upload
                    && baseline.inst_base.get(*path) != Some(&be.etag)
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in repair_candidates {
            let key = self.cfg.file_key(&path);
            let be = baseline.entries[&path].clone();
            match self.store.head(&key).await {
                Ok(meta) if meta.etag == be.etag => {
                    let stamps = GenerationStamps::from_meta(&meta.meta);
                    let scan_entry = scanned.get(&path);
                    upserts.insert(
                        path.clone(),
                        LeanEntry {
                            key,
                            etag: meta.etag.clone(),
                            crc64_b64: meta.crc64_b64.clone(),
                            size: meta.size,
                            mode: stamps
                                .as_ref()
                                .and_then(|s| s.posix)
                                .map(|p| p.mode)
                                .or(scan_entry.map(|s| s.mode))
                                .unwrap_or(0o644),
                            mtime_unix: scan_entry.map(|s| s.mtime_unix).unwrap_or(0),
                            generation: stamps.map(|s| s.generation).unwrap_or(be.generation),
                            epoch,
                        },
                    );
                }
                // Moved again or gone: the next consume reconciles it.
                _ => {}
            }
        }

        // Step 5: the manifest CAS (three-way merge; bounded retries).
        self.verify_not_deposed().await?;
        let mut foreign_entries: Vec<(String, LeanEntry)> = vec![];
        let mut attempt = 0;
        let (installed, installed_etag) = loop {
            attempt += 1;
            if attempt > 4 {
                return Err(LeanError::State(
                    "manifest CAS lost 4 merge races — refusing this barrier".into(),
                ));
            }
            let current = manifest::load(self.store.as_ref(), &self.cfg).await?;
            let (theirs, expected) = match &current {
                Some(l) => (l.manifest.clone(), Some(l.etag.clone())),
                None => (Default::default(), None),
            };
            let (merged, foreign) =
                manifest::merge(&baseline.inst_base, &theirs, &upserts, &classified.deletes, &parked);
            match manifest::cas_write(
                self.store.as_ref(),
                &self.cfg,
                &merged,
                expected.as_deref(),
                epoch,
                &flush_uuid,
            )
            .await
            {
                Ok(meta) => {
                    foreign_entries = foreign;
                    break (merged, meta.etag);
                }
                Err(LeanError::Store(StoreError::PreconditionFailed(_)))
                | Err(LeanError::Store(StoreError::Conflict(_))) => {
                    // Re-verify the cell before retrying: a rotation is
                    // exactly this 412, and re-merging past it would be
                    // the straggler install.
                    self.verify_not_deposed().await?;
                    continue;
                }
                Err(e) => return Err(e),
            }
        };
        report.seq = Some(installed.seq);
        report.foreign_queued = foreign_entries.len();

        // Step 6: deletes LAST — GC of keys the NEW manifest no longer
        // references, HEAD-guarded on the recognized ETag.
        for path in &classified.deletes {
            if installed.entries.contains_key(path) {
                continue; // delete/modify resolved foreign-wins: not garbage
            }
            let key = self.cfg.file_key(path);
            let recognized = baseline.entries.get(path).map(|b| b.etag.clone());
            match self.store.head(&key).await {
                Err(StoreError::NotFound(_)) => {
                    report.deleted.push(path.clone());
                }
                Ok(meta) if Some(&meta.etag) == recognized.as_ref() => {
                    self.store.delete(&key).await?;
                    report.deleted.push(path.clone());
                }
                Ok(meta) => {
                    // An ETag this sidecar does not recognize is NEVER
                    // deleted (a HITL re-create landed after our CAS).
                    self.state.append_conflict(&ConflictRecord {
                        path: path.clone(),
                        foreign_etag: meta.etag,
                        preserved_key: None,
                        kind: "gc-skip".into(),
                        at_unix: now_unix(),
                    })?;
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Step 7: baseline rewrite, intent clear, window clear (+ queue
        // the merge-preserved foreign entries for the next consume).
        for (path, be) in new_baseline_entries {
            baseline.entries.insert(path, be);
        }
        for path in &report.deleted {
            baseline.entries.remove(path);
        }
        baseline.inst_base =
            installed.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        baseline.seq = installed.seq;
        baseline.manifest_etag = Some(installed_etag);
        baseline.prev_scan = scanned.keys().cloned().collect();
        self.state.save_baseline(&baseline)?;
        self.state.clear_intent_keys()?;
        let queue: Vec<InboxEntry> = foreign_entries
            .into_iter()
            .map(|(path, e)| InboxEntry {
                path,
                etag: e.etag,
                author: "merge-preserved".into(),
                added_unix: now_unix(),
            })
            .collect();
        inbox::clear_window(self.store.as_ref(), &self.cfg, epoch, &queue).await?;
        Ok(report)
    }

    async fn upload_one(
        &self,
        path: &str,
        scanned: &scan::ScanEntry,
        base: Option<&BaselineEntry>,
        epoch: u64,
        flush_uuid: &str,
        prior_uuids: &[String],
    ) -> LeanResult<UploadOutcome> {
        let key = self.cfg.file_key(path);
        let local_path = self.cfg.root.join(path);
        let body = std::fs::read(&local_path)?;
        let crc = crc64_nvme(&body);
        let generation = base.map(|b| b.generation + 1).unwrap_or(1);
        let posix = std::fs::metadata(&local_path).ok().map(|m| PosixStamps::from_metadata(&m));
        let stamps = GenerationStamps {
            generation,
            epoch,
            flush_uuid: flush_uuid.to_string(),
            posix,
        };
        let condition = match base {
            Some(b) => PutCondition::IfMatch(b.etag.clone()),
            None => PutCondition::IfNoneMatchAny,
        };
        // NOTE v1: whole-object only. Files over cfg.whole_put_max go
        // through compose_generation (streaming multipart) — wired in
        // the follow-up; the put_whole path is exact for the battery.
        match self
            .store
            .put_whole(&key, Bytes::from(body.clone()), &condition, &stamps, crc)
            .await
        {
            Ok(meta) => Ok(UploadOutcome::published(path, key, meta.etag, crc, scanned, generation, epoch)),
            Err(StoreError::PreconditionFailed(_)) => {
                // The 412 policy: my own crashed/torn PUT ⇒ adopt;
                // foreign ⇒ park. NEVER the inherited LOCAL-WINS
                // overwrite.
                let head = self.store.head(&key).await?;
                let head_stamps = GenerationStamps::from_meta(&head.meta);
                let own = head_stamps
                    .as_ref()
                    .map(|s| s.flush_uuid == flush_uuid || prior_uuids.contains(&s.flush_uuid))
                    .unwrap_or(false);
                if !own {
                    return Ok(UploadOutcome::Parked { foreign_etag: head.etag });
                }
                if head.crc64_b64.as_deref() == Some(crc64_to_b64(crc).as_str()) {
                    // Bytes already there (torn response): cite it.
                    let g = head_stamps.map(|s| s.generation).unwrap_or(generation);
                    return Ok(UploadOutcome::published(path, key, head.etag, crc, scanned, g, epoch));
                }
                // Our earlier PUT, older content: supersede it knowingly.
                let meta = self
                    .store
                    .put_whole(
                        &key,
                        Bytes::from(body),
                        &PutCondition::IfMatch(head.etag),
                        &stamps,
                        crc,
                    )
                    .await?;
                Ok(UploadOutcome::published(path, key, meta.etag, crc, scanned, generation, epoch))
            }
            Err(e) => Err(e.into()),
        }
    }
}

enum UploadOutcome {
    Published { entry: LeanEntry, baseline_entry: BaselineEntry },
    Parked { foreign_etag: String },
}

impl UploadOutcome {
    fn published(
        path: &str,
        key: String,
        etag: String,
        crc: u64,
        scanned: &scan::ScanEntry,
        generation: u64,
        epoch: u64,
    ) -> UploadOutcome {
        let _ = path;
        UploadOutcome::Published {
            entry: LeanEntry {
                key,
                etag: etag.clone(),
                crc64_b64: Some(crc64_to_b64(crc)),
                size: scanned.size,
                mode: scanned.mode,
                mtime_unix: scanned.mtime_unix,
                generation,
                epoch,
            },
            baseline_entry: BaselineEntry {
                etag,
                generation,
                // The PRE-read stat: if the agent wrote during our read,
                // the next scan sees the drift and re-queues (the
                // re-stat/re-queue valve).
                size: scanned.size,
                mtime_unix: scanned.mtime_unix,
            },
        }
    }
}

fn local_dirty(local: &Path, base: Option<&BaselineEntry>) -> bool {
    match (std::fs::metadata(local), base) {
        (Err(_), None) => false,                   // both absent: clean
        (Err(_), Some(_)) => true,                 // locally deleted vs baseline
        (Ok(_), None) => true,                     // local exists, never published
        (Ok(m), Some(b)) => m.len() != b.size || mtime_of(&m) != b.mtime_unix,
    }
}

pub(super) fn mtime_of(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(super) fn write_file_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> LeanResult<()> {
    fn ctx(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> super::LeanError {
        let p = path.display().to_string();
        move |e| super::LeanError::State(format!("{op} {p}: {e}"))
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ctx("mkdir for", path))?;
    }
    // NOT with_extension(): that REPLACES the final extension, so
    // "a.txt" and "a.md" would collide on one tmp name.
    let tmp = path.with_file_name(format!(
        "{}.flint-sync-tmp",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    ));
    std::fs::write(&tmp, bytes).map_err(ctx("write tmp for", path))?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode & 0o7777));
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path).map_err(ctx("rename into", path))?;
    Ok(())
}
