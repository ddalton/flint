//! Durable emptyDir bookkeeping (plan §2.1 "restart matrix").
//!
//! Everything the barrier needs to survive a CONTAINER restart lives
//! here as small JSON files under `<root>/.flint-sync/`, each written
//! temp+rename. The baseline is the exception (lever D): a snapshot
//! written temp+rename and a journal that each save appends to in place,
//! held parsed in memory for the life of the process.
//!
//! A POD replacement gets a fresh emptyDir and therefore a fresh
//! identity — that asymmetry is deliberate (the plan's P4: the
//! incarnation id is emptyDir-scoped, so only the same pod's restarted
//! container may self-supersede the lease).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::tracked::{TrackedMap, TrackedSet};
use super::{LeanError, LeanResult};

/// One published path as this syncer last knew it: the handle it
/// integrated (the citation repair re-cites it, the merge recognises it
/// as this tree's own) and the ETag that attests its bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineEntry {
    pub etag: String,
    /// The HANDLE this tree integrated at the path — its own upload, the
    /// entry it consumed, the citation it checked out (design 2026-09-19,
    /// R1-R2). The merge counts a document citing it as citing THIS tree's
    /// version, not a foreign one. `None` only on a baseline written
    /// before handles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub generation: u64,
    pub size: u64,
    pub mtime_unix: i64,
    /// Sub-second mtime, when this entry was written by a syncer that
    /// stamps it; `None` compares as "unknown" (`scan::stat_changed`).
    #[serde(default)]
    pub mtime_nanos: Option<u32>,
    /// The mode (`scan::PERM_BITS` of it) the document cites for the
    /// version this tree integrated — its own upload's, or the one it
    /// materialised, which is the tree's whenever the write could apply
    /// it. A `chmod` moves neither size nor mtime, so the scan compares
    /// the tree's mode with this (`scan::stat_changed`); and it publishes
    /// the same bytes under the same etag, so a consume owes an entry
    /// whose cited mode is not this one (`cites_mode`). A mode the tree
    /// cannot hold (a bit the kernel refuses an unprivileged owner)
    /// scans as a change and publishes what the tree does hold, once.
    /// `None` compares as "unknown", as above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// CRC-64/NVME (wire form) of the object bytes this syncer
    /// integrated at this path — computed from the bytes it wrote or
    /// uploaded, never from a HEAD: content convergence compares it, and
    /// it is what lets the manifest carry a CRC for an object whose
    /// backend returns none (Ozone). Every writer sets it; `None` only on
    /// a baseline from before it existed.
    pub crc64_b64: Option<String>,
    /// The stat above was taken within `scan::RACY_WINDOW_NANOS` of the
    /// file's later stamp (`scan::later`), so it cannot vouch for the bytes
    /// on its own: the next scan hashes the file before calling it clean
    /// (`scan::confirm_racy`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub racy: bool,
    /// The file's ctime and inode as recorded with the stat above
    /// (`scan::ChangeStamp`): one that moved while size, mtime and mode did
    /// not makes the entry unsure, and its bytes decide. Taken after the
    /// last thing the recording write did to the file (its rename), or the
    /// first scan re-hashes it. `None` compares as unknown.
    #[serde(rename = "cs", default, skip_serializing_if = "Option::is_none")]
    pub change: Option<super::scan::ChangeStamp>,
}

impl BaselineEntry {
    /// Whether `cited`, a document entry's mode, is the one this entry
    /// integrated. With the etag, what a consume asks before it skips an
    /// entry: a `chmod` publishes the same bytes, so the same etag.
    pub fn cites_mode(&self, cited: u32) -> bool {
        self.mode.is_none_or(|m| (m ^ cited) & super::scan::PERM_BITS == 0)
    }
}

/// The persisted baseline snapshot: what this syncer believes the
/// bucket holds AND has integrated locally — and, since P1-lite
/// (2026-09-25), the three-way MERGE BASE too. A separate merge base
/// existed because a tree could adopt a UI write the document did not
/// cite yet; under P2 every UI edit commits first, so no tree holds a
/// version the document never cited, and the split had nothing left to
/// keep apart (the sandbox pair `LeanCoreP1`/`LeanCoreP2R`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Baseline {
    /// Manifest seq at our last install/checkout.
    pub seq: u64,
    /// Manifest document ETag we expect at the next CAS.
    pub manifest_etag: Option<String>,
    pub entries: TrackedMap<BaselineEntry>,
    /// THE CHEAP PATH'S RECORD (scan trigger). The pointer etag of the
    /// document the last full consume derived against, with nothing left
    /// but the agent's own work; `None` = derive at the next consume (a
    /// failed fetch, a sync, a baseline from before this field). Only the
    /// consume writes it — and the commit, when its CAS replaced exactly
    /// this document, so the new one is it plus this tree's own changes.
    #[serde(default)]
    pub derived_etag: Option<String>,
    /// The paths that derive left untaken because the agent is working on
    /// them (dirty against the baseline where the document differs). The
    /// cheap path holds only while every one is still dirty: an agent that
    /// backs out makes the document's version owed.
    #[serde(default)]
    pub skipped: TrackedSet,
    /// Paths present at the PREVIOUS scan (the two-consecutive-scans
    /// deletion rule: absence must survive two scans).
    pub prev_scan: TrackedSet,
    /// This snapshot's id, minted at each whole write: the journal
    /// (`baseline.log`) applies to the snapshot its first line names
    /// (lever D). An id rather than a checksum, so a snapshot edited in
    /// place keeps its journal, and one written whole anew does not.
    /// LAST, so the fields before it keep the order the e2e plant relies
    /// on (`run-verbs.sh`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) journal: Option<String>,
}

