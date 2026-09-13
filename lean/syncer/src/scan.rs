//! The barrier's scan-diff (plan §2.1 step 2).
//!
//! A full walk against the PERSISTED baseline — never a re-seeded
//! bucket manifest. Deletion basis: a path is delete-eligible only if
//! it is absent in THIS scan AND was absent in the PREVIOUS scan AND is
//! present in our own baseline (two-consecutive-scans: the
//! rename-vs-walk race guard — a directory renamed mid-walk can appear
//! in neither pass of one readdir, and that must never read as mass
//! deletion).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{state::Baseline, LeanResult, CONTROL_DIR, STATE_DIR};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub size: u64,
    pub mtime_unix: i64,
    /// The sub-second part of mtime. Compared only against a baseline
    /// that recorded one (review 2026-09-12, atomicity-4: a same-size
    /// rewrite inside the recorded second was invisible forever).
    pub mtime_nanos: u32,
    pub mode: u32,
}

/// The consume's temp sibling (`safefs`): a crash between its create and
/// its rename leaves it as a regular file in the tree, and the scan used
/// to publish it as data (review 2026-09-12, atomicity-7).
pub const TMP_SUFFIX: &str = ".flint-sync-tmp";

/// Whether a file's stat differs from its baseline stamp. Size and
/// seconds always; nanoseconds only when the baseline recorded them — a
/// baseline written before nanoseconds were stamped has `None`, and
/// comparing against a missing value would re-upload every file once.
pub fn stat_changed(b_size: u64, b_mtime: i64, b_nanos: Option<u32>, size: u64, mtime: i64, nanos: u32) -> bool {
    b_size != size || b_mtime != mtime || b_nanos.map(|n| n != nanos).unwrap_or(false)
}

/// Walk the workspace. Skips the state dir, symlinks (v1 non-goal, as
/// the tier manifest's), and anything unreadable (reported upstream by
/// the barrier as a warning, not a wedge).
pub fn scan(root: &Path) -> LeanResult<BTreeMap<String, ScanEntry>> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, ScanEntry>) -> LeanResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if dir == root {
            let n = name.to_string_lossy();
            // The state dir and the control namespace (boundary-verbs
            // plan D0). Without the second exclusion `.flint/publish`
            // is live ammunition: an ordinary regular file, scanned and
            // PUBLISHED to `<prefix>/files/.flint/publish`.
            if n == STATE_DIR || n == CONTROL_DIR {
                continue;
            }
        }
        if name.to_string_lossy().ends_with(TMP_SUFFIX) {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("walk stays under root")
                .to_string_lossy()
                .replace('\\', "/");
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::MetadataExt;
                meta.mode()
            };
            #[cfg(not(unix))]
            let mode = 0o644;
            let (mtime_unix, mtime_nanos) = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
                .unwrap_or((0, 0));
            out.insert(rel, ScanEntry { size: meta.len(), mtime_unix, mtime_nanos, mode });
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct Classified {
    /// Changed or new vs the baseline (by size/mtime; mtime-granularity
    /// evasion is a stated v1 residual).
    pub uploads: BTreeSet<String>,
    /// Absent this scan AND the previous scan, present in the baseline.
    pub deletes: BTreeSet<String>,
    /// Absent this scan but present in the previous one: NOT yet
    /// delete-eligible (first absence).
    pub first_absence: BTreeSet<String>,
}

/// Legacy citations under the reserved control namespace (boundary-verbs
/// plan D0.2). A workspace that legally published `files/.flint/...`
/// under a pre-D0 syncer has those paths in its baseline; the new scan
/// skips them, so two consecutive scans would classify them absent and
/// publish their DELETION. An upgrade must never delete data: they are
/// carried forward frozen — never re-uploaded, never deleted by us.
pub fn is_control_path(path: &str) -> bool {
    // Both reserved names (review 2026-09-12, inbox-8: only `.flint/`
    // was refused, so a citation naming `.flint-sync/scope-intent.json`
    // was materialised INTO the state directory and replayed).
    [CONTROL_DIR, STATE_DIR].iter().any(|d| {
        path == *d || path.strip_prefix(d).map(|r| r.starts_with('/')).unwrap_or(false)
    })
}

pub fn classify(scan: &BTreeMap<String, ScanEntry>, baseline: &Baseline) -> Classified {
    let mut c = Classified::default();
    for (path, s) in scan {
        match baseline.entries.get(path) {
            None => {
                c.uploads.insert(path.clone());
            }
            Some(b) => {
                if stat_changed(b.size, b.mtime_unix, b.mtime_nanos, s.size, s.mtime_unix, s.mtime_nanos) {
                    c.uploads.insert(path.clone());
                }
            }
        }
    }
    for path in baseline.entries.keys() {
        if scan.contains_key(path) {
            continue;
        }
        if is_control_path(path) {
            // Frozen legacy citation (D0.2): absent from every scan by
            // construction now, and NEVER delete-eligible.
            continue;
        }
        if baseline.prev_scan.contains(path) {
            // Present a scan ago: first observed absence.
            c.first_absence.insert(path.clone());
        } else {
            c.deletes.insert(path.clone());
        }
    }
    c
}
