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

        let t_start = std::time::Instant::now();
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
        report.manifest_secs = t_start.elapsed().as_secs_f64();
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

        // Materialize under a bounded fan-out window: each entry's
        // guarded fetch + atomic write is independent (the 0b rig
        // measured the sequential loop at ~1,000-2,000 files/s and
        // 3.3 s/GiB; fan-out multiplies directly against both).
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
        let mut present: BTreeSet<String> = BTreeSet::new();
        let t_fetch = std::time::Instant::now();
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
        let mut admission: Vec<(&String, &super::manifest::LeanEntry)> =
            m.entries.iter().collect();
        admission.sort_unstable_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.cmp(b.0)));
        // The in-flight BYTE bound (see `fetch_inflight_max_bytes`).
        // Permits are 1 MiB units; an entry larger than the whole budget
        // clamps to the budget rather than deadlocking on a permit count
        // the semaphore can never grant.
        const FETCH_UNIT: u64 = 1 << 20;
        let budget_units =
            (self.cfg.fetch_inflight_max_bytes / FETCH_UNIT).clamp(1, u32::MAX as u64) as u32;
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(budget_units as usize));
        let results: Vec<LeanResult<Fetched>> = {
            use futures::stream::{self, StreamExt};
            let this: &super::Sidecar = &*self;
            let pinned = m.pinned_reads;
            // A mirror's publisher is the only party entitled to write
            // it, so an object off its citation was moved by a
            // stranger. Adopting it would copy bytes no manifest cites
            // into this tree, silently — drill C4.
            let sole_writer = m.sole_writer;
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
            .collect()
            .await
        };
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
        baseline.inst_base = m.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
        baseline.prev_scan = present;
        // Every materialised file reaches stable storage BEFORE the
        // baseline and the marker that vouch for it: after a power loss
        // the tree is then at least as durable as its description, so
        // the next scan cannot read a zero-length survivor as a local
        // edit and publish it over the good version.
        self.state.sync_tree()?;
        self.state.save_baseline(&baseline)?;
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
