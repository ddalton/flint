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
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

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
    /// What only the kernel stamps (`ChangeStamp`).
    pub change: ChangeStamp,
}

/// The stamp no agent can set: a file's ctime, which every write, chmod,
/// rename and `utimensat` moves to now, and its inode. mtime is the
/// agent's to set (`touch -d`, `cp -p`, `rsync -t`): a same-size rewrite
/// under the recorded mtime left size and mtime as recorded, and was never
/// published (AGENTS.md forbade setting timestamps for that reason). An
/// entry whose change stamp moved while size, mtime and mode did not is
/// UNSURE: its bytes decide (`confirm_racy`, `barrier::local_dirty`), so a
/// stamp that moved with no change costs a hash, never an upload. The
/// model's `StampMoved`, `CtimeConfirm` (`LeanScanRacy.tla`).
///
/// Kept as `[ctime, nanos, ino]` (a baseline of a million entries carries
/// a million of them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "(i64, u32, u64)", into = "(i64, u32, u64)")]
pub struct ChangeStamp {
    pub ctime: i64,
    pub ctime_nanos: u32,
    pub ino: u64,
}

impl From<(i64, u32, u64)> for ChangeStamp {
    fn from((ctime, ctime_nanos, ino): (i64, u32, u64)) -> Self {
        ChangeStamp { ctime, ctime_nanos, ino }
    }
}

impl From<ChangeStamp> for (i64, u32, u64) {
    fn from(c: ChangeStamp) -> Self {
        (c.ctime, c.ctime_nanos, c.ino)
    }
}

impl ChangeStamp {
    pub fn of(m: &std::fs::Metadata) -> ChangeStamp {
        use std::os::unix::fs::MetadataExt;
        ChangeStamp { ctime: m.ctime(), ctime_nanos: m.ctime_nsec() as u32, ino: m.ino() }
    }
}

/// Whether `b`'s change stamp is not `now`'s. `None`, a baseline entry
/// written before the stamp, compares as unknown, as `stat_changed`'s
/// optional fields do.
pub fn stamp_moved(b: &super::state::BaselineEntry, now: &ChangeStamp) -> bool {
    b.change.is_some_and(|c| c != *now)
}

/// The stamp the racy window is measured from: the LATER of mtime and
/// ctime. Measured from an mtime the agent set into the past, a second
/// same-size `cp -p` inside the tick of the first hid (the model's
/// `RacyOnChange`).
pub fn later(mtime: i64, mtime_nanos: u32, change: Option<&ChangeStamp>) -> (i64, u32) {
    match change {
        Some(c) if (c.ctime, c.ctime_nanos) > (mtime, mtime_nanos) => (c.ctime, c.ctime_nanos),
        _ => (mtime, mtime_nanos),
    }
}

