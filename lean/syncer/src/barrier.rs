//! The publish barrier (plan §2.1, seven steps) — the machine
//! `lean/formal/LeanSubtree.tla` checks. Order is load-bearing:
//! consume → scan → intent/window → uploads → manifest CAS (merge) →
//! GC deletes LAST → baseline rewrite.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bytes::Bytes;

use flint_store::{
    EpochState,
    crc64_nvme, crc64_to_b64, GenerationStamps, PosixStamps, PutCondition, StoreError,
};

use super::inbox::{self, InboxDoc, InboxEntry, Refusal, Removal};
use super::manifest::{self, LeanEntry};
use super::scan;
use super::state::{BaselineEntry, ConflictRecord, ForeignChange, IntentJournal};
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

/// What a pass over the cell's DECLARED removals did (`apply_removals`).
#[derive(Debug, Default)]
pub struct RemovalPass {
    /// Performed: the local file is gone (or already was) and the path
    /// is in `declared`. Dropped from the cell with the window clear —
    /// AFTER the manifest CAS — so a listing keeps hiding the path
    /// until the manifest stops citing it.
    pub applied: Vec<Removal>,
    /// Refused for good, `refused` filled in: the cell is told at once
    /// and the removal is never retried.
    pub refused: Vec<Removal>,
    /// Left in the cell for the next barrier: a rename whose
    /// destination is not integrated yet, or an unlink that failed for
    /// a transient reason.
    pub deferred: usize,
    /// The paths this barrier cites OUT: `applied`, plus any a crashed
    /// earlier barrier had unlinked and journalled.
    pub declared: BTreeSet<String>,
}

/// Upload waves per chunk. The chunk is `fanout * this`, so a wave
/// still saturates fan-out and the sync point between chunks is rare.
const UPLOAD_CHUNK_WAVES: usize = 16;