/// A rescope in flight (scoped-read design §4.3). `target` is the scope
/// the workspace is moving TO; `None` means the whole tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeIntent {
    pub target: Option<Vec<String>>,
    /// The paths this rescope is removing from the held set, recorded
    /// BEFORE the first mutation.
    ///
    /// Load-bearing, and the mutation check that found it: replay
    /// cannot RE-DERIVE this set. Once a path is out of
    /// `baseline.entries` it is indistinguishable from a file the agent
    /// just created — same scan row, same absence from the baseline —
    /// and the two have opposite correct answers: unlink the leftover,
    /// KEEP the agent's new file. Deriving the drop set from the
    /// baseline made a crash between the uncite and the unlink leave
    /// six uncited files on disk forever; deriving it from the tree
    /// would delete the agent's work instead.
    #[serde(default)]
    pub drop: Vec<String>,
    /// What the uncite dropped, per path, recorded BEFORE the uncite (and
    /// kept across a replay, which can no longer read it off the
    /// baseline). The unlink takes a file only while its bytes are still
    /// these: the agent may write a dropped path between the uncite and
    /// the unlink — a crash leaves that window open until the next
    /// barrier — and once uncited, its write and the leftover read alike
    /// (the model's `LeanP1UnlinkBlind`, 2026-10-02).
    #[serde(default)]
    pub held: BTreeMap<String, BaselineEntry>,
}

/// The pod-incarnation identity + lease bookkeeping ({last_token,
/// quiet_polls} persist so container restarts RESUME the takeover
/// observation instead of resetting it — plan §2.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incarnation {
    pub holder_id: String,
    pub epoch: u64,
    pub last_token: Option<String>,
    pub quiet_polls: u32,
}

/// What the conditional-write probes saw this store do
/// (`conformance.rs`). Cached because the probes cost ~10 requests and
/// a store does not change its mind between two barriers — but keyed by
/// `identity`, because a workspace re-pointed at a different endpoint or
/// bucket is a different store and must be asked again.
///
/// `probe_version` is what makes the cache safe to extend: a build that
/// asks a NEW question must not read an old verdict as the answer to
/// it, so a bump invalidates every cached verdict rather than inheriting
/// a silent pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoreConformance {
    pub identity: String,
    pub probe_version: u32,
    /// Whether the probe got an ANSWER at all. False means it could not
    /// write its own object — a credential, a policy, the network — and
    /// the two flags below say nothing. Never cached false: an outage
    /// must not become this workspace's standing verdict.
    pub answered: bool,
    /// `If-None-Match:*` and `If-Match` on PUT — the manifest CAS, the
    /// upload gate, the lease cell. Nothing works without it.
    pub conditional_put: bool,
    /// `If-Match` on DELETE — the file collector, and only it.
    pub conditional_delete: bool,
    /// What the failing probe said, for the operator message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at_unix: u64,
}

/// The intent journal: the flush id of the barrier in flight, and the H10
/// carrier. (Its `keys`, `recent_uuids` and `declared_deletes` served the
/// slot's AdoptOwn and the cell's declared removals; nothing read them
/// once both were gone — M6 of the 2026-09-24 simplification analysis.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentJournal {
    pub flush_uuid: String,
    /// What the installs that CARRIED the standing publish declaration
    /// published, journalled in the write that follows each CAS (review
    /// 2026-09-18, H10). The ack reads the barrier that honors the
    /// declaration, and that is not always the one that carried it: a
    /// restart after step 7, an ack write that failed, or an honor that
    /// failed before the floor's cadence barrier published the
    /// declaration, each left the ack to a later barrier — and when a
    /// peer had deleted a declared path in between, that barrier was a
    /// pull of the peer's document, answered `ok` while the tree still
    /// held the declared bytes. Carried forward across barriers; matched
    /// on the pending record's `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carrier: Option<Carrier>,
}

/// See `IntentJournal::carrier`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carrier {
    /// `PendingSentinel::id` of the declaration these installs carried.
    pub pending_id: String,
    /// The dirty paths they published — uploads and deletes.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub paths: BTreeSet<String>,
    /// Which of `paths` were DELETES: a document that does not cite one
    /// of those is the declaration honoured, not dropped.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deletes: BTreeSet<String>,
    /// Carried uploads the document the LAST barrier ended at does not
    /// cite with the bytes this writer published: a peer deleted them after
    /// this writer published them, or replaced them (L-137: deleted, then
    /// renamed another file onto the path). The ack's document is that
    /// one, so it says so (H10). Recomputed at every barrier's end from the
    /// document in hand; no request.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub uncited: BTreeSet<String>,
    /// The bytes each carried upload published: the `crc64_b64` of the
    /// entry its CAS wrote. What `uncited` compares the document against.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub published: BTreeMap<String, String>,
    /// Declared paths they did not carry (parked, deferred, or a delete
    /// the merge outranked) — the ack's `report.dropped` survives them.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub dropped: BTreeSet<String>,
}

/// One surfaced conflict: both versions stay recoverable (local bytes in
/// the tree, foreign bytes preserved at `preserved_key` or still current
/// under the data key). Never a silent winner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictRecord {
    pub path: String,
    pub foreign_etag: String,
    /// Where the foreign bytes were preserved (server-side copy), when
    /// the local version is about to overwrite them.
    pub preserved_key: Option<String>,
    pub kind: String, // "consume-dirty" | "upload-412-parked" | "sync-dirty" | "gc-skip"
    pub at_unix: u64,
}

