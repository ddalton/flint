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

use flint_store::{GenerationStamps, ListedVersion, StoreError};

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
    /// Legacy-cited paths (no `version_id`) whose cited etag matched no
    /// surviving version, so the backfill below could not make them
    /// addressable. Remembered so one unresolvable entry does not buy a
    /// whole version listing at every subsequent citation.
    #[serde(default)]
    pub legacy_unresolved: BTreeSet<String>,
    /// Staged paths the last lane pass did not see on disk. A staged
    /// path that was never CITED has no baseline entry, so
    /// `scan::classify` cannot call it a delete — it cannot see it at
    /// all — and the stage would carry its version into a boundary for
    /// a file the agent removed. This is that path's own
    /// two-observations memory, the rename-vs-walk guard applied where
    /// classify cannot reach.
    #[serde(default)]
    pub staged_absent_once: BTreeSet<String>,
    /// Versions this workspace superseded or cancelled between
    /// citations, per path — RECORDED by the lane, DELETED only by the
    /// guarded citation-time reaper.
    ///
    /// The lane used to delete these itself, with one guard ("not the
    /// version I just wrote") and no reference to the installed
    /// manifest. `citation_pass` clears the stage LAST — after the CAS,
    /// the reaper, the baseline save and the intent clear — and four
    /// ordinary `?` returns sit in that window, so one transient store
    /// error leaves a stage naming versions the boundary now CITES. The
    /// next lane pass then deleted the cited version. One guarded
    /// delete site was the fix; this ledger is what keeps the
    /// intermediate versions NAMEABLE while it is the only one.
    ///
    /// Without it, `base_version_id` is pinned to the cited generation
    /// (it comes from the baseline, which gated mode advances only at
    /// citation), so a path re-staged N times between citations leaves
    /// versions 2..N named by nothing at all — falling through to
    /// `noncurrentRetentionDays`, which D8 is explicit is a crash-window
    /// backstop and not a GC policy, because it runs its clock against
    /// live cited data.
    #[serde(default)]
    pub pending_reclaims: BTreeMap<String, Vec<String>>,
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
    /// A repair-only pass: nothing of the agent's is staged, but the
    /// manifest cites an older object than the one this sidecar has
    /// already INTEGRATED (a consumed HITL write, checkout's S3-wins
    /// arm). §2.4.2 exempts these from gating, and D13's promise to
    /// pinned readers — "within one floor" — is this source.
    Repair,
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
            CitationSource::Repair => "repair",
        }
    }
}

/// Re-verify the lease every N paths inside the reaper. The loop is
/// O(cited) sequential round trips with no renewal arm behind it, so a
/// long sweep both starves the lease and outlives the holder's right to
/// delete. 8 keeps the added epoch reads under ~12% of the LISTs the
/// sweep already pays for.
const RECLAIM_FENCE_EVERY: usize = 8;

#[derive(Debug, Default)]
pub struct LaneReport {
    pub staged: Vec<String>,
    pub parked: Vec<String>,
    pub consumed: usize,
    pub staged_bytes: u64,
    /// Superseded uncited versions reclaimed by the lane itself (free,
    /// version-scoped): uncited work holds at most one version per path
    /// plus crash remnants.
    /// Versions the lane RECORDED for the reaper to reclaim at the
    /// next citation. The lane deletes nothing.
    pub superseded_recorded: usize,
    pub withheld_deletes: usize,
    /// Paths this lane pass saw absent for the FIRST time — withheld to
    /// the next scan by the rename-vs-walk guard, and so NOT part of
    /// the boundary a citation from this pass would install.
    pub first_absence: Vec<String>,
    /// First-absence paths a DECLARED lane pass confirmed gone by
    /// direct lstat and staged as ordinary withheld deletes.
    pub absences_confirmed: usize,
}

#[derive(Debug, Default)]
pub struct CitationReport {
    pub seq: Option<u64>,
    pub source: Option<String>,
    pub cited: usize,
    pub dropped_stale_base: Vec<String>,
    /// Paths dropped because a HITL write landed between the lane's
    /// consume and this citation's window. Unlike the stale-base drop,
    /// the agent's latest bytes are NOT in the installed boundary — so
    /// an ack for a declared point that names one of these is a lie
    /// (D1's at-least guarantee).
    pub dropped_inflight: Vec<String>,
    /// Paths re-cited onto bytes this sidecar had already integrated —
    /// no data moved (§2.4.2's ungated repair).
    pub repaired: Vec<String>,
    /// Inherited entries this boundary made version-addressable before
    /// stamping `pinned_reads` over them.
    pub backfilled: Vec<String>,
    pub deleted: Vec<String>,
    /// Versions reaped by flint's EXACT per-citation GC — the reaper.
    /// Lifecycle is only the backstop (D8), and on `files/` it cannot
    /// tell cited from uncited at all.
    pub versions_reclaimed: usize,
    pub no_change: bool,
}

