//! Gated manifest advance: durability split from visibility
//! (boundary-verbs plan §2.4 — D6, D7, D8, D13).
//!
//! In `hybrid` (the default) uploads and citations coincide, so the
//! uncited-object set is exactly today's crash-window set. `gated`
//! splits them:
//!
//! - **Upload lane (durability), every floor tick.** Renew, verify
//!   epoch, consume the inbox, scan, stage-diff, guarded uploads — but
//!   the PUTs land **in place, as new object versions**, and no
//!   manifest CAS runs. RPO for bytes stays at the floor.
//! - **Citation lane (visibility), at coherent points only.** ONE CAS
//!   installing the entire pending set — the versions already exist, so
//!   there is no copy phase — then version-scoped GC of superseded
//!   generations and the baseline rewrite.
//!
//! **Why versions and not a staging prefix.** The copy design rested on
//! one premise: `upload_one` PUTs to `files/<path>`, so a gated eager
//! upload landing in place would DESTROY the cited object. On a
//! versioned bucket that premise is false — an in-place PUT destroys
//! nothing, the cited generation survives as a version, and a citation
//! becomes one CAS naming versions that already exist: O(1) requests
//! per boundary instead of O(dirty files), atomic by construction
//! rather than by a recovery protocol. What it deletes is machinery:
//! the `eager/` namespace, per-citation `CopyObject` (and
//! `UploadPartCopy` for >5 GiB files), the citation-intent document and
//! successor roll-forward, the stage-NotFound arm, and the transient
//! copy-phase reader window.
//!
//! **What it costs, stated rather than hidden.** Uncited generations
//! are the CURRENT version of the real key, so any reader that does not
//! resolve through the manifest — an import tool, `aws s3 cp`, a human —
//! sees mid-logical-change bytes where it previously saw the last
//! boundary. Flint's own readers qualify as manifest-resolving under
//! D13, so the guarantee holds where it is promised, but the promise is
//! scoped to manifest-resolving readers (§3 residual 11).
//!
//! **The lane opens NO HITL window.** The window exists to fence HITL
//! out of the upload→CAS span; a lane with no CAS that opened a
//! 180 s-deadline window every 60 s would refuse HITL admission
//! essentially forever between citations — and it protects nothing,
//! because a lane PUT creates a new version and destroys nothing HITL
//! wrote. Window open/clear belong exclusively to the citation lane.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use flint_store::{GenerationStamps, StoreError};

use super::manifest::{self, LeanEntry};
use super::state::{BaselineEntry, ConflictRecord};
use super::{inbox, now_unix, scan, BoundaryMode, LeanError, LeanResult, Sidecar};

const PENDING: &str = "pending.json";

/// One staged-but-uncited generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEntry {
    pub key: String,
    pub etag: String,
    pub crc64_b64: Option<String>,
    pub size: u64,
    pub mode: u32,
    pub mtime_unix: i64,
    pub generation: u64,
    pub epoch: u64,
    /// The version this staging PUT created.
    pub version_id: Option<String>,
    /// The version the BASELINE cited when we staged (D7's
    /// re-validation guard). If the baseline's version has moved by
    /// citation time, a HITL consume or sync landed after staging, and
    /// installing our staged version would UNCITE the foreign bytes.
    pub base_version_id: Option<String>,
    pub staged_unix: u64,
}

/// The gated lane's durable bookkeeping — its own file, so
/// `clear_intent_keys` cannot take it with the intent journal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingStage {
    pub entries: BTreeMap<String, PendingEntry>,
    /// Delete classifications WITHHELD from the manifest until a
    /// citation pass (§2.4.2): a rename r→s must not become
    /// reader-visible as r-gone/s-absent at an undeclared point.
    #[serde(default)]
    pub withheld_deletes: BTreeSet<String>,
    pub last_citation_unix: u64,
    /// Scan-to-scan stability tracking (§2.4.1's quiescence, as
    /// corrected: "no scan diff vs baseline" could never fire once
    /// anything was pending, since pending paths stay
    /// classified-changed until citation — quiescence would have been
    /// dead code and every citation would ride the lag cap).
    pub last_scan_fingerprint: Option<String>,
    pub stable_since_unix: u64,
}

/// Which coherent point fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    Sentinel,
    Quiescence,
    ForcedLagCap,
    ForcedBacklogCap,
    Cadence,
    Drain,
    Recovered,
}