pub struct SyncerState {
    dir: PathBuf,
    /// The state-directory occupancy lock (flock, held for the process
    /// lifetime). Self-recognition of the lease via the persisted
    /// incarnation id is only sound because the PREVIOUS process is
    /// gone — and this lock is what makes that true. Without it, a
    /// second flint-sync on the same workspace self-recognizes,
    /// deposes a LIVE sibling, and both write the tree concurrently
    /// (observed on the 0b rig: a diagnostic re-run raced a live 1M
    /// checkout into tmp-rename ENOENT collisions). The hub has the
    /// identical gate (`state_backend::is_single_occupant`).
    _lock: std::fs::File,
    /// The baseline, parsed once per process (lever D).
    baseline: std::sync::Mutex<BaselineStore>,
}

const MARKER: &str = "checkout-complete";
/// Written by the SIGTERM drain only after it published: the node
/// plugin's evidence that a worker that is now gone actually drained
/// (audit 2026-09-03, finding 3). Absent ⇒ the tree is preserved.
pub const DRAINED: &str = "drained.json";
const LOCK: &str = "lock";
const BASELINE: &str = "baseline.json";
/// The admitted set of a SCOPED checkout. Deliberately its own
/// document rather than a field on `Baseline`: the baseline is
/// rewritten by every barrier, every sync and every consume, and a
/// scope that rides along is one `Baseline::default()` away from
/// silently widening a workspace to the whole tree. This file is
/// written once, by checkout, and thereafter only read. Absent means
/// UNSCOPED — the only encoding of "no restriction", since an empty
/// admitted set is refused before it can be written.
const SCOPE: &str = "scope.json";

/// A rescope IN FLIGHT: the target scope, written before the first
/// mutation and removed only after the last (scoped-read design §4.3).
///
/// Its own document and not a field on `scope.json`, because the two
/// answer different questions — `scope.json` is what this workspace
/// holds, this is what it is on its way to holding — and a crash must
/// leave both answers readable.
///
/// `null` is a LEGAL target (the whole tree), so absence of the file
/// means "no rescope in flight", never "unscoped". That is why the
/// document wraps the target in a struct instead of being a bare
/// `Option<Vec<String>>`: a bare `null` on disk and a missing file
/// would deserialize to the same thing.
const SCOPE_INTENT: &str = "scope-intent.json";
const INCARNATION: &str = "incarnation.json";
const INTENT: &str = "intent.json";
/// When this writer last swept for untracked uploads (`untracked.rs`).
const UNTRACKED_SWEEP: &str = "untracked-sweep";
const ORPHAN_SWEEP: &str = "orphan-sweep";
/// When this writer's commit section last ran the collection pass
/// (`LeanConfig::collect_every_secs`, latency plan lever C).
const COLLECT_AT: &str = "collect-at";
/// The retire logs this writer has read (`untracked.rs`): immutable, so each
/// is fetched once.
const RETIRE_CACHE: &str = "retire-cache.json";
/// The cached conditional-write verdict for this store (`conformance.rs`).
const CONFORMANCE: &str = "conformance.json";
const CONFLICTS: &str = "conflicts.jsonl";
/// The rotated generation. `load_conflicts` reads it FIRST, so the
/// sequence a reader sees is unbroken across a rotation — which matters
/// because `honor_sync` takes the count before a sync and `skip`s it
/// after, and a rotation that SHORTENED the list would make it skip past
/// the very records the sync just produced.
const CONFLICTS_PREV: &str = "conflicts.1.jsonl";
/// How many records were dropped when a SECOND rotation overwrote the
/// first. Truncation that nobody can count is truncation that reads as
/// "there were no more".
const CONFLICTS_DROPPED: &str = "conflicts.dropped";
/// Rotate past this. The file is append-only and `load_conflicts` parses
/// it WHOLE — twice per sync honor, again per status read and per
/// scrape (U23) — so an unbounded file is an unbounded parse on a hot
/// path, and a standing condition (a UI autosaving over a path the agent
/// holds dirty) appends on every barrier. Two generations bound the
/// resident set at ~2 MiB, which is thousands of records.
const CONFLICTS_MAX_BYTES: u64 = 1 << 20;

fn write_atomic(path: &Path, bytes: &[u8]) -> LeanResult<()> {
    // The state dir lives in the same app-writable emptyDir.
    super::safefs::check_parent(path)?;
    let tmp = path.with_extension("tmp");
    super::safefs::write_via_tmp(path, &tmp, bytes, None).map(|_| ())
}

/// The journal beside `baseline.json`: what each save changed since the
/// snapshot it names (lever D).
const BASELINE_LOG: &str = "baseline.log";

/// A journal longer than this share of its snapshot is folded into a new
/// snapshot at the next save: a reload never replays more than a quarter
/// of the document, and a full write is paid once per that many bytes of
/// change rather than at every save.
const COMPACT_SHARE: u64 = 4;
/// ...and never for a journal shorter than this.
const COMPACT_MIN_BYTES: u64 = 256 * 1024;

/// The journal's first line: the snapshot it applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JournalHead {
    snapshot: String,
}