/// Whether a baseline entry is racy measured at `at` (`racy`, from its
/// later stamp). An entry recorded without sub-second mtime takes the end
/// of its second.
pub fn racy_entry(b: &super::state::BaselineEntry, at: i128) -> bool {
    let (sec, nanos) = later(b.mtime_unix, b.mtime_nanos.unwrap_or(999_999_999), b.change.as_ref());
    racy(sec, nanos, at)
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

/// What a walk found: every regular file it could stat, and every path
/// it was refused (`held`).
#[derive(Debug, Default)]
pub struct Scan {
    pub entries: BTreeMap<String, ScanEntry>,
    /// Paths the syncer may not read (EACCES or EPERM, which std reports
    /// alike as `PermissionDenied`: either is a refusal the tree's owner
    /// set), each standing for itself and everything under it: a
    /// directory it may not list (written with
    /// its trailing `/`), an entry it may not lstat (a name in a directory
    /// it may list but not search, so even its type is unknown). And a
    /// file or directory whose name is not UTF-8, which no key can name:
    /// held under its lossy form (`read_dir_entries`). The
    /// barrier adds the files it may not open and the absences it may not
    /// confirm. Nothing held is published or deleted, and no boundary is
    /// complete while anything is held.
    ///
    /// This is how each one is SHOWN (in the report, the ack, the log).
    /// What each one stands for is its `Hold`, kept apart (`hold`): the
    /// lossy form of a name that is not UTF-8 may spell a real name, and a
    /// file has nothing under it.
    pub held: BTreeSet<String>,
    /// The held paths that stand for everything under them (`Hold::Tree`),
    /// each without a trailing `/`.
    held_trees: BTreeSet<String>,
    /// The held paths that stand for themselves alone (`Hold::File`).
    held_files: BTreeSet<String>,
}

/// What a held path stands for: which keys it makes unseen rather than
/// absent (`Scan::holds`, `Scan::present`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// It and everything under it: a directory the walk may not list, or
    /// a name it may not stat, which may be a directory.
    Tree,
    /// Itself alone: a file the syncer may not read, a path too long for
    /// a key, an absence it may not confirm. Nothing can be under a file:
    /// held as a tree, a file standing where a cited directory was kept
    /// every citation under it, a first absence at every barrier (L-147).
    File,
    /// No key at all: a name that is not UTF-8, shown with U+FFFD. No
    /// cited path can be at or under it, and its lossy form may spell a
    /// real name, which held as a tree it held too, withholding that
    /// name's deletes (the collision L-141 accepted).
    Unnamed,
}

impl Scan {
    /// Hold the path shown as `shown` (a directory with its trailing `/`).
    pub fn hold(&mut self, shown: String, kind: Hold) {
        match kind {
            Hold::Tree => {
                self.held_trees.insert(shown.strip_suffix('/').unwrap_or(&shown).to_string());
            }
            Hold::File => {
                self.held_files.insert(shown.clone());
            }
            Hold::Unnamed => {}
        }
        self.held.insert(shown);
    }

    /// Whether `path` is held: a held file itself, or at or under a held
    /// tree.
    pub fn holds(&self, path: &str) -> bool {
        self.held_files.contains(path)
            || self
                .held_trees
                .iter()
                .any(|base| path.strip_prefix(base.as_str()).is_some_and(|r| r.is_empty() || r.starts_with('/')))
    }

    /// What `prev_scan` records after this scan, in order: every path the
    /// walk saw, and every path in `cited` that it held. A held path is
    /// unseen, not absent, so it is recorded PRESENT: a path that turns
    /// out to be gone once it is readable again is then a first absence,
    /// and the two-scan rule withholds its delete one more scan, exactly
    /// as for a path seen a scan ago.
    pub fn present<'a, V>(&'a self, cited: &'a BTreeMap<String, V>) -> impl Iterator<Item = &'a String> + 'a {
        let mut unseen: Vec<&'a String> = self
            .held_trees
            .iter()
            .flat_map(|h| under(cited, h))
            .chain(self.held_files.iter().filter_map(|f| cited.get_key_value(f.as_str()).map(|(k, _)| k)))
            .collect();
        unseen.sort_unstable();
        unseen.dedup();
        let mut seen = self.entries.keys().peekable();
        let mut unseen = unseen.into_iter().peekable();
        std::iter::from_fn(move || match (seen.peek(), unseen.peek()) {
            (Some(a), Some(b)) => match a.cmp(b) {
                std::cmp::Ordering::Less => seen.next(),
                std::cmp::Ordering::Greater => unseen.next(),
                std::cmp::Ordering::Equal => {
                    unseen.next();
                    seen.next()
                }
            },
            (Some(_), None) => seen.next(),
            (None, _) => unseen.next(),
        })
    }
}