/// How stale the lease may get inside a barrier before it is renewed:
/// comfortably below the 6-poll (~60 s) takeover window, so a holder
/// busy in a long commit section never reads as quiet to a waiter.
const RENEW_WITHIN_SECS: u64 = 20;

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

    /// Step 1: consume the inbox. A barrier NEVER runs against an
    /// unconsumed inbox — this is what makes HITL uploads structurally
    /// un-amputatable. Returns the entries integrated (dropped from the
    /// cell at the window-open CAS).
    pub async fn consume_inbox(&mut self) -> LeanResult<Vec<InboxEntry>> {
        let loaded = inbox::load(self.store.as_ref(), &self.cfg).await?;
        self.consume_inbox_doc(&loaded.doc).await
    }

    /// `consume_inbox` over a cell the caller has already read — the
    /// barrier reads it once and hands the same document to
    /// `apply_removals`, so the removal pass adds no request.
    pub async fn consume_inbox_doc(&mut self, doc: &InboxDoc) -> LeanResult<Vec<InboxEntry>> {
        Ok(self.consume_counted(doc).await?.0)
    }

    /// `consume_inbox_doc`, plus how many changes from this writer's LOCAL
    /// queue it settled. Those leave no entry in the cell to drop, so they
    /// are not in the returned list — but they are consumptions, and the
    /// report (and the ack) count them.
    async fn consume_counted(&mut self, doc: &InboxDoc) -> LeanResult<(Vec<InboxEntry>, usize)> {
        // §2.5's layered doors ride the inbox GET this function already
        // pays for: promptness is one tick, and the added request count
        // is ZERO. A failure here must never fail the consume — the
        // request is idempotent state and the next tick re-reads it.
        if let Err(e) = self.note_verb_requests(doc) {
            eprintln!("flint-sync: verb request not consumed (retrying next tick): {e}");
        }
        // Indices into `entries`: the writer-local queue's upserts first,
        // then the shared inbox's. Both run the same rules below.
        let mut consumed: Vec<usize> = vec![];
        let mut baseline = self.state.load_baseline()?;
        // An install whose step 7 a restart cut off left the other writers'
        // changes it carried in the intent journal (`installed_foreign`).
        let requeued = self.state.requeue_installed_foreign()?;
        if requeued > 0 {
            self.trace("queue", serde_json::json!({"requeued_from_intent": requeued}));
        }
        // The writer-local queue: changes other writers made that this
        // workspace's own merges carried into the manifest but not yet
        // into the tree (`state::ForeignChange` says why it is not the
        // shared inbox any more).
        let queue = self.state.load_foreign_queue()?;
        let queued: Vec<InboxEntry> = queue
            .iter()
            .filter_map(|c| {
                c.etag.as_ref().map(|etag| InboxEntry {
                    path: c.path.clone(),
                    etag: etag.clone(),
                    author: "merge-preserved".into(),
                    added_unix: 0,
                    crc64_b64: c.crc64_b64.clone(),
                })
            })
            .collect();
        let n_queued = queued.len();
        let entries: Vec<InboxEntry> = queued.into_iter().chain(doc.entries.iter().cloned()).collect();
        for (idx, entry) in entries.iter().enumerate() {
            let key = self.cfg.file_key(&entry.path);
            // Containment BEFORE anything else: a path we could never
            // safely materialize must be surfaced and dropped, not
            // routed through the locally-dirty branch — which would
            // "preserve" it with a GET+PUT and leave a baseline entry
            // for a path the scanner can never see (the planted-symlink
            // shape: `inputs -> /root/.aws` reads as locally-present,
            // therefore dirty, therefore a conflict-preserve of someone
            // else's file).
            if let Err(e) = check_contained(&self.cfg.root, &entry.path) {
                self.state.append_conflict(&ConflictRecord {
                    path: entry.path.clone(),
                    foreign_etag: entry.etag.clone(),
                    preserved_key: None,
                    kind: format!("consume-refused-containment: {e}"),
                    at_unix: now_unix(),
                })?;
                self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "refused"}));
                consumed.push(idx);
                continue;
            }
            // Already integrated (a crashed earlier consume): idempotent.
            if baseline.entries.get(&entry.path).map(|b| b.etag == entry.etag).unwrap_or(false) {
                self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "already"}));
                consumed.push(idx);
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
                    self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "missing"}));
                    consumed.push(idx);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if head.etag != entry.etag {
                // Superseded by a newer write (its own inbox entry
                // follows, or it is the syncer's): drop.
                self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "superseded"}));
                consumed.push(idx);
                continue;
            }
            let local_path = self.cfg.root.join(&entry.path);
            let mut dirty = local_dirty(&local_path, baseline.entries.get(&entry.path));
            // Review 2026-09-12, atomicity-3 / inbox-2: the fetch is a
            // network round trip and the agent may write the path inside
            // it. The stat that licenses the adopt is repeated AFTER the
            // fetch, before anything touches the file: a write in that
            // window makes this the dirty case — the foreign bytes are
            // preserved and a record names them, the agent's stay.
            let mut fetched = None;
            if !dirty {
                let got = self.store.get_whole(&key, Some(&entry.etag)).await.map_err(|e| match e {
                    StoreError::PreconditionFailed(m) => {
                        StoreError::Other(format!("consume raced a newer write: {m}"))
                    }
                    other => other,
                })?;
                if local_dirty(&local_path, baseline.entries.get(&entry.path)) {
                    dirty = true;
                } else {
                    fetched = Some(got);
                }
            }
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
                        mtime_nanos: None,
                        // The sentinel's bytes are the LOCAL edit; the
                        // publish that supersedes hashes them itself.
                        crc64_b64: None,
                    },
                );
                self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "dirty-preserved"}));
            } else {
                // Clean, re-checked after the fetch: adopt the foreign
                // content into the tree.
                let (meta, body) = fetched.expect("fetched while clean");
                let mode = PosixStamps::from_meta(&meta.meta).map(|p| p.mode);
                // VERIFIED before it is written, as checkout's fresh
                // fetch is: against the writer's CRC when the inbox
                // entry carries one (the gateway hashes what it sent),
                // else the backend's attestation when it offers one.
                // Ozone offers none, so the inbox's is what a HITL
                // upload gets checked against there. What the baseline
                // records — and the next citation repair cites — is
                // OURS, over the bytes actually written.
                let got = crc64_to_b64(crc64_nvme(&body));
                let want = entry.crc64_b64.clone().or_else(|| meta.crc64_b64.clone());
                if let Some(want) = want {
                    if want != got {
                        // NOT consumed: the entry stays in the cell and
                        // the record repeats — a permanent, visible
                        // contradiction is the right failure for
                        // bytes nobody vouches for.
                        self.state.append_conflict(&ConflictRecord {
                            path: entry.path.clone(),
                            foreign_etag: entry.etag.clone(),
                            preserved_key: None,
                            kind: format!(
                                "consume-refused-checksum: etag {} is attested with CRC-64 \
                                 {want} but the bytes fetched under it hash to {got}",
                                entry.etag
                            ),
                            at_unix: now_unix(),
                        })?;
                        self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "refused-checksum"}));
                        continue;
                    }
                }
                // CONTAINMENT and I/O are split, because the answers are
                // opposite. Containment was already decided above by
                // `check_contained`, so a refusal reaching here is a
                // path that only became unsafe once parents were
                // created — still a permanent property of the path, so
                // it is surfaced and DROPPED, exactly as above.
                //
                // Everything else — ENOSPC, EACCES, EIO, a read-only
                // filesystem — is TRANSIENT, and dropping the entry for
                // one is silent permanent loss: the foreign bytes stay
                // in the bucket, the workspace never adopts them, and
                // nothing ever re-offers the entry. It used to be
                // recorded as `consume-refused-containment`, which sent
                // whoever read it looking for a hostile path that was
                // never there. A full disk is not a planted symlink.
                let target = match contained_path(&self.cfg.root, &entry.path) {
                    Ok(t) => t,
                    Err(e) => {
                        self.state.append_conflict(&ConflictRecord {
                            path: entry.path.clone(),
                            foreign_etag: entry.etag.clone(),
                            preserved_key: None,
                            kind: format!("consume-refused-containment: {e}"),
                            at_unix: now_unix(),
                        })?;
                        self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "refused"}));
                        consumed.push(idx);
                        continue;
                    }
                };
                if let Err(e) = write_file_atomic(&target, &body, mode) {
                    // NOT consumed: the entry stays in the cell so the
                    // next barrier retries it. A visible conflict record
                    // that repeats is a far better failure than one
                    // silent drop.
                    self.state.append_conflict(&ConflictRecord {
                        path: entry.path.clone(),
                        foreign_etag: entry.etag.clone(),
                        preserved_key: None,
                        kind: format!("consume-write-failed (will retry): {e}"),
                        at_unix: now_unix(),
                    })?;
                    self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "deferred"}));
                    continue;
                }
                let st = std::fs::metadata(&local_path)?;
                let stamps = GenerationStamps::from_meta(&meta.meta);
                baseline.entries.insert(
                    entry.path.clone(),
                    BaselineEntry {
                        etag: meta.etag.clone(),
                        generation: stamps.map(|s| s.generation).unwrap_or(0),
                        size: st.len(),
                        mtime_unix: mtime_of(&st),
                        mtime_nanos: Some(mtime_nanos_of(&st)),
                        crc64_b64: Some(got),
                    },
                );
                baseline.prev_scan.insert(entry.path.clone());
                self.trace("consume", serde_json::json!({"path": entry.path, "etag": entry.etag, "from": if idx < n_queued { "queue" } else { "inbox" }, "action": "adopted"}));
            }
            consumed.push(idx);
        }
        let mut settled: BTreeSet<String> =
            consumed.iter().filter(|i| **i < n_queued).map(|i| entries[*i].path.clone()).collect();

        // A queued DELETION: another writer removed the path and this
        // workspace's merge cited that. A clean local copy goes; a dirty
        // one is the agent's newer work and stays — it publishes, and a
        // modify beats a delete at the merge. Never a declared removal of
        // our own: the deletion is already in the manifest.
        for change in queue.iter().filter(|c| c.etag.is_none()) {
            if check_contained(&self.cfg.root, &change.path).is_err() {
                // Never materializable here, so nothing to remove.
                settled.insert(change.path.clone());
                continue;
            }
            let local = self.cfg.root.join(&change.path);
            let be = baseline.entries.get(&change.path).cloned();
            let record = |kind: String| ConflictRecord {
                path: change.path.clone(),
                foreign_etag: String::new(),
                preserved_key: None,
                kind,
                at_unix: now_unix(),
            };
            match std::fs::symlink_metadata(&local) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.trace("tombstone", serde_json::json!({"path": change.path, "action": "absent"}));
                }
                Err(e) => {
                    // Unreadable is not absent: keep it queued.
                    self.trace("tombstone", serde_json::json!({"path": change.path, "action": "deferred"}));
                    self.state.append_conflict(&record(format!(
                        "consume-foreign-delete-deferred: cannot stat the path: {e}"
                    )))?;
                    continue;
                }
                Ok(m) if !m.is_file() || be.is_none() || local_dirty(&local, be.as_ref()) => {
                    self.state.append_conflict(&record(
                        "consume-foreign-delete-vs-dirty: another writer deleted this path; the \
                         local version has unpublished changes, so it stays and publishes"
                            .into(),
                    ))?;
                    self.trace("tombstone", serde_json::json!({"path": change.path, "action": "kept-dirty"}));
                    settled.insert(change.path.clone());
                    continue;
                }
                Ok(_) => {
                    if let Err(e) = std::fs::remove_file(&local) {
                        self.state.append_conflict(&record(format!(
                            "consume-foreign-delete-failed (will retry): {e}"
                        )))?;
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
                        continue;
                    }
                    self.trace("tombstone", serde_json::json!({"path": change.path, "action": "removed"}));
                }
            }
            baseline.entries.remove(&change.path);
            baseline.prev_scan.remove(&change.path);
            settled.insert(change.path.clone());
        }

        // The files this baseline vouches for reached the disk before
        // the baseline does (audit 2026-09-03, finding 9).
        self.state.sync_tree()?;
        self.state.save_baseline(&baseline)?;
        // After the baseline: a crash between the two re-applies a
        // settled change, and both arms are idempotent (an upsert the
        // baseline holds is "already integrated"; a deletion with nothing
        // on disk only drops a baseline entry that is already gone).
        if !settled.is_empty() {
            let remaining: Vec<_> =
                queue.into_iter().filter(|c| !settled.contains(&c.path)).collect();
            self.state.save_foreign_queue(&remaining)?;
        }
        // Only the SHARED inbox's entries leave the cell at the window
        // clear; the local queue was settled above.
        let n_settled = settled.len();
        let shared = consumed.into_iter().filter(|i| *i >= n_queued).map(|i| entries[i].clone()).collect();
        Ok((shared, n_settled))
    }

    /// Step 1b: perform the DECLARED removals (delete/rename design
    /// §4-§6). Runs after `consume_inbox_doc`, so a rename's destination
    /// is in the tree before its source leaves it — §5: create first,
    /// removal second; a partial application leaves an EXTRA file,
    /// never a missing one.
    ///
    /// Per removal, in order:
    /// - the path must be containable, as a consume's must;
    /// - a rename WAITS until its destination is integrated (in the
    ///   baseline): a consume that deferred the destination defers the
    ///   removal with it;
    /// - a locally-DIRTY path — unpublished edits, or a file the agent
    ///   created there — is REFUSED, nothing applied, the agent's work
    ///   untouched. The same rule a consume applies to a HITL write
    ///   over dirty bytes, and the reason a declared removal cannot be
    ///   "just a delete". Refused for good: the cell is told, and a
    ///   retry that waited for the agent to publish would delete the
    ///   very edit the refusal protected;
    /// - a clean file is unlinked, and the unlink is CONFIRMED by lstat
    ///   before the path is declared — an unlink that did not take
    ///   publishes no deletion (`confirm_absences`' rule, kept);
    /// - a path already absent locally and known to the baseline or the
    ///   merge base is declared as it stands: the agent got there
    ///   first, or this tree never held it (a scoped workspace).
    ///
    /// A declared path skips the two-scan guard (§4): that guard
    /// protects against absence INFERRED by a walk, and a declaration
    /// is not an inference. That is what lets a rename's two halves
    /// ride one manifest generation.
    pub async fn apply_removals(&mut self, doc: &InboxDoc) -> LeanResult<RemovalPass> {
        let mut pass = RemovalPass::default();
        let baseline = self.state.load_baseline()?;
        // A crashed earlier barrier that had unlinked and journalled but
        // not installed: its declarations are still deletions with a
        // recorded basis — unless the agent has since put a file back.
        for p in self.state.load_intent()?.declared_deletes {
            let gone = matches!(
                std::fs::symlink_metadata(self.cfg.root.join(&p)),
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
            );
            if gone && (baseline.entries.contains_key(&p) || baseline.inst_base.contains_key(&p)) {
                pass.declared.insert(p);
            }
        }
        for r in &doc.removals {
            if r.refused.is_some() {
                continue;
            }
            let refusal = |kind: &str, message: String| Removal {
                refused: Some(Refusal { kind: kind.into(), message, at_unix: now_unix() }),
                ..r.clone()
            };
            let mut refuse = |kind: &str, message: String| -> LeanResult<()> {
                self.state.append_conflict(&ConflictRecord {
                    path: r.path.clone(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: format!("{kind}: {message}"),
                    at_unix: now_unix(),
                })?;
                pass.refused.push(refusal(kind, message));
                Ok(())
            };
            if let Err(e) = check_contained(&self.cfg.root, &r.path) {
                refuse("removal-refused-containment", e.to_string())?;
                continue;
            }
            if let Some(dst) = &r.moved_to {
                match baseline.entries.get(dst) {
                    // Not integrated yet (a consume deferred it): wait.
                    None => {
                        pass.deferred += 1;
                        continue;
                    }
                    // Integrated as a CONFLICT: the agent had an
                    // unpublished file at the destination, its version
                    // won, and the moved bytes are preserved under
                    // `conflicts/`. Deleting the source now would leave
                    // the user's file nowhere in the tree — so the move
                    // is refused and the source stays. The sentinel is
                    // the consume-dirty baseline entry's `u64::MAX`.
                    Some(b) if b.size == u64::MAX => {
                        refuse(
                            "removal-refused-destination-conflict",
                            format!(
                                "{} was not moved to {dst}: the agent has an unpublished                                  file at the destination and it wins; the moved bytes are                                  preserved under the conflict record and the source is kept",
                                r.path
                            ),
                        )?;
                        continue;
                    }
                    Some(_) => {}
                }
            }
            let local = self.cfg.root.join(&r.path);
            let be = baseline.entries.get(&r.path);
            match std::fs::symlink_metadata(&local) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if be.is_some() || baseline.inst_base.contains_key(&r.path) {
                        pass.declared.insert(r.path.clone());
                    }
                    // Nothing here and nothing cited: a removal of a path
                    // that was never there, or that a sibling already
                    // removed. Applied as a no-op.
                    pass.applied.push(r.clone());
                }
                Err(e) => {
                    // Unreadable is not absent (`confirm_absences`' rule):
                    // fail closed, and say so where the pod-side reader
                    // looks. Transient, so deferred rather than refused.
                    self.state.append_conflict(&ConflictRecord {
                        path: r.path.clone(),
                        foreign_etag: String::new(),
                        preserved_key: None,
                        kind: format!("removal-deferred: cannot stat the path: {e}"),
                        at_unix: now_unix(),
                    })?;
                    pass.deferred += 1;
                }
                Ok(m) if !m.is_file() => {
                    refuse(
                        "removal-refused-not-a-file",
                        format!("{} is not a regular file in the workspace", r.path),
                    )?;
                }
                Ok(_) => {
                    if local_dirty(&local, be) {
                        refuse(
                            "removal-refused-dirty",
                            format!(
                                "{} has unpublished local changes; the removal {} asked for                                  was not applied and the agent's work is kept — publish or                                  discard the local version, then ask again",
                                r.path, r.author
                            ),
                        )?;
                        continue;
                    }
                    if let Err(e) = std::fs::remove_file(&local) {
                        self.state.append_conflict(&ConflictRecord {
                            path: r.path.clone(),
                            foreign_etag: String::new(),
                            preserved_key: None,
                            kind: format!("removal-unlink-failed (will retry): {e}"),
                            at_unix: now_unix(),
                        })?;
                        pass.deferred += 1;
                        continue;
                    }
                    // CONFIRM: only NotFound is absence.
                    match std::fs::symlink_metadata(&local) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            pass.declared.insert(r.path.clone());
                            pass.applied.push(r.clone());
                        }
                        _ => {
                            self.state.append_conflict(&ConflictRecord {
                                path: r.path.clone(),
                                foreign_etag: String::new(),
                                preserved_key: None,
                                kind: "removal-unlink-unconfirmed (will retry)".into(),
                                at_unix: now_unix(),
                            })?;
                            pass.deferred += 1;
                        }
                    }
                }
            }
        }
        Ok(pass)
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
            epoch: self
                .lease
                .as_ref()
                .map(|l| l.epoch)
                .or_else(|| self.state.load_incarnation().ok().flatten().map(|i| i.epoch))
                .unwrap_or(0),
            flush_uuid: "conflict-preserve".into(),
            boundary_source: None,
            posix: PosixStamps::from_meta(&meta.meta),
        };
        self.store
            .put_whole(&dst, body, &PutCondition::IfNoneMatchAny, &stamps, crc)
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

    /// Renew the lease if it has gone stale MID-BARRIER, and hand back
    /// the cell as it was read so the CALLER can fence on it.
    ///
    /// Costs nothing on a short barrier: the renewal is skipped unless
    /// it was already due, so the request count is unchanged — this
    /// moves WHEN the renewal can happen, not how often.
    ///
    /// What this does NOT do, stated because the previous comment
    /// claimed it did: it never raises `Fenced` for a deposed holder.
    /// `lease::renew` can only 412 when the cell is still at OUR epoch
    /// with a token we do not hold; a cell at a FOREIGN epoch is
    /// skipped here (renewing it would be nonsense), so a deposed
    /// straggler passed straight through. The 2026-09-03 audit found
    /// the cadence barrier relying on exactly that phantom fence
    /// between upload chunks and completing every remaining data PUT
    /// after a takeover. The cell is returned so the caller can compare
    /// it against its lease — the cadence barrier uses the read it
    /// already paid for rather than a separate fence. `None` when there is
    /// no lease or the read failed: neither is a fence, and the
    /// ordinary renewal arm will try again.
    pub(crate) async fn renew_if_due(&mut self) -> LeanResult<Option<EpochState>> {
        let Some(lease) = self.lease.clone() else { return Ok(None) };
        let state = match self.store.epoch_read(&self.cfg.epoch_key()).await {
            Ok(Some(s)) => s,
            _ => return Ok(None),
        };
        let now = super::now_unix();
        let fresh = state
            .last_renew_unix
            .map(|t| now.saturating_sub(t) < RENEW_WITHIN_SECS)
            .unwrap_or(false);
        if fresh || state.epoch != lease.epoch {
            // Not due, or not ours to renew. The caller decides what a
            // foreign cell means; this function only knows it cannot
            // renew one.
            return Ok(Some(state));
        }
        super::lease::renew(self).await?;
        Ok(Some(state))
    }

    /// The between-chunk fence of the cadence barrier: a cell that no
    /// longer names this holder at this epoch stops the upload set
    /// HERE, before the next chunk's PUTs. Every one of those PUTs
    /// carries If-Match on a baseline etag that still matches — the
    /// successor has published nothing yet — so without this a deposed
    /// straggler overwrites the cited generation of every key it still
    /// had to upload, and every reader's S3-wins arm then adopts the
    /// uncited bytes silently (audit 2026-09-03, finding 1).
    fn fence_on_cell(&self, cell: &EpochState) -> LeanResult<()> {
        let Some(lease) = self.lease.as_ref() else {
            return Err(LeanError::Fenced("lease dropped mid-barrier".into()));
        };
        if cell.epoch != lease.epoch || cell.holder_id != lease.holder_id {
            return Err(LeanError::Fenced(format!(
                "deposed between upload chunks: cell at epoch {} holder {} (we are epoch {})",
                cell.epoch, cell.holder_id, lease.epoch
            )));
        }
        Ok(())
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

        // Step 1: the cell, read ONCE — HITL entries into the tree, then
        // the DECLARED removals (delete/rename design §4-§6).
        let inbox_doc = inbox::load(self.store.as_ref(), &self.cfg).await?.doc;
        let (consumed, settled_locally) = self.consume_counted(&inbox_doc).await?;
        report.consumed = consumed.len() + settled_locally;
        let removals = self.apply_removals(&inbox_doc).await?;
        report.removed = removals.declared.iter().cloned().collect();
        report.removals_refused = removals.refused.len();
        // A refusal is settled NOW, whatever else this barrier does: the
        // cell is where the human who asked reads the answer, and a
        // refused removal is never retried, so saying so before the
        // no-diff return below loses nothing.
        inbox::settle_removals(self.store.as_ref(), &self.cfg, epoch_hint, &[], &removals.refused)
            .await?;

        // Step 2: scan-diff against the persisted baseline.
        let mut baseline = self.state.load_baseline()?;
        let scanned = scan::scan(&self.cfg.root)?;
        let mut classified = scan::classify(&scanned, &baseline);
        if declared {
            report.absences_confirmed = self.confirm_absences(&mut classified)?;
        }
        // A declared removal is a deletion with its basis RECORDED, so
        // it skips the two-scan guard the walk needs and goes straight
        // into the delete set — which is what lets a rename's two halves
        // ride ONE manifest generation (§4).
        for p in &removals.declared {
            classified.first_absence.remove(p);
            if classified.uploads.contains(p) {
                // The agent re-created the path between the unlink and
                // the scan. The declaration named the OLD file, which is
                // gone from the tree (and, for a rename, moved); what is
                // here now is a new file the agent wrote, and it
                // publishes as one — its PUT supersedes the old object
                // under the same key. Deleting it would un-cite the
                // agent's work for a barrier and GC-skip its object; the
                // model's install gives the upload the same precedence.
                continue;
            }
            classified.deletes.insert(p.clone());
        }
        report.first_absence = classified.first_absence.iter().cloned().collect();

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
            && removals.applied.is_empty()
            && !repairs_pending
        {
            // Read the POINTER, never the entries: the 0b rig measured
            // the idle tick at 1M entries as 27 s / 1.3 GiB — dominated
            // by fetching and parsing a 264 MiB document just to read
            // `seq`. The pointer is a few hundred bytes and carries the
            // seq in its body, so a GET of it costs one round trip and
            // answers strictly more than a HEAD of the old object did.
            //
            // Legacy workspaces (no pointer yet) keep the HEAD they had.
            let unchanged = match manifest::load_pointer(self.store.as_ref(), &self.cfg).await? {
                Some(manifest::LoadedPointer { pointer: p, etag, .. }) => {
                    // The news ticker rides this request for free — D5's
                    // "zero added bucket requests" is still literal.
                    report.observed_seq = Some(p.seq);
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
        let prior_uuids = {
            let mut v = intent.recent_uuids.clone();
            if !intent.flush_uuid.is_empty() {
                v.push(intent.flush_uuid.clone());
            }
            v
        };
        // Carried forward, not dropped: it stays true until we install
        // again, and the merge below is what reads it.
        let prev_installed = intent.installed_etag.clone();
        intent = IntentJournal {
            flush_uuid: flush_uuid.clone(),
            keys: classified.uploads.iter().map(|p| self.cfg.file_key(p)).collect(),
            recent_uuids: prior_uuids.clone(),
            installed_etag: prev_installed.clone(),
            installed_foreign: intent.installed_foreign.clone(),
            declared_deletes: removals.declared.iter().cloned().collect(),
        };
        self.state.save_intent(&intent)?;
        // The window is NOT opened here any more. It is the bucket's
        // "a barrier is committing" sign for HITL writers, and it opens
        // in the commit section, under the lease, at the epoch the
        // commit really holds. The uploads below race a HITL write the
        // way two writers race each other: If-Match decides, the loser
        // is preserved and recorded (`LeanNoWindowHolds` proves safety
        // never depended on the window).
        //
        // The consumed entries are not dropped here either — "durably
        // in the baseline" is true of a container restart and false of
        // a pod REPLACEMENT: the emptyDir goes with the pod. They leave
        // the cell with the window clear, after the manifest cites
        // them; a successor that finds them re-consumes, idempotently.

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
                            let prior_uuids = &prior_uuids;
                            async move {
                                let r = this
                                    .upload_one(
                                        path, scanned_entry, base, epoch_hint, flush_uuid,
                                        prior_uuids,
                                    )
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
                        "outcome": if adopted { "adopted" } else { "put" }}));
                    if adopted {
                        observed.insert(path.clone());
                    }
                    report.published_bytes += entry.size;
                    upserts.insert(path.clone(), entry);
                    new_baseline_entries.insert(path.clone(), baseline_entry);
                    report.uploaded.push(path.clone());
                }
                UploadOutcome::Parked { foreign_etag } => {
                    self.trace("upload", serde_json::json!({"flush": flush_uuid, "path": path, "etag": foreign_etag, "outcome": "parked"}));
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
                UploadOutcome::Deferred => {
                    self.trace("upload", serde_json::json!({"flush": flush_uuid, "path": path, "outcome": "deferred"}));
                    report.deferred.push(path.clone());
                }
            }
        }

        // Citation repairs: paths whose integrated object (the
        // baseline) differs from the manifest's citation — consumed
        // HITL adoptions and checkout's S3-wins arm. No bytes move; the
        // manifest re-cites what this syncer already integrated.
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
                    let crc = repair_crc(&path, &be, &meta)?;
                    let stamps = GenerationStamps::from_meta(&meta.meta);
                    let scan_entry = scanned.get(&path);
                    upserts.insert(
                        path.clone(),
                        LeanEntry {
                            key,
                            etag: meta.etag.clone(),
                            crc64_b64: crc,
                            size: meta.size,
                            mode: stamps
                                .as_ref()
                                .and_then(|s| s.posix)
                                .map(|p| p.mode)
                                .or(scan_entry.map(|s| s.mode))
                                .unwrap_or(0o644),
                            mtime_unix: scan_entry.map(|s| s.mtime_unix).unwrap_or(0),
                            generation: stamps.map(|s| s.generation).unwrap_or(be.generation),
                            epoch: epoch_hint,
                        },
                    );
                    observed.insert(path.clone());
                }
                // Moved again or gone: the next consume reconciles it.
                _ => {}
            }
        }

        // A PULL-ONLY boundary: nothing uploaded, deleted, consumed, removed
        // or re-cited, so the merge can only add nothing — and then the
        // commit section writes nothing to the bucket (no CAS, no GC, no
        // inbox entry to drop) and what remains is local: queue the other
        // writers' changes and take theirs as the merge base. That needs no
        // fence and no window; claiming for it queued every such writer
        // behind the publishing ones (65 of 191 claims in the writers drill).
        // A merge that does add something (a mirror flag to restamp, say)
        // falls through to the commit section.
        if classified.uploads.is_empty()
            && classified.deletes.is_empty()
            && upserts.is_empty()
            && consumed.is_empty()
            && removals.applied.is_empty()
            && observed.is_empty()
        {
            let current = manifest::load(self.store.as_ref(), &self.cfg).await?;
            let m = self.merge_onto(
                current.as_ref(),
                prev_installed.as_deref(),
                &baseline.inst_base,
                &upserts,
                &classified.deletes,
                &parked,
                &flush_uuid,
            );
            if let Some(handle) = m.expected.as_ref().filter(|_| m.adds_nothing()) {
                if !m.foreign.is_empty() || !m.gone.is_empty() {
                    self.state.queue_foreign(&foreign_changes(&m.foreign, &m.gone))?;
                    self.trace("queue", serde_json::json!({"flush": flush_uuid, "upserts": m.foreign.len(), "tombstones": m.gone.len(), "fence": false}));
                }
                report.seq = Some(m.theirs.seq);
                report.no_change = true;
                report.manifest_etag = Some(handle.etag.clone());
                report.observed_seq = Some(m.theirs.seq);
                report.observed_etag = Some(handle.etag.clone());
                report.foreign_queued = m.foreign.len();
                baseline.inst_base = m.theirs.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
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
            self.verify_not_deposed().await?;
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
            // be overtaken before this CAS. Whatever is gone or replaced is
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
            // The window: the bucket-visible "a barrier is committing" sign
            // HITL writers wait on, at the epoch this commit holds.
            let deadline = now_unix() + self.cfg.window_slack_secs;
            inbox::open_window(self.store.as_ref(), &self.cfg, epoch, deadline).await?;
            let mut foreign_entries: Vec<(String, LeanEntry)> = vec![];
            let mut foreign_gone: Vec<String> = vec![];
            // Set when the merge added nothing to the document: nothing is
            // installed, and theirs becomes the merge base as it stands.
            let mut installed_nothing = false;
            let mut attempt = 0;
            let (installed, installed_etag) = loop {
                attempt += 1;
                if attempt > 4 {
                    return Err(LeanError::State(
                        "manifest CAS lost 4 merge races — refusing this barrier".into(),
                    ));
                }
                let current = manifest::load(self.store.as_ref(), &self.cfg).await?;
                let MergeOnto { theirs, expected, merged, foreign, gone } = self.merge_onto(
                    current.as_ref(),
                    prev_installed.as_deref(),
                    &baseline.inst_base,
                    &upserts,
                    &classified.deletes,
                    &parked,
                    &flush_uuid,
                );
                // Nothing of ours changes the document — a barrier that only
                // found the manifest moved by another writer. Installing it
                // anyway was an empty generation, and the peer's next tick
                // then found the manifest moved and did the same: two idle
                // writers traded generations (and cell claims) for as long
                // as both ran. Theirs becomes the merge base as it stands.
                if let Some(handle) = expected.as_ref() {
                    if merged.entries == theirs.entries && merged.sole_writer == theirs.sole_writer {
                        foreign_entries = foreign;
                        foreign_gone = gone;
                        installed_nothing = true;
                        break (theirs, handle.etag.clone());
                    }
                }
                match manifest::cas_write_stamped(
                    self.store.as_ref(),
                    &self.cfg,
                    &merged,
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
                        intent.installed_etag = Some(meta.etag.clone());
                        intent.installed_foreign = foreign_changes(&foreign, &gone);
                        self.state.save_intent(&intent)?;
                        foreign_entries = foreign;
                        foreign_gone = gone;
                        break (merged, meta.etag);
                    }
                    Err(LeanError::Store(StoreError::PreconditionFailed(_)))
                    | Err(LeanError::Store(StoreError::Conflict(_))) => {
                        self.trace("cas", serde_json::json!({"flush": flush_uuid, "seq": merged.seq,
                            "expected": expected.as_ref().map(|h| h.etag.clone()), "result": "lost"}));
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

            // Step 6: deletes LAST — GC of keys the NEW manifest no longer
            // references, HEAD-guarded on the recognized ETag.
            let mut swept = 0usize;
            let mut gc_held = false;
            for path in &classified.deletes {
                // A mass delete is the one long stretch of the commit
                // section: keep the token moving so a waiter does not count
                // a live holder dead, and fence on what the read returns.
                swept += 1;
                if swept % 200 == 0 {
                    if let Some(cell) = self.renew_if_due().await? {
                        self.fence_on_cell(&cell)?;
                    }
                }
                if installed.entries.contains_key(path) {
                    continue; // delete/modify resolved foreign-wins: not garbage
                }
                let key = self.cfg.file_key(path);
                let recognized = baseline.entries.get(path).map(|b| b.etag.clone()).or_else(|| {
                    // A DECLARED removal of a path this tree never held (a
                    // scoped workspace's out-of-scope citation): what the
                    // declaration named is the object the manifest cited
                    // when this barrier began, and that is the merge base.
                    removals
                        .declared
                        .contains(path)
                        .then(|| baseline.inst_base.get(path).cloned())
                        .flatten()
                });
                // The HEAD answers most paths in one request (gone, or an
                // etag we do not recognize). The DELETE itself carries
                // If-Match on the recognized etag: another writer's
                // upload holds no lease and can replace the object
                // between the two requests, and an unconditional delete
                // then removed bytes that writer's commit cites (the
                // model's LeanBarrierLeaseGCUnconditional).
                let unrecognized = match self.store.head(&key).await {
                    Err(StoreError::NotFound(_)) => {
                        self.trace("gc", serde_json::json!({"flush": flush_uuid, "path": path, "head": null, "result": "absent"}));
                        report.deleted.push(path.clone());
                        continue;
                    }
                    Ok(meta) if Some(&meta.etag) == recognized.as_ref() => {
                        if self.cfg.drill_hold_gc_secs > 0 && !gc_held {
                            gc_held = true;
                            eprintln!(
                                "flint-sync: DRILL: holding the GC for {}s between the HEAD of {path} and its DELETE",
                                self.cfg.drill_hold_gc_secs
                            );
                            self.trace("drill_hold", serde_json::json!({"flush": flush_uuid, "where": "gc", "path": path,
                                "secs": self.cfg.drill_hold_gc_secs}));
                            tokio::time::sleep(std::time::Duration::from_secs(self.cfg.drill_hold_gc_secs)).await;
                        }
                        match self.store.delete_if_match(&key, &meta.etag).await {
                            Ok(()) | Err(StoreError::NotFound(_)) => {
                                self.trace("gc", serde_json::json!({"flush": flush_uuid, "path": path, "head": meta.etag, "result": "deleted"}));
                                report.deleted.push(path.clone());
                                continue;
                            }
                            // Replaced between the HEAD and the DELETE:
                            // whatever is there now is not ours to
                            // collect. Name it, if it still exists.
                            Err(StoreError::PreconditionFailed(_)) => match self.store.head(&key).await {
                                Ok(now) => now.etag,
                                Err(StoreError::NotFound(_)) => {
                                    self.trace("gc", serde_json::json!({"flush": flush_uuid, "path": path, "head": meta.etag, "result": "replaced-absent"}));
                                    report.deleted.push(path.clone());
                                    continue;
                                }
                                Err(e) => return Err(e.into()),
                            },
                            Err(e) => return Err(e.into()),
                        }
                    }
                    Ok(meta) => meta.etag,
                    Err(e) => return Err(e.into()),
                };
                // An ETag this syncer does not recognize is NEVER deleted
                // (a HITL re-create landed after our CAS, or another
                // writer's upload).
                self.trace("gc", serde_json::json!({"flush": flush_uuid, "path": path, "head": unrecognized,
                    "recognized": recognized, "result": "skip"}));
                self.state.append_conflict(&ConflictRecord {
                    path: path.clone(),
                    foreign_etag: unrecognized,
                    preserved_key: None,
                    kind: "gc-skip".into(),
                    at_unix: now_unix(),
                })?;
            }

            // Step 7: the foreign queue, then the baseline rewrite, intent
            // clear and window clear.
            //
            // Other writers' changes go to THIS writer's queue BEFORE the
            // merge base below moves past them: a crash between the two
            // re-applies them idempotently, where the other order would
            // leave a base that claims changes the tree never received.
            if !foreign_entries.is_empty() || !foreign_gone.is_empty() {
                self.state.queue_foreign(&foreign_changes(&foreign_entries, &foreign_gone))?;
                self.trace("queue", serde_json::json!({"flush": flush_uuid, "upserts": foreign_entries.len(), "tombstones": foreign_gone.len()}));
            }
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
            // No `merge-preserved` entries into the SHARED inbox any more:
            // they are this writer's, and sit in its local queue.
            inbox::clear_window_settling(
                self.store.as_ref(),
                &self.cfg,
                epoch,
                &[],
                &consumed,
                &removals.applied,
            )
            .await?;
            // Reap superseded generations. Immutable metadata that is never
            // collected is a leak that grows by a whole manifest per
            // publish, and this also collects the orphan a crash between the
            // entries PUT and the pointer CAS leaves behind. Best effort by
            // design: a publish that succeeded is not un-done by a failure
            // to tidy up after it.
            match manifest::sweep_generations(self.store.as_ref(), &self.cfg).await {
                Ok(0) => {}
                Ok(n) => eprintln!("flint-sync: reaped {n} superseded manifest generation(s)"),
                Err(e) => eprintln!("flint-sync: generation sweep: {e}"),
            }
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
            match manifest::sweep_chunks(self.store.as_ref(), &self.cfg).await {
                Ok(0) => {}
                Ok(n) => eprintln!("flint-sync: reaped {n} unreferenced manifest chunk(s)"),
                Err(e) => eprintln!("flint-sync: chunk sweep: {e}"),
            }
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
                "parked": report.parked.len(), "consumed": report.consumed, "no_change": report.no_change,
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
        local_path: &Path,
        size: u64,
        scanned: &scan::ScanEntry,
        condition: PutCondition,
        stamps: GenerationStamps,
        generation: u64,
        epoch: u64,
        prior_uuids: &[String],
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


        // At most three attempts: the caller's condition; then once more
        // If-Match on what a 412 showed us — our own torn Complete
        // (recognized through the crash journal, review 2026-09-12
        // atomicity-2), or a foreign version now PRESERVED (inbox-1) —
        // or a create if the base object vanished (inbox-5). A 412 on
        // the retry means the path is being written under us: park.
        let mut condition = condition;
        for attempt in 0..3u32 {
            let spec = flint_store::ComposeSpec {
                progress: None,
                key,
                local_path,
                parts: parts.clone(),
                base_key: None,
                base_etag: None,
                condition: condition.clone(),
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
                    return Ok(UploadOutcome::published(
                        path, key.to_string(), meta.etag, crc, size, scanned, generation, epoch,
                    ));
                }
                Err(StoreError::ChecksumMismatch(_)) | Err(StoreError::NoSuchUpload(_)) => {
                    // Drift mid-compose, or the operator sweep aborted us:
                    // nothing published; the next barrier re-queues.
                    return Ok(UploadOutcome::Deferred);
                }
                // S3's answer to If-Match on a missing key (see upload_one):
                // the base vanished, and a vanished base is a create.
                Err(StoreError::NotFound(_)) if matches!(condition, PutCondition::IfMatch(_)) => {
                    condition = PutCondition::IfNoneMatchAny;
                    continue;
                }
                Err(StoreError::PreconditionFailed(_)) => {
                    // The AdoptOwn recognizer compares OUR bytes' checksum
                    // against what landed, and without the pre-pass we do
                    // not have one — so the file is read HERE, on the rare
                    // recovery path, instead of on every publish.
                    let crc = file_crc(local_path)?;
                    let head = match self.store.head(key).await {
                        Ok(h) => h,
                        Err(StoreError::NotFound(_)) => {
                            condition = PutCondition::IfNoneMatchAny;
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    let head_stamps = GenerationStamps::from_meta(&head.meta);
                    if head.crc64_b64.as_deref() == Some(crc64_to_b64(crc).as_str()) {
                        // These bytes are already there (a torn Complete):
                        // cite them — re-read under the fence first.
                        let g = head_stamps.map(|s| s.generation).unwrap_or(generation);
                        return Ok(UploadOutcome::adopted(
                            path, key.to_string(), head.etag, crc, head.size, scanned, g, epoch,
                        ));
                    }
                    if attempt >= 1 {
                        return Ok(UploadOutcome::Parked { foreign_etag: head.etag });
                    }
                    let own = head_stamps
                        .as_ref()
                        .map(|s| s.flush_uuid == stamps.flush_uuid || prior_uuids.contains(&s.flush_uuid))
                        .unwrap_or(false);
                    if !own && self.preserve_foreign_412(path, &head).await.is_none() {
                        return Ok(UploadOutcome::Parked { foreign_etag: head.etag });
                    }
                    condition = PutCondition::IfMatch(head.etag);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(UploadOutcome::Deferred)
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
        let generation = base.map(|b| b.generation + 1).unwrap_or(1);
        // Review 2026-09-12, atomicity-6: the scan skipped symlinks; this
        // read followed them. A regular file swapped for a symlink between
        // the two published the link's TARGET — a file outside the
        // workspace, the syncer's own /proc/self/environ included. Every
        // stat here is an lstat and the read opens O_NOFOLLOW; a path that
        // is no longer a regular file is refused, recorded, and deferred
        // (the next scan skips it, and the two-scan rule retires it).
        let lmeta = std::fs::symlink_metadata(&local_path)
            .map_err(|e| LeanError::State(format!("stat {}: {e}", local_path.display())))?;
        if !lmeta.is_file() {
            self.state.append_conflict(&ConflictRecord {
                path: path.to_string(),
                foreign_etag: String::new(),
                preserved_key: None,
                kind: "upload-refused-not-regular: no longer a regular file after the scan (a symlink \
                       or special file replaced it); nothing published"
                    .into(),
                at_unix: now_unix(),
            })?;
            return Ok(UploadOutcome::Deferred);
        }
        let posix = Some(PosixStamps::from_metadata(&lmeta));
        let stamps = GenerationStamps {
            generation,
            epoch,
            flush_uuid: flush_uuid.to_string(),
            boundary_source: None,
            posix,
        };
        let condition = match base {
            Some(b) => PutCondition::IfMatch(b.etag.clone()),
            None => PutCondition::IfNoneMatchAny,
        };
        let size = lmeta.len();
        if size > self.cfg.whole_put_max {
            // Streaming multipart compose: put_whole is never fed past
            // whole_put_max (unbounded memory + S3's 5 GiB wall).
            return self
                .upload_compose(
                    path, &key, &local_path, size, scanned, condition, stamps, generation, epoch,
                    prior_uuids,
                )
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
        let body = Bytes::from(read_nofollow(&local_path)?);
        let uploaded_len = body.len() as u64;
        let crc = crc64_nvme(&body);
        match self
            .store
            .put_whole(&key, body.clone(), &condition, &stamps, crc)
            .await
        {
            Ok(meta) => Ok(UploadOutcome::published(
                path, key, meta.etag, crc, uploaded_len, scanned, generation, epoch,
            )),
            // S3 answers If-Match on a key that no longer exists with 404
            // NoSuchKey, not 412 (Ozone answers 412, and so did the
            // double): the base is gone — a peer's GC collected it, or a
            // bucket-level delete. The same rule as the 412 arm's vanished
            // HEAD below: a vanished base is a create. Without this arm the
            // rule never ran on S3, and a writer whose edited path a peer
            // deleted failed EVERY barrier (drill host leg H2, 2026-09-13).
            Err(StoreError::NotFound(_)) if matches!(condition, PutCondition::IfMatch(_)) => {
                let meta = self
                    .store
                    .put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps, crc)
                    .await?;
                Ok(UploadOutcome::published(
                    path, key, meta.etag, crc, uploaded_len, scanned, generation, epoch,
                ))
            }
            Err(StoreError::PreconditionFailed(_)) => {
                // The 412 policy: my own crashed/torn PUT ⇒ adopt; a
                // foreign version ⇒ the consume-dirty rule, at upload time
                // (preserve it, then supersede it knowingly). NEVER the
                // inherited LOCAL-WINS overwrite, and never a park with
                // no way out.
                let head = match self.store.head(&key).await {
                    Ok(h) => h,
                    Err(StoreError::NotFound(_)) => {
                        // Review 2026-09-12, inbox-5: the base object is
                        // gone (a bucket-level delete, or our own GC after
                        // a crash between the CAS and the baseline
                        // rewrite). Failing the barrier here failed EVERY
                        // barrier, forever. A vanished base is a create.
                        let meta = self
                            .store
                            .put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps, crc)
                            .await?;
                        return Ok(UploadOutcome::published(
                            path, key, meta.etag, crc, uploaded_len, scanned, generation, epoch,
                        ));
                    }
                    Err(e) => return Err(e.into()),
                };
                let head_stamps = GenerationStamps::from_meta(&head.meta);
                let own = head_stamps
                    .as_ref()
                    .map(|s| s.flush_uuid == flush_uuid || prior_uuids.contains(&s.flush_uuid))
                    .unwrap_or(false);
                if head.crc64_b64.as_deref() == Some(crc64_to_b64(crc).as_str()) {
                    // Bytes already there (a torn response, ours or a
                    // foreign write of the same content): cite it — re-read
                    // under the fence first.
                    let g = head_stamps.map(|s| s.generation).unwrap_or(generation);
                    return Ok(UploadOutcome::adopted(
                        path, key, head.etag, crc, head.size, scanned, g, epoch,
                    ));
                }
                if !own && self.preserve_foreign_412(path, &head).await.is_none() {
                    return Ok(UploadOutcome::Parked { foreign_etag: head.etag });
                }
                // Our earlier PUT (older content), or a foreign version
                // now preserved: supersede it knowingly, If-Match on what
                // we saw. A second 412 means the path is being written
                // under us right now; that one parks.
                let meta = match self
                    .store
                    .put_whole(&key, body, &PutCondition::IfMatch(head.etag.clone()), &stamps, crc)
                    .await
                {
                    Ok(m) => m,
                    // 412 (a writer landed between) or 404 (a GC removed
                    // it between the HEAD and this PUT): written under us
                    // right now. Park; the next barrier retries.
                    Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) => {
                        return Ok(UploadOutcome::Parked { foreign_etag: head.etag });
                    }
                    Err(e) => return Err(e.into()),
                };
                Ok(UploadOutcome::published(
                    path, key, meta.etag, crc, uploaded_len, scanned, generation, epoch,
                ))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// A 412 against a version this syncer did not write, met at upload
    /// time. The contract's rule for a foreign write to a path the agent
    /// modified is the consume-dirty rule — the agent's version wins and
    /// the foreign bytes are preserved — and a "park" was that case with
    /// no resolution: nothing ever un-parked, and every ack said `ok`
    /// (review 2026-09-12, inbox-1). Preserve, record, and let the caller
    /// supersede. `None` = the preserve failed; the caller parks, and the
    /// boundary is `partial`.
    async fn preserve_foreign_412(&self, path: &str, head: &flint_store::ObjectMeta) -> Option<()> {
        let preserved = match self.preserve_conflict_copy(path, &head.etag).await {
            Ok(k) => k,
            Err(e) => {
                eprintln!("flint-sync: {path}: foreign version {} could not be preserved ({e}); parking", head.etag);
                return None;
            }
        };
        self.state
            .append_conflict(&ConflictRecord {
                path: path.to_string(),
                foreign_etag: head.etag.clone(),
                preserved_key: Some(preserved),
                kind: "upload-412-preserved".into(),
                at_unix: now_unix(),
            })
            .ok()?;
        Some(())
    }
}

/// Read a workspace file for upload without following a symlink at the
/// final component, and refuse anything that is not a regular file once
/// open (review 2026-09-12, atomicity-6).
fn read_nofollow(p: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(p)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    Ok(v)
}

enum UploadOutcome {
    /// `adopted`: the etag was OBSERVED at the key (bytes already there,
    /// no PUT of ours produced it), so the commit section must re-read it
    /// before citing — see `verify_observed_citations` in the barrier.
    Published { entry: LeanEntry, baseline_entry: BaselineEntry, adopted: bool },
    Parked { foreign_etag: String },
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
                key,
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

fn local_dirty(local: &Path, base: Option<&BaselineEntry>) -> bool {
    match (std::fs::metadata(local), base) {
        (Err(_), None) => false,                   // both absent: clean
        (Err(_), Some(_)) => true,                 // locally deleted vs baseline
        (Ok(_), None) => true,                     // local exists, never published
        (Ok(m), Some(b)) => {
            scan::stat_changed(b.size, b.mtime_unix, b.mtime_nanos, m.len(), mtime_of(&m), mtime_nanos_of(&m))
        }
    }
}

/// Full-object CRC-64/NVME of a local file, streamed.
///
/// Reached only by the 412 recovery path: the ordinary publish gets its
/// checksum from the store, which computes one while uploading.
/// The CRC a citation repair cites: the bytes this syncer integrated,
/// as the baseline recorded them when it wrote or uploaded them. The
/// HEAD's own checksum is a cross-check when the backend offers one,
/// never the source — Ozone offers none, a HITL uploader may have sent
/// none, and the manifest must carry a CRC either way, because every
/// reader now verifies a fresh fetch against it.
///
/// A baseline entry with no CRC is refused loudly rather than cited
/// bare: the only entry built without one is the consume-dirty sentinel,
/// which the candidate filters exclude, so reaching this is a defect.
/// A HEAD that attests a DIFFERENT value is refused the same way — the
/// workspace holds bytes the object does not, and citing either would
/// make one reader or another refuse the path forever.
pub(crate) fn repair_crc(
    path: &str,
    be: &BaselineEntry,
    head: &flint_store::ObjectMeta,
) -> LeanResult<String> {
    let Some(ours) = be.crc64_b64.clone() else {
        return Err(LeanError::State(format!(
            "citation repair of {path}: the baseline records no CRC for the bytes it \
             integrated at etag {} — refusing to cite bytes nothing hashed",
            be.etag
        )));
    };
    if let Some(theirs) = head.crc64_b64.as_deref() {
        if theirs != ours {
            return Err(LeanError::State(format!(
                "citation repair of {path}: the store attests CRC-64 {theirs} for etag {} but \
                 the bytes this workspace integrated under that etag hash to {ours} — the \
                 workspace holds bytes the object does not; refusing to cite either",
                be.etag
            )));
        }
    }
    Ok(ours)
}

fn file_crc(local_path: &Path) -> LeanResult<u64> {
    use std::io::Read;
    let mut crc = flint_store::Crc64Nvme::new();
    let mut f = std::fs::File::open(local_path)?;
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

/// Containment-safe workspace write (boundary-verbs plan §2.2 security
/// gate). `rel` is a workspace-relative path; the target may not escape
/// the root, traverse a symlink component, or land in the reserved
/// control namespace.
///
/// The hazard this closes is pre-existing and reachable from three
/// callers (checkout, inbox consume, sync): `write_file_atomic` did
/// `create_dir_all(parent)` + write with no `O_NOFOLLOW` and no
/// root-containment check, while the scanner SKIPS symlinks — so a
/// planted symlink is invisible to the syncer. An unprivileged app
/// that plants `inputs -> /root/.aws`, lands an object at
/// `inputs/<path>` and drops a scoped sync turns the credential-holding
/// syncer into an on-demand arbitrary-file-write primitive outside the
/// workspace.
pub(super) fn write_file_atomic_in(
    root: &Path,
    rel: &str,
    bytes: &[u8],
    mode: Option<u32>,
) -> LeanResult<()> {
    let target = contained_path(root, rel)?;
    write_file_atomic(&target, bytes, mode).map(|_| ())
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

/// The queue form of a merge's foreign upserts and deletions.
fn foreign_changes(entries: &[(String, LeanEntry)], gone: &[String]) -> Vec<ForeignChange> {
    entries
        .iter()
        .map(|(path, e)| ForeignChange {
            path: path.clone(),
            etag: Some(e.etag.clone()),
            crc64_b64: Some(e.crc64_b64.clone()),
        })
        .chain(gone.iter().map(|path| ForeignChange { path: path.clone(), etag: None, crc64_b64: None }))
        .collect()
}

/// One three-way merge of a barrier's changes onto the bucket's manifest.
struct MergeOnto {
    theirs: manifest::LeanManifest,
    /// The CAS handle over `theirs`; `None` when the bucket has no manifest.
    expected: Option<manifest::ManifestHandle>,
    merged: manifest::LeanManifest,
    /// Other writers' changes since the merge base that the tree lacks.
    foreign: Vec<(String, LeanEntry)>,
    /// Other writers' deletions since the merge base.
    gone: Vec<String>,
}

impl MergeOnto {
    /// The merge adds nothing to the document: installing it would be an
    /// empty generation.
    fn adds_nothing(&self) -> bool {
        self.merged.entries == self.theirs.entries && self.merged.sole_writer == self.theirs.sole_writer
    }
}

impl Syncer {
    #[allow(clippy::too_many_arguments)]
    fn merge_onto(
        &self,
        current: Option<&manifest::LoadedManifest>,
        prev_installed: Option<&str>,
        inst_base: &BTreeMap<String, String>,
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
        // If the bucket is still at the document THIS workspace
        // installed, that document IS the merge base — whatever the
        // persisted one says. Step 7 rewrites the merge base after
        // the CAS, so a restart in between leaves it behind a
        // document we wrote, and every entry in it would read as
        // somebody else's change. See `IntentJournal::installed_etag`.
        let own_base: BTreeMap<String, String>;
        let base: &BTreeMap<String, String> =
            if prev_installed.is_some() && prev_installed == expected.as_ref().map(|h| h.etag.as_str()) {
                own_base = theirs.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();
                &own_base
            } else {
                inst_base
            };
        let (mut merged, foreign) = manifest::merge(base, &theirs, upserts, deletes, parked);
        // Deletions another writer made since this workspace's
        // merge base: in the base, gone from theirs, and not this
        // barrier's own upsert, delete or park. `merge` has no use
        // for them — theirs already lacks the path — but the TREE
        // does: they reach it through the local queue, as the
        // foreign upserts do.
        let gone: Vec<String> = base
            .keys()
            .filter(|p| {
                !theirs.entries.contains_key(*p)
                    && !upserts.contains_key(*p)
                    && !deletes.contains(*p)
                    && !parked.contains(*p)
            })
            .cloned()
            .collect();
        // `merge` clears it; the installing pass owns it. A mirror
        // is a property of how this workspace is DEPLOYED, so it
        // comes from config on every publish rather than being
        // inherited from whatever wrote last.
        merged.sole_writer = self.cfg.sole_writer;
        self.trace("merge", serde_json::json!({"flush": flush_uuid, "theirs_seq": theirs.seq, "upserts": upserts.len(),
            "deletes": deletes.len(), "foreign": foreign.len(), "gone": gone.len(),
            "adds_nothing": merged.entries == theirs.entries}));
        MergeOnto { theirs, expected, merged, foreign, gone }
    }
}