/// One save: its scalars, and the members it changed.
#[derive(Debug, Default, Serialize, Deserialize)]
struct JournalRecord {
    seq: u64,
    #[serde(default)]
    manifest_etag: Option<String>,
    #[serde(default)]
    derived_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    put: Vec<(String, BaselineEntry)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    del: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    skipped_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    skipped_del: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scan_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scan_del: Vec<String>,
}

impl JournalRecord {
    /// What `b` changed since its lineage, draining what it recorded.
    fn drain(b: &mut Baseline) -> JournalRecord {
        let mut r = JournalRecord {
            seq: b.seq,
            manifest_etag: b.manifest_etag.clone(),
            derived_etag: b.derived_etag.clone(),
            ..Default::default()
        };
        for k in b.entries.take_touched() {
            match b.entries.get(&k) {
                Some(e) => r.put.push((k, e.clone())),
                None => r.del.push(k),
            }
        }
        for k in b.skipped.take_touched() {
            match b.skipped.contains(&k) {
                true => r.skipped_add.push(k),
                false => r.skipped_del.push(k),
            }
        }
        for k in b.prev_scan.take_touched() {
            match b.prev_scan.contains(&k) {
                true => r.scan_add.push(k),
                false => r.scan_del.push(k),
            }
        }
        r
    }

    fn changes_members(&self) -> bool {
        !(self.put.is_empty()
            && self.del.is_empty()
            && self.skipped_add.is_empty()
            && self.skipped_del.is_empty()
            && self.scan_add.is_empty()
            && self.scan_del.is_empty())
    }

    fn apply(self, b: &mut Baseline) {
        b.seq = self.seq;
        b.manifest_etag = self.manifest_etag;
        b.derived_etag = self.derived_etag;
        for (k, e) in self.put {
            b.entries.apply_put(k, e);
        }
        for k in &self.del {
            b.entries.apply_del(k);
        }
        for k in self.skipped_add {
            b.skipped.apply_add(k);
        }
        for k in &self.skipped_del {
            b.skipped.apply_del(k);
        }
        for k in self.scan_add {
            b.prev_scan.apply_add(k);
        }
        for k in &self.scan_del {
            b.prev_scan.apply_del(k);
        }
    }
}

/// `<crc64 hex> <json>\n`. serde_json never writes a raw newline, so a
/// line is one record, and a line without its newline is a torn one.
fn journal_line(json: &[u8]) -> Vec<u8> {
    let mut l = format!("{:016x} ", flint_store::crc64_nvme(json)).into_bytes();
    l.extend_from_slice(json);
    l.push(b'\n');
    l
}

/// The JSON of one verified line, or `None` (torn, or not this format).
fn journal_payload(line: &[u8]) -> Option<&[u8]> {
    if line.len() < 18 || line[16] != b' ' { return None }
    let crc = u64::from_str_radix(std::str::from_utf8(&line[..16]).ok()?, 16).ok()?;
    let json = &line[17..];
    (flint_store::crc64_nvme(json) == crc).then_some(json)
}

/// The scalars a save writes every time: a save that changed none of
/// them and no member writes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Scalars {
    seq: u64,
    manifest_etag: Option<String>,
    derived_etag: Option<String>,
}

impl Scalars {
    fn of(b: &Baseline) -> Scalars {
        Scalars { seq: b.seq, manifest_etag: b.manifest_etag.clone(), derived_etag: b.derived_etag.clone() }
    }
}

/// The baseline as the disk holds it, parsed once per process. The state
/// directory's flock makes this process its only writer, so nothing but
/// this process's own saves can move the disk out from under it.
#[derive(Default)]
struct BaselineStore {
    /// The disk's state, unless a caller holds it (`take_baseline`).
    cache: Option<Baseline>,
    /// The lineage the disk holds; 0 = not loaded yet.
    lineage: u64,
    next_lineage: u64,
    /// The snapshot's length on disk; `None` = no snapshot.
    snapshot: Option<u64>,
    /// Verified bytes of `baseline.log`, head included; 0 = no journal
    /// that belongs to the snapshot.
    log_len: u64,
    scalars: Scalars,
    /// Whole-snapshot parses this process made (`tests_baseline`).
    parses: u64,
    #[cfg(test)]
    compact_min_bytes: Option<u64>,
}

impl BaselineStore {
    fn next(&mut self) -> u64 {
        self.next_lineage += 1;
        self.next_lineage
    }
}

fn stamp(b: &mut Baseline, lineage: u64) {
    b.entries.stamp(lineage);
    b.skipped.stamp(lineage);
    b.prev_scan.stamp(lineage);
}

impl SyncerState {
    fn baseline_store(&self) -> std::sync::MutexGuard<'_, BaselineStore> {
        // A panic while holding it leaves the cache as it was: the disk
        // is the truth either way.
        self.baseline.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Parse the disk's baseline into the store if no copy is there.
    fn fill(&self, st: &mut BaselineStore) -> LeanResult<()> {
        if st.cache.is_some() {
            return Ok(());
        }
        let disk = read_baseline_files(&self.dir, true)?;
        st.parses += disk.snapshot_len.is_some() as u64;
        let mut b = disk.baseline;
        let lineage = st.next();
        stamp(&mut b, lineage);
        st.lineage = lineage;
        st.snapshot = disk.snapshot_len;
        st.log_len = disk.log_len;
        st.scalars = Scalars::of(&b);
        st.cache = Some(b);
        Ok(())
    }

