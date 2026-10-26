//! Writes that cannot be redirected by the process on the other side of
//! the mount.
//!
//! Every durable write this syncer makes is write-temp-then-rename, and
//! every one of those temp files lives in a directory the APP owns: the
//! workspace tree itself, `.flint/` (the agent drops its sentinels
//! there by design), and the `.flint-sync` state dir. `contained_path`
//! validates the rename TARGET — it never sees the temp sibling, which
//! is computed afterwards — so `fs::write` on a planted
//! `<name>.flint-sync-tmp` symlink followed it and wrote remote-supplied
//! bytes wherever it pointed, inside the credential-holding syncer's
//! own mount namespace. The scanner skips symlinks, so the plant is
//! invisible; `.flint/remote.seq` is rewritten every tick, so the
//! syncer's own tick is a sufficient trigger.
//!
//! The rule is therefore not "validate the target" but **every path the
//! write touches**:
//!
//! 1. create it `O_CREAT|O_EXCL`, which POSIX requires to fail with
//!    `EEXIST` on a symlink *whatever it points at* — a plant is a
//!    refusal, not a redirect;
//! 2. only on `EEXIST`, unlink the temp name and create once more. A
//!    leftover is crash garbage, `remove_file` removes a symlink itself
//!    and never its target, and a plant re-established in the gap is
//!    `EEXIST` again. Unlinking FIRST on every write bought nothing the
//!    exclusive create does not already refuse, and cost one syscall
//!    plus the parent directory's write lock per file: 20,006
//!    `unlinkat`, every one ENOENT, on a 20,000-file checkout
//!    (2026-09-12);
//! 3. refuse outright when the parent directory is itself a symlink,
//!    which `create_dir_all` would happily walk through.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use super::{LeanError, LeanResult};

fn refuse(path: &Path, why: &str) -> LeanError {
    LeanError::State(format!("refusing write to {}: {why} (containment)", path.display()))
}

/// Open `rel` under `root` for reading with NO symbolic link anywhere in
/// the path: each directory is opened `O_NOFOLLOW|O_DIRECTORY` relative
/// to the one before it, the file `O_NOFOLLOW|O_NONBLOCK` relative to its
/// directory, and only a regular file is returned. An lstat, or an
/// `O_NOFOLLOW` open of the whole path, guards the LAST component only:
/// `mv d d.bak; ln -s /proc/self d` between a scan and a read made
/// `d/environ` a regular file outside the workspace (review 2026-09-18,
/// H5). Resolving from a held directory descriptor leaves no name to
/// swap. `O_NONBLOCK` because a FIFO swapped in at the last moment must
/// be refused by the type check, not block the open forever.
pub(crate) fn open_beneath_nofollow(root: &Path, rel: &str) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let invalid = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{rel}: {why}"));
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() || comps.iter().any(|c| *c == "." || *c == "..") {
        return Err(invalid("not a plain relative path"));
    }
    let mut dir = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC).open(root)?;
    for (i, c) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        let name = std::ffi::CString::new(*c).map_err(|_| invalid("a component holds a NUL"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last { libc::O_NONBLOCK } else { libc::O_DIRECTORY };
        // SAFETY: a NUL-terminated name relative to a directory fd we own.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by openat and is owned by nobody else.
        let f = unsafe { std::fs::File::from_raw_fd(fd) };
        if last {
            if !f.metadata()?.is_file() {
                return Err(invalid("not a regular file"));
            }
            return Ok(f);
        }
        dir = f;
    }
    unreachable!("the loop returns at the last component")
}

/// Refuse a write whose parent directory is a symlink. Callers that
/// `create_dir_all(parent)` must ask FIRST: an app that replaces
/// `.flint` (or the state dir) with a link to `/etc` would otherwise
/// have every subsequent control write land there.
pub(crate) fn check_parent(path: &Path) -> LeanResult<()> {
    let Some(parent) = path.parent() else { return Ok(()) };
    if let Ok(m) = std::fs::symlink_metadata(parent) {
        if m.file_type().is_symlink() {
            return Err(refuse(path, "parent directory is a symlink"));
        }
    }
    Ok(())
}

