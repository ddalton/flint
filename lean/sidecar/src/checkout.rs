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

        // Materialize under a bounded fan-out window: each entry's
        // guarded fetch + atomic write is independent (the 0b rig
        // measured the sequential loop at ~1,000-2,000 files/s and
        // 3.3 s/GiB; fan-out multiplies directly against both).
        struct Fetched {
            path: String,
            be: Option<BaselineEntry>,
            skipped: bool,
            bytes: u64,
            /// A citation this checkout refused to materialize (D0.3 /
            /// containment): left cited, surfaced as a conflict.
            refused: Option<String>,
        }
        let mut present: BTreeSet<String> = BTreeSet::new();
        let results: Vec<LeanResult<Fetched>> = {
            use futures::stream::{self, StreamExt};
            let this: &super::Sidecar = &*self;
            let pinned = m.pinned_reads;
            stream::iter(m.entries.iter().map(|(path, entry)| {
                let store = this.store.clone();
                let root = this.cfg.root.clone();
                let local = this.cfg.root.join(path);
                async move {
                    // D0.3: a legacy `files/.flint/...` citation is
                    // never materialized (it would collide with the
                    // control files); it stays cited and a conflict
                    // record names it. Same arm refuses a citation
                    // whose path escapes the workspace.
                    let target = match contained_path(&root, path) {
                        Ok(t) => t,
                        Err(e) => {
                            return Ok(Fetched {
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
        Ok(report)
    }
}