/// The keys of `cited` at or under the held path `held`. Two lookups,
/// not one run: `d-1` and `d.txt` sort between `d` and `d/`.
fn under<'a, V>(cited: &'a BTreeMap<String, V>, held: &'a str) -> impl Iterator<Item = &'a String> + 'a {
    use std::ops::Bound;
    let base = held.strip_suffix('/').unwrap_or(held);
    let dir = format!("{base}/");
    let exact = cited.get_key_value(base).map(|(k, _)| k);
    let below: Vec<&'a String> = cited
        .range::<str, _>((Bound::Included(dir.as_str()), Bound::Unbounded))
        .map(|(k, _)| k)
        .take_while(|k| k.starts_with(dir.as_str()))
        .collect();
    exact.into_iter().chain(below)
}

/// What an error on one name says about that name. Every look a barrier
/// takes at the tree by name (the walk's stat and its open of a directory,
/// the upload's open, the confirmation of an absence) sorts its error by
/// this one rule, so an error about one name decides that name, and only
/// an error about none of them fails the barrier (L-144). Before it, each
/// call site kept its own list, and any errno not on it, ENOENT among
/// them, failed the whole barrier: a tree in which anything made and
/// removed files stopped publishing for as long as it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Nothing is at the name any more (ENOENT): it, or a directory above
    /// it, was removed or renamed since it was listed.
    Gone,
    /// Something other than what was listed is there: a component that is
    /// no longer a directory (ENOTDIR), a symlink where a file or a
    /// directory was (ELOOP), or not a regular file (`safefs`'s
    /// InvalidInput).
    Replaced,
    /// The tree's owner refused the syncer (EACCES or EPERM): held.
    Refused,
    /// Anything else (EIO, ENAMETOOLONG, EMFILE...): no standing condition
    /// of one name, so it fails the barrier.
    Other,
}

pub fn name_error(e: &std::io::Error) -> NameError {
    match e.raw_os_error() {
        Some(libc::ENOENT) => NameError::Gone,
        Some(libc::ENOTDIR) | Some(libc::ELOOP) => NameError::Replaced,
        _ if e.kind() == std::io::ErrorKind::PermissionDenied => NameError::Refused,
        _ if e.kind() == std::io::ErrorKind::InvalidInput => NameError::Replaced,
        _ => NameError::Other,
    }
}

/// A test window inside the walk (`barrier::CONSUME_WINDOW_HOOK`), on the
/// one-thread walk only: the hook is the calling thread's. `rel` is built
/// only when a hook is armed.
#[cfg(test)]
fn walk_window(stage: &'static str, rel: impl FnOnce() -> String) {
    if super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_some()) {
        super::barrier::consume_window(stage, &rel());
    }
}

#[cfg(not(test))]
#[inline(always)]
fn walk_window(_: &'static str, _: impl FnOnce() -> String) {}

/// Whether the syncer may read the file at `rel`: false only when the
/// kernel answers EACCES or EPERM. Asked of each upload before the
/// barrier's idle check, so a file it may not read is held there and an
/// otherwise idle tick stays idle; `upload_one` holds one that turns
/// unreadable after this. The syncer's real and effective uids are one,
/// so `access` is the open's answer.
pub fn may_read(root: &Path, rel: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(root.join(rel).as_os_str().as_bytes()) else { return true };
        if unsafe { libc::access(c.as_ptr(), libc::R_OK) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().kind() != std::io::ErrorKind::PermissionDenied
    }
    #[cfg(not(unix))]
    {
        let _ = (root, rel);
        true
    }
}

/// Walk the workspace. Skips the state dir, the control namespace and
/// symlinks (v1 non-goal, as the tier manifest's).
///
/// What the syncer may not read (EACCES or EPERM) is HELD (`Scan::held`),
/// never skipped: a directory it may not list, an entry it may not lstat.
/// `classify` reads a path missing from two scans as the agent's delete,
/// and the barrier then publishes the deletion and deletes the object,
/// so a skipped directory would cost every file under it; a held one
/// costs nothing, because `classify` neither uploads nor deletes a path
/// at or under it, and `Scan::present` keeps it in `prev_scan`. The rest
/// of the tree publishes. The walk used to FAIL instead (c24beeb7), and
/// then one 0700 directory made by an app running as a uid other than
/// the CR's stopped every publish in the workspace until it was readable.
///
/// A path's key is its names, byte for byte, so the barrier opens the
/// file it scanned. A name that is not UTF-8 has no such key and is held
/// too (L-141); a `\` in a name is a byte like any other (L-142). Either
/// used to give a key that named another file or none, and the upload's
/// ENOENT failed every barrier in the workspace.
///
/// A name that is GONE by the time the walk stats it, or a directory gone
/// or replaced by the time the walk opens it, is absent from this scan
/// (`NameError`, L-144): the agent removed or moved it while the walk ran,
/// and the two-scan rule is what makes reading that as absence safe.
///
/// Every other error FAILS the walk, and with it the barrier: an EIO is
/// not a standing condition the tree's owner set, and an unreadable ROOT
/// fails too (nothing could publish; the error names why).
pub fn scan(root: &Path) -> LeanResult<Scan> {
    scan_with(root, default_threads())
}

/// Threads the walk uses unless told otherwise: the CPUs this process may
/// use (the cgroup's quota and the affinity mask count), at most 8.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
}