    /// Write `b` to disk: what it changed, as one journal line, when it
    /// is the disk's own lineage and the journal has room; else the whole
    /// document, as before D, with a fresh journal. On success `b` is the
    /// disk's state and of its new lineage.
    fn persist(&self, st: &mut BaselineStore, b: &mut Baseline) -> LeanResult<()> {
        let ours = st.lineage != 0
            && b.entries.lineage() == st.lineage
            && b.skipped.lineage() == st.lineage
            && b.prev_scan.lineage() == st.lineage;
        let room = match st.snapshot {
            Some(len) if st.log_len > 0 => {
                #[cfg(test)]
                let min = st.compact_min_bytes.unwrap_or(COMPACT_MIN_BYTES);
                #[cfg(not(test))]
                let min = COMPACT_MIN_BYTES;
                st.log_len < (len / COMPACT_SHARE).max(min)
            }
            _ => false,
        };
        // A save that changed much of the tree (a checkout, a rescope) is a
        // new snapshot, not a journal line as long as one.
        let changed = b.entries.touched_len() + b.skipped.touched_len() + b.prev_scan.touched_len();
        let small = changed <= (b.entries.len() / 8).max(4096);
        if ours && room && small {
            let rec = JournalRecord::drain(b);
            if !rec.changes_members() && Scalars::of(b) == st.scalars {
                return Ok(());
            }
            let json = serde_json::to_vec(&rec).map_err(|e| LeanError::State(format!("baseline journal: {e}")))?;
            let line = journal_line(&json);
            let path = self.dir.join(BASELINE_LOG);
            super::safefs::check_parent(&path)?;
            // Written at the verified length, not appended: a write that
            // failed half-way left bytes past it, and this one replaces
            // them rather than following them.
            use std::io::{Seek, SeekFrom, Write};
            let mut f = fs::OpenOptions::new().write(true).open(&path)?;
            f.seek(SeekFrom::Start(st.log_len))?;
            f.write_all(&line)?;
            let len = st.log_len + line.len() as u64;
            f.set_len(len)?;
            f.sync_data()?;
            st.log_len = len;
        } else {
            // Written whole: what it recorded is in the document.
            b.entries.take_touched();
            b.skipped.take_touched();
            b.prev_scan.take_touched();
            let id = uuid::Uuid::new_v4().to_string();
            b.journal = Some(id.clone());
            // COMPACT JSON: the snapshot is the O(files) document.
            let bytes = serde_json::to_vec(&*b).map_err(|e| LeanError::State(format!("baseline: {e}")))?;
            write_atomic(&self.dir.join(BASELINE), &bytes)?;
            // The snapshot first, then its journal: a death between the two
            // leaves the old journal naming the old snapshot's id, which the
            // next load ignores.
            let json = serde_json::to_vec(&JournalHead { snapshot: id })
                .map_err(|e| LeanError::State(format!("baseline journal: {e}")))?;
            let line = journal_line(&json);
            write_atomic(&self.dir.join(BASELINE_LOG), &line)?;
            // Held as the bytes just written parse, not as the map that was
            // written: a map a checkout built by inserting in arrival order is
            // scattered across the heap, and every later walk of it, and of
            // the documents allocated around it, pays for that (measured at
            // 200k files: `merge` 225-235 -> 167-168 ms and `retire_set`
            // 181-184 -> 100-104 ms per publish). A parse allocates in key
            // order. A whole write is rare (a checkout, a compaction, a large
            // change): one parse each. The written map is freed first, so
            // the two are never held at once.
            *b = Baseline::default();
            *b = serde_json::from_slice(&bytes).map_err(|e| LeanError::State(format!("baseline: {e}")))?;
            st.snapshot = Some(bytes.len() as u64);
            st.log_len = line.len() as u64;
        }
        // Every copy made before this save is now stale: a later save of
        // one writes it whole, as every save did before D.
        let lineage = st.next();
        stamp(b, lineage);
        st.lineage = lineage;
        st.scalars = Scalars::of(b);
        Ok(())
    }

    /// A copy of the baseline. O(entries): for reads off the hot path and
    /// the battery. The hot paths use `take_baseline` / `with_baseline`.
    pub fn load_baseline(&self) -> LeanResult<Baseline> {
        let mut st = self.baseline_store();
        self.fill(&mut st)?;
        Ok(st.cache.clone().expect("filled above"))
    }

    /// Read the baseline in place.
    pub fn with_baseline<R>(&self, f: impl FnOnce(&Baseline) -> R) -> LeanResult<R> {
        let mut st = self.baseline_store();
        self.fill(&mut st)?;
        Ok(f(st.cache.as_ref().expect("filled above")))
    }

    /// The baseline itself, to change and hand back with
    /// `commit_baseline`. Until then the store holds none: a caller that
    /// never hands it back costs the next load one parse, and changes
    /// nothing on disk.
    pub fn take_baseline(&self) -> LeanResult<Baseline> {
        let mut st = self.baseline_store();
        self.fill(&mut st)?;
        Ok(st.cache.take().expect("filled above"))
    }

    /// Persist a baseline taken with `take_baseline` and keep it. A
    /// baseline that changed nothing writes nothing.
    pub fn commit_baseline(&self, mut b: Baseline) -> LeanResult<()> {
        let mut st = self.baseline_store();
        if let Err(e) = self.persist(&mut st, &mut b) {
            // What the disk holds is no longer known here (a snapshot
            // renamed before its journal failed, say): forget it, and the
            // next load reads the disk.
            st.cache = None;
            st.lineage = 0;
            st.snapshot = None;
            st.log_len = 0;
            return Err(e);
        }
        st.cache = Some(b);
        Ok(())
    }