/// Write `bytes` to `tmp`, never following a symlink at that name, then
/// rename onto `path`. `mode`, when given, is applied to the open
/// handle — never to the path, which would be one more lookup to race.
///
/// DURABLE: the file is fsynced before the rename and its directory
/// after, so a power loss leaves either the old file or the new one,
/// never a zero-length name. This is the writer for every STATE and
/// CONTROL file (baseline, marker, incarnation, intent, acks, pending);
/// those are small and few, and each one vouches for data elsewhere —
/// a baseline that survives a crash while the files it describes come
/// back empty makes the next scan publish zeros over the good version
/// (audit 2026-09-03, finding 9). Bulk materialisations go through
/// `write_via_tmp_fast` and are made durable by `sync_tree` before the
/// record that vouches for them is written.
///
/// A missing parent is an ERROR here: a state or control directory
/// that vanished mid-run is not something to quietly recreate.
/// Returns the written file's metadata (an `fstat` on the handle, so
/// the caller never pays a `stat` to learn what it just wrote).
pub(crate) fn write_via_tmp(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> LeanResult<std::fs::Metadata> {
    write_via_tmp_opts(path, tmp, bytes, mode, true, false, None).map(|m| m.expect("unguarded"))
}

/// The same write without the per-file fsync: for checkout, consume and
/// sync materialisations, where a million fsyncs would be the cost and
/// one `sync_tree` before the marker/baseline is the equivalent. A
/// parent that vanished after containment created it (an app deleting
/// a directory mid-checkout) is recreated on the retry path — the one
/// place `create_dir_all` runs, and only after the caller's
/// `check_parent`.
pub(crate) fn write_via_tmp_fast(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> LeanResult<std::fs::Metadata> {
    write_via_tmp_opts(path, tmp, bytes, mode, false, true, None).map(|m| m.expect("unguarded"))
}

/// `write_via_tmp_fast`, but the rename happens only if `proceed()` still
/// says so once the temp is written — the last check before the name
/// moves, for a caller whose licence to overwrite the target can lapse
/// while the bytes are written (the consume: the agent may write the path
/// meanwhile). `Ok(None)` when it did not: the temp is removed and the
/// target is untouched.
pub(crate) fn write_via_tmp_fast_if(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    proceed: &dyn Fn() -> bool,
) -> LeanResult<Option<std::fs::Metadata>> {
    write_via_tmp_opts(path, tmp, bytes, mode, false, true, Some(proceed))
}

/// `O_CREAT|O_EXCL` at `tmp`, and the two retries the common path never
/// pays for: on `EEXIST` unlink the name (a symlink is removed, never
/// followed) and create once more; on `ENOENT`, when the caller allows
/// it, recreate the parent and create once more. Anything else, and a
/// second failure of either kind, is a refusal.
fn create_exclusive(tmp: &Path, mkdir_parent: bool) -> LeanResult<std::fs::File> {
    let open = || OpenOptions::new().write(true).create_new(true).open(tmp);
    let first = match open() {
        Ok(f) => return Ok(f),
        Err(e) => e,
    };
    let again = |why: &str, e: std::io::Error| {
        refuse(tmp, &format!("temp file is not exclusively creatable after {why}: {e}"))
    };
    match first.kind() {
        std::io::ErrorKind::AlreadyExists => {
            if let Err(e) = std::fs::remove_file(tmp) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(refuse(tmp, &format!("stale temp file is not removable: {e}")));
                }
            }
            open().map_err(|e| again("removing a leftover", e))
        }
        std::io::ErrorKind::NotFound if mkdir_parent => {
            if let Some(parent) = tmp.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| refuse(tmp, &format!("mkdir for temp: {e}")))?;
            }
            open().map_err(|e| again("recreating its parent", e))
        }
        _ => Err(refuse(tmp, &format!("temp file is not exclusively creatable: {first}"))),
    }
}