impl CitationSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            CitationSource::Sentinel => "sentinel",
            CitationSource::Quiescence => "quiescence",
            CitationSource::ForcedLagCap => "forced-lag-cap",
            CitationSource::ForcedBacklogCap => "forced-backlog-cap",
            CitationSource::Cadence => "cadence",
            CitationSource::Drain => "drain",
            CitationSource::Recovered => "recovered",
        }
    }
}

#[derive(Debug, Default)]
pub struct LaneReport {
    pub staged: Vec<String>,
    pub parked: Vec<String>,
    pub consumed: usize,
    pub staged_bytes: u64,
    /// Superseded uncited versions reclaimed by the lane itself (free,
    /// version-scoped): uncited work holds at most one version per path
    /// plus crash remnants.
    pub superseded_reclaimed: usize,
    pub withheld_deletes: usize,
}

#[derive(Debug, Default)]
pub struct CitationReport {
    pub seq: Option<u64>,
    pub source: Option<String>,
    pub cited: usize,
    pub dropped_stale_base: Vec<String>,
    pub deleted: Vec<String>,
    /// Versions reaped by flint's EXACT per-citation GC — the reaper.
    /// Lifecycle is only the backstop (D8), and on `files/` it cannot
    /// tell cited from uncited at all.
    pub versions_reclaimed: usize,
    pub no_change: bool,
}

impl Sidecar {
    fn stage_path(&self) -> std::path::PathBuf {
        self.cfg.state_dir().join(PENDING)
    }

    pub fn load_stage(&self) -> LeanResult<PendingStage> {
        let p = self.stage_path();
        if !p.exists() {
            return Ok(PendingStage::default());
        }
        let bytes = std::fs::read(&p)?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    pub fn save_stage(&self, s: &PendingStage) -> LeanResult<()> {
        let bytes =
            serde_json::to_vec_pretty(s).map_err(|e| LeanError::State(format!("pending: {e}")))?;
        super::control::write_atomic(&self.stage_path(), &bytes)
    }

    /// The upload lane: durability, every floor tick, no citation.
    pub async fn upload_lane(&mut self) -> LeanResult<LaneReport> {
        let epoch = self
            .lease
            .as_ref()
            .map(|l| l.epoch)
            .ok_or_else(|| LeanError::State("upload lane without a held lease".into()))?;
        let mut report = LaneReport::default();
        self.verify_not_deposed_pub().await?;

        // Inbound HITL adoption stays exactly today's behavior: a HITL
        // write is already a coherent, whole-object act by a foreign
        // author, so it is not gated.
        let consumed = self.consume_inbox().await?;
        report.consumed = consumed.len();
        if !consumed.is_empty() {
            inbox::drop_entries(self.store.as_ref(), &self.cfg, epoch, &consumed).await?;
        }

        let mut baseline = self.state.load_baseline()?;
        let scanned = scan::scan(&self.cfg.root)?;
        let classified = scan::classify(&scanned, &baseline);
        let mut stage = self.load_stage()?;

        // Stage-diff base is baseline ∪ PENDING. Diffing against the
        // baseline alone — which gated mode leaves un-advanced until
        // citation — would re-PUT every staged-but-quiet file every
        // tick: 50 quiet files × 60 ticks = 3,000 PUTs/hour vs today's
        // 50, which would falsify the plan's own economics.
        let to_stage: Vec<String> = classified
            .uploads
            .iter()
            .filter(|p| match stage.entries.get(*p) {
                None => true,
                Some(pe) => {
                    let s = &scanned[*p];
                    pe.size != s.size || pe.mtime_unix != s.mtime_unix
                }
            })
            .cloned()
            .collect();

        let flush_uuid = uuid::Uuid::new_v4().to_string();
        let mut intent = self.state.load_intent()?;
        let prior_uuids = {
            let mut v = intent.recent_uuids.clone();
            if !intent.flush_uuid.is_empty() {
                v.push(intent.flush_uuid.clone());
            }
            v
        };
        intent = super::state::IntentJournal {
            flush_uuid: flush_uuid.clone(),
            keys: to_stage.iter().map(|p| self.cfg.file_key(p)).collect(),
            recent_uuids: prior_uuids.clone(),
        };
        self.state.save_intent(&intent)?;

        for path in &to_stage {
            let scanned_entry = &scanned[path];
            let base = baseline.entries.get(path).cloned();
            let superseded = stage.entries.get(path).and_then(|p| p.version_id.clone());
            let outcome = self
                .stage_one(path, scanned_entry, base.as_ref(), epoch, &flush_uuid, &prior_uuids)
                .await?;
            match outcome {
                Some(entry) => {
                    report.staged_bytes += entry.size;
                    report.staged.push(path.clone());
                    // The lane reclaims the version it just superseded:
                    // free, version-scoped, and it keeps uncited work at
                    // one version per path plus crash remnants.
                    if let Some(vid) = superseded {
                        if Some(&vid) != entry.version_id.as_ref() {
                            let _ =
                                self.store.delete_version(&entry.key, &vid).await;
                            report.superseded_reclaimed += 1;
                        }
                    }
                    stage.entries.insert(path.clone(), entry);
                }
                None => report.parked.push(path.clone()),
            }
        }

        // Deletes are WITHHELD until a citation pass (unchanged rule).
        for p in &classified.deletes {
            stage.withheld_deletes.insert(p.clone());
        }
        report.withheld_deletes = stage.withheld_deletes.len();

        // Quiescence = scan-to-scan STABILITY: no path's (size, mtime)
        // changed between consecutive lane ticks and nothing new
        // appeared.
        let fingerprint = scan_fingerprint(&scanned);
        if stage.last_scan_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            if stage.stable_since_unix == 0 {
                stage.stable_since_unix = now_unix();
            }
        } else {
            stage.last_scan_fingerprint = Some(fingerprint);
            stage.stable_since_unix = 0;
        }
        if stage.last_citation_unix == 0 {
            stage.last_citation_unix = now_unix();
        }

        baseline.prev_scan = scanned.keys().cloned().collect();
        self.state.save_baseline(&baseline)?;
        self.save_stage(&stage)?;
        Ok(report)
    }