/// `scan`, by `threads` workers (lever E). They take directories from one
/// queue, so a tree that is one huge directory and a tree of thousands of
/// small ones both spread. Each entry is stat'ed relative to its
/// directory's descriptor, not by its full path (`read_dir_entries`). What
/// it returns is what the one-thread walk returned:
/// the same entries under the same exclusions, and the first error any
/// worker meets fails the whole walk (`Queue::failed`), never dropped
/// with the worker that met it. One worker walks on the calling thread.
/// What it holds is the same set at any thread count.
pub fn scan_with(root: &Path, threads: usize) -> LeanResult<Scan> {
    let queue = Mutex::new(Queue { dirs: vec![(root.to_path_buf(), String::new())], busy: 0, failed: None });
    let wake = Condvar::new();
    let found: Vec<Found> = if threads <= 1 {
        vec![walk_worker(&queue, &wake)]
    } else {
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..threads).map(|_| s.spawn(|| walk_worker(&queue, &wake))).collect();
            workers.into_iter().map(|w| w.join().expect("a scan worker panicked")).collect()
        })
    };
    if let Some(e) = queue.into_inner().unwrap_or_else(|p| p.into_inner()).failed {
        return Err(e.into());
    }
    // The keys the map keeps are made here, on the calling thread, never
    // in a worker. mimalloc, which flint-sync ships with, returns a freed
    // block to the heap of the thread that allocated it, so a million keys
    // made by workers left this thread's next large allocations (the
    // commit's documents) on cold pages: at 1M files, 480 ms more across
    // classify, load, merge, cas and retire_set (latency plan §13).
    let mut all: Vec<(String, ScanEntry)> = Vec::with_capacity(found.iter().map(|f| f.files.len()).sum());
    let mut scan = Scan::default();
    for f in found {
        for (shown, kind) in f.held {
            scan.hold(shown, kind);
        }
        let mut start = 0;
        for (end, e) in f.files {
            all.push((f.names[start..end].to_string(), e));
            start = end;
        }
    }
    // No two files give one key: a key is the names on the file's path,
    // byte for byte (`read_dir_entries`). Two did once, when a `\` became
    // `/` and a name not UTF-8 was keyed by its lossy form, and the walk
    // had to choose one by a rule of the tree. Sorted here, `from_iter`
    // finds the order made and builds in bulk, not by a million inserts.
    all.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    scan.entries = all.into_iter().collect();
    Ok(scan)
}

/// The walk's shared state: directories not yet read (with the relative
/// prefix of what is under each), how many are being read, and the first
/// error.
struct Queue {
    dirs: Vec<(PathBuf, String)>,
    busy: usize,
    failed: Option<std::io::Error>,
}