/// The RANGED sibling of `write_via_tmp_fast`: the caller streams an
/// object in at offsets instead of handing over one finished buffer.
///
/// Same no-per-file-fsync rule — a materialisation is made durable by
/// `sync_tree` before the marker or baseline that vouches for it — and
/// the same exclusive-temp discipline, so a stale temp from a killed
/// checkout is refused rather than appended to. The rename still makes
/// the visible file atomic: a reader sees the whole object or no
/// object, never a half-filled one, which is the property the ranged
/// write must not lose.
pub(crate) struct RangedTmp {
    file: std::fs::File,
    tmp: std::path::PathBuf,
    path: std::path::PathBuf,
    /// Bytes actually written, so `commit` can refuse a short object
    /// rather than renaming a file with a hole in it. ATOMIC, not Cell:
    /// ranges are written from the blocking pool now, so several
    /// threads bump this concurrently and a Cell would also make the
    /// whole sink `!Sync` and unshareable.
    written: std::sync::atomic::AtomicU64,
    expect: u64,
    /// Applied at `commit`, after the last range: a write clears setuid
    /// and setgid (L-146).
    mode: Option<u32>,
}

impl RangedTmp {
    pub(crate) fn create(
        path: &Path,
        tmp: &Path,
        size: u64,
        mode: Option<u32>,
    ) -> LeanResult<RangedTmp> {
        let f = create_exclusive(tmp, true)?;
        Ok(RangedTmp {
            file: f,
            tmp: tmp.to_path_buf(),
            path: path.to_path_buf(),
            written: std::sync::atomic::AtomicU64::new(0),
            expect: size,
            mode,
        })
    }

    /// Write one range at its offset. Ranges may land in any order —
    /// that is the whole point — so this is a positional write and
    /// never a seek the concurrent siblings could race.
    pub(crate) fn write_at(&self, offset: u64, bytes: &[u8]) -> LeanResult<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file
                .write_all_at(bytes, offset)
                .map_err(|e| refuse(&self.tmp, &format!("write at {offset}: {e}")))?;
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = &self.file;
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| refuse(&self.tmp, &format!("seek to {offset}: {e}")))?;
            f.write_all(bytes)
                .map_err(|e| refuse(&self.tmp, &format!("write at {offset}: {e}")))?;
        }
        self.written
            .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Rename into place. Refuses unless every expected byte arrived:
    /// a rename here is what makes the object visible, and a hole in a
    /// materialised file reads to the next scan as the agent's own
    /// truncation and publishes back over the good version.
    pub(crate) fn commit(self) -> LeanResult<()> {
        let written = self.written.load(std::sync::atomic::Ordering::Relaxed);
        if written != self.expect {
            let _ = std::fs::remove_file(&self.tmp);
            return Err(LeanError::State(format!(
                "ranged materialisation of {} wrote {} of {} bytes — refusing to rename a \
                 file with a hole in it",
                self.path.display(),
                written,
                self.expect
            )));
        }
        #[cfg(unix)]
        if let Some(mode) = self.mode {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = self.file.set_permissions(std::fs::Permissions::from_mode(mode)) {
                let _ = std::fs::remove_file(&self.tmp);
                return Err(refuse(&self.tmp, &format!("mode: {e}")));
            }
        }
        drop(self.file);
        std::fs::rename(&self.tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&self.tmp);
            refuse(&self.path, &format!("rename from temp: {e}"))
        })
    }
}