    /// Persist a copy, keeping one (O(entries)).
    pub fn save_baseline(&self, b: &Baseline) -> LeanResult<()> {
        self.commit_baseline(b.clone())
    }

    /// Whole-snapshot parses this process has made: one at the first
    /// load, and then one after a save failed or a taken baseline was not
    /// handed back.
    pub fn baseline_parses(&self) -> u64 {
        self.baseline_store().parses
    }

    /// Drop the held baseline, as a new process would start without one.
    #[cfg(test)]
    pub(crate) fn forget_baseline(&self) {
        let mut st = self.baseline_store();
        st.cache = None;
        st.lineage = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_baseline_compact_min(&self, bytes: u64) {
        self.baseline_store().compact_min_bytes = Some(bytes);
    }
}

/// The baseline as the disk holds it: the snapshot, and the journal lines
/// that belong to it.
struct DiskBaseline {
    baseline: Baseline,
    /// The snapshot's length; `None` = there is none.
    snapshot_len: Option<u64>,
    /// Verified bytes of the journal; 0 = none that belongs.
    log_len: u64,
}

/// Read `dir`'s baseline. `owner` (the process holding the state
/// directory's flock) also cuts a torn last line off, so its next save
/// follows the last good one. Any other reader leaves the file alone: the
/// line it found torn may be the owner's write in progress.
fn read_baseline_files(dir: &Path, owner: bool) -> LeanResult<DiskBaseline> {
    // NotFound alone means absent: `exists()` answered false for EACCES
    // too, and an unreadable baseline read as an empty one is the
    // "unreadable means deleted" class (`load_scope`).
    match fs::read(dir.join(BASELINE)) {
        Ok(bytes) => {
            let b = serde_json::from_slice(&bytes).map_err(|e| LeanError::State(format!("baseline: {e}")))?;
            replay_onto(dir, b, bytes.len() as u64, owner)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(DiskBaseline { baseline: Baseline::default(), snapshot_len: None, log_len: 0 })
        }
        Err(e) => Err(LeanError::State(format!("read {BASELINE}: {e}"))),
    }
}

fn replay_onto(dir: &Path, mut b: Baseline, snapshot_len: u64, owner: bool) -> LeanResult<DiskBaseline> {
    let none = |b| Ok(DiskBaseline { baseline: b, snapshot_len: Some(snapshot_len), log_len: 0 });
    let Some(id) = b.journal.clone() else { return none(b) };
    let path = dir.join(BASELINE_LOG);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return none(b),
        Err(e) => return Err(LeanError::State(format!("read {BASELINE_LOG}: {e}"))),
    };
    let mut lines = bytes.split_inclusive(|c| *c == b'\n');
    let Some(first) = lines.next().filter(|l| l.ends_with(b"\n")) else { return none(b) };
    let belongs = journal_payload(&first[..first.len() - 1])
        .and_then(|j| serde_json::from_slice::<JournalHead>(j).ok())
        .is_some_and(|h| h.snapshot == id);
    if !belongs {
        // Written for another snapshot: a whole write that landed its new
        // snapshot and died before its fresh journal. Everything in this
        // one is in that snapshot. The next save replaces it.
        return none(b);
    }
    let mut good = first.len() as u64;
    for line in lines {
        let rec = line
            .strip_suffix(b"\n")
            .and_then(journal_payload)
            .and_then(|j| serde_json::from_slice::<JournalRecord>(j).ok());
        match rec {
            Some(r) => {
                r.apply(&mut b);
                good += line.len() as u64;
            }
            None => break,
        }
    }
    if owner && good < bytes.len() as u64 {
        let f = fs::OpenOptions::new().write(true).open(&path)?;
        f.set_len(good)?;
        f.sync_all()?;
    }
    Ok(DiskBaseline { baseline: b, snapshot_len: Some(snapshot_len), log_len: good })
}

/// The baseline of a state directory this process does not hold: what
/// its owner's next load would read, never cutting anything. For
/// `status`, which runs beside a live syncer.
pub fn read_baseline_at(dir: &Path) -> LeanResult<Baseline> {
    Ok(read_baseline_files(dir, false)?.baseline)
}

