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

use super::barrier::{
    contained_path, contained_path_stat, local_dirty, mode_of, mtime_nanos_of, mtime_of, write_file_atomic,
    write_file_atomic_if,
};
use super::manifest;
use super::state::BaselineEntry;
use super::{LeanError, LeanResult, Syncer};

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
    /// Passes re-run because the pointer moved past the document a
    /// pass read and a handle it cited was collected (handles design
    /// 2026-09-19): the checkout re-resolves instead of asking the
    /// caller to.
    pub reresolved: u32,
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
/// two different things here depending on `sole_writer`,
/// and all three refusals are written once, in the whole-object arm
/// below. Re-deciding them here would put the same policy in two
/// places, which is how the two drift apart. The cost is one wasted
/// request on a path that is already an incident.
///
/// Every other error propagates after the per-range retries: a transport
/// failure is a failure, not a quiet fallback that would hide a broken
/// network behind a slower code path.
///
/// `over` is a while-clean licence (`Licence::WhileClean`): the path the
/// agent may be writing, and the baseline entry it must still read as
/// when every range is in the temp. Under it the three outcomes that are
/// not the store's failure come back as values, for the caller's record,
/// instead of ending the pass: the agent wrote first (`Kept`), the bytes
/// are not the cited ones (`Corrupt`), the tree could not take them
/// (`WriteFailed`). Without it they are errors, as a checkout needs.
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
    expect_crc: u64,
    over: Option<(std::path::PathBuf, Option<BaselineEntry>)>,
) -> LeanResult<Ranged> {
    use futures::stream::StreamExt;

    /// A range that failed for a reason the caller must distinguish.
    enum RangeFail {
        /// 412/404 — the whole-object arm owns this policy.
        Fallback,
        /// The temp would not take the range (a full disk): the tree's
        /// failure, not the store's.
        Write(LeanError),
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
        return Ok(Ranged::Declined);
    }

    // SHARED so each range writes ITSELF, on the blocking pool. Before
    // this the write ran on the task that polls the fan-out, so a pwrite
    // of one range stalled the FETCH of every other range in flight —
    // concurrency that could never overlap I/O with I/O. Measured on
    // i4i.large against an in-memory S3: system time beat user time 6:1
    // and the process never exceeded 1.31 of 2 cores, flat from 4
    // streams to 32.
    let sink = match super::safefs::RangedTmp::create(target, tmp, size, mode) {
        Ok(sink) => std::sync::Arc::new(sink),
        Err(e) if over.is_some() => return Ok(Ranged::WriteFailed(e)),
        Err(e) => return Err(e),
    };
    let mut ranges = futures::stream::iter(parts.into_iter().map(|(off, len)| {
        let store = store.clone();
        let key = key.to_string();
        let etag = etag.to_string();
        let sink = sink.clone();
        async move {
            let mut attempt: u32 = 0;
            let b = loop {
                // EVERY range carries the same If-Match, which is what
                // makes the assembled file one object rather than a
                // splice of two: a write between range 3 and range 4
                // fails range 4 instead of silently interleaving
                // generations.
                match store.get_range_segments(&key, off, len, &etag).await {
                    Ok(b) => break b,
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
            };
            // The SDK's own frames, never flattened into one buffer.
            // Summed rather than `b.is_empty()`: an empty range can also
            // arrive as one zero-length segment, which is the same hole.
            let n: u64 = b.iter().map(|seg| seg.len() as u64).sum();
            // Checked HERE rather than at the drain: a hole must be
            // refused before it is written, not after.
            if n == 0 {
                return Err(RangeFail::Fatal(LeanError::State(format!(
                    "get_range {key} at {off} returned an empty range before the object's \
                     end — refusing a hole"
                ))));
            }
            let crc = tokio::task::spawn_blocking(move || {
                // Segments are contiguous and in order within the range,
                // so each lands at the running offset. The file is
                // positional throughout, so siblings still cannot race.
                // The range's CRC-64 is taken here, on the same pass, so
                // the drain can fold the ranges in offset order and
                // compare the whole against the manifest before the
                // rename makes the file visible.
                let mut h = flint_store::Crc64Nvme::new();
                let mut at = off;
                for seg in &b {
                    sink.write_at(at, seg)?;
                    h.update(seg);
                    at += seg.len() as u64;
                }
                Ok::<u64, LeanError>(h.finalize())
            })
            .await
            .map_err(|e| {
                RangeFail::Fatal(LeanError::State(format!(
                    "write task for {key} at {off} did not complete: {e}"
                )))
            })?
            .map_err(RangeFail::Write)?;
            Ok((off, n, crc))
        }
    }))
    .buffer_unordered(parallelism.max(1));

    let mut bytes = 0u64;
    let mut crcs: Vec<(u64, u64, u64)> = Vec::new();
    while let Some(next) = ranges.next().await {
        match next {
            Ok((off, n, crc)) => {
                bytes += n;
                crcs.push((off, n, crc));
            }
            Err(RangeFail::Fallback) => {
                drop(ranges);
                let _ = std::fs::remove_file(tmp);
                return Ok(Ranged::Declined);
            }
            Err(RangeFail::Write(e)) => {
                drop(ranges);
                let _ = std::fs::remove_file(tmp);
                return if over.is_some() { Ok(Ranged::WriteFailed(e)) } else { Err(e) };
            }
            Err(RangeFail::Fatal(e)) => {
                drop(ranges);
                let _ = std::fs::remove_file(tmp);
                return Err(e);
            }
        }
    }
    // Every range future is finished and dropped, so this is the last
    // reference. try_unwrap rather than a clone-tolerant commit: if a
    // writer were somehow still alive, renaming now would publish a
    // file still being written into.
    drop(ranges);
    let sink = std::sync::Arc::try_unwrap(sink).map_err(|_| {
        LeanError::State(format!(
            "ranged materialisation of {}: a range writer outlived the fan-out",
            target.display()
        ))
    })?;
    // The whole object's CRC-64 from its ranges: `crc64_combine`
    // composes A-then-B, so the fold runs in offset order whatever
    // order the ranges landed in. Checked BEFORE the rename, for the
    // same reason a hole is: a wrong file must never become visible.
    // Unconditional: this arm fetches only under If-Match on the cited
    // etag, so the bytes are always the manifest's to describe.
    crcs.sort_unstable_by_key(|(off, _, _)| *off);
    let mut folded: Option<u64> = None;
    for (_, len, crc) in &crcs {
        folded = Some(match folded {
            None => *crc,
            Some(acc) => flint_store::crc64_combine(acc, *crc, *len),
        });
    }
    if folded != Some(expect_crc) {
        drop(sink);
        let _ = std::fs::remove_file(tmp);
        let want = flint_store::crc64_to_b64(expect_crc);
        let got = folded.map(flint_store::crc64_to_b64).unwrap_or_else(|| "nothing".into());
        if over.is_some() {
            return Ok(Ranged::Corrupt { want, got });
        }
        return Err(LeanError::State(format!(
            "manifest cites {key} at etag {etag} with CRC-64 {want}, but the {} ranges fetched \
             under that etag fold to {got} — the object is corrupt or the store returned the \
             wrong bytes; refusing to materialise {} (nothing renamed into place)",
            crcs.len(),
            target.display()
        )));
    }
    let Some((local, base)) = over else {
        sink.commit()?;
        return Ok(Ranged::Done { bytes, st: None });
    };
    // H7, for an object fetched over seconds: the licence is checked once
    // more with every range in the temp, immediately before the rename. A
    // write since is the agent's, and it stays. On the blocking pool: the
    // check may have to hash the agent's file to tell.
    let committed = tokio::task::spawn_blocking(move || sink.commit_if(&|| !local_dirty(&local, base.as_ref())))
        .await
        .map_err(|e| LeanError::State(format!("commit task for {key} did not complete: {e}")))?;
    Ok(match committed {
        Ok(Some(st)) => Ranged::Done { bytes, st: Some(st) },
        Ok(None) => Ranged::Kept,
        Err(e) => Ranged::WriteFailed(e),
    })
}

/// How `fetch_ranged` left one object.
enum Ranged {
    /// Ranging does not apply, or the store answered 404 or 412: the
    /// whole-object arm decides.
    Declined,
    /// Renamed into place. `st` is the written file's own metadata, read
    /// through its descriptor: under a while-clean licence only (a
    /// checkout stats the path, which nothing else writes yet).
    Done { bytes: u64, st: Option<std::fs::Metadata> },
    /// While clean: the agent wrote the path first, and its version stays.
    Kept,
    /// While clean: the ranges fetched under the cited etag do not fold to
    /// the cited CRC-64. Nothing was renamed into place.
    Corrupt { want: String, got: String },
    /// While clean: the tree would not take the bytes.
    WriteFailed(LeanError),
}

/// A read buffer for streaming a file of `size` bytes: the file's size,
/// at least one byte, at most 1 MiB. A fixed 1 MiB, zeroed and faulted
/// in afresh for every file, was most of the CPU of re-verifying a tree
/// of small files. The size only chunks the reads: the loop reads to the
/// end whatever the file does meanwhile.
fn read_buf_len(size: u64) -> usize {
    size.clamp(1, 1 << 20) as usize
}

/// CRC-64/NVME of a local file, in the same base64 form the manifest
/// carries. Streamed: a resumed checkout may be verifying a 20 GiB
/// workspace and must not hold it in memory.
fn local_crc64_b64(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = flint_store::Crc64Nvme::new();
    let mut buf = vec![0u8; read_buf_len(f.metadata().ok()?.len())];
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
    pub(super) struct Fetched {
        /// Took the ranged path (report-only).
        pub(super) ranged: bool,
        pub(super) path: String,
        pub(super) be: Option<BaselineEntry>,
        pub(super) skipped: bool,
        pub(super) bytes: u64,
        /// A citation this checkout refused to materialize (D0.3 /
        /// containment): left cited, surfaced as a conflict.
        pub(super) refused: Option<String>,
        /// Under `Licence::WhileClean` only: why nothing was written.
        pub(super) not_taken: Option<NotTaken>,
    }

impl Fetched {
    fn not_taken(path: &str, why: NotTaken) -> Fetched {
        Fetched {
            ranged: false,
            path: path.to_string(),
            be: None,
            skipped: true,
            bytes: 0,
            refused: None,
            not_taken: Some(why),
        }
    }
}

/// What entitles the fetcher to write an admitted path.
pub(super) enum Licence {
    /// A checkout, and a rescope's widen over the paths the tree does not
    /// hold: the path is this pass's to fill. One that already holds the
    /// cited bytes is kept (the resume); anything else there is
    /// overwritten; and a citation that cannot be served is an error,
    /// because the tree would be short of it.
    Fill,
    /// The consume and the sync: the agent is live, and the path is its
    /// own to write at any moment. The path is overwritten only while it
    /// still reads as this baseline entry (`None`: while it is absent),
    /// asked once more with the bytes in the temp, immediately before
    /// the rename (H7). Nothing is resumed, and what cannot be taken is
    /// a `NotTaken` for the caller's record: the path stays owed.
    WhileClean(Option<BaselineEntry>),
}

/// Why a path fetched under `Licence::WhileClean` was not written.
#[derive(Debug)]
pub(super) enum NotTaken {
    /// The store answered 404 or 412 for the cited handle: a newer
    /// document replaced it (and its collector took the handle). Owed
    /// again against that one.
    Missing,
    /// The bytes fetched under the cited etag do not hash to the CRC-64
    /// cited for it (`want`: the document's, else the backend's).
    Corrupt { want: String, got: String },
    /// The agent wrote the path while it was fetched: its version stays.
    Kept,
    /// The tree would not take the bytes (a directory that cannot be
    /// made, the reserve, a full disk): transient.
    WriteFailed(String),
    /// Another fetch of this pass had already failed, so this one was
    /// not started. `take_owed` returns that failure, never this.
    NotAttempted,
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

impl Syncer {
    /// Materialize an ADMITTED SET under a bounded fan-out window:
    /// each entry's guarded fetch + atomic write is independent (the
    /// 0b rig measured the sequential loop at ~1,000-2,000 files/s and
    /// 3.3 s/GiB; fan-out multiplies directly against both).
    ///
    /// Takes the admitted set rather than the manifest, so the one
    /// decision about WHICH citations get materialized lives at the call
    /// site and every caller inherits the same LPT ordering, the same
    /// in-flight byte bound, and the same D0.3 refusals. A caller
    /// that admits a subset owes its own budget arithmetic over that
    /// subset — this function refuses nothing on size.
    async fn materialize<'a>(
        &'a self,
        admission: Vec<(&'a String, &'a super::manifest::LeanEntry)>,
        sole_writer: bool,
    ) -> Vec<LeanResult<Fetched>> {
        self.fetch_all(admission.into_iter().map(|(p, e)| (p, e, Licence::Fill)).collect(), sole_writer).await
    }

    /// Take what a live tree is OWED from the document: the consume's and
    /// the sync's fetch, through the fan-out a checkout uses instead of
    /// one whole-object GET at a time. Each path comes with the baseline
    /// entry it must still read as when its bytes are ready
    /// (`Licence::WhileClean`).
    ///
    /// Keyed by path. A path that was not written says why in
    /// `Fetched::not_taken`; one that was carries the baseline entry of
    /// the inode this pass wrote. An error is the store's (anything but a
    /// 404 or 412) or the pass's own: nothing is started after the first,
    /// and what was written before it is not lost, only unrecorded (the
    /// next derive finds the bytes there and adopts them).
    pub(super) async fn take_owed<'a>(
        &'a self,
        owed: Vec<(&'a String, &'a super::manifest::LeanEntry, Option<BaselineEntry>)>,
    ) -> LeanResult<std::collections::BTreeMap<String, Fetched>> {
        let mut out = std::collections::BTreeMap::new();
        if owed.is_empty() {
            return Ok(out);
        }
        let admission = owed.into_iter().map(|(p, e, base)| (p, e, Licence::WhileClean(base))).collect();
        for r in self.fetch_all(admission, false).await {
            let f = r?;
            out.insert(f.path.clone(), f);
        }
        Ok(out)
    }

    /// `materialize` and `take_owed`: one fan-out, each entry under its
    /// own licence.
    async fn fetch_all<'a>(
        &'a self,
        mut admission: Vec<(&'a String, &'a super::manifest::LeanEntry, Licence)>,
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
        let this: &super::Syncer = self;
            let range_min = this.cfg.range_get_min_bytes;
            let range_chunk = this.cfg.range_get_chunk_bytes;
            let range_par = this.cfg.range_get_parallelism;
            let (tree_reserve, tree_room) = (this.cfg.tree_reserve, this.cfg.tree_room_for_tests);
        // SHARDED DRIVERS. Every fetch future used to be polled by ONE
        // `buffer_unordered` task on the block_on thread, so each
        // request's SDK work (build, sign, orchestrate, parse, collect,
        // checksum) ran on one thread whatever `fanout` said: the main
        // thread saturated at ~8,000 files/s on a 2-vCPU VM with half a
        // core idle. Spawning every fetch as its own task fixed that and
        // then lost to the single driver on a 6-vCPU loopback rig (7.6k
        // vs 11.4k) — with no RTT to hide behind, a wake per response
        // per task costs more than the parallelism buys.
        //
        // So: `fetch_drivers` driver tasks, each running its own
        // `buffer_unordered` over a round-robin slice of the (already
        // largest-first) admission list. A driver's fetches stay on one
        // task — one wake path, the batching the single driver had —
        // and the per-request work spreads over that many cores.
        let drivers = this.cfg.fetch_drivers.max(1);
        let per_driver = (this.cfg.fanout.max(1) / drivers).max(1);
        let mut shards: Vec<Vec<(String, super::manifest::LeanEntry, Licence)>> =
            (0..drivers).map(|_| Vec::new()).collect();
        for (i, (path, entry, licence)) in admission.into_iter().enumerate() {
            shards[i % drivers].push((path.clone(), entry.clone(), licence));
        }
        let store_all = this.store.clone();
        let root_all = this.cfg.root.clone();
        // Raised by the first fetch that fails. Read under a while-clean
        // licence only: a consume used to stop at its first failed GET,
        // and against a store that answers nothing each further fetch is
        // a timeout the barrier would wait out, 128 at a time.
        let failed_all = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut set = tokio::task::JoinSet::new();
        for shard in shards {
            let store_d = store_all.clone();
            let root_d = root_all.clone();
            let gate_d = gate.clone();
            let failed_d = failed_all.clone();
            set.spawn(async move {
        stream::iter(shard.into_iter().map(|(path, entry, licence)| {
            let failed = failed_d.clone();
            let failed_set = failed_d.clone();
            let store = store_d.clone();
            let root = root_d.clone();
            let local = root_d.join(&path);
            let gate = gate_d.clone();
            // THE CHARGE MUST DESCRIBE THE PATH THAT WILL RUN.
            //
            // `fetch_inflight_max_bytes` is a bound on BYTES HELD IN
            // MEMORY, and the two arms hold wildly different amounts.
            // `get_whole` returns the entire object, so charging
            // `entry.size` is exactly right for it. The ranged arm does
            // not: it is `buffer_unordered(range_par)` over per-chunk
            // `get_range` calls, each written at its offset as it
            // lands, so it holds `range_par * range_chunk` — 64 MiB at
            // the shipped defaults — WHATEVER the object's size.
            //
            // Charging a ranged 1 GiB fetch 1 GiB of a 512 MiB budget
            // therefore mis-states its footprint by ~16x, and the
            // `clamp` turns that into the pathology: an entry at or
            // over the budget takes the WHOLE semaphore, so object
            // concurrency collapses to ONE and `fanout` is dead code.
            // On a 6 x 1 GiB tree that leaves 4 streams on a 25 Gbps
            // link — measured at 325 MiB/s, 11% of the guarantee, and
            // identical on a node with 12x the vCPU and 32x the
            // bandwidth. The bound was never the memory it names.
            let ranged_eligible = range_min > 0 && entry.size >= range_min;
            let whole_units =
                entry.size.div_ceil(FETCH_UNIT).clamp(1, budget_units as u64) as u32;
            let ranged_units = (range_par.max(1) as u64)
                .saturating_mul(range_chunk.max(1))
                .min(entry.size)
                .div_ceil(FETCH_UNIT)
                .clamp(1, budget_units as u64) as u32;
            let want = if ranged_eligible { ranged_units } else { whole_units };
            let fetch = async move {
                // `Some` under a while-clean licence: the baseline entry
                // the path must still read as for its bytes to land.
                let over: Option<Option<BaselineEntry>> = match licence {
                    Licence::Fill => None,
                    Licence::WhileClean(base) => Some(base),
                };
                // Held until this entry's bytes have reached disk.
                let mut permit = gate
                    .clone()
                    .acquire_many_owned(want)
                    .await
                    .map_err(|_| LeanError::State("fetch budget closed".into()))?;
                if over.is_some() && failed.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(Fetched::not_taken(&path, NotTaken::NotAttempted));
                }
                // D0.3: a legacy `files/.flint/...` citation is
                // never materialized (it would collide with the
                // control files); it stays cited and a conflict
                // record names it. Same arm refuses a citation
                // whose path escapes the workspace.
                // The walk hands back the final component's lstat as
                // well, which is the resume check's whole question.
                let (target, present) = match contained_path_stat(&root, &path) {
                    Ok(t) => t,
                    // While clean, the caller has already refused what can
                    // never be written here (`check_contained`, which
                    // makes nothing): this is a directory that could not
                    // be made, and the path stays owed.
                    Err(e) if over.is_some() => {
                        return Ok(Fetched::not_taken(&path, NotTaken::WriteFailed(e.to_string())))
                    }
                    Err(e) => {
                        return Ok(Fetched {
                            ranged: false,
                            path: path.clone(),
                            be: None,
                            skipped: true,
                            bytes: 0,
                            refused: Some(e.to_string()),
                            not_taken: None,
                        })
                    }
                };
                // The resume is a checkout's: on a live tree a present
                // path is the baseline's or the agent's, and the licence
                // below is what tells them apart.
                if let Some(st) = present.filter(|_| over.is_none()) {
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
                    // the walk already took, then crc of the local
                    // file. No bucket request either way — which is
                    // why it can be unconditional rather than a knob.
                    //
                    // And the mode, first: a chmod publishes a new
                    // generation of the SAME bytes, and adopting the
                    // old one would record its mode as the tree's own.
                    let same = !super::scan::mode_differs(mode_of(&st), entry.mode)
                        && st.len() == entry.size
                        && local_crc64_b64(&local)
                            .map(|got| got == entry.crc64_b64)
                            .unwrap_or(false);
                    if same {
                        return Ok(Fetched {
                            ranged: false,
                            path: path.clone(),
                            be: Some(BaselineEntry {
                                etag: entry.etag.clone(),
                                key: Some(entry.key.clone()),
                                generation: entry.generation,
                                size: st.len(),
                                mtime_unix: mtime_of(&st),
                                mtime_nanos: Some(mtime_nanos_of(&st)),
                                mode: Some(entry.mode & super::scan::PERM_BITS),
                                racy: super::barrier::racy_stat(&st),
                                change: Some(super::scan::ChangeStamp::of(&st)),
                                crc64_b64: Some(entry.crc64_b64.clone()),
                            }),
                            skipped: true,
                            bytes: 0,
                            refused: None,
                            not_taken: None,
                        });
                    }
                    // Fall through and re-materialize.
                }
                // Before a byte is fetched: on a quota'd tree a body the
                // tenant could not have written itself would spend the
                // syncer's reserve (`safefs::leave_the_reserve`).
                if tree_reserve {
                    if let Err(e) = super::safefs::leave_the_reserve(&root, &path, entry.size, tree_room) {
                        if over.is_some() {
                            return Ok(Fetched::not_taken(&path, NotTaken::WriteFailed(e.to_string())));
                        }
                        return Err(e);
                    }
                }
                // RANGED FIRST, when the object is big enough to
                // pay for it. While clean, only when the document cites a
                // CRC-64 the fold can be held to: an entry without one is
                // verified by the whole-object arm, against the backend's.
                let cited_crc = flint_store::crc64_from_b64(&entry.crc64_b64);
                let ranged = if range_min > 0 && entry.size >= range_min && (over.is_none() || cited_crc.is_some()) {
                    let tmp = target.with_file_name(format!(
                        "{}.flint-sync-tmp",
                        target.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
                    ));
                    let want = cited_crc.ok_or_else(|| {
                        LeanError::State(format!(
                            "manifest cites {} with a CRC-64 that is not one: {:?}",
                            entry.key, entry.crc64_b64
                        ))
                    })?;
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
                        want,
                        over.clone().map(|base| (local.clone(), base)),
                    )
                    .await?
                } else {
                    Ranged::Declined
                };
                let ranged_bytes = match ranged {
                    Ranged::Declined => None,
                    Ranged::Done { bytes, st } => Some((bytes, st)),
                    Ranged::Kept => return Ok(Fetched::not_taken(&path, NotTaken::Kept)),
                    Ranged::Corrupt { want, got } => {
                        return Ok(Fetched::not_taken(&path, NotTaken::Corrupt { want, got }))
                    }
                    Ranged::WriteFailed(e) => {
                        return Ok(Fetched::not_taken(&path, NotTaken::WriteFailed(e.to_string())))
                    }
                };
                if let Some((n, st)) = ranged_bytes {
                    // While clean, the inode this fetch wrote (H7): never
                    // a fresh stat of a path the agent may have written
                    // since the rename.
                    let st = match st {
                        Some(st) => st,
                        None => std::fs::metadata(&local)?,
                    };
                    return Ok(Fetched {
                        ranged: true,
                        path: path.clone(),
                        be: Some(BaselineEntry {
                            etag: entry.etag.clone(),
                            key: Some(entry.key.clone()),
                            generation: entry.generation,
                            size: st.len(),
                            mtime_unix: mtime_of(&st),
                            mtime_nanos: Some(mtime_nanos_of(&st)),
                            mode: Some(entry.mode & super::scan::PERM_BITS),
                            racy: super::barrier::racy_stat(&st),
                            change: Some(super::scan::ChangeStamp::of(&st)),
                            // The fold above equalled it, or we would
                            // not be here.
                            crc64_b64: Some(entry.crc64_b64.clone()),
                        }),
                        skipped: false,
                        bytes: n,
                        refused: None,
                        not_taken: None,
                    });
                }

                // TOP UP BEFORE BUFFERING THE WHOLE OBJECT.
                //
                // Reaching here having been charged the ranged rate
                // means `fetch_ranged` declined — either the object is
                // one chunk (harmless: it holds less than the ranged
                // charge) or it hit the 412/404 policy fallback, which
                // hands the whole object to `get_whole` below. In that
                // second case we are about to hold `entry.size` while
                // charged for 64 MiB, and N concurrent fallbacks would
                // walk RSS straight through the bound this semaphore
                // exists to enforce.
                //
                // Released BEFORE re-acquiring, never upgraded in
                // place: holding `want` while waiting for
                // `whole_units` is a deadlock the moment two entries
                // do it at once against a full budget.
                if want < whole_units {
                    drop(permit);
                    permit = gate
                        .clone()
                        .acquire_many_owned(whole_units)
                        .await
                        .map_err(|_| LeanError::State("fetch budget closed".into()))?;
                }
                let _permit = permit;

                let (meta, body) = match store.get_whole(&entry.key, Some(&entry.etag)).await {
                        Ok(ok) => ok,
                        // While clean, the reading that accuses nobody: a
                        // newer document replaced the entry and its
                        // collector took the handle. Owed again against
                        // that one.
                        Err(StoreError::PreconditionFailed(_)) | Err(StoreError::NotFound(_)) if over.is_some() => {
                            return Ok(Fetched::not_taken(&path, NotTaken::Missing));
                        }
                        // A cited HANDLE is immutable: nothing in the
                        // protocol writes one twice (P2: a UI save goes to
                        // a fresh handle and commits), so a handle whose
                        // etag moved was written by something outside it.
                        // Refused, never adopted — adopting bytes no
                        // manifest cites was a citation-repair source
                        // (M4), and the S3-wins arm that did it served a
                        // HITL write the inbox had not re-cited yet, which
                        // no longer exists.
                        Err(StoreError::PreconditionFailed(_)) => {
                            let who = if sole_writer {
                                " This workspace is published by a SOLE WRITER — if this is \
                                 forge's legible export, look for a read-write mount over its \
                                 prefix; the export republishes only what git changed and will \
                                 not repair this on its own."
                            } else {
                                ""
                            };
                            return Err(LeanError::State(format!(
                                "manifest cites {} at an etag the object no longer carries: \
                                 something outside the protocol overwrote an immutable handle. \
                                 Refusing to adopt bytes no manifest cites.{who}",
                                entry.key
                            )));
                        }
                        Err(StoreError::NotFound(_)) => {
                            return Err(LeanError::State(format!(
                                "manifest cites {} but the object is gone — refusing a \
                                 silent hole (mixed-writer bucket?)",
                                entry.key
                            )));
                        }
                        Err(e) => return Err(e.into()),
                };
                // OFF the fan-out task, for the same reason the ranged arm
                // is: write_file_atomic is a blocking create+write+fsync+
                // rename, and running it inline here froze every other
                // in-flight fetch sharing this task. Small files ALL come
                // through this arm — 8 KiB is far below the 16 MiB
                // effective ranged threshold — which is why raising fanout
                // stopped paying above 128 with half a core sitting idle:
                // 3,200 files/s at 1.29 of 2 cores, flat from 128 to 512.
                //
                // And VERIFIED against the manifest before it is written.
                // This is the one integrity check that does not depend on
                // the backend: S3 returns a checksum header the SDK
                // validates, Ozone returns no CRC-64 at all so the SDK
                // validates nothing, and a body that is wrong under the
                // cited etag — bit-rot, a broken gateway, a cache serving
                // the wrong object — passes If-Match either way. Until
                // 2026-09-12 the manifest's CRC was compared only on the
                // RESUME path, so a corrupt fresh fetch was written, cited
                // in the baseline and read by the agent as the file.
                let expect_crc = match &over {
                    None => Some(entry.crc64_b64.clone()),
                    // While clean, as the consume always verified: the CRC
                    // the document cites, else the backend's attestation.
                    Some(_) => Some(entry.crc64_b64.clone())
                        .filter(|c| !c.is_empty())
                        .or_else(|| meta.crc64_b64.clone()),
                };
                let wrote = {
                    let over_w = over.clone();
                    let local_w = local.clone();
                    let target_w = target.clone();
                    let body_w = body.clone(); // Bytes: a refcount bump, not a copy
                    let mode_w = entry.mode;
                    let key_err = entry.key.clone();
                    let key_crc = entry.key.clone();
                    let etag_err = entry.etag.clone();
                    let rel_err = path.clone();
                    tokio::task::spawn_blocking(
                        move || -> LeanResult<Result<(std::fs::Metadata, String), NotTaken>> {
                            let got = flint_store::crc64_to_b64(flint_store::crc64_nvme(&body_w));
                            if let Some(want) = expect_crc {
                                if got != want {
                                    if over_w.is_some() {
                                        return Ok(Err(NotTaken::Corrupt { want, got }));
                                    }
                                    let who = format!("manifest cites {rel_err} ({key_crc} at etag {etag_err}) with");
                                    return Err(LeanError::State(format!(
                                        "{who} CRC-64 {want}, but the bytes fetched under that \
                                         etag hash to {got} — the object is corrupt or the store \
                                         returned the wrong bytes; refusing to materialise it \
                                         (nothing written)"
                                    )));
                                }
                            }
                            let st = match &over_w {
                                None => write_file_atomic(&target_w, &body_w, Some(mode_w))?,
                                // H7: the licence to overwrite is asked
                                // once more with the temp written,
                                // immediately before the rename. A write
                                // since is the agent's, and it stays.
                                Some(base) => {
                                    let still_clean = || !local_dirty(&local_w, base.as_ref());
                                    match write_file_atomic_if(&target_w, &body_w, Some(mode_w), &still_clean) {
                                        Ok(Some(st)) => st,
                                        Ok(None) => return Ok(Err(NotTaken::Kept)),
                                        Err(e) => return Ok(Err(NotTaken::WriteFailed(e.to_string()))),
                                    }
                                }
                            };
                            Ok(Ok((st, got)))
                        },
                    )
                    .await
                    .map_err(|e| {
                        LeanError::State(format!(
                            "write task for {key_err} did not complete: {e}"
                        ))
                    })??
                };
                let (st, got) = match wrote {
                    Ok(done) => done,
                    Err(why) => return Ok(Fetched::not_taken(&path, why)),
                };
                Ok(Fetched {
                    ranged: false,
                    path: path.clone(),
                    be: Some(BaselineEntry {
                        etag: meta.etag.clone(),
                        key: Some(entry.key.clone()),
                        generation: match &over {
                            None => entry.generation,
                            // While clean, the object's own stamp when it
                            // carries one, as the consume recorded it.
                            Some(_) => flint_store::GenerationStamps::from_meta(&meta.meta)
                                .map(|s| s.generation)
                                .unwrap_or(entry.generation),
                        },
                        size: st.len(),
                        mtime_unix: mtime_of(&st),
                        mtime_nanos: Some(mtime_nanos_of(&st)),
                        mode: Some(entry.mode & super::scan::PERM_BITS),
                        racy: super::barrier::racy_stat(&st),
                        change: Some(super::scan::ChangeStamp::of(&st)),
                        crc64_b64: Some(got),
                    }),
                    skipped: false,
                    bytes: body.len() as u64,
                    refused: None,
                    not_taken: None,
                })
            };
            async move {
                let fetched: LeanResult<Fetched> = fetch.await;
                if fetched.is_err() {
                    failed_set.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                fetched
            }
        }))
        .buffer_unordered(per_driver)
        .collect::<Vec<LeanResult<Fetched>>>()
        .await
            });
        }
        let mut out = Vec::new();
        while let Some(r) = set.join_next().await {
            match r {
                Ok(v) => out.extend(v),
                Err(e) => out.push(Err(LeanError::State(format!(
                    "fetch driver did not complete: {e}"
                )))),
            }
        }
        out
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
    /// classified absent and can never have its object DELETEd. So the
    /// DELETE sites need no scope filter: the admission filter here is
    /// that whole argument. What the tree is OWED does need one, below.
    ///
    /// The held set is what the baseline cites plus what the scope covers.
    /// A path outside both that changes REMOTELY is not owed: the consume
    /// and the sync decline it (`Prop_ScopeRespected` in `LeanP1.tla`).
    /// A file the AGENT writes outside the scope publishes like any other,
    /// and from then on the baseline holds it. `rescope` narrows or widens
    /// the set later.
    /// Name the right culprit when a fetch could not be served.
    ///
    /// Two of `materialize`'s refusals — the SOLE WRITER 412 and the
    /// cited-object-is-gone 404 — read the same
    /// evidence ("this object is not what the manifest said") and
    /// accuse a stranger of writing the bucket. There is a second
    /// explanation, and after 2026-09-11 it is the LIKELIER one:
    /// `checkout` no longer holds the epoch, so the workspace's own
    /// publisher may have published a new generation between the
    /// manifest we loaded and the object we asked for. A reader is then
    /// citing a manifest one generation stale, and every word of
    /// "something other than its publisher wrote that object" is false.
    ///
    /// One GET of the pointer tells them apart, and it is paid only on
    /// a path that is already returning an error. A pointer that has
    /// not moved leaves the original refusal exactly as written — the
    /// stranger accusation is still the right one, and softening it for
    /// every caller to buy this would be the worse trade.
    /// Has the pointer moved past the document this checkout read? One
    /// GET of the pointer; unreadable reads as "no", so nothing is
    /// retried on a store that cannot answer.
    async fn pointer_moved_past(&self, read_seq: u64) -> bool {
        match manifest::load_pointer(self.store.as_ref(), &self.cfg).await {
            Ok(Some(p)) => p.pointer.seq > read_seq,
            Ok(None) | Err(_) => false,
        }
    }

    async fn say_who_moved_it(&self, read_seq: u64, e: LeanError) -> LeanError {
        let now = match manifest::load_pointer(self.store.as_ref(), &self.cfg).await {
            Ok(Some(p)) => p.pointer.seq,
            // No pointer, or we cannot read one. Either way we have
            // learned nothing, so we must not overwrite what we know.
            Ok(None) | Err(_) => return e,
        };
        if now <= read_seq {
            return e;
        }
        LeanError::State(format!(
            "this checkout read manifest seq {read_seq} and the workspace is now at \
             seq {now}: its publisher published while the checkout was running, so \
             the citation it was still fetching is one generation stale. Re-run \
             checkout. (The underlying refusal, which names a stranger and in this \
             case means nothing of the kind: {e})"
        ))
    }

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

        let mut baseline = self.state.take_baseline()?;
        let mut present: BTreeSet<String> = BTreeSet::new();
        let mut attempt = 0u32;
        let (m, metag, results) = loop {
            attempt += 1;
            let t_start = std::time::Instant::now();
            let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?;
            report.manifest_secs = t_start.elapsed().as_secs_f64();
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

            let t_fetch = std::time::Instant::now();
            // A mirror's publisher is the only party entitled to write it,
            // so an object off its citation was moved by a stranger.
            // Adopting it would copy bytes no manifest cites into this
            // tree, silently — drill C4.
            let results = self.materialize(admission, m.sole_writer).await;
            report.fetch_secs += t_fetch.elapsed().as_secs_f64();
            // A citation that could not be served has a second explanation
            // under handles (design 2026-09-19): the workspace's own
            // publisher moved the pointer past the document this pass read
            // and its collector took the handle. Then the pass is simply
            // re-run against the new document — bounded, and cheap: every
            // file already on disk at the cited bytes is adopted, not
            // fetched again — instead of telling the caller to. A pointer
            // that has not moved leaves the refusal exactly as written.
            let moved = results.iter().any(|r| r.is_err()) && self.pointer_moved_past(m.seq).await;
            if moved && attempt < 3 {
                report.reresolved += 1;
                continue;
            }
            break (m, metag, results);
        };
        let t_commit = std::time::Instant::now();
        for r in results {
            let f = match r {
                Ok(f) => f,
                Err(e) => return Err(self.say_who_moved_it(m.seq, e).await),
            };
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
        // Checked out whole or scoped, the tree owes nothing: the baseline
        // (the merge base since P1-lite) holds what was materialised, and the
        // consume never owes a path the scope declined. So this document is
        // derived, and nothing is skipped.
        baseline.derived_etag = Some(baseline.manifest_etag.clone().unwrap_or_default());
        baseline.skipped.clear();
        baseline.prev_scan.sync_to(&present);
        // Every materialised file reaches stable storage BEFORE the
        // baseline and the marker that vouch for it: after a power loss
        // the tree is then at least as durable as its description, so
        // the next scan cannot read a zero-length survivor as a local
        // edit and publish it over the good version.
        self.state.sync_tree()?;
        // THE GATE (latency plan §17; LeanScanRacy's `Open`). Every file
        // above was stat'ed in the tick it was written, so it is racy, and
        // the first scan would hash the whole tree. But nothing writes the
        // tree before the marker below exists (the CSI node plugin binds it
        // into the pod only then), so each flag is measured now: a file
        // written a window before the gate cannot hide a write, which can
        // only come after it. What this checkout wrote in its last window
        // stays racy, and the first scan hashes only that. Measured before
        // the marker, so earlier than the gate: the safe direction. After
        // the sync above, so its time counts toward the window for free.
        super::barrier::consume_window("before-gate", "");
        let mut gate = super::scan::now_nanos();
        // When what it wrote in its last window is many files, the gate
        // opens the rest of their window later instead: at most a window on
        // the agent's start, against hashing every one of them in the first
        // scan. Nothing writes before the gate, so it may open at any tick.
        if self.cfg.racy_at_gate && self.cfg.racy_wait_min > 0 {
            let (n, newest) = baseline
                .entries
                .values()
                .filter(|e| e.racy && super::scan::racy_entry(e, gate))
                // From the later stamp, as `racy_entry` measures: a
                // materialised file's ctime is its chmod and rename, after
                // its mtime.
                .map(|e| {
                    let (sec, nanos) =
                        super::scan::later(e.mtime_unix, e.mtime_nanos.unwrap_or(999_999_999), e.change.as_ref());
                    sec as i128 * 1_000_000_000 + nanos as i128
                })
                .fold((0usize, i128::MIN), |(n, m), t| (n + 1, m.max(t)));
            if n >= self.cfg.racy_wait_min {
                // A clock that runs backwards, or an mtime in the future,
                // leaves those entries racy: the safe direction.
                let rest = (newest + super::scan::RACY_WINDOW_NANOS - gate).clamp(0, super::scan::RACY_WINDOW_NANOS);
                tokio::time::sleep(std::time::Duration::from_nanos(rest as u64 + 1_000_000)).await;
                gate = super::scan::now_nanos();
            }
        }
        let settled: Vec<String> = baseline
            .entries
            .iter()
            .filter(|_| self.cfg.racy_at_gate)
            .filter(|(_, e)| e.racy && !super::scan::racy_entry(e, gate))
            .map(|(p, _)| p.clone())
            .collect();
        for p in settled {
            if let Some(e) = baseline.entries.get_mut(&p) {
                e.racy = false;
            }
        }
        self.state.commit_baseline(baseline)?;
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
        self.write_capabilities(&posture)?;
        self.write_gauges(None)?;
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
    /// Paths whose unpublished local changes a rescope kept, each with a
    /// conflict record: on a narrow, paths that should have left the held
    /// set (KEPT, cited — a narrow may unwatch a file, never discard an
    /// edit); on a widen, admitted paths where the agent had made a file
    /// the tree does not cite yet (kept, uncited, published by the next
    /// barrier).
    pub kept_dirty: Vec<String>,
    /// This call finished a rescope a crash had left half-applied.
    pub replayed: bool,
}

impl Syncer {
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
                "these paths would leave the scope but have unpublished local changes, or the \
                 syncer may not read them: {:?} — publish or discard them, or make them readable, \
                 first",
                dirty
            )));
        }

        let intent = super::state::ScopeIntent { target, drop: leaving, held: Default::default() };
        self.state.save_scope_intent(&intent)?;
        // The model's RescopeBegin: the intent durable, nothing applied.
        self.trace("rescope", serde_json::json!({"stage": "begin", "target": intent.target, "drop": intent.drop}));
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
        let scanned = super::scan::scan_with(&self.cfg.root, self.cfg.scan_threads)?;
        let mut c = super::scan::classify(&scanned, &baseline);
        super::scan::confirm_racy(&self.cfg.root, &scanned, &baseline, &mut c);
        Ok(paths
            .iter()
            .filter(|p| {
                c.uploads.contains(*p)
                    || c.deletes.contains(*p)
                    || c.first_absence.contains(*p)
                    // Unseen: kept, as a dirty path is.
                    || scanned.holds(p)
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

        // What the uncite drops, durable with the intent before it lands. A
        // replay keeps what an earlier run recorded: those paths are no
        // longer in the baseline to read it from.
        let mut recorded = intent.clone();
        for p in &drop_set {
            if let Some(be) = baseline.entries.get(p) {
                recorded.held.insert(p.clone(), be.clone());
            }
        }
        if recorded != *intent {
            self.state.save_scope_intent(&recorded)?;
        }

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
        // The model's RescopeFirst: the uncite durable, what it kept named.
        self.trace("rescope", serde_json::json!({"stage": "uncited", "kept": dirty, "uncited": drop_set}));
        let mut unlinked: Vec<&String> = vec![];

        for p in &drop_set {
            let local = self.cfg.root.join(p);
            // The same containment the barrier demands: a citation is
            // not a licence to unlink whatever a path resolves to.
            if contained_path(&self.cfg.root, p).is_err() {
                continue;
            }
            super::barrier::consume_window("rescope-before-unlink", p);
            // Only the bytes the uncite dropped. A file that is no longer
            // them is the agent's, written since: kept, uncited, recorded;
            // the next barrier publishes it as the agent's add.
            if std::fs::symlink_metadata(&local).is_ok()
                && super::barrier::local_dirty(&local, recorded.held.get(p))
            {
                self.state.append_conflict(&super::state::ConflictRecord {
                    path: p.clone(),
                    foreign_etag: String::new(),
                    preserved_key: None,
                    kind: "rescope-kept-agent-write".into(),
                    at_unix: super::now_unix(),
                })?;
                report.kept_dirty.push(p.clone());
                continue;
            }
            match std::fs::remove_file(&local) {
                Ok(()) => {
                    report.unlinked += 1;
                    unlinked.push(p);
                    // Each unlink as it lands: a crash part-way through the
                    // loop leaves some done, and the replay does the rest.
                    self.trace("rescope", serde_json::json!({"stage": "unlink", "path": p}));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(LeanError::Io(e)),
            }
        }

        // WIDEN: admitted citations this workspace does not hold.
        let mut add: Vec<(&String, &super::manifest::LeanEntry)> =
            m.entries.iter().filter(|(p, _)| covered(p) && !baseline.entries.contains_key(*p)).collect();
        // A file already at an admitted path that this tree does not cite
        // is the agent's (made since the last barrier; the scan has not
        // published it yet). Materialize would fetch the document's
        // version over it (checkout's resume rule, which is right only
        // for its own half-written fetches). The consume calls such a path
        // dirty and leaves it; so does the widen: kept, uncited, and named
        // in a record. The next barrier publishes it as the agent's add,
        // and the merge records the version it replaces (review
        // 2026-09-29, R3). Bytes that ARE the document's are adopted as
        // before, which is what lets a crashed widen's replay converge.
        let mut kept_local: Vec<String> = Vec::new();
        add.retain(|(p, e)| {
            let local = self.cfg.root.join(p);
            let Ok(st) = std::fs::symlink_metadata(&local) else { return true };
            let same = st.is_file()
                && st.len() == e.size
                && local_crc64_b64(&local).map(|got| got == e.crc64_b64).unwrap_or(false);
            if !same {
                kept_local.push((*p).clone());
            }
            same
        });
        for p in &kept_local {
            self.state.append_conflict(&super::state::ConflictRecord {
                path: p.clone(),
                foreign_etag: m.entries[p].etag.clone(),
                preserved_key: None,
                kind: "rescope-kept-local-unpublished".into(),
                at_unix: super::now_unix(),
            })?;
        }
        report.kept_dirty.extend(kept_local);
        report.already_held =
            m.entries.keys().filter(|p| covered(p) && baseline.entries.contains_key(*p)).count();
        let mut taken: Vec<String> = vec![];
        if !add.is_empty() {
            // Review 2026-09-18, M3: the rescope holds the fence for its
            // whole run, and the widen's fetch is its long stretch. Renew
            // before it (deposed here, nothing is fetched yet and the
            // baseline above is saved: stop) and after it (best effort: the
            // rescope writes nothing to the store, so a fence lost during
            // the fetch costs a release, not a byte).
            super::lease::renew_if_due(self).await?;
            let results = self.materialize(add, m.sole_writer).await;
            let _ = super::lease::renew_if_due(self).await;
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
                    taken.push(f.path.clone());
                    baseline.entries.insert(f.path, be);
                }
            }
        }

        baseline.seq = m.seq;
        baseline.manifest_etag = metag;
        // What the held set is owed changed with it: the next consume
        // derives in full rather than trusting the cheap path's record
        // (a widened path it kept or could not take is owed from here).
        baseline.derived_etag = None;
        baseline.skipped.clear();

        self.state.sync_tree()?;
        self.state.commit_baseline(baseline)?;
        self.state.save_scope(target.as_deref())?;
        // LAST. While this document exists the workspace is mid-rescope
        // and `run_barrier` will replay before doing anything else.
        self.state.clear_scope_intent()?;
        // The model's RescopeSecond: the unlink and the widen, the new scope.
        self.trace("rescope", serde_json::json!({"stage": "applied", "unlinked": unlinked, "taken": taken,
            "target": target}));
        Ok(report)
    }
}