    /// One in-place staging PUT. The guard chain is unchanged (If-Match
    /// on the recognized baseline etag, AdoptOwn, park on foreign); what
    /// changes is that the response's version id is KEPT.
    async fn stage_one(
        &self,
        path: &str,
        scanned: &scan::ScanEntry,
        base: Option<&BaselineEntry>,
        epoch: u64,
        flush_uuid: &str,
        prior_uuids: &[String],
    ) -> LeanResult<Option<PendingEntry>> {
        let key = self.cfg.file_key(path);
        match self.upload_one_pub(path, scanned, base, epoch, flush_uuid, prior_uuids).await? {
            Some((entry, _be)) => Ok(Some(PendingEntry {
                key,
                etag: entry.etag,
                crc64_b64: entry.crc64_b64,
                size: entry.size,
                mode: entry.mode,
                mtime_unix: entry.mtime_unix,
                generation: entry.generation,
                epoch: entry.epoch,
                version_id: entry.version_id,
                base_version_id: base.and_then(|b| b.version_id.clone()),
                staged_unix: now_unix(),
            })),
            None => {
                self.state.append_conflict(&ConflictRecord {
                    path: path.to_string(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: "stage-412-parked".into(),
                    at_unix: now_unix(),
                })?;
                Ok(None)
            }
        }
    }

    /// Which coherent point, if any, is due right now.
    pub fn citation_due(&self, sentinel_pending: bool) -> LeanResult<Option<CitationSource>> {
        let stage = self.load_stage()?;
        if stage.entries.is_empty() && stage.withheld_deletes.is_empty() {
            // Nothing to cite. A repair-only pass is still driven by the
            // ordinary barrier path.
            return Ok(None);
        }
        if sentinel_pending {
            return Ok(Some(CitationSource::Sentinel));
        }
        let now = now_unix();
        // The lag cap forces one even mid-change: unbounded citation
        // staleness is impossible by construction, which is why `gated`
        // is REFUSED without a bound rather than defaulted.
        if let Some(bound) = self.cfg.visibility_lag_bound_secs {
            if now.saturating_sub(stage.last_citation_unix) >= bound {
                return Ok(Some(CitationSource::ForcedLagCap));
            }
        }
        // The backlog cap bounds the preStop drain BY CONSTRUCTION.
        let bytes: u64 = stage.entries.values().map(|e| e.size).sum();
        if stage.entries.len() as u64 >= self.cfg.staged_backlog_cap_objects
            || bytes >= self.cfg.staged_backlog_cap_bytes
        {
            return Ok(Some(CitationSource::ForcedBacklogCap));
        }
        if stage.stable_since_unix > 0
            && now.saturating_sub(stage.stable_since_unix) >= self.cfg.quiesce_bound_secs
        {
            return Ok(Some(CitationSource::Quiescence));
        }
        Ok(None)
    }

    /// The citation lane: ONE CAS installing the entire pending set.
    ///
    /// The versions already exist, so there is no copy phase — hence no
    /// half-boundary, no intent document, no roll-forward, and no
    /// interval in which a version-resolving reader can see
    /// boundary-new bytes under old citations. `Inv_BoundaryAtomic`
    /// holds by construction.
    pub async fn citation_pass(&mut self, source: CitationSource) -> LeanResult<CitationReport> {
        let epoch = self
            .lease
            .as_ref()
            .map(|l| l.epoch)
            .ok_or_else(|| LeanError::State("citation without a held lease".into()))?;
        let mut report = CitationReport { source: Some(source.as_str().into()), ..Default::default() };
        self.verify_not_deposed_pub().await?;

        let mut stage = self.load_stage()?;
        let mut baseline = self.state.load_baseline()?;
        if stage.entries.is_empty() && stage.withheld_deletes.is_empty() {
            report.no_change = true;
            return Ok(report);
        }

        // Base-version re-validation (D7). If the baseline's cited
        // version moved between staging and now, a HITL consume or sync
        // landed in between; installing our staged version would UNCITE
        // the foreign bytes. The damage class is softer than the copy
        // design's — there the amputation DESTROYED the foreign object,
        // here it only hides it and the version stays fetchable — but
        // the rule stands.
        let scanned = scan::scan(&self.cfg.root)?;
        let mut upserts: BTreeMap<String, LeanEntry> = BTreeMap::new();
        let mut drop_paths: Vec<String> = vec![];
        for (path, pe) in &stage.entries {
            let cited_now = baseline.entries.get(path).and_then(|b| b.version_id.clone());
            let stale = pe.base_version_id.is_some() && cited_now != pe.base_version_id;
            if stale {
                let still_differs = match (scanned.get(path), baseline.entries.get(path)) {
                    (Some(s), Some(b)) => s.size != b.size || s.mtime_unix != b.mtime_unix,
                    (Some(_), None) => true,
                    _ => false,
                };
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: baseline
                        .entries
                        .get(path)
                        .map(|b| b.etag.clone())
                        .unwrap_or_default(),
                    preserved_key: None,
                    kind: format!(
                        "citation-stale-base (superseded generation {}, still-differs={})",
                        pe.generation, still_differs
                    ),
                    at_unix: now_unix(),
                })?;
                if !still_differs {
                    // The foreign bytes ARE the local bytes now: drop
                    // the staged citation rather than uncite them. The
                    // next lane tick re-stages if the tree moves again.
                    drop_paths.push(path.clone());
                    report.dropped_stale_base.push(path.clone());
                    continue;
                }
            }
            upserts.insert(
                path.clone(),
                LeanEntry {
                    key: pe.key.clone(),
                    etag: pe.etag.clone(),
                    crc64_b64: pe.crc64_b64.clone(),
                    size: pe.size,
                    mode: pe.mode,
                    mtime_unix: pe.mtime_unix,
                    generation: pe.generation,
                    epoch: pe.epoch,
                    version_id: pe.version_id.clone(),
                },
            );
        }
        for p in &drop_paths {
            stage.entries.remove(p);
        }

