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

/// One published path as this sidecar last knew it: the recognized ETag
/// is the If-Match guard for the next publish and the HEAD-guard for GC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineEntry {
    pub etag: String,
    pub generation: u64,
    pub size: u64,
    pub mtime_unix: i64,
    /// The version the manifest cites for this path, when the bucket is
    /// versioned (boundary-verbs plan D7). Carried so a gated citation
    /// can re-validate its staging base: if this moved between staging
    /// and citation, a HITL consume or sync landed in between and
    /// installing the staged version would UNCITE the foreign bytes.
    #[serde(default)]
    pub version_id: Option<String>,
}

/// The persisted baseline snapshot: what this sidecar believes the
/// bucket holds AND has integrated locally. Distinct from `inst_base`
/// (the manifest view at our last install — the three-way merge base):
/// consuming a HITL entry advances the baseline for that path but not
/// the merge base. The formal model carries the same split
/// (baseline vs instBase in LeanSubtree.tla) — collapsing them made a
/// sidecar mistake its own consumed adoption for a foreign entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Baseline {
    /// Manifest seq at our last install/checkout.
    pub seq: u64,
    /// Manifest document ETag we expect at the next CAS.
    pub manifest_etag: Option<String>,
    pub entries: BTreeMap<String, BaselineEntry>,
    /// The merge base: path -> ETag as cited by the manifest we last
    /// installed (or checked out).
    pub inst_base: BTreeMap<String, String>,
    /// Paths present at the PREVIOUS scan (the two-consecutive-scans
    /// deletion rule: absence must survive two scans).
    pub prev_scan: BTreeSet<String>,
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

/// The intent journal written BEFORE uploads: which keys this barrier
/// will touch and under which flush_uuid, so a restarted container can
/// recognize its own crashed/torn PUT at the 412 (AdoptOwn) instead of
/// mistaking it for a foreign write. `recent_uuids` keeps the last few
/// barriers' uuids for the same reason.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentJournal {
    pub flush_uuid: String,
    pub keys: Vec<String>,
    pub recent_uuids: Vec<String>,
    /// The ETag of the manifest document THIS workspace last installed,
    /// written immediately after the CAS.
    ///
    /// The merge base (`Baseline::inst_base`) and the baseline are both
    /// rewritten at step 7, after the CAS and after the GC deletes. A
    /// container restart in that window leaves the bucket holding a
    /// document we wrote and our persisted merge base one generation
    /// behind it — so at the next merge our own entries read as foreign
    /// changes, delete/modify resolves conservatively against the
    /// agent's own delete, and the path is queued into the inbox as a
    /// conflict nobody else ever touched. Recording the installed ETag
    /// costs one small local write and restores exactly what step 7 was
    /// going to say: if the bucket is still at this document, the merge
    /// base IS this document.
    #[serde(default)]
    pub installed_etag: Option<String>,
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

pub struct SidecarState {
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
const LOCK: &str = "lock";
const BASELINE: &str = "baseline.json";
const INCARNATION: &str = "incarnation.json";
const INTENT: &str = "intent.json";
const CONFLICTS: &str = "conflicts.jsonl";

fn write_atomic(path: &Path, bytes: &[u8]) -> LeanResult<()> {
    // The state dir lives in the same app-writable emptyDir.
    super::safefs::check_parent(path)?;
    let tmp = path.with_extension("tmp");
    super::safefs::write_via_tmp(path, &tmp, bytes, None)
}

impl SidecarState {
    pub fn open(dir: PathBuf) -> LeanResult<SidecarState> {
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
                     refusing to run two sidecars over one tree",
                    dir.display()
                )));
            }
        }
        Ok(SidecarState { dir, _lock: lock })
    }

    pub fn marker_present(&self) -> bool {
        self.dir.join(MARKER).exists()
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

    pub fn save_baseline(&self, b: &Baseline) -> LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(b)
            .map_err(|e| LeanError::State(format!("baseline: {e}")))?;
        write_atomic(&self.dir.join(BASELINE), &bytes)
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

    /// Clear the per-barrier key list but KEEP the uuid history (the
    /// AdoptOwn recognizer needs uuids from completed barriers whose
    /// baseline rewrite raced a crash).
    pub fn clear_intent_keys(&self) -> LeanResult<()> {
        let mut j = self.load_intent()?;
        if !j.flush_uuid.is_empty() {
            if !j.recent_uuids.contains(&j.flush_uuid) {
                j.recent_uuids.push(j.flush_uuid.clone());
            }
            let excess = j.recent_uuids.len().saturating_sub(8);
            if excess > 0 {
                j.recent_uuids.drain(..excess);
            }
        }
        j.flush_uuid = String::new();
        j.keys.clear();
        self.save_intent(&j)
    }

    pub fn append_conflict(&self, c: &ConflictRecord) -> LeanResult<()> {
        use std::io::Write;
        let line =
            serde_json::to_string(c).map_err(|e| LeanError::State(format!("conflict: {e}")))?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(CONFLICTS))?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    pub fn load_conflicts(&self) -> LeanResult<Vec<ConflictRecord>> {
        let p = self.dir.join(CONFLICTS);
        if !p.exists() {
            return Ok(vec![]);
        }
        let text = fs::read_to_string(&p)?;
        let mut out = vec![];
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push(
                serde_json::from_str(line)
                    .map_err(|e| LeanError::State(format!("conflict line: {e}")))?,
            );
        }
        Ok(out)
    }
}