/// What one worker found: the files' relative paths end to end in one
/// buffer, and per file where its path ends there and what was stat'ed.
/// A few large allocations per worker, not one per file. And what it
/// held (`Scan::held`), which is rare.
#[derive(Default)]
struct Found {
    names: String,
    files: Vec<(usize, ScanEntry)>,
    held: Vec<(String, Hold)>,
}

fn walk_worker(queue: &Mutex<Queue>, wake: &Condvar) -> Found {
    let mut out = Found::default();
    loop {
        let (dir, prefix) = {
            let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if q.failed.is_some() {
                    return out;
                }
                if let Some(next) = q.dirs.pop() {
                    q.busy += 1;
                    break next;
                }
                if q.busy == 0 {
                    // Nothing queued and nobody reading: the tree is done.
                    wake.notify_all();
                    return out;
                }
                q = wake.wait(q).unwrap_or_else(|p| p.into_inner());
            }
        };
        let mut subdirs = vec![];
        let read = read_dir_entries(&dir, &prefix, &mut out, &mut subdirs);
        let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
        q.busy -= 1;
        match read {
            Ok(()) => q.dirs.extend(subdirs),
            Err(e) => {
                if q.failed.is_none() {
                    q.failed = Some(e);
                }
            }
        }
        wake.notify_all();
    }
}