impl SyncerState {
    pub fn open(dir: PathBuf) -> LeanResult<SyncerState> {
        fs::create_dir_all(&dir)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock on an owned, open fd.
            let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                return Err(LeanError::State(format!(
                    "another flint-sync already holds this workspace ({}): \
                     refusing to run two syncers over one tree",
                    dir.display()
                )));
            }
        }
        Ok(SyncerState { dir, _lock: lock, baseline: Default::default() })
    }

    pub fn marker_present(&self) -> bool {
        self.dir.join(MARKER).exists()
    }

    /// Flush the filesystem holding the tree (the state dir lives in
    /// it) so everything materialised so far is on stable storage.
    pub fn sync_tree(&self) -> LeanResult<()> {
        super::safefs::sync_tree(&self.dir)
    }

    /// The drain's attestation. `seq` is the manifest the drain left
    /// installed, when it knows it.
    pub fn write_drained(&self, seq: Option<u64>, acks: usize) -> LeanResult<()> {
        let doc = serde_json::json!({
            "unix": super::now_unix(),
            "seq": seq,
            "acks": acks,
        });
        write_atomic(&self.dir.join(DRAINED), doc.to_string().as_bytes())
    }

    /// A fresh incarnation owes its own attestation; a stale one from
    /// an earlier life of this tree must not vouch for it.
    pub fn clear_drained(&self) -> LeanResult<()> {
        match fs::remove_file(self.dir.join(DRAINED)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(LeanError::State(format!("clear {DRAINED}: {e}"))),
        }
    }

    pub fn drained_path(&self) -> PathBuf {
        self.dir.join(DRAINED)
    }

    /// Written LAST at checkout: the agent-start gate.
    pub fn write_marker(&self) -> LeanResult<()> {
        write_atomic(&self.dir.join(MARKER), b"ok\n")
    }

    /// What this workspace was admitted to hold, or `None` for an
    /// unscoped (whole-manifest) checkout.
    ///
    /// `fs::read` directly, with NO `exists()` pre-check: `exists()`
    /// answers false for EACCES and EIO alike, and an unreadable scope
    /// answered as "unscoped" is the same class of bug as an unreadable
    /// path answered as "deleted" (`barrier.rs`, 14b3637c). Only
    /// NotFound means absent.
    pub fn load_scope(&self) -> LeanResult<Option<Vec<String>>> {
        let p = self.dir.join(SCOPE);
        let bytes = match fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(LeanError::State(format!(
                    "cannot read the checkout scope at {}: {e} — refusing to \
                     guess what this workspace is holding",
                    p.display()
                )))
            }
        };
        serde_json::from_slice(&bytes).map_err(|e| LeanError::State(format!("scope: {e}")))
    }

    /// `Some` writes the admitted set; `None` REMOVES the document.
    /// Removing matters: a scoped checkout that crashed before its
    /// marker leaves a scope behind, and the whole-tree checkout that
    /// replaces it must not inherit a claim it did not make.
    pub fn save_scope(&self, scope: Option<&[String]>) -> LeanResult<()> {
        let p = self.dir.join(SCOPE);
        match scope {
            Some(entries) => {
                let bytes = serde_json::to_vec(entries)
                    .map_err(|e| LeanError::State(format!("scope: {e}")))?;
                write_atomic(&p, &bytes)
            }
            None => match fs::remove_file(&p) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(LeanError::State(format!("clear {SCOPE}: {e}"))),
            },
        }
    }

    /// Read a rescope in flight. `None` = none in flight.
    pub fn load_scope_intent(&self) -> LeanResult<Option<ScopeIntent>> {
        let p = self.dir.join(SCOPE_INTENT);
        let bytes = match fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(LeanError::State(format!(
                    "cannot read the rescope intent at {}: {e} — refusing to run a barrier \
                     over a half-applied scope",
                    p.display()
                )))
            }
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| LeanError::State(format!("scope intent: {e}")))
    }

    pub fn save_scope_intent(&self, intent: &ScopeIntent) -> LeanResult<()> {
        let bytes = serde_json::to_vec(intent)
            .map_err(|e| LeanError::State(format!("scope intent: {e}")))?;
        write_atomic(&self.dir.join(SCOPE_INTENT), &bytes)
    }

    /// Cleared LAST, after the new scope is durable. Clearing it first
    /// would turn a crash into a workspace whose scope says one thing
    /// and whose held set says another, with nothing left to replay.
    pub fn clear_scope_intent(&self) -> LeanResult<()> {
        match fs::remove_file(self.dir.join(SCOPE_INTENT)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(LeanError::State(format!("clear {SCOPE_INTENT}: {e}"))),
        }
    }

    pub fn load_incarnation(&self) -> LeanResult<Option<Incarnation>> {
        let p = self.dir.join(INCARNATION);
        if !p.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&p)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| LeanError::State(format!("incarnation: {e}")))
    }

    pub fn save_incarnation(&self, i: &Incarnation) -> LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(i)
            .map_err(|e| LeanError::State(format!("incarnation: {e}")))?;
        write_atomic(&self.dir.join(INCARNATION), &bytes)
    }

    /// Unix seconds of this writer's last untracked-upload sweep; 0 when it
    /// has never swept.
    pub fn load_untracked_sweep_at(&self) -> LeanResult<u64> {
        match fs::read_to_string(self.dir.join(UNTRACKED_SWEEP)) {
            Ok(s) => Ok(s.trim().parse().unwrap_or(0)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_untracked_sweep_at(&self, at: u64) -> LeanResult<()> {
        write_atomic(&self.dir.join(UNTRACKED_SWEEP), at.to_string().as_bytes())
    }

    /// The orphan sweep's clock (`untracked.rs`, `sweep_orphans_if_due`):
    /// 0 = never swept, and the first commit starts it.
    pub fn load_orphan_sweep_at(&self) -> LeanResult<u64> {
        match fs::read_to_string(self.dir.join(ORPHAN_SWEEP)) {
            Ok(s) => Ok(s.trim().parse().unwrap_or(0)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_orphan_sweep_at(&self, at: u64) -> LeanResult<()> {
        write_atomic(&self.dir.join(ORPHAN_SWEEP), at.to_string().as_bytes())
    }

    /// The last collection pass, unix seconds; 0 = never (or unreadable,
    /// which runs the pass: the safe direction is collecting).
    pub fn load_collect_at(&self) -> u64 {
        fs::read_to_string(self.dir.join(COLLECT_AT)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    pub fn save_collect_at(&self, at: u64) -> LeanResult<()> {
        write_atomic(&self.dir.join(COLLECT_AT), at.to_string().as_bytes())
    }

    /// The retire logs read so far, by key. A cache: unreadable reads as
    /// empty (the logs are fetched again), never as an error.
    pub fn load_retire_cache(&self) -> BTreeMap<String, super::manifest::RetireLog> {
        fs::read(self.dir.join(RETIRE_CACHE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_retire_cache(&self, c: &BTreeMap<String, super::manifest::RetireLog>) -> LeanResult<()> {
        let bytes = serde_json::to_vec(c).map_err(|e| LeanError::State(format!("retire cache: {e}")))?;
        write_atomic(&self.dir.join(RETIRE_CACHE), &bytes)
    }

    /// A verdict that cannot be parsed reads as ABSENT, never as a pass:
    /// the probe runs again. The expensive outcome of a corrupt cache is
    /// ten requests; the cheap one would be running the collector on a
    /// store nobody asked.
    pub fn load_conformance(&self) -> LeanResult<Option<StoreConformance>> {
        match fs::read(self.dir.join(CONFORMANCE)) {
            Ok(b) => Ok(serde_json::from_slice(&b).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_conformance(&self, c: &StoreConformance) -> LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(c)
            .map_err(|e| LeanError::State(format!("conformance: {e}")))?;
        write_atomic(&self.dir.join(CONFORMANCE), &bytes)
    }

    pub fn load_intent(&self) -> LeanResult<IntentJournal> {
        let p = self.dir.join(INTENT);
        if !p.exists() {
            return Ok(IntentJournal::default());
        }
        let bytes = fs::read(&p)?;
        serde_json::from_slice(&bytes).map_err(|e| LeanError::State(format!("intent: {e}")))
    }

    pub fn save_intent(&self, j: &IntentJournal) -> LeanResult<()> {
        let bytes =
            serde_json::to_vec_pretty(j).map_err(|e| LeanError::State(format!("intent: {e}")))?;
        write_atomic(&self.dir.join(INTENT), &bytes)
    }

    /// The barrier is over: no flush in flight. The carrier stays.
    pub fn clear_intent_keys(&self) -> LeanResult<()> {
        let mut j = self.load_intent()?;
        j.flush_uuid = String::new();
        self.save_intent(&j)
    }

    pub fn append_conflict(&self, c: &ConflictRecord) -> LeanResult<()> {
        use std::io::Write;
        let line =
            serde_json::to_string(c).map_err(|e| LeanError::State(format!("conflict: {e}")))?;
        let live = self.dir.join(CONFLICTS);
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&live)?;
        writeln!(f, "{line}")?;
        // Size off the handle we just wrote through — no extra stat of
        // the path, and no line count, which would make every append
        // O(records).
        let len = f.metadata()?.len();
        drop(f);
        if len > CONFLICTS_MAX_BYTES {
            self.rotate_conflicts()?;
        }
        Ok(())
    }

    /// Rotate live -> `.1`, counting whatever the previous `.1` held so
    /// the loss is a NUMBER rather than a silence. Best effort: a
    /// rotation that fails leaves a large file, which is a performance
    /// problem; losing the record we just appended would be a
    /// correctness one.
    fn rotate_conflicts(&self) -> LeanResult<()> {
        let live = self.dir.join(CONFLICTS);
        let prev = self.dir.join(CONFLICTS_PREV);
        // Only read the outgoing generation at ROTATION time — rare by
        // construction, since it takes CONFLICTS_MAX_BYTES to get here.
        if let Ok(text) = fs::read_to_string(&prev) {
            let dropped = text.lines().filter(|l| !l.trim().is_empty()).count() as u64;
            let total = self.conflicts_dropped()? + dropped;
            write_atomic(&self.dir.join(CONFLICTS_DROPPED), total.to_string().as_bytes())?;
        }
        fs::rename(&live, &prev)?;
        Ok(())
    }

    /// Records lost to rotation. A caller rendering conflicts owes the
    /// user this number: a truncated list that does not say it was
    /// truncated reads as a complete one.
    pub fn conflicts_dropped(&self) -> LeanResult<u64> {
        match fs::read_to_string(self.dir.join(CONFLICTS_DROPPED)) {
            Ok(s) => Ok(s.trim().parse().unwrap_or(0)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(LeanError::State(format!("{CONFLICTS_DROPPED}: {e}"))),
        }
    }

    /// Every record ever appended: the ones rotation dropped plus the
    /// ones held. Unlike the held count it never goes down, so a reader
    /// that takes it before an operation knows exactly how many records
    /// the operation produced, across any rotation (`conflicts_since`).
    pub fn conflicts_appended(&self) -> LeanResult<u64> {
        Ok(self.conflicts_dropped()? + self.load_conflicts()?.len() as u64)
    }

    /// The rotated generation first, then the live one: oldest to
    /// newest, unbroken across a single rotation.
    pub fn load_conflicts(&self) -> LeanResult<Vec<ConflictRecord>> {
        let mut out = vec![];
        for name in [CONFLICTS_PREV, CONFLICTS] {
            let p = self.dir.join(name);
            // `fs::read_to_string` directly: `exists()` answers false
            // for EACCES and EIO alike, and a conflict log that reads as
            // EMPTY because it is unreadable is the same shape of bug as
            // an unreadable path reading as deleted.
            let text = match fs::read_to_string(&p) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(LeanError::State(format!(
                        "cannot read {name}: {e} — refusing to report an unreadable \
                         conflict log as an empty one"
                    )))
                }
            };
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                out.push(
                    serde_json::from_str(line)
                        .map_err(|e| LeanError::State(format!("conflict line: {e}")))?,
                );
            }
        }
        Ok(out)
    }
}
