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

use super::barrier::{contained_path, mtime_of, write_file_atomic};
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
    /// Citations left in place rather than materialized (D0.3 legacy
    /// `.flint/` paths, containment refusals). Never a silent drop:
    /// each has a conflict record.
    pub refused: usize,
    /// Phase attribution for the agent-blocking path. Without these the
    /// checkout wall clock cannot be split between the manifest GET,
    /// the fan-out fetch window, and the local commit — which is the
    /// one fact any read-path decision here rests on.
    pub manifest_secs: f64,
    pub fetch_secs: f64,
    pub commit_secs: f64,
    /// The admitted set this checkout was scoped to, or `None` for the
    /// whole manifest.
    pub scope: Option<Vec<String>>,
    /// Citations the scope DECLINED. Zero for an unscoped checkout, and
    /// the number that says a scope did anything at all: a scope that
    /// admits everything reads exactly like no scope.
    pub out_of_scope: usize,
    /// Objects materialised through the RANGED path. On a cluster
    /// nothing else can tell "ranging did not help" from "ranging did
    /// not run" — the knob is a threshold and a threshold that never
    /// fires reads exactly like an optimisation that does not work.
    pub ranged: usize,
}

/// Materialise one object as PARALLEL RANGES, writing each at its
/// offset as it lands.
///
/// Returns `Ok(None)` when the caller should fall through to the
/// whole-object arm — either because ranging does not apply, or because
/// the store answered with one of the two POLICY-BEARING errors. That
/// second case is deliberate: `PreconditionFailed` and `NotFound` mean
/// three different things here depending on `pinned` and `sole_writer`,
/// and all three refusals are written once, in the whole-object arm
/// below. Re-deciding them here would put the same policy in two
/// places, which is how the two drift apart. The cost is one wasted
/// request on a path that is already an incident.
///
/// Every other error propagates after the per-range retries: a transport
/// failure is a failure, not a quiet fallback that would hide a broken
/// network behind a slower code path.
async fn fetch_ranged(
    store: &std::sync::Arc<dyn flint_store::ObjectStore>,
    key: &str,
    etag: &str,
    size: u64,
    target: &std::path::Path,
    tmp: &std::path::Path,
    mode: Option<u32>,
    chunk_bytes: u64,
    parallelism: usize,
) -> LeanResult<Option<u64>> {
    use futures::stream::StreamExt;

    /// A range that failed for a reason the caller must distinguish.
    enum RangeFail {
        /// 412/404 — the whole-object arm owns this policy.
        Fallback,
        Fatal(LeanError),
    }

    const RANGE_RETRIES: u32 = 3;
    let chunk_bytes = chunk_bytes.max(1);
    let mut parts: Vec<(u64, u64)> = Vec::new();
    let mut at = 0u64;
    while at < size {
        let len = chunk_bytes.min(size - at);
        parts.push((at, len));
        at += len;
    }
    if parts.len() < 2 {
        // One range is one whole GET with extra steps.
        return Ok(None);
    }

    let sink = super::safefs::RangedTmp::create(target, tmp, size, mode)?;
    let mut ranges = futures::stream::iter(parts.into_iter().map(|(off, len)| {
        let store = store.clone();
        let key = key.to_string();
        let etag = etag.to_string();
        async move {
            let mut attempt: u32 = 0;
            loop {
                // EVERY range carries the same If-Match, which is what
                // makes the assembled file one object rather than a
                // splice of two: a write between range 3 and range 4
                // fails range 4 instead of silently interleaving
                // generations.
                match store.get_range(&key, off, len, &etag).await {
                    Ok(b) => return Ok((off, b)),
                    Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                        return Err(RangeFail::Fallback)
                    }
                    Err(_) if attempt < RANGE_RETRIES => {
                        attempt += 1;
                        // The retry is per RANGE: a cut connection at
                        // range N of a multi-GiB object must not throw
                        // away N ranges of progress.
                        tokio::time::sleep(std::time::Duration::from_millis(
                            300 * u64::from(attempt),
                        ))
                        .await;
                    }
                    Err(e) => {
                        return Err(RangeFail::Fatal(LeanError::State(format!(
                            "get_range {key} at {off}+{len} after {RANGE_RETRIES} retries: {e}"
                        ))))
                    }
                }
            }
        }
    }))
    .buffer_unordered(parallelism.max(1));

    let mut bytes = 0u64;
    while let Some(next) = ranges.next().await {
        match next {
            Ok((off, b)) => {
                if b.is_empty() {
                    drop(ranges);
                    let _ = std::fs::remove_file(tmp);
                    return Err(LeanError::State(format!(
                        "get_range {key} at {off} returned an empty range before the object's \
                         end — refusing a hole"
                    )));
                }
                bytes += b.len() as u64;
                sink.write_at(off, &b)?;
            }
            Err(RangeFail::Fallback) => {
                drop(ranges);
                let _ = std::fs::remove_file(tmp);
                return Ok(None);
            }
            Err(RangeFail::Fatal(e)) => {
                drop(ranges);
                let _ = std::fs::remove_file(tmp);
                return Err(e);
            }
        }
    }
    sink.commit()?;
    Ok(Some(bytes))
}