/// What one `recover-staged` pass found and installed (D9).
#[derive(Debug, Default)]
pub struct RecoverReport {
    /// Whether D9's durable summary drove this pass (the cheap path) or
    /// it fell back to the prefix-wide version LIST.
    pub from_summary: bool,
    /// Paths re-cited onto their surviving current version.
    pub recited: Vec<String>,
    /// Paths whose CITED version no longer exists — the D8
    /// abandoned-mid-stage endgame, where the noncurrent backstop
    /// reaped live cited data. A subset of `recited` when a newer
    /// version survives to roll forward onto.
    pub dangling: Vec<String>,
    /// Dangling with NOTHING left to cite: the backstop reaped the
    /// cited version and no newer generation exists. Named loudly
    /// because no verb can fix it — the bytes are gone.
    pub unrecoverable: Vec<String>,
    pub seq: Option<u64>,
    pub no_change: bool,
}

/// D9's durable orphan summary — the bucket-side answer to "is there
/// work here that no manifest cites?", readable by the operator, a
/// sibling cluster, or a human, long after the pod that staged it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OrphanDoc {
    pub written_unix: u64,
    /// The epoch that staged them — a summary from a deposed holder is
    /// still true about the bytes, and saying whose it was lets a
    /// reader tell a live backlog from a dead one.
    pub epoch: u64,
    pub boundary_mode: String,
    pub candidates: Vec<OrphanCandidate>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrphanCandidate {
    pub path: String,
    pub version_id: Option<String>,
    pub size: u64,
    pub generation: u64,
    pub epoch: u64,
    pub staged_unix: u64,
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

    /// The key D9's durable orphan summary lives at.
    pub fn orphans_key(&self) -> String {
        format!("{}/{}/orphans.json", self.cfg.prefix, super::LEAN_DIR)
    }

    /// Surface the staged-but-uncited set into the BUCKET (D9).
    ///
    /// `pending.json` lives on the emptyDir, which is exactly the thing
    /// a pure-spot replacement destroys — so on the routine failure of
    /// this fleet the pending record can name nothing, and the operator
    /// (a different process, often a different cluster) has no way to
    /// know that durable work is sitting uncited. This is that summary,
    /// and it is written where it survives cluster loss.
    ///
    /// Written only when the candidate set CHANGES: a re-PUT per tick
    /// would be a request the design does not need, and an empty doc
    /// rewritten forever would be worse than none. The clearing write
    /// matters as much as the surfacing one — a stale summary claiming
    /// candidates pages an operator about work that was cited minutes
    /// ago.
    pub async fn surface_orphans(&self, stage: &PendingStage) -> LeanResult<bool> {
        let epoch = self.lease.as_ref().map(|l| l.epoch).unwrap_or(0);
        let candidates: Vec<OrphanCandidate> = stage
            .entries
            .iter()
            .map(|(path, e)| OrphanCandidate {
                path: path.clone(),
                version_id: e.version_id.clone(),
                size: e.size,
                generation: e.generation,
                epoch: e.epoch,
                staged_unix: e.staged_unix,
            })
            .collect();
        let fingerprint = {
            let mut h = flint_store::Crc64Nvme::new();
            for c in &candidates {
                h.update(c.path.as_bytes());
                h.update(c.version_id.as_deref().unwrap_or("").as_bytes());
            }
            format!("{:016x}", h.finalize())
        };
        let marker = self.cfg.state_dir().join("orphans-fingerprint");
        if std::fs::read_to_string(&marker).ok().as_deref() == Some(fingerprint.as_str()) {
            return Ok(false);
        }
        let doc = OrphanDoc {
            written_unix: super::now_unix(),
            epoch,
            boundary_mode: self.cfg.boundary_mode.as_str().to_string(),
            candidates,
        };
        let bytes = bytes::Bytes::from(
            serde_json::to_vec_pretty(&doc).map_err(|e| LeanError::State(format!("orphans: {e}")))?,
        );
        let crc = flint_store::crc64_nvme(&bytes);
        let stamps = GenerationStamps {
            generation: 0,
            epoch,
            flush_uuid: "orphan-summary".into(),
            boundary_source: None,
            posix: None,
        };
        // Unconditional: this is a summary, not a protocol cell. A
        // successor's summary must overwrite a dead holder's, and a
        // 412-parked orphan doc would strand the very information the
        // successor needs.
        let key = self.orphans_key();
        let cond = match self.store.head(&key).await {
            Ok(m) => flint_store::PutCondition::IfMatch(m.etag),
            Err(_) => flint_store::PutCondition::IfNoneMatchAny,
        };
        match self.store.put_whole(&key, bytes, &cond, &stamps, crc).await {
            Ok(_) => {}
            // Losing the race is fine: whoever won wrote a summary at
            // least as new as ours, and the next change re-runs this.
            Err(StoreError::PreconditionFailed(_)) => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        let _ = super::control::write_atomic(&marker, fingerprint.as_bytes());
        Ok(true)
    }

    /// The upload lane: durability, every floor tick, no citation.
    pub async fn upload_lane(&mut self) -> LeanResult<LaneReport> {
        self.lane_inner(false).await
    }

    /// The lane pass behind a DECLARED boundary — a publish sentinel
    /// (D1 × D6) or the preStop drain (D10). The citation that follows
    /// is going to be acked, so a delete the agent made before the
    /// touch may not sit out the boundary waiting for a second walk:
    /// `confirm_absences` takes that observation by lstat instead.
    pub async fn declared_lane(&mut self) -> LeanResult<LaneReport> {
        self.lane_inner(true).await
    }

    async fn lane_inner(&mut self, declared: bool) -> LeanResult<LaneReport> {
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
        let mut classified = scan::classify(&scanned, &baseline);
        if declared {
            report.absences_confirmed = self.confirm_absences(&mut classified);
        }
        report.first_absence = classified.first_absence.iter().cloned().collect();
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
            installed_etag: intent.installed_etag.clone(),
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
                    // RECORD, never delete. The lane cannot prove a
                    // version is uncited: it does not read the installed
                    // manifest, and its own stage may have outlived a
                    // citation. The reaper can, and does, under four
                    // guards.
                    if let Some(vid) = superseded {
                        if Some(&vid) != entry.version_id.as_ref() {
                            stage
                                .pending_reclaims
                                .entry(path.clone())
                                .or_default()
                                .push(vid);
                            report.superseded_recorded += 1;
                        }
                    }
                    // The path is back. A tombstone standing from an
                    // earlier tick would otherwise ride into the same
                    // citation as this upsert, and merge applies
                    // deletes last: delete-then-recreate inside one
                    // citation interval would install a boundary that
                    // omits a file present on disk and staged this
                    // pass.
                    stage.withheld_deletes.remove(path);
                    stage.entries.insert(path.clone(), entry);
                }
                None => report.parked.push(path.clone()),
            }
        }

        // A staged version is cancelled by the file's departure, the
        // mirror of the tombstone cancellation above. The lane has now
        // seen the path GONE, later than it saw it present; a citation
        // carrying both the stage entry and the tombstone would install
        // one of them by accident of merge order, and merge has no way
        // to know which came last. Reclaiming the version costs
        // nothing — it was never cited.
        let mut absent_now: BTreeSet<String> = BTreeSet::new();
        let staged_paths: Vec<String> = stage.entries.keys().cloned().collect();
        let mut cancel: Vec<String> = vec![];
        for path in staged_paths {
            if scanned.contains_key(&path) {
                continue;
            }
            if classified.deletes.contains(&path) {
                cancel.push(path);
            } else if stage.staged_absent_once.contains(&path)
                || (declared
                    && std::fs::symlink_metadata(self.cfg.root.join(&path)).is_err())
            {
                // Never cited, so classify cannot see it: two lane
                // observations, or one direct lstat on a DECLARED
                // boundary, stand in for the two-scan rule.
                cancel.push(path);
            } else {
                absent_now.insert(path);
            }
        }
        for path in cancel {
            if let Some(stale) = stage.entries.remove(&path) {
                // Same rule as the supersede site above: a cancelled
                // path's staged version can be the CITED one if this
                // stage outlived a citation, and this site had no
                // manifest reference at all.
                if let Some(vid) = stale.version_id {
                    stage.pending_reclaims.entry(path.clone()).or_default().push(vid);
                    report.superseded_recorded += 1;
                }
            }
        }
        stage.staged_absent_once = absent_now;

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
        // D9: the same set, surfaced where a pure-spot replacement
        // cannot destroy it. Best-effort by construction — a failed
        // summary must never fail the lane that made the bytes durable.
        if let Err(e) = self.surface_orphans(&stage).await {
            eprintln!("flint-sync: orphan summary not written (retrying next tick): {e}");
        }
        // Durability moved even though visibility did not — the whole
        // point of the split, and the number `rpo_secs` must track.
        if !report.staged.is_empty() {
            self.note_durable()?;
        }
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

    /// Paths whose INTEGRATED object (the baseline) differs from the
    /// manifest's citation: a consumed HITL write, or checkout's
    /// S3-wins adoption. The fused barrier has carried this repair
    /// since the amputation leg; gated mode never runs the fused
    /// barrier, so without the same pass here an acked HITL write is
    /// adopted into the tree, scans clean forever after, and stays
    /// cited at its PREVIOUS version — invisible to exactly the
    /// pinned readers D13 governs.
    pub fn repair_candidates(&self) -> LeanResult<Vec<String>> {
        let baseline = self.state.load_baseline()?;
        let stage = self.load_stage()?;
        Ok(baseline
            .entries
            .iter()
            .filter(|(p, be)| {
                be.size != u64::MAX // consume-dirty sentinel: publishes via the lane
                    && !stage.entries.contains_key(*p)
                    && !stage.withheld_deletes.contains(*p)
                    && baseline.inst_base.get(*p) != Some(&be.etag)
            })
            .map(|(p, _)| p.clone())
            .collect())
    }

    /// `(key, etag)` -> version id, over this subtree's whole version
    /// history. One listing answers for every legacy entry at once,
    /// including paths whose CURRENT version has already moved past
    /// the citation — which a HEAD, by construction, cannot.
    async fn version_index(&self) -> LeanResult<BTreeMap<(String, String), String>> {
        let files_prefix = format!("{}/files/", self.cfg.prefix);
        let mut ix: BTreeMap<(String, String), String> = BTreeMap::new();
        // A backend that cannot list versions leaves every legacy entry
        // unresolved, which the reader rule then refuses rather than
        // adopts. Gated already refuses such a backend at startup.
        for v in self.store.list_versions(&files_prefix).await.unwrap_or_default() {
            if v.is_delete_marker {
                continue;
            }
            ix.entry((v.key, v.etag)).or_insert(v.version_id);
        }
        Ok(ix)
    }

    /// Which coherent point, if any, is due right now.
    pub fn citation_due(&self, sentinel_pending: bool) -> LeanResult<Option<CitationSource>> {
        let stage = self.load_stage()?;
        let now = now_unix();
        if stage.entries.is_empty() && stage.withheld_deletes.is_empty() {
            // Nothing of the agent's is staged — but a repair still
            // owes the readers a boundary. Not a forced cap: nothing is
            // late, and stamping these "forced" would misread the
            // operator's own tell.
            if !self.repair_candidates()?.is_empty()
                && now.saturating_sub(stage.last_citation_unix) >= self.cfg.floor_secs
            {
                return Ok(Some(CitationSource::Repair));
            }
            return Ok(None);
        }
        if sentinel_pending {
            return Ok(Some(CitationSource::Sentinel));
        }
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
        // Recorded reclaims are counted as OBJECTS, because they are
        // exactly the drain work this cap exists to bound: the citation
        // the drain runs issues one `delete_version` per recorded id.
        // Counting only staged PATHS would let one hot file rewritten
        // every tick accumulate a version per tick forever — one stage
        // entry, one size, and a cap that never fires — while
        // `drain_need_secs` sized the grace from that same cap.
        let recorded: u64 =
            stage.pending_reclaims.values().map(|v| v.len() as u64).sum();
        if stage.entries.len() as u64 + recorded >= self.cfg.staged_backlog_cap_objects
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
    /// EXACT version reclamation (D8) — the reaper, not lifecycle.
    ///
    /// Reclaims ONLY the generations this workspace itself superseded:
    /// for each cited path, the version its own pending record names as
    /// `base_version_id`. Everything else — crash remnants, foreign
    /// writes, a successor's work — is left to the noncurrent backstop.
    ///
    /// THE RULE USED TO BE "delete every version of a touched key that
    /// is neither `keep` nor `is_current`", and that destroys committed
    /// data. The `is_current` guard protects exactly ONE version, and
    /// its reasoning ("a foreign write landed between the lane and this
    /// citation") assumes at most one foreign generation. A successor in
    /// gated mode does not stop at one: its cadence is stage → cite →
    /// stage. So a straggler whose CAS landed and which then stalled in
    /// this loop would find, on resuming, the successor's CITED version
    /// sitting noncurrent-and-not-`keep` — and delete it. Every cited
    /// version surviving a deposed straggler is the premise §8 Q2 chose
    /// versioned staging on ("they destroy nothing"), so the old rule
    /// falsified the reason the mode exists.
    ///
    /// Lifecycle cannot do this job on `files/`: gated staging makes the
    /// CITED version noncurrent the moment a newer generation is staged,
    /// so a `NoncurrentVersionExpiration` rule runs a clock against live
    /// cited data and never reaches the newest uncited bytes, which are
    /// current. That inversion is why the standing retention is a long
    /// BACKSTOP and a shorter fleet-wide rule is a refusal condition.
    pub(crate) async fn reclaim_superseded(
        &mut self,
        upserts: &BTreeMap<String, LeanEntry>,
        installed: &super::manifest::LeanManifest,
        stage: &PendingStage,
        report: &mut CitationReport,
    ) -> LeanResult<()> {
        for (i, path) in upserts.keys().enumerate() {
            // The loop is O(cited) round trips and the run loop's
            // renewal arm cannot fire while it runs (one `select!`, the
            // chosen branch awaited inline). Renew on our own account,
            // and re-verify: a straggler that lost the lease mid-loop
            // must stop deleting, not finish the sweep.
            if i % RECLAIM_FENCE_EVERY == 0 {
                self.renew_if_due().await?;
                self.verify_not_deposed_pub().await?;
            }
            // The ONLY version this pass is entitled to reclaim: the one
            // our own pending record says we superseded. If we cannot
            // name it we reclaim nothing — the backstop exists for
            // exactly the remnants we cannot name.
            let Some(superseded) =
                stage.entries.get(path).and_then(|pe| pe.base_version_id.clone())
            else {
                continue;
            };
            if superseded.is_empty() {
                continue;
            }
            // FAIL CLOSED. If the installed manifest does not name a
            // version for this path — an unversioned backend, a proxy
            // that stripped the header, a merge that resolved
            // foreign-wins — then we do not know what is cited, and
            // reclaiming against it is guesswork.
            let Some(keep) = installed
                .entries
                .get(path)
                .and_then(|e| e.version_id.clone())
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            // The merge may have resolved foreign-wins onto the very
            // version we were about to reclaim.
            if superseded == keep {
                continue;
            }
            let key = self.cfg.file_key(path);
            let Ok(versions) = self.store.list_versions(&key).await else { continue };
            let Some(v) = versions
                .iter()
                .find(|v| v.key == key && !v.is_delete_marker && v.version_id == superseded)
            else {
                continue;
            };
            // NEVER the current version. If what we superseded is
            // somehow current again, a foreign writer restored it and
            // those are live bytes somebody is about to read.
            if v.is_current {
                continue;
            }
            let _ = self.store.delete_version(&key, &superseded).await;
            report.versions_reclaimed += 1;
        }

        // Then everything the LANE recorded and did not delete. Same
        // four guards; the only difference is where the candidate came
        // from. `base_version_id` names exactly one version — the one
        // superseded relative to the CITED generation — so without this
        // pass every intermediate generation between two citations
        // would be named by nothing and left to the retention backstop.
        let recorded: Vec<(String, Vec<String>)> =
            stage.pending_reclaims.iter().map(|(p, v)| (p.clone(), v.clone())).collect();
        for (i, (path, vids)) in recorded.iter().enumerate() {
            if i % RECLAIM_FENCE_EVERY == 0 {
                self.renew_if_due().await?;
                self.verify_not_deposed_pub().await?;
            }
            // Two distinct "no keep" cases, and they are NOT the same:
            //
            // - the installed manifest does not cite this path AT ALL
            //   (a withheld delete installed, a path that never made a
            //   boundary) — there is no cited version to protect, so
            //   the recorded ids are reclaimable;
            // - it cites the path but names no version — an unversioned
            //   backend, a stripping proxy, a foreign-wins merge. We do
            //   not know what is cited, so FAIL CLOSED, exactly as the
            //   pass above does.
            let keep = match installed.entries.get(path) {
                None => None,
                Some(e) => match e.version_id.clone().filter(|v| !v.is_empty()) {
                    Some(v) => Some(v),
                    None => continue,
                },
            };
            let key = self.cfg.file_key(path);
            let Ok(versions) = self.store.list_versions(&key).await else { continue };
            for vid in vids {
                if vid.is_empty() || Some(vid) == keep.as_ref() {
                    continue;
                }
                let Some(v) = versions
                    .iter()
                    .find(|v| v.key == key && !v.is_delete_marker && &v.version_id == vid)
                else {
                    continue;
                };
                // The `is_current` guard is CONDITIONAL here, and the
                // asymmetry is deliberate. Where the boundary cites this
                // path, a recorded id that is current again means a
                // foreign writer restored it and those are live bytes —
                // skip, exactly as the pass above does. Where the
                // boundary does not cite the path AT ALL — a withheld
                // delete that installed, a scratch file that never made
                // a boundary — current is the ORDINARY state of the last
                // version this workspace staged before the file went
                // away, and skipping it would leave precisely the litter
                // this pass exists to collect. We delete by exact
                // version id, so a foreign write is still safe: it would
                // be a different id, and this one would not be current.
                if keep.is_some() && v.is_current {
                    continue;
                }
                let _ = self.store.delete_version(&key, vid).await;
                report.versions_reclaimed += 1;
            }
        }
        Ok(())
    }

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
        let repairs = self.repair_candidates()?;
        if stage.entries.is_empty() && stage.withheld_deletes.is_empty() && repairs.is_empty() {
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

        // The ungated repair (§2.4.2). No bytes move: the object IS
        // what this sidecar already integrated, and the citation is
        // catching up to it. The HEAD is also what supplies the VERSION
        // id — a consume records none, and under `pinned_reads` a
        // citation without one is unreadable by the very readers this
        // pass exists for.
        for path in &repairs {
            if upserts.contains_key(path) {
                continue;
            }
            let Some(be) = baseline.entries.get(path).cloned() else { continue };
            let key = self.cfg.file_key(path);
            let meta = match self.store.head(&key).await {
                Ok(m) if m.etag == be.etag => m,
                // Moved again or gone: citing a version this workspace
                // has not integrated would be the lie the repair exists
                // to prevent. The next consume reconciles it.
                _ => continue,
            };
            let stamps = GenerationStamps::from_meta(&meta.meta);
            let scan_entry = scanned.get(path);
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
                    mtime_unix: scan_entry.map(|s| s.mtime_unix).unwrap_or(be.mtime_unix),
                    generation: stamps.map(|s| s.generation).unwrap_or(be.generation),
                    epoch,
                    version_id: meta.version_id.clone(),
                },
            );
            report.repaired.push(path.clone());
        }

        // Window open/clear belong to THIS lane only.
        let deadline = now_unix() + self.cfg.window_slack_secs;
        let opened =
            inbox::open_window(self.store.as_ref(), &self.cfg, epoch, deadline).await?;

        // A HITL write that landed AFTER the lane staged this path.
        //
        // The base-version check above cannot see it: it reads the
        // BASELINE, and the citation lane consumes nothing, so the
        // baseline has not moved. And the upload lane opens no window —
        // deliberately, because a lane that fenced HITL out every floor
        // tick would refuse admission essentially forever between
        // citations. So the gap is real and it is the mode's own doing.
        //
        // The inbox is what can see it, and the window CAS just loaded
        // it: zero added requests. Drop the path rather than cite bytes
        // that PREDATE the user's write over it. The entry stays queued,
        // so the next lane consumes it the ordinary way.
        //
        // (Found by the formal model — tranche 3 product 2. Before the
        // companion rule below, the reaper then DELETED the user's
        // version, because it was not the one the manifest cited.)
        let inflight: BTreeSet<String> =
            opened.doc.entries.iter().map(|e| e.path.clone()).collect();
        for path in &inflight {
            if upserts.remove(path).is_some() {
                stage.entries.remove(path);
                // Kept separate from the stale-base drop on purpose:
                // this one, and only this one, removes the agent's
                // LATEST bytes from a boundary it may be acked for.
                // The stale-base drop fires only when the foreign bytes
                // ARE the local bytes, so the point still carries the
                // agent's content.
                report.dropped_inflight.push(path.clone());
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: "citation-hitl-inflight".into(),
                    at_unix: now_unix(),
                })?;
            }
        }

        let deletes = stage.withheld_deletes.clone();
        let parked: BTreeSet<String> = BTreeSet::new();
        let flush_uuid = uuid::Uuid::new_v4().to_string();
        let mut intent = self.state.load_intent()?;
        let prev_installed = intent.installed_etag.clone();
        let mut attempt = 0;
        let mut version_index: Option<BTreeMap<(String, String), String>> = None;
        let mut backfilled: Vec<String> = vec![];
        let mut unresolved: BTreeSet<String> = BTreeSet::new();
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
            // Same rule as the fused barrier: if the bucket is still at
            // the document we installed, that document IS the merge
            // base (`IntentJournal::installed_etag`). A citation pass
            // that crashed between its CAS and the baseline rewrite
            // would otherwise read its own boundary as foreign.
            let own_base;
            let base: &BTreeMap<String, String> =
                if prev_installed.is_some() && prev_installed == expected {
                    own_base = theirs
                        .entries
                        .iter()
                        .map(|(p, e)| (p.clone(), e.etag.clone()))
                        .collect();
                    &own_base
                } else {
                    &baseline.inst_base
                };
            let (mut merged, foreign) =
                manifest::merge(base, &theirs, &upserts, &deletes, &parked);
            // D13: this citation's readers resolve by version, never by
            // S3-wins adoption of the current version.
            merged.pinned_reads = true;
            // The mixed-manifest cell, where D7's entry schema and D13
            // collide. Stamping `pinned_reads` over an entry a pre-D7
            // writer cited leaves it addressable only by etag — and the
            // moment the lane stages that path, its current version is
            // uncited mid-change bytes that checkout's fallback arm
            // would adopt. Resolve the CITED etag in the bucket's own
            // version history before installing: one listing, and it
            // answers even where the current version has already moved.
            let legacy: Vec<String> = merged
                .entries
                .iter()
                .filter(|(p, e)| {
                    e.version_id.is_none() && !stage.legacy_unresolved.contains(*p)
                })
                .map(|(p, _)| p.clone())
                .collect();
            if !legacy.is_empty() {
                if version_index.is_none() {
                    version_index = Some(self.version_index().await?);
                }
                let ix = version_index.as_ref().expect("just built");
                for path in legacy {
                    let Some(e) = merged.entries.get_mut(&path) else { continue };
                    match ix.get(&(e.key.clone(), e.etag.clone())) {
                        Some(vid) => {
                            e.version_id = Some(vid.clone());
                            backfilled.push(path);
                        }
                        None => {
                            unresolved.insert(path);
                        }
                    }
                }
            }
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
                Ok(meta) => {
                    intent.installed_etag = Some(meta.etag.clone());
                    self.state.save_intent(&intent)?;
                    break (merged, meta.etag, foreign);
                }
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
        backfilled.sort();
        backfilled.dedup();
        report.backfilled = backfilled;
        stage.legacy_unresolved.extend(unresolved);
        self.note_boundary(source.as_str(), installed.seq)?;

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
        self.reclaim_superseded(&upserts, &installed, &stage, &mut report).await?;

        // Baseline rewrite.
        for (path, e) in &installed.entries {
            // A repaired path keeps its integrated stat; only the
            // version id it is now cited by is new.
            if report.repaired.contains(path) {
                if let Some(be) = baseline.entries.get_mut(path) {
                    be.version_id = e.version_id.clone();
                }
            }
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
        stage.pending_reclaims.clear();
        stage.last_citation_unix = now_unix();
        stage.stable_since_unix = 0;
        self.save_stage(&stage)?;
        // The CLEARING write matters as much as the surfacing one: a
        // stale summary claiming candidates pages an operator about
        // work that was cited minutes ago (D9).
        if let Err(e) = self.surface_orphans(&stage).await {
            eprintln!("flint-sync: orphan summary not cleared (retrying next tick): {e}");
        }

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
    ///
    /// Both halves are reported because the floor arm has to describe
    /// them separately — a tick that staged 40 files and cited nothing
    /// is the mode working, and a tick that staged nothing and cited
    /// nothing is idle. Collapsing them into one "no change" would make
    /// gated mode indistinguishable from a wedged one in the log.
    pub async fn gated_tick(
        &mut self,
        sentinel_pending: bool,
    ) -> LeanResult<(LaneReport, CitationReport)> {
        let lane = self.upload_lane().await?;
        let cite = match self.citation_due(sentinel_pending)? {
            Some(source) => self.citation_pass(source).await?,
            None => CitationReport { no_change: true, ..Default::default() },
        };
        Ok((lane, cite))
    }

    /// The startup gate for gated mode (D8, D11): probe the version
    /// surface BEFORE a single byte is staged, and refuse rather than
    /// degrade. Inert in `cadence`/`hybrid`, which need no version
    /// surface at all — a gate that took every default workspace down
    /// with it would be worse than the hazard.
    pub async fn gated_startup_check(&mut self) -> LeanResult<()> {
        if !self.is_gated() {
            return Ok(());
        }
        self.versioning_conformance().await
    }

    /// `flint-sync recover-staged` (D9): re-cite durable-but-uncited
    /// work as ONE flagged boundary — a manifest CAS with no data
    /// movement, because the versions already exist.
    ///
    /// The routine path into this verb is a pure-spot pod replacement:
    /// the emptyDir that held `pending.json` is gone, so the pending
    /// record can name nothing and **the bucket is the only source of
    /// truth**. Two shapes are recovered, and they overlap:
    ///
    /// - *uncited work* — the key's current version is not the one the
    ///   manifest cites (or the key is not cited at all: a brand-new
    ///   file staged and never installed);
    /// - *a dangling citation* — the cited version no longer exists,
    ///   because gated staging made it NONCURRENT and the retention
    ///   backstop ran its clock against live cited data (D8's
    ///   inversion). Checkout refuses on this rather than serving a
    ///   hole, so recovery is what makes the workspace usable again.
    ///
    /// Recovery rolls **forward**, onto the newer bytes, and says so:
    /// every re-citation writes a conflict record. It is deliberately
    /// NOT a three-way merge — a replacement pod has no merge base, and
    /// merging against an empty one would classify the entire existing
    /// manifest as foreign and queue the whole tree into the HITL
    /// inbox.
    pub async fn recover_staged(&mut self) -> LeanResult<RecoverReport> {
        let epoch = self
            .lease
            .as_ref()
            .map(|l| l.epoch)
            .ok_or_else(|| LeanError::State("recover-staged without a held lease".into()))?;
        let mut report = RecoverReport::default();
        let files_prefix = format!("{}/files/", self.cfg.prefix);

        // D9's durable summary is the MECHANISM; the prefix-wide version
        // LIST is the fallback (review: U1). `flint-store`'s own trait
        // doc has said so since the version surface landed — "the
        // claim-time/DR fallback when `orphans.json` is missing or
        // stale — the expensive path, which is why the durable summary
        // is written eagerly" — but nothing in the sidecar read the
        // summary, so recovery always took the expensive path and the
        // eager write bought nothing here.
        //
        // The summary narrows WHICH keys we list, never what we
        // conclude about them: every candidate is still resolved
        // against the bucket below. A stale or partial summary
        // therefore costs coverage, not correctness — so a missing one,
        // or one written under a different boundary mode, falls back to
        // the full sweep rather than trusting a narrower answer.
        let summary = match self.store.get_whole(&self.orphans_key(), None).await {
            Ok((_, bytes)) => serde_json::from_slice::<OrphanDoc>(&bytes).ok(),
            Err(_) => None,
        };
        // The manifest is read BEFORE the narrowing, because the
        // narrowed key set is the union of the summary's candidates and
        // everything the manifest already CITES.
        //
        // Getting that wrong is a data-loss-shaped bug, and leg B11b
        // caught it: `orphans.json` names only the STAGED candidates, so
        // narrowing to those alone left every cited-but-not-staged path
        // with no listed version at all — and recovery classifies a path
        // with no surviving version as UNRECOVERABLE. A workspace with
        // one quiet cited file would have been told its data was gone.
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        let cited_now = loaded.as_ref().map(|l| l.manifest.clone()).unwrap_or_default();

        let narrowed: Option<BTreeSet<String>> = summary.as_ref().and_then(|d| {
            if d.boundary_mode != self.cfg.boundary_mode.as_str() || d.candidates.is_empty() {
                return None;
            }
            let mut keys: BTreeSet<String> =
                d.candidates.iter().map(|c| self.cfg.file_key(&c.path)).collect();
            keys.extend(cited_now.entries.keys().map(|p| self.cfg.file_key(p)));
            Some(keys)
        });
        report.from_summary = narrowed.is_some();

        let listed: Vec<ListedVersion> = match &narrowed {
            Some(keys) => {
                let mut out = vec![];
                for key in keys {
                    out.extend(self.store.list_versions(key).await.unwrap_or_default());
                }
                out
            }
            None => self.store.list_versions(&files_prefix).await?,
        };
        let mut current: BTreeMap<String, ListedVersion> = BTreeMap::new();
        let mut known: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for v in listed {
            known.entry(v.key.clone()).or_default().insert(v.version_id.clone());
            if v.is_current {
                current.insert(v.key.clone(), v);
            }
        }

        let mut paths: BTreeSet<String> = cited_now.entries.keys().cloned().collect();
        for key in current.keys() {
            if let Some(p) = key.strip_prefix(&files_prefix) {
                // D0.2 again: a legacy `files/.flint/` citation must not
                // be recovered INTO the control namespace.
                if !p.is_empty() && !scan::is_control_path(p) {
                    paths.insert(p.to_string());
                }
            }
        }

        let mut upserts: BTreeMap<String, LeanEntry> = BTreeMap::new();
        for path in paths {
            let key = self.cfg.file_key(&path);
            let cited = cited_now.entries.get(&path).cloned();
            let live = current.get(&key).filter(|v| !v.is_delete_marker).cloned();

            // A citation dangles only if it names a version BY ID that
            // the bucket no longer holds. A legacy entry citing no
            // version resolves by etag and cannot dangle this way.
            let dangles = match &cited {
                Some(e) => match &e.version_id {
                    Some(v) => !known.get(&key).map(|s| s.contains(v)).unwrap_or(false),
                    None => false,
                },
                None => false,
            };
            if dangles {
                report.dangling.push(path.clone());
            }
            let Some(live) = live else {
                if dangles {
                    report.unrecoverable.push(path.clone());
                }
                continue;
            };
            let already = match &cited {
                None => false,
                Some(e) => match &e.version_id {
                    Some(v) => v == &live.version_id,
                    None => e.etag == live.etag,
                },
            };
            if already {
                continue;
            }

            // One HEAD per re-cited path to recover the stamps the
            // manifest entry needs. A HEAD is not data movement.
            let meta = self.store.head_version(&key, &live.version_id).await?;
            let stamps = GenerationStamps::from_meta(&meta.meta);
            let posix = stamps.as_ref().and_then(|s| s.posix);
            upserts.insert(
                path.clone(),
                LeanEntry {
                    key: key.clone(),
                    etag: meta.etag.clone(),
                    crc64_b64: meta.crc64_b64.clone(),
                    size: meta.size,
                    mode: posix
                        .map(|p| p.mode)
                        .or_else(|| cited.as_ref().map(|e| e.mode))
                        .unwrap_or(0o100_644),
                    mtime_unix: posix
                        .map(|p| p.mtime_unix)
                        .or_else(|| meta.last_modified_unix.map(|u| u as i64))
                        .unwrap_or(0),
                    generation: stamps
                        .as_ref()
                        .map(|s| s.generation)
                        .or_else(|| cited.as_ref().map(|e| e.generation + 1))
                        .unwrap_or(1),
                    epoch: stamps.as_ref().map(|s| s.epoch).unwrap_or(epoch),
                    version_id: Some(live.version_id.clone()),
                },
            );
            self.state.append_conflict(&ConflictRecord {
                path: path.clone(),
                foreign_etag: cited.as_ref().map(|e| e.etag.clone()).unwrap_or_default(),
                preserved_key: None,
                kind: if dangles {
                    "recovered-staged (dangling citation rolled forward)".into()
                } else {
                    "recovered-staged".to_string()
                },
                at_unix: now_unix(),
            })?;
            report.recited.push(path.clone());
        }

        if upserts.is_empty() {
            report.no_change = true;
            report.seq = loaded.map(|l| l.manifest.seq);
            return Ok(report);
        }

        // The upserts are ABSOLUTE — derived from bucket versions, not
        // from a diff — so a lost CAS re-applies them onto whatever is
        // current without recomputing anything.
        let flush_uuid = uuid::Uuid::new_v4().to_string();
        let mut fresh = loaded;
        for attempt in 0..4u32 {
            let (mut m, expected) = match &fresh {
                Some(l) => (l.manifest.clone(), Some(l.etag.clone())),
                None => (manifest::LeanManifest::default(), None),
            };
            for (p, e) in &upserts {
                m.entries.insert(p.clone(), e.clone());
            }
            m.seq += 1;
            m.pinned_reads = self.is_gated();
            match manifest::cas_write_stamped(
                self.store.as_ref(),
                &self.cfg,
                &m,
                expected.as_deref(),
                epoch,
                &flush_uuid,
                Some(CitationSource::Recovered.as_str()),
            )
            .await
            {
                Ok(_) => {
                    report.seq = Some(m.seq);
                    self.note_boundary(CitationSource::Recovered.as_str(), m.seq)?;
                    return Ok(report);
                }
                Err(LeanError::Store(StoreError::PreconditionFailed(_)))
                | Err(LeanError::Store(StoreError::Conflict(_))) if attempt < 3 => {
                    self.verify_not_deposed_pub().await?;
                    fresh = manifest::load(self.store.as_ref(), &self.cfg).await?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(LeanError::State(
            "recover-staged lost 4 CAS races — a live writer is still publishing".into(),
        ))
    }

    /// Is this workspace gated?
    pub fn is_gated(&self) -> bool {
        self.cfg.boundary_mode == BoundaryMode::Gated
    }

    /// The versioning conformance probe (D8) — the SHARED
    /// implementation in `flint_store::probe`, because the operator
    /// runs the same probe on its reconcile cadence and the two
    /// verdicts must never disagree. A workspace the operator calls
    /// conformant and the sidecar refuses to start is worse than
    /// either answer alone.
    ///
    /// A project-scoped proxy that strips `x-amz-version-id` degrades
    /// gated mode SILENTLY — pending entries would carry `None`, and
    /// citation would fall back to etag semantics on a key whose
    /// current version is uncited, which is precisely the torn view the
    /// mode exists to prevent. So the probe REFUSES rather than
    /// degrading, and is re-run at startup and on the operator cadence:
    /// proxies upgrade, and a bucket's posture changes under you.
    pub async fn versioning_conformance(&self) -> LeanResult<()> {
        let key = format!("{}/{}/probe/versioning", self.cfg.prefix, super::LEAN_DIR);
        flint_store::probe::probe_version_surface(self.store.as_ref(), &key)
            .await
            .map_err(|m| LeanError::State(format!("versioning conformance probe FAILED: {m}")))
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
