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
use flint_store::crc64_to_b64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub size: u64,
    pub mtime_unix: i64,
    /// The sub-second part of mtime. Compared only against a baseline
    /// that recorded one (review 2026-09-12, atomicity-4: a same-size
    /// rewrite inside the recorded second was invisible forever).
    pub mtime_nanos: u32,
    pub mode: u32,
    /// When this entry was stat'ed, in nanoseconds since the epoch: an
    /// upper bound on when the scan saw the file and a LOWER bound on
    /// when anything after it read the file's bytes. The racy-clean test
    /// measures from here (`racy`).
    pub seen_nanos: i128,
}

/// RACY CLEAN (git's rule, `racy-git.txt`). A file's mtime comes from a
/// coarse kernel clock (a few milliseconds a tick), so two same-size
/// writes inside one tick leave identical stat. A baseline entry whose
/// bytes were read within a tick of the file's mtime cannot vouch for
/// the content: a second write in that tick, after the read, is
/// invisible to `stat_changed` until the file changes again — if it
/// never does, that write is never published. Such an entry is recorded
/// `racy` and is believed only after its content hash agrees
/// (`confirm_racy`, `barrier::local_dirty`). Two seconds covers every
/// clock this runs on, coarse-second filesystems included.
pub const RACY_WINDOW_NANOS: i128 = 2_000_000_000;

pub fn now_nanos() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Whether a stat with this mtime, observed at `at_nanos`, is racy.
pub fn racy(mtime_unix: i64, mtime_nanos: u32, at_nanos: i128) -> bool {
    at_nanos - (mtime_unix as i128 * 1_000_000_000 + mtime_nanos as i128) < RACY_WINDOW_NANOS
}

/// The consume's temp sibling (`safefs`): a crash between its create and
/// its rename leaves it as a regular file in the tree, and the scan used
/// to publish it as data (review 2026-09-12, atomicity-7).
pub const TMP_SUFFIX: &str = ".flint-sync-tmp";

/// The bits of `st_mode` a mode change can move and a checkout restores:
/// permissions, setuid/setgid and sticky, never the file type.
pub const PERM_BITS: u32 = 0o7777;

/// Whether two modes differ in `PERM_BITS`. Never off unix, where the
/// walk reports a constant and a cited mode could never be matched.
pub fn mode_differs(a: u32, b: u32) -> bool {
    cfg!(unix) && (a ^ b) & PERM_BITS != 0
}

/// Whether a file's stat differs from its baseline stamp. Size and
/// seconds always; nanoseconds and permission bits only when the
/// baseline recorded them — a baseline written before either was
/// stamped has `None`, and comparing against a missing value would
/// re-upload every file once. The permission bits are here because a
/// `chmod` moves only ctime: without them `chmod +x` was never
/// published, and no checkout ever saw it. `mode` is the full
/// `st_mode`.
pub fn stat_changed(b: &super::state::BaselineEntry, size: u64, mtime: i64, nanos: u32, mode: u32) -> bool {
    b.size != size
        || b.mtime_unix != mtime
        || b.mtime_nanos.map(|n| n != nanos).unwrap_or(false)
        || b.mode.is_some_and(|m| mode_differs(m, mode))
}

/// Walk the workspace. Skips the state dir, the control namespace and
/// symlinks (v1 non-goal, as the tier manifest's).
///
/// Anything it cannot read FAILS the walk, and with it the barrier: a
/// directory it cannot list, an entry it cannot lstat. Never skipped.
/// `classify` reads a path missing from two scans as the agent's
/// delete, and the barrier then publishes the deletion and deletes the
/// object, so a skipped directory would cost every file under it.
/// `Syncer::confirm_absences` keeps the same policy for the same
/// syscall, and a file the walk can stat but `upload_one` cannot open
/// fails the barrier the same way. The price is that nothing publishes
/// until the tree is readable again: an app running as a uid other than
/// the CR's that makes a 0700 directory stops the whole workspace (as
/// mkfs's root-owned `lost+found` did, before the quota removed it).
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
            out.insert(rel, ScanEntry { size: meta.len(), mtime_unix, mtime_nanos, mode, seen_nanos: now_nanos() });
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct Classified {
    /// Changed or new vs the baseline (by size/mtime; an entry whose stat
    /// is too close to its mtime to vouch for the bytes is settled by
    /// `confirm_racy`, which callers run next).
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

/// The racy half of `classify`: an entry whose stat is unchanged but was
/// recorded `racy` is hashed, and is an upload if its bytes are not the
/// ones the baseline integrated (or cannot be read to tell). Returns the
/// racy entries the hash vouched for AND whose mtime is now outside the
/// window — those can no longer hide a same-tick write, and the caller
/// may clear the flag. Run after `classify`, before anything trusts it.
pub fn confirm_racy(
    root: &Path,
    scan: &BTreeMap<String, ScanEntry>,
    baseline: &Baseline,
    c: &mut Classified,
) -> Vec<String> {
    let now = now_nanos();
    let mut cleared = vec![];
    for (path, s) in scan {
        let Some(b) = baseline.entries.get(path) else { continue };
        if !b.racy || c.uploads.contains(path) {
            continue;
        }
        match super::barrier::file_crc(root, path) {
            Ok(crc) if b.crc64_b64.as_deref() == Some(crc64_to_b64(crc).as_str()) => {
                if !racy(s.mtime_unix, s.mtime_nanos, now) {
                    cleared.push(path.clone());
                }
            }
            _ => {
                c.uploads.insert(path.clone());
            }
        }
    }
    cleared
}

pub fn classify(scan: &BTreeMap<String, ScanEntry>, baseline: &Baseline) -> Classified {
    let mut c = Classified::default();
    for (path, s) in scan {
        match baseline.entries.get(path) {
            None => {
                c.uploads.insert(path.clone());
            }
            Some(b) => {
                if stat_changed(b, s.size, s.mtime_unix, s.mtime_nanos, s.mode) {
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