/// CRC-64/NVME of a local file, in the same base64 form the manifest
/// carries. Streamed: a resumed checkout may be verifying a 20 GiB
/// workspace and must not hold it in memory.
fn local_crc64_b64(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = flint_store::Crc64Nvme::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(flint_store::crc64_to_b64(h.finalize()))
}

/// One admitted citation's outcome from the fan-out materializer.
    struct Fetched {
        /// Took the ranged path (report-only).
        ranged: bool,
        path: String,
        be: Option<BaselineEntry>,
        skipped: bool,
        bytes: u64,
        /// A citation this checkout refused to materialize (D0.3 /
        /// containment): left cited, surfaced as a conflict.
        refused: Option<String>,
    }

/// Names a scope for a human in an error: the whole manifest, or the
/// entries themselves. The entries matter — "a different scope" sends
/// the reader to go and diff two things they cannot see.
fn describe_scope(s: Option<&[String]>) -> String {
    match s {
        None => "UNSCOPED (the whole manifest)".to_string(),
        Some(e) => format!("scoped to {:?}", e),
    }
}

impl Sidecar {
    /// Materialize an ADMITTED SET under a bounded fan-out window:
    /// each entry's guarded fetch + atomic write is independent (the
    /// 0b rig measured the sequential loop at ~1,000-2,000 files/s and
    /// 3.3 s/GiB; fan-out multiplies directly against both).
    ///
    /// Takes the admitted set rather than the manifest, so the one
    /// decision about WHICH citations get materialized lives at the call
    /// site and every caller inherits the same LPT ordering, the same
    /// in-flight byte bound, and the same D13/D0.3 refusals. A caller
    /// that admits a subset owes its own budget arithmetic over that
    /// subset — this function refuses nothing on size.
    async fn materialize<'a>(
        &'a self,
        mut admission: Vec<(&'a String, &'a super::manifest::LeanEntry)>,
        pinned: bool,
        sole_writer: bool,
    ) -> Vec<LeanResult<Fetched>> {
        // LARGEST FIRST. `m.entries` is a BTreeMap, so iterating it
        // admits in PATH order, which appends the biggest object's
        // transfer to the tail of the fan-out window: a multi-GiB
        // checkpoint whose name sorts late is a whole transfer of pure
        // makespan after every other slot has drained. Longest-
        // processing-time-first bounds that at (4/3 - 1/3k) x optimal.
        // `size` is already in the entry, so this costs no request and
        // one sort; on a size-uniform tree it is exactly a no-op.
        //
        // Nothing downstream may depend on admission order:
        // `buffer_unordered` already yields in COMPLETION order, and the
        // budget refusals above ran over the whole map before this.
        admission.sort_unstable_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.cmp(b.0)));
        // The in-flight BYTE bound (see `fetch_inflight_max_bytes`).
        // Permits are 1 MiB units; an entry larger than the whole budget
        // clamps to the budget rather than deadlocking on a permit count
        // the semaphore can never grant.
        const FETCH_UNIT: u64 = 1 << 20;
        let budget_units =
            (self.cfg.fetch_inflight_max_bytes / FETCH_UNIT).clamp(1, u32::MAX as u64) as u32;
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(budget_units as usize));
        use futures::stream::{self, StreamExt};
        let this: &super::Sidecar = self;
            let range_min = this.cfg.range_get_min_bytes;
            let range_chunk = this.cfg.range_get_chunk_bytes;
            let range_par = this.cfg.range_get_parallelism;
        stream::iter(admission.into_iter().map(|(path, entry)| {
            let store = this.store.clone();
            let root = this.cfg.root.clone();
            let local = this.cfg.root.join(path);
            let gate = gate.clone();
            let want =
                entry.size.div_ceil(FETCH_UNIT).clamp(1, budget_units as u64) as u32;
            async move {
                // Held until this entry's bytes have reached disk.
                let _permit = gate
                    .acquire_many(want)
                    .await
                    .map_err(|_| LeanError::State("fetch budget closed".into()))?;
                // D0.3: a legacy `files/.flint/...` citation is
                // never materialized (it would collide with the
                // control files); it stays cited and a conflict
                // record names it. Same arm refuses a citation
                // whose path escapes the workspace.
                let target = match contained_path(&root, path) {
                    Ok(t) => t,
                    Err(e) => {
                        return Ok(Fetched {
                            ranged: false,
                            path: path.clone(),
                            be: None,
                            skipped: true,
                            bytes: 0,
                            refused: Some(e.to_string()),
                        })
                    }
                };
                let _ = &target;
                if local.exists() {
                    // Resume: a present path is one THIS checkout
                    // already fetched, so re-downloading it would
                    // pay bucket GETs for bytes that are on disk.
                    //
                    // But only if it is the same bytes. A checkout
                    // that died halfway leaves generation N on
                    // disk, and the manifest can MOVE before the
                    // replacement pod resumes (a HITL write, a
                    // sibling's barrier — routine on this fleet).
                    // Adopting then would stamp the baseline with
                    // the NEW entry's etag over the OLD content:
                    // the scan reads the file as clean and never
                    // uploads it, a sync reads baseline == manifest
                    // and never re-fetches it, and the workspace
                    // holds bytes nothing will ever reconcile. The
                    // divergence is silent and permanent.
                    //
                    // The check is local-only: size from the stat
                    // we already took, then crc of the local file.
                    // No bucket request either way — which is why
                    // it can be unconditional rather than a knob.
                    let st = std::fs::metadata(&local)?;
                    let same = st.len() == entry.size
                        && match &entry.crc64_b64 {
                            Some(want) => local_crc64_b64(&local)
                                .map(|got| &got == want)
                                .unwrap_or(false),
                            // A legacy entry attests nothing beyond
                            // its size; adopting on size alone is
                            // the same residual the scan carries.
                            None => true,
                        };
                    if same {
                        return Ok(Fetched {
                            ranged: false,
                            path: path.clone(),
                            be: Some(BaselineEntry {
                                etag: entry.etag.clone(),
                                generation: entry.generation,
                                size: st.len(),
                                mtime_unix: mtime_of(&st),
                                version_id: entry.version_id.clone(),
                            }),
                            skipped: true,
                            bytes: 0,
                            refused: None,
                        });
                    }
                    // Fall through and re-materialize.
                }
                // RANGED FIRST, when the object is big enough to
                // pay for it and the citation is a plain etag. A
                // pinned entry naming a VERSION is excluded: the
                // store's ranged read is guarded by If-Match, which
                // attests the object's identity but does not
                // ADDRESS a noncurrent version, so ranging a pinned
                // citation could only read the current object —
                // exactly what D13 forbids. Those stay whole.
                let ranged_bytes = if range_min > 0
                    && entry.size >= range_min
                    && !(pinned && entry.version_id.is_some())
                {
                    let tmp = target.with_file_name(format!(
                        "{}.flint-sync-tmp",
                        target.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
                    ));
                    fetch_ranged(
                        &store,
                        &entry.key,
                        &entry.etag,
                        entry.size,
                        &target,
                        &tmp,
                        Some(entry.mode),
                        range_chunk,
                        range_par,
                    )
                    .await?
                } else {
                    None
                };
                if let Some(n) = ranged_bytes {
                    let st = std::fs::metadata(&local)?;
                    return Ok(Fetched {
                        ranged: true,
                        path: path.clone(),
                        be: Some(BaselineEntry {
                            etag: entry.etag.clone(),
                            generation: entry.generation,
                            size: st.len(),
                            mtime_unix: mtime_of(&st),
                            version_id: None,
                        }),
                        skipped: false,
                        bytes: n,
                        refused: None,
                    });
                }

                // D13, the reader rule. Under a GATED citation the
                // manifest is stamped `pinned_reads` and every
                // entry names the version it cites: readers resolve
                // that version EXCLUSIVELY and never S3-wins-adopt
                // the current one.
                //
                // This is load-bearing, not a refinement. The
                // moment the gated lane stages a path, the cited
                // etag stops matching current — so without it EVERY
                // gated checkout would 412 on EVERY dirty path and
                // adopt uncited mid-logical-change bytes through
                // exactly the arm the mode exists to avoid. HITL
                // writes still reach readers, through the ungated
                // repair pass, within one floor.
                let (meta, body) = match (pinned, entry.version_id.as_deref()) {
                    (true, Some(vid)) => match store.get_version(&entry.key, vid).await {
                        Ok(ok) => ok,
                        Err(StoreError::NotFound(_)) => {
                            // The dangling-citation endgame (D8):
                            // the backstop reaped a cited noncurrent
                            // version. REFUSE loudly — the bytes are
                            // not lost, `recover-staged` re-cites the
                            // surviving current version forward — and
                            // never serve a hole.
                            return Err(LeanError::State(format!(
                                "manifest cites {} version {} but that version is gone — \
                                 the noncurrent backstop reaped a cited version. Run \
                                 `flint-sync recover-staged` to re-cite forward; refusing \
                                 a silent hole",
                                entry.key, vid
                            )));
                        }
                        Err(e) => return Err(e.into()),
                    },
                    _ => match store.get_whole(&entry.key, Some(&entry.etag)).await {
                        Ok(ok) => ok,
                        Err(StoreError::PreconditionFailed(_)) if sole_writer => {
                            // Deliberately NOT the `recover-staged`
                            // advice below: nothing was staged
                            // here, the citation is intact, and the
                            // thing to go and find is the second
                            // writer.
                            return Err(LeanError::State(format!(
                                "manifest cites {} at an etag the object no longer \
                                 carries, and this workspace is published by a SOLE \
                                 WRITER — so something other than its publisher wrote \
                                 that object. Refusing to adopt bytes no manifest \
                                 cites. If this is forge's legible export, look for a \
                                 read-write mount over its prefix; the export \
                                 republishes only what git changed and will not repair \
                                 this on its own",
                                entry.key
                            )));
                        }
                        Err(StoreError::PreconditionFailed(_)) if pinned => {
                            // The mixed-manifest cell: a pinned
                            // boundary carrying an entry the
                            // citation could not make
                            // version-addressable (its cited etag
                            // matched no surviving version). D13
                            // says readers under `pinned_reads`
                            // never S3-wins-adopt, and here the
                            // current version is precisely what the
                            // rule excludes — uncited, possibly
                            // mid-logical-change bytes. Refuse
                            // loudly; the bytes are not lost.
                            return Err(LeanError::State(format!(
                                "manifest cites {} at an etag the object no longer carries, and \
                                 the entry names no version to resolve instead — refusing \
                                 to adopt uncited bytes into a pinned checkout. Run \
                                 `flint-sync recover-staged` to re-cite forward",
                                entry.key
                            )));
                        }
                        Err(StoreError::PreconditionFailed(_)) => {
                            // S3-wins: the object moved past the
                            // manifest (a HITL write not yet
                            // re-cited). Adopt the CURRENT version
                            // — its inbox entry reconciles the
                            // manifest at the next barrier. Reached
                            // only for cadence/hybrid/legacy
                            // manifests, so the shipped
                            // `hitl_upload_survives_two_barriers`
                            // behaviour is untouched in the default
                            // mode.
                            store.get_whole(&entry.key, None).await?
                        }
                        Err(StoreError::NotFound(_)) => {
                            return Err(LeanError::State(format!(
                                "manifest cites {} but the object is gone — refusing a \
                                 silent hole (mixed-writer bucket?)",
                                entry.key
                            )));
                        }
                        Err(e) => return Err(e.into()),
                    },
                };
                write_file_atomic(&target, &body, Some(entry.mode))?;
                let st = std::fs::metadata(&local)?;
                Ok(Fetched {
                    ranged: false,
                    path: path.clone(),
                    be: Some(BaselineEntry {
                        etag: meta.etag.clone(),
                        generation: entry.generation,
                        size: st.len(),
                        mtime_unix: mtime_of(&st),
                        version_id: None,
                    }),
                    skipped: false,
                    bytes: body.len() as u64,
                    refused: None,
                })
            }
        }))
        .buffer_unordered(this.cfg.fanout.max(1))
        .collect::<Vec<LeanResult<Fetched>>>()
        .await
    }

    /// Materialize the workspace from the manifest. Idempotent across
    /// crashes (resume skips present paths); refuses over budget
    /// BEFORE the first byte.
    pub async fn checkout(&mut self) -> LeanResult<CheckoutReport> {
        self.checkout_scoped(None).await
    }

    /// Materialize only the citations an explicit scope admits.
    ///
    /// The safety argument is `classify` (`scan.rs`), which derives
    /// deletions by iterating `baseline.entries.keys()`: a path this
    /// checkout never materialized is never cited, so it can never be
    /// classified absent and can never have its object DELETEd. That is
    /// why no scope filter is needed in the barrier, the sync or the
    /// delete sites — the admission filter here IS the whole argument,
    /// and four copies of a rule are four places for it to drift.
    ///
    /// What a scoped workspace does NOT get is a frozen held set. A path
    /// outside the scope that changes REMOTELY arrives through the inbox
    /// and enters the baseline, and from then on it is owned like any
    /// other — probe P1/P3 of 2026-09-11 measured both halves. There is
    /// no verb to shed it again, and no verb to ask for a path the
    /// remote never touched. Scoping a checkout is therefore a bet that
    /// the admitted set is the set the agent needs for its whole life.
    pub async fn checkout_scoped(
        &mut self,
        scope: Option<Vec<String>>,
    ) -> LeanResult<CheckoutReport> {
        // Validated BEFORE the live-tree row, because a malformed scope
        // is a configuration error whether or not this tree is already
        // up, and the one thing it must never do is quietly widen.
        // `Scope::new` silently drops entries that are too long, past
        // MAX_SCOPE_ENTRIES, or contain `.`/`..`; three malformed paths
        // normalize to an EMPTY scope, and an empty scope reads
        // everywhere as "no restriction" = the whole manifest. Same rule
        // and same shape as `sync_scoped`.
        let scope = match scope {
            None => None,
            Some(raw) => {
                let s = super::sync::Scope::new(&raw);
                if s.is_empty() {
                    return Err(LeanError::State(format!(
                        "checkout scope named {} entr{} and NONE survived validation — \
                         refusing, because an empty scope widens to the WHOLE MANIFEST",
                        raw.len(),
                        if raw.len() == 1 { "y" } else { "ies" }
                    )));
                }
                Some(s)
            }
        };
        let requested: Option<Vec<String>> = scope.as_ref().map(|s| s.entries().to_vec());

        let mut report = CheckoutReport::default();
        report.scope = requested.clone();
        if self.state.marker_present() {
            // The live-tree row: never re-materialize. But a live tree
            // holds what it holds, and `checkout` cannot change that —
            // so a caller whose scope disagrees with the one on disk
            // gets an error, never a success naming a set it did not
            // get. The dangerous direction is the quiet one: a caller
            // that asks for the whole tree and resumes a 3-file
            // workspace believes it is holding 2001 paths.
            let held = self.state.load_scope()?;
            if held != requested {
                return Err(LeanError::State(format!(
                    "this workspace is already checked out {}, and the request asks for \
                     {} — checkout cannot change the admitted set of a live tree, and \
                     resuming it would return success for a set you did not get",
                    describe_scope(held.as_deref()),
                    describe_scope(requested.as_deref()),
                )));
            }
            report.resumed_live_tree = true;
            return Ok(report);
        }

        let t_start = std::time::Instant::now();
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        report.manifest_secs = t_start.elapsed().as_secs_f64();
        let mut baseline = self.state.load_baseline()?;
        let (m, metag) = match loaded {
            Some(l) => (l.manifest, Some(l.etag)),
            None => (Default::default(), None),
        };

        // ADMISSION FIRST, budgets second. The order is load-bearing:
        // a budget is a promise about what THIS checkout will write, and
        // a 3-file scoped checkout summed over the whole 2001-file
        // manifest is refused for bytes it was never going to fetch.
        let admission: Vec<(&String, &super::manifest::LeanEntry)> = match &scope {
            None => m.entries.iter().collect(),
            Some(s) => m.entries.iter().filter(|(p, _)| s.covers(p)).collect(),
        };
        report.out_of_scope = m.entries.len() - admission.len();

        // Budgets: refuse before materializing anything.
        let total_bytes: u64 = admission.iter().map(|(_, e)| e.size).sum();
        if self.cfg.max_bytes > 0 && total_bytes > self.cfg.max_bytes {
            return Err(LeanError::Budget(format!(
                "checkout is {} bytes; budget {}",
                total_bytes, self.cfg.max_bytes
            )));
        }
        if self.cfg.max_files > 0 && admission.len() as u64 > self.cfg.max_files {
            return Err(LeanError::Budget(format!(
                "checkout is {} files; budget {}",
                admission.len(),
                self.cfg.max_files
            )));
        }

        let mut present: BTreeSet<String> = BTreeSet::new();
        let t_fetch = std::time::Instant::now();
        // A mirror's publisher is the only party entitled to write it,
        // so an object off its citation was moved by a stranger.
        // Adopting it would copy bytes no manifest cites into this
        // tree, silently — drill C4.
        let results = self.materialize(admission, m.pinned_reads, m.sole_writer).await;
        report.fetch_secs = t_fetch.elapsed().as_secs_f64();
        let t_commit = std::time::Instant::now();
        for r in results {
            let f = r?;
            if let Some(why) = f.refused {
                self.state.append_conflict(&super::state::ConflictRecord {
                    path: f.path.clone(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: format!("checkout-refused: {why}"),
                    at_unix: super::now_unix(),
                })?;
                report.refused += 1;
                continue;
            }
            if f.ranged {
                report.ranged += 1;
            }
            if f.skipped {
                report.skipped_present += 1;
            } else {
                report.materialized += 1;
                report.bytes += f.bytes;
            }
            present.insert(f.path.clone());
            if let Some(be) = f.be {
                baseline.entries.insert(f.path, be);
            }
        }

        baseline.seq = m.seq;
        baseline.manifest_etag = metag;
        // THE WHOLE MANIFEST, scope or no scope. `manifest.rs` reads an
        // entry absent from the merge base as CHANGED
        // (`base.get(p).map(..).unwrap_or(true)`), so an `inst_base`
        // narrowed to the admitted set makes every unadmitted citation
        // read as foreign, queue into the inbox, and land in the tree at
        // the next barrier — a scoped checkout that downloads everything
        // one barrier later. The constraint is not a refinement: the
        // fast path and the safe path are the same line.
        baseline.inst_base = m.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        baseline.prev_scan = present;
        // Every materialised file reaches stable storage BEFORE the
        // baseline and the marker that vouch for it: after a power loss
        // the tree is then at least as durable as its description, so
        // the next scan cannot read a zero-length survivor as a local
        // edit and publish it over the good version.
        self.state.sync_tree()?;
        self.state.save_baseline(&baseline)?;
        // Before the marker, always — including the `None` case, which
        // REMOVES any scope a crashed predecessor left behind. The
        // marker is the agent-start gate; a scope that landed after it
        // would leave a window in which an agent is cleared to run
        // against a tree whose admitted set is not yet durable, and a
        // crash there yields a workspace holding three files that
        // claims, by the absence of any scope, to hold all of them.
        self.state.save_scope(requested.as_deref())?;
        // D11: the capability marker and the gauges exist BEFORE the
        // agent-start gate opens, so the first thing the agent does can
        // be to read them. `run` writes capabilities around checkout
        // too; doing it here as well covers the standalone `checkout`
        // subcommand, which otherwise leaves an agent with no marker to
        // read and therefore no way to know the verbs exist.
        let posture = self.sentinel_preflight()?;
        self.write_capabilities(&posture, false)?;
        self.write_gauges(false, None)?;
        // The marker is written LAST: the agent-start gate.
        self.state.write_marker()?;
        report.commit_secs = t_commit.elapsed().as_secs_f64();
        Ok(report)
    }
}

