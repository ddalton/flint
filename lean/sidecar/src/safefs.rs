//! Writes that cannot be redirected by the process on the other side of
//! the mount.
//!
//! Every durable write this sidecar makes is write-temp-then-rename, and
//! every one of those temp files lives in a directory the APP owns: the
//! workspace tree itself, `.flint/` (the agent drops its sentinels
//! there by design), and the `.flint-sync` state dir. `contained_path`
//! validates the rename TARGET — it never sees the temp sibling, which
//! is computed afterwards — so `fs::write` on a planted
//! `<name>.flint-sync-tmp` symlink followed it and wrote remote-supplied
//! bytes wherever it pointed, inside the credential-holding sidecar's
//! own mount namespace. The scanner skips symlinks, so the plant is
//! invisible; `.flint/remote.seq` is rewritten every tick, so the
//! sidecar's own heartbeat is a sufficient trigger.
//!
//! The rule is therefore not "validate the target" but **every path the
//! write touches**:
//!
//! 1. unlink the temp name first — a leftover is crash garbage, and
//!    `remove_file` removes a symlink itself, never its target;
//! 2. create it `O_CREAT|O_EXCL`, which POSIX requires to fail with
//!    `EEXIST` on a symlink *whatever it points at* — so a plant
//!    re-established in the gap between the two is a refusal, not a
//!    redirect;
//! 3. refuse outright when the parent directory is itself a symlink,
//!    which `create_dir_all` would happily walk through.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use super::{LeanError, LeanResult};

fn refuse(path: &Path, why: &str) -> LeanError {
    LeanError::State(format!("refusing write to {}: {why} (containment)", path.display()))
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
pub(crate) fn write_via_tmp(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> LeanResult<()> {
    write_via_tmp_opts(path, tmp, bytes, mode, true)
}

/// The same write without the per-file fsync: for checkout, consume and
/// sync materialisations, where a million fsyncs would be the cost and
/// one `sync_tree` before the marker/baseline is the equivalent.
pub(crate) fn write_via_tmp_fast(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> LeanResult<()> {
    write_via_tmp_opts(path, tmp, bytes, mode, false)
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
) -> LeanResult<()> {
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(refuse(tmp, &format!("stale temp file is not removable: {e}"))),
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|e| refuse(tmp, &format!("temp file is not exclusively creatable: {e}")))?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777));
    }
    #[cfg(not(unix))]
    let _ = mode;
    f.write_all(bytes)
        .map_err(|e| LeanError::State(format!("write tmp for {}: {e}", path.display())))?;
    if durable {
        f.sync_all()
            .map_err(|e| LeanError::State(format!("fsync tmp for {}: {e}", path.display())))?;
    }
    drop(f);
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
    Ok(())
}
