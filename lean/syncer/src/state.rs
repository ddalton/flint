//! Durable emptyDir bookkeeping (plan §2.1 "restart matrix").
//!
//! Everything the barrier needs to survive a CONTAINER restart lives
//! here as small JSON files under `<root>/.flint-sync/`, each written
//! temp+rename. A POD replacement gets a fresh emptyDir and therefore a
//! fresh identity — that asymmetry is deliberate (the plan's P4: the
//! incarnation id is emptyDir-scoped, so only the same pod's restarted
//! container may self-supersede the lease).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// CRC-64/NVME (wire form) of the object bytes this syncer
    /// integrated at this path — computed from the bytes it wrote or
    /// uploaded, never from a HEAD: content convergence compares it, and
    /// it is what lets the manifest carry a CRC for an object whose
    /// backend returns none (Ozone). Every writer sets it; `None` only on
    /// a baseline from before it existed.
    pub crc64_b64: Option<String>,
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
    pub entries: BTreeMap<String, BaselineEntry>,
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
    pub skipped: BTreeSet<String>,
    /// Paths present at the PREVIOUS scan (the two-consecutive-scans
    /// deletion rule: absence must survive two scans).
    pub prev_scan: BTreeSet<String>,
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
    /// cite: a peer deleted them after this writer published them. The
    /// ack's document is that one, so it says so (H10). Recomputed at
    /// every barrier's end from the document in hand; no request.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub uncited: BTreeSet<String>,
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
        Ok(SyncerState { dir, _lock: lock })
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

    pub fn load_baseline(&self) -> LeanResult<Baseline> {
        let p = self.dir.join(BASELINE);
        if !p.exists() {
            return Ok(Baseline::default());
        }
        let bytes = fs::read(&p)?;
        serde_json::from_slice(&bytes).map_err(|e| LeanError::State(format!("baseline: {e}")))
    }

    /// COMPACT: the baseline is the other O(files) document, rewritten
    /// on the cadence tick and re-parsed several times per barrier. The
    /// agent-facing O(1) files (gauges, capabilities, acks) stay pretty
    /// — those exist to be `cat`-ed.
    pub fn save_baseline(&self, b: &Baseline) -> LeanResult<()> {
        let bytes =
            serde_json::to_vec(b).map_err(|e| LeanError::State(format!("baseline: {e}")))?;
        write_atomic(&self.dir.join(BASELINE), &bytes)
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