// ── the narrow / widen verb (scoped-read design §4) ──────────────────

#[derive(Debug, Default)]
pub struct RescopeReport {
    /// The scope this workspace now holds. `None` = the whole tree.
    pub target: Option<Vec<String>>,
    /// Citations removed from the held set — the NARROW half.
    pub uncited: usize,
    /// Files removed from the tree. Normally equal to `uncited`; lower
    /// when a path was already gone.
    pub unlinked: usize,
    /// Newly admitted citations fetched — the WIDEN half.
    pub materialized: usize,
    pub bytes: u64,
    /// Admitted and already held: the scope grew over paths the
    /// workspace had. Zero work, and the number that tells a widen that
    /// did nothing from a widen that had nothing to do.
    pub already_held: usize,
    /// Paths that should have left the held set but carry unpublished
    /// local changes. KEPT, cited, on disk, with a conflict record —
    /// a narrow may unwatch a file, never discard an edit.
    pub kept_dirty: Vec<String>,
    /// This call finished a rescope a crash had left half-applied.
    pub replayed: bool,
}

impl Sidecar {
    /// Move the workspace to `target`: stop holding what it no longer
    /// admits, and fetch what it newly does.
    ///
    /// ## Why this cannot be `rm` plus a scope edit
    ///
    /// `classify` reads exactly the two states a narrow passes through:
    /// present-in-scan-absent-from-baseline is an **upload**, and
    /// present-in-baseline-absent-from-scan-and-`prev_scan` is a
    /// **delete**. So unlink-then-uncite crashes into publishing
    /// deletions, and uncite-then-unlink crashes into re-uploading
    /// identical bytes and re-citing everything just dropped. Neither
    /// order is safe alone. The invariant, stated for the model:
    /// **a narrow is an unwatch, never an absence.**
    ///
    /// What makes it safe is the INTENT, not the order: the target is
    /// durable before the first mutation and cleared after the last,
    /// `run_barrier` replays any intent it finds before it does
    /// anything else, and the replay is idempotent. The order within
    /// still matters for the window where the intent itself is lost —
    /// uncite first, so the failure mode is a file that gets
    /// re-uploaded rather than one that gets DELETED from the bucket.
    ///
    /// ## Why the verb takes a SET
    ///
    /// `manifest::load` fetches the pointer and every chunk regardless
    /// of scope, so the floor is one whole-manifest load per call.
    /// A per-path verb would pay that floor per path.
    pub async fn rescope(&mut self, target: Option<Vec<String>>) -> LeanResult<RescopeReport> {
        if let Some(raw) = &target {
            // An all-rejected scope is REFUSED, never widened to the
            // whole tree — `Scope::new` silently drops malformed
            // entries, and `sync.rs` already shipped the bug where that
            // turned a typo into maximum privilege.
            if super::sync::Scope::new(raw).is_empty() {
                return Err(LeanError::State(format!(
                    "every entry of the requested scope was rejected ({raw:?}); refusing to \
                     read that as the whole tree"
                )));
            }
        }
        if !self.state.marker_present() {
            return Err(LeanError::State(
                "rescope needs a checked-out workspace — run checkout first".into(),
            ));
        }

        // The strict check belongs at the DOOR, before anything is
        // written: a caller asking to drop a path it has unpublished
        // edits to gets told, and the old scope stands untouched. The
        // replay below cannot be this strict — it must converge — so it
        // keeps such paths instead. Two postures, deliberately.
        let leaving = self.paths_leaving(&target)?;
        let dirty = self.dirty_among(&leaving)?;
        if !dirty.is_empty() {
            return Err(LeanError::State(format!(
                "these paths would leave the scope but have unpublished local changes: {:?} — \
                 publish or discard them first",
                dirty
            )));
        }

        let intent = super::state::ScopeIntent { target, drop: leaving };
        self.state.save_scope_intent(&intent)?;
        self.apply_scope_intent(&intent).await
    }