/// One directory: its files into `out`, its subdirectories onto `subdirs`.
/// `prefix` is the directory's path relative to the root, with a trailing
/// `/`; empty at the root.
///
/// The directory is read with `getdents64` and each name is stat'ed
/// relative to the directory's own descriptor (`fstatat`,
/// `AT_SYMLINK_NOFOLLOW`): no entry's full path is built, and the kernel
/// resolves one name, not every component of the path. std's
/// `DirEntry::metadata` does the same on glibc, but on musl, which
/// flint-sync ships with, it joins the full path and `lstat`s it, and
/// musl's `readdir` reads 2 KiB at a time (latency plan §13).
fn read_dir_entries(
    dir: &Path,
    prefix: &str,
    out: &mut Found,
    subdirs: &mut Vec<(PathBuf, String)>,
) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use std::os::unix::ffi::OsStrExt;
    walk_window("walk-before-open", || prefix.to_string());
    let opened = rustix::fs::open(dir, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty());
    let fd = match opened.map_err(std::io::Error::from) {
        Ok(fd) => fd,
        // Below the root (`scan`): a directory gone or replaced since it
        // was listed is absent from this scan; one the syncer may not
        // list is held, not failed.
        Err(e) if !prefix.is_empty() => match name_error(&e) {
            NameError::Gone | NameError::Replaced => return Ok(()),
            NameError::Refused => {
                out.held.push((prefix.to_string(), Hold::Tree));
                return Ok(());
            }
            NameError::Other => return Err(e),
        },
        Err(e) => return Err(e),
    };
    let mut entries = rustix::fs::Dir::new(fd)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(bytes);
        // A name that is not UTF-8 has no key. Its lossy form names another
        // file or none, so the upload opened that and its ENOENT failed the
        // barrier at every tick (L-141). Held, under the lossy form.
        let keyless = matches!(name, std::borrow::Cow::Owned(_));
        if prefix.is_empty() {
            // The state dir and the control namespace (boundary-verbs
            // plan D0). Without the second exclusion `.flint/publish`
            // is live ammunition: an ordinary regular file, scanned and
            // PUBLISHED to `<prefix>/files/.flint/publish`.
            if name == STATE_DIR || name == CONTROL_DIR {
                continue;
            }
        }
        if name.ends_with(TMP_SUFFIX) {
            continue;
        }
        walk_window("walk-before-stat", || format!("{prefix}{name}"));
        let stat = rustix::fs::statat(entries.fd()?, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW);
        let st = match stat.map_err(std::io::Error::from) {
            Ok(st) => st,
            Err(e) => match name_error(&e) {
                // Removed since `getdents` listed it: absent from this scan.
                NameError::Gone | NameError::Replaced => continue,
                // A directory the syncer may list but not search: each
                // name in it is held, file or directory alike.
                NameError::Refused => {
                    out.held.push((format!("{prefix}{name}"), if keyless { Hold::Unnamed } else { Hold::Tree }));
                    continue;
                }
                NameError::Other => return Err(e),
            },
        };
        match FileType::from_raw_mode(st.st_mode as _) {
            FileType::Directory => {
                let rel = format!("{prefix}{name}/");
                if keyless {
                    // Nothing under it has a key either.
                    out.held.push((rel, Hold::Unnamed));
                } else {
                    subdirs.push((dir.join(std::ffi::OsStr::from_bytes(bytes)), rel));
                }
            }
            FileType::RegularFile if keyless => out.held.push((format!("{prefix}{name}"), Hold::Unnamed)),
            FileType::RegularFile => {
                // As std's `modified()` and `duration_since(UNIX_EPOCH)`
                // gave it: a time before the epoch is (0, 0).
                let (mtime_unix, mtime_nanos) = if st.st_mtime >= 0 && (st.st_mtime_nsec as u64) < 1_000_000_000 {
                    (st.st_mtime as i64, st.st_mtime_nsec as u32)
                } else {
                    (0, 0)
                };
                out.names.push_str(prefix);
                out.names.push_str(&name);
                out.files.push((
                    out.names.len(),
                    ScanEntry {
                        size: st.st_size as u64,
                        mtime_unix,
                        mtime_nanos,
                        mode: st.st_mode as u32,
                        seen_nanos: now_nanos(),
                        change: ChangeStamp {
                            ctime: st.st_ctime as i64,
                            ctime_nanos: st.st_ctime_nsec as u32,
                            ino: st.st_ino as u64,
                        },
                    },
                ));
            }
            // Symlinks are never followed, and what is neither a
            // directory nor a regular file is not data.
            _ => {}
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

/// What a hash that vouched for an unsure entry leaves in its baseline
/// entry (`confirm_racy`): the change stamp the scan saw, and whether the
/// entry is still racy, measured now from its later stamp.
#[derive(Debug, Clone)]
pub struct Vouched {
    pub path: String,
    pub change: ChangeStamp,
    pub racy: bool,
}

/// The unsure half of `classify`: an entry whose stat is unchanged but
/// cannot vouch for its bytes (recorded `racy`, or its change stamp moved:
/// `stamp_moved`) is hashed, and is an upload if its bytes are not the
/// ones the baseline integrated (or cannot be read to tell). Returns the
/// unsure entries the hash vouched for, each with the stamp to record and
/// whether it is still racy; the caller writes the ones that changed. Run
/// after `classify`, before anything trusts it. The model's `Confirm`.
pub fn confirm_racy(
    root: &Path,
    scan: &Scan,
    baseline: &Baseline,
    c: &mut Classified,
) -> Vec<Vouched> {
    let now = now_nanos();
    let mut vouched = vec![];
    for (path, s) in &scan.entries {
        let Some(b) = baseline.entries.get(path) else { continue };
        if !(b.racy || stamp_moved(b, &s.change)) || c.uploads.contains(path) {
            continue;
        }
        match super::barrier::file_crc(root, path) {
            Ok(crc) if b.crc64_b64.as_deref() == Some(crc64_to_b64(crc).as_str()) => {
                let (sec, nanos) = later(s.mtime_unix, s.mtime_nanos, Some(&s.change));
                vouched.push(Vouched { path: path.clone(), change: s.change, racy: racy(sec, nanos, now) });
            }
            _ => {
                c.uploads.insert(path.clone());
            }
        }
    }
    vouched
}

pub fn classify(scan: &Scan, baseline: &Baseline) -> Classified {
    let mut c = Classified::default();
    for (path, s) in &scan.entries {
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
        if scan.entries.contains_key(path) {
            continue;
        }
        if scan.holds(path) {
            // Unseen, not absent: neither a delete nor a first absence.
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