/// Refuse `bytes` of TREE content at `rel` when its tenant could not
/// have written them itself (`LeanConfig::tree_reserve`).
///
/// The s3csi quota image reserves 1/16 of itself for a group only the
/// syncer's worker holds (`resgid`), so a tenant that fills its ceiling
/// still leaves the syncer room for the bookkeeping a barrier and a drain
/// cannot do without: the intent journal, the baseline, the conflict
/// record, the drain's attestation (S18: a full tree's drain met ENOSPC
/// on `intent.json` and could neither publish nor let go of its tree).
/// The kernel cannot tell the syncer's two kinds of write apart, though.
/// A peer's file consumed into a full tree would spend the same reserve,
/// and the bookkeeping after it would meet ENOSPC again. So tree content
/// is held here to what an unprivileged writer may use, `f_bavail`, and
/// only the bookkeeping reaches the reserve. A tenant writing at the same
/// moment can still race the check by the size of its own write; the
/// next check sees it.
///
/// `room` replaces the `statvfs` answer (tests: a full filesystem needs
/// root to build). The refusal is an I/O error, which every caller
/// already handles as the transient full disk it describes.
pub(crate) fn leave_the_reserve(root: &Path, rel: &str, bytes: u64, room: Option<u64>) -> LeanResult<()> {
    let (room, block) = match room {
        Some(r) => (r, 4096),
        None => {
            let st = rustix::fs::statvfs(root).map_err(std::io::Error::from)?;
            let block = st.f_frsize.max(1);
            (st.f_bavail.saturating_mul(block), block)
        }
    };
    let need = bytes_with_slack(bytes, block);
    if need > room {
        return Err(LeanError::Io(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            format!(
                "{rel}: its {bytes} bytes would land in the syncer's reserve: the tree has {room} bytes left \
                 for its tenant, and the reserve is kept for the syncer's own bookkeeping (the tree is full)"
            ),
        )));
    }
    Ok(())
}

/// What a file of `bytes` costs a filesystem of `block`-byte blocks:
/// whole blocks, plus four for the directory and extent blocks a new
/// file can take.
pub(crate) fn bytes_with_slack(bytes: u64, block: u64) -> u64 {
    let block = block.max(1);
    bytes.div_ceil(block).saturating_add(4).saturating_mul(block)
}

/// Flush every dirty page of the filesystem holding `dir` to stable
/// storage. Linux `syncfs(2)`; elsewhere an fsync of the directory,
/// which is the best the platform offers. Called before the checkout
/// marker and before a baseline that vouches for materialised files.
pub(crate) fn sync_tree(dir: &Path) -> LeanResult<()> {
    let d = std::fs::File::open(dir)
        .map_err(|e| LeanError::State(format!("open {} to sync: {e}", dir.display())))?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: syncfs on an owned, open fd.
        if unsafe { libc::syncfs(d.as_raw_fd()) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(LeanError::State(format!("syncfs {}: {e}", dir.display())));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        d.sync_all()
            .map_err(|e| LeanError::State(format!("fsync {}: {e}", dir.display())))
    }
}

fn write_via_tmp_opts(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    durable: bool,
    mkdir_parent: bool,
    proceed: Option<&dyn Fn() -> bool>,
) -> LeanResult<Option<std::fs::Metadata>> {
    let mut f = create_exclusive(tmp, mkdir_parent)?;
    f.write_all(bytes)
        .map_err(|e| LeanError::State(format!("write tmp for {}: {e}", path.display())))?;
    // The mode AFTER the bytes (L-146): a write by a process without
    // CAP_FSETID clears setuid, and setgid with group-execute, so a mode
    // set first came out 0755 under a document citing 4755, and the
    // tree's next barrier published the stripped mode.
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777));
    }
    #[cfg(not(unix))]
    let _ = mode;
    if durable {
        f.sync_all()
            .map_err(|e| LeanError::State(format!("fsync tmp for {}: {e}", path.display())))?;
    }
    // The rename moves the name, not the inode: this is the metadata
    // the caller would otherwise `stat` the target for.
    let meta = f
        .metadata()
        .map_err(|e| LeanError::State(format!("fstat tmp for {}: {e}", path.display())))?;
    drop(f);
    if proceed.is_some_and(|p| !p()) {
        let _ = std::fs::remove_file(tmp);
        return Ok(None);
    }
    std::fs::rename(tmp, path)
        .map_err(|e| LeanError::State(format!("rename into {}: {e}", path.display())))?;
    if durable {
        // The rename is a directory write; without this the name can
        // vanish on power loss even though the bytes reached the disk.
        if let Some(parent) = path.parent() {
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
    }
    Ok(Some(meta))
}