        // Window open/clear belong to THIS lane only.
        let deadline = now_unix() + self.cfg.window_slack_secs;
        inbox::open_window(self.store.as_ref(), &self.cfg, epoch, deadline).await?;

        let deletes = stage.withheld_deletes.clone();
        let parked: BTreeSet<String> = BTreeSet::new();
        let flush_uuid = uuid::Uuid::new_v4().to_string();
        let mut attempt = 0;
        let (installed, installed_etag, foreign) = loop {
            attempt += 1;
            if attempt > 4 {
                return Err(LeanError::State(
                    "citation CAS lost 4 merge races — refusing this boundary".into(),
                ));
            }
            let current = manifest::load(self.store.as_ref(), &self.cfg).await?;
            let (theirs, expected) = match &current {
                Some(l) => (l.manifest.clone(), Some(l.etag.clone())),
                None => (Default::default(), None),
            };
            let (mut merged, foreign) =
                manifest::merge(&baseline.inst_base, &theirs, &upserts, &deletes, &parked);
            // D13: this citation's readers resolve by version, never by
            // S3-wins adoption of the current version.
            merged.pinned_reads = true;
            match manifest::cas_write_stamped(
                self.store.as_ref(),
                &self.cfg,
                &merged,
                expected.as_deref(),
                epoch,
                &flush_uuid,
                Some(source.as_str()),
            )
            .await
            {
                Ok(meta) => break (merged, meta.etag, foreign),
                Err(LeanError::Store(StoreError::PreconditionFailed(_)))
                | Err(LeanError::Store(StoreError::Conflict(_))) => {
                    self.verify_not_deposed_pub().await?;
                    continue;
                }
                Err(e) => return Err(e),
            }
        };
        report.seq = Some(installed.seq);
        report.cited = upserts.len();