    /// Finish a rescope a crash left half-applied. `None` when there is
    /// none in flight. Idempotent: running it twice is running it once.
    pub async fn replay_scope_intent(&mut self) -> LeanResult<Option<RescopeReport>> {
        let Some(intent) = self.state.load_scope_intent()? else { return Ok(None) };
        let mut report = self.apply_scope_intent(&intent).await?;
        report.replayed = true;
        Ok(Some(report))
    }

    /// Which held paths the target scope would stop admitting.
    fn paths_leaving(&self, target: &Option<Vec<String>>) -> LeanResult<Vec<String>> {
        let baseline = self.state.load_baseline()?;
        let scope = target.as_ref().map(|r| super::sync::Scope::new(r));
        Ok(baseline
            .entries
            .keys()
            .filter(|p| scope.as_ref().map(|s| !s.covers(p)).unwrap_or(false))
            .cloned()
            .collect())
    }

    /// Which of `paths` carry local changes the bucket has not seen.
    /// The same three states `sync` calls dirty, for the same reason.
    fn dirty_among(&self, paths: &[String]) -> LeanResult<Vec<String>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }
        let baseline = self.state.load_baseline()?;
        let scanned = super::scan::scan(&self.cfg.root)?;
        let c = super::scan::classify(&scanned, &baseline);
        Ok(paths
            .iter()
            .filter(|p| {
                c.uploads.contains(*p) || c.deletes.contains(*p) || c.first_absence.contains(*p)
            })
            .cloned()
            .collect())
    }

    async fn apply_scope_intent(
        &mut self,
        intent: &super::state::ScopeIntent,
    ) -> LeanResult<RescopeReport> {
        let target = intent.target.clone();
        let mut report = RescopeReport { target: target.clone(), ..Default::default() };
        let scope = target.as_ref().map(|r| super::sync::Scope::new(r));
        let covered = |p: &str| scope.as_ref().map(|s| s.covers(p)).unwrap_or(true);

        // §4.4's floor: the whole manifest, scope or no scope.
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        let (m, metag) = match loaded {
            Some(l) => (l.manifest, Some(l.etag)),
            None => (Default::default(), None),
        };

        let mut baseline = self.state.load_baseline()?;
        // THE RECORDED set, never re-derived — see `ScopeIntent::drop`.
        // Re-deriving from the baseline loses the whole set the moment
        // the uncite lands; re-deriving from the tree cannot tell a
        // leftover from a file the agent created.
        let leaving: Vec<String> = intent.drop.clone();
        // Converging posture: a dirty path is KEPT rather than dropped,
        // so a replay can never be stuck refusing forever.
        //
        // A path already uncited by a crashed run reads as dirty here
        // (present in scan, absent from the baseline ⇒ `uploads`), and
        // keeping it would undo the very step that crashed. So dirt is
        // judged against what the path IS: still cited and modified, or
        // not cited at all and therefore already half-dropped.
        let still_cited: Vec<String> =
            leaving.iter().filter(|p| baseline.entries.contains_key(*p)).cloned().collect();
        let dirty = self.dirty_among(&still_cited)?;
        for p in &dirty {
            self.state.append_conflict(&super::state::ConflictRecord {
                path: p.clone(),
                foreign_etag: String::new(),
                preserved_key: None,
                kind: "rescope-kept-locally-dirty".into(),
                at_unix: super::now_unix(),
            })?;
        }
        report.kept_dirty = dirty.clone();
        let drop_set: Vec<String> =
            leaving.into_iter().filter(|p| !dirty.contains(p)).collect();

        // UNCITE FIRST, and durably, before a single file leaves the
        // tree. Both halves must happen, and the intent guarantees
        // they will; this order decides only which way a lost intent
        // fails — toward a file that reads as a local add (re-uploaded,
        // recoverable) rather than one that reads as a local delete
        // (published as a DELETE, not recoverable).
        for p in &drop_set {
            baseline.entries.remove(p);
            baseline.prev_scan.remove(p);
        }
        report.uncited = drop_set.len();
        self.state.save_baseline(&baseline)?;

        for p in &drop_set {
            let local = self.cfg.root.join(p);
            // The same containment the barrier demands: a citation is
            // not a licence to unlink whatever a path resolves to.
            if contained_path(&self.cfg.root, p).is_err() {
                continue;
            }
            match std::fs::remove_file(&local) {
                Ok(()) => report.unlinked += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(LeanError::Io(e)),
            }
        }

        // WIDEN: admitted citations this workspace does not hold.
        let add: Vec<(&String, &super::manifest::LeanEntry)> =
            m.entries.iter().filter(|(p, _)| covered(p) && !baseline.entries.contains_key(*p)).collect();
        report.already_held =
            m.entries.keys().filter(|p| covered(p) && baseline.entries.contains_key(*p)).count();
        if !add.is_empty() {
            let results = self.materialize(add, m.pinned_reads, m.sole_writer).await;
            for r in results {
                let f = r?;
                if let Some(why) = f.refused {
                    self.state.append_conflict(&super::state::ConflictRecord {
                        path: f.path.clone(),
                        foreign_etag: String::new(),
                        preserved_key: None,
                        kind: format!("rescope-refused: {why}"),
                        at_unix: super::now_unix(),
                    })?;
                    continue;
                }
                if !f.skipped {
                    report.materialized += 1;
                    report.bytes += f.bytes;
                }
                baseline.prev_scan.insert(f.path.clone());
                if let Some(be) = f.be {
                    baseline.entries.insert(f.path, be);
                }
            }
        }

        baseline.seq = m.seq;
        baseline.manifest_etag = metag;
        // C2, unchanged and non-negotiable: the merge base is the WHOLE
        // manifest. Narrow it with the held set and every unadmitted
        // citation reads as foreign at the next merge, queues into the
        // inbox, and lands in the tree one barrier later — a narrow
        // that downloads everything it just dropped.
        baseline.inst_base = m.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();

        self.state.sync_tree()?;
        self.state.save_baseline(&baseline)?;
        self.state.save_scope(target.as_deref())?;
        // LAST. While this document exists the workspace is mid-rescope
        // and `run_barrier` will replay before doing anything else.
        self.state.clear_scope_intent()?;
        Ok(report)
    }
}