        // GC of keys the NEW manifest no longer references — deletes
        // LAST, HEAD-guarded on the recognized ETag, unchanged.
        for path in &deletes {
            if installed.entries.contains_key(path) {
                continue;
            }
            let key = self.cfg.file_key(path);
            let recognized = baseline.entries.get(path).map(|b| b.etag.clone());
            match self.store.head(&key).await {
                Err(StoreError::NotFound(_)) => report.deleted.push(path.clone()),
                Ok(meta) if Some(&meta.etag) == recognized.as_ref() => {
                    self.store.delete(&key).await?;
                    report.deleted.push(path.clone());
                }
                Ok(meta) => {
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

        // EXACT version reclamation (D8) — this is the reaper, not
        // lifecycle. After the CAS, every version of a touched key that
        // neither the installed manifest nor the (now empty) pending set
        // names is deleted, version-scoped and free. Steady state ⇒ one
        // version per key.
        //
        // Lifecycle cannot do this job on `files/`: gated staging makes
        // the CITED version noncurrent the moment a newer generation is
        // staged, so a `NoncurrentVersionExpiration` rule runs a clock
        // against live cited data and never reaches the newest uncited
        // bytes, which are current. That inversion is why the standing
        // retention is a long BACKSTOP and a shorter fleet-wide rule is
        // a refusal condition.
        for path in upserts.keys() {
            let key = self.cfg.file_key(path);
            let keep = installed.entries.get(path).and_then(|e| e.version_id.clone());
            // FAIL CLOSED. If the installed manifest does not name a
            // version for this path — an unversioned backend, a proxy
            // that stripped the header, a merge that resolved
            // foreign-wins — then we do not know what is cited, and
            // "delete everything we did not recognize" would reap live
            // cited data. Reclaim NOTHING and let the backstop handle
            // the remnant.
            let Some(keep) = keep.filter(|v| !v.is_empty()) else { continue };
            if let Ok(versions) = self.store.list_versions(&key).await {
                for v in versions {
                    if v.key != key || v.is_delete_marker {
                        continue;
                    }
                    if v.version_id == keep {
                        continue;
                    }
                    let _ = self.store.delete_version(&key, &v.version_id).await;
                    report.versions_reclaimed += 1;
                }
            }
        }

        // Baseline rewrite.
        for (path, e) in &installed.entries {
            if let Some(pe) = stage.entries.get(path) {
                baseline.entries.insert(
                    path.clone(),
                    BaselineEntry {
                        etag: e.etag.clone(),
                        generation: e.generation,
                        size: pe.size,
                        mtime_unix: pe.mtime_unix,
                        version_id: e.version_id.clone(),
                    },
                );
            }
        }
        for path in &report.deleted {
            baseline.entries.remove(path);
        }
        baseline.inst_base =
            installed.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        baseline.seq = installed.seq;
        baseline.manifest_etag = Some(installed_etag);
        self.state.save_baseline(&baseline)?;
        self.state.clear_intent_keys()?;

        stage.entries.clear();
        stage.withheld_deletes.clear();
        stage.last_citation_unix = now_unix();
        stage.stable_since_unix = 0;
        self.save_stage(&stage)?;

        let queue: Vec<inbox::InboxEntry> = foreign
            .into_iter()
            .map(|(path, e)| inbox::InboxEntry {
                path,
                etag: e.etag,
                author: "merge-preserved".into(),
                added_unix: now_unix(),
            })
            .collect();
        inbox::clear_window(self.store.as_ref(), &self.cfg, epoch, &queue).await?;
        Ok(report)
    }

    /// The gated floor tick: lane, then a citation if one is due.
    pub async fn gated_tick(&mut self, sentinel_pending: bool) -> LeanResult<CitationReport> {
        self.upload_lane().await?;
        match self.citation_due(sentinel_pending)? {
            Some(source) => self.citation_pass(source).await,
            None => Ok(CitationReport { no_change: true, ..Default::default() }),
        }
    }

    /// Is this workspace gated?
    pub fn is_gated(&self) -> bool {
        self.cfg.boundary_mode == BoundaryMode::Gated
    }

    /// The versioning conformance probe (D8). A project-scoped proxy
    /// that strips `x-amz-version-id` degrades gated mode SILENTLY —
    /// pending entries would carry `None`, and citation would fall back
    /// to etag semantics on a key whose current version is uncited,
    /// which is precisely the torn view the mode exists to prevent. So
    /// the probe REFUSES rather than degrading, and is re-run at
    /// startup and on the operator cadence: proxies upgrade, and a
    /// bucket's posture changes under you.
    pub async fn versioning_conformance(&self) -> LeanResult<()> {
        let key = format!("{}/{}/probe/versioning", self.cfg.prefix, super::LEAN_DIR);
        let stamps = GenerationStamps {
            generation: 0,
            epoch: 0,
            flush_uuid: "version-probe".into(),
            boundary_source: None,
            posix: None,
        };
        let refuse =
            |m: &str| LeanError::State(format!("versioning conformance probe FAILED: {m}"));

        let b1 = bytes::Bytes::from_static(b"probe-1");
        let m1 = self
            .store
            .put_whole(&key, b1, &flint_store::PutCondition::IfNoneMatchAny, &stamps, flint_store::crc64_nvme(b"probe-1"))
            .await
            .or_else(|e| match e {
                // A leftover probe from a previous run: overwrite it.
                StoreError::PreconditionFailed(_) => Err(e),
                other => Err(other),
            })
            .or(Err(refuse("cannot write the probe object")))?;
        let v1 = m1
            .version_id
            .clone()
            .ok_or_else(|| refuse("PUT returned no x-amz-version-id (versioning off, or a proxy strips the header)"))?;

        let b2 = bytes::Bytes::from_static(b"probe-2");
        let m2 = self
            .store
            .put_whole(
                &key,
                b2,
                &flint_store::PutCondition::IfMatch(m1.etag.clone()),
                &stamps,
                flint_store::crc64_nvme(b"probe-2"),
            )
            .await
            .or(Err(refuse("cannot supersede the probe object")))?;
        let v2 = m2.version_id.clone().ok_or_else(|| refuse("no version id on the second PUT"))?;
        if v1 == v2 {
            return Err(refuse("the backend reused one version id for two PUTs"));
        }

        // The first version must still be fetchable BY ID — this is the
        // read `pinned_reads` citations depend on.
        let (_, body) = self
            .store
            .get_version(&key, &v1)
            .await
            .or(Err(refuse("version-scoped GET is unavailable")))?;
        if body.as_ref() != b"probe-1" {
            return Err(refuse("version-scoped GET returned the wrong generation"));
        }
        self.store
            .head_version(&key, &v1)
            .await
            .or(Err(refuse("version-scoped HEAD is unavailable")))?;
        let listed = self
            .store
            .list_versions(&key)
            .await
            .or(Err(refuse("ListObjectVersions is not permitted")))?;
        if listed.iter().filter(|v| v.key == key && !v.is_delete_marker).count() < 2 {
            return Err(refuse("ListObjectVersions did not report both generations"));
        }
        self.store
            .delete_version(&key, &v1)
            .await
            .or(Err(refuse("version-scoped DELETE is unavailable")))?;
        if self.store.head_version(&key, &v1).await.is_ok() {
            return Err(refuse("version-scoped DELETE did not remove the version"));
        }
        let _ = self.store.delete_version(&key, &v2).await;
        Ok(())
    }
}

/// A stable fingerprint of a scan: quiescence is scan-to-scan
/// STABILITY, so what matters is that this value stops changing.
fn scan_fingerprint(scanned: &BTreeMap<String, scan::ScanEntry>) -> String {
    let mut h = flint_store::Crc64Nvme::new();
    for (p, e) in scanned {
        h.update(p.as_bytes());
        h.update(&e.size.to_be_bytes());
        h.update(&e.mtime_unix.to_be_bytes());
    }
    format!("{:016x}", h.finalize())
}
