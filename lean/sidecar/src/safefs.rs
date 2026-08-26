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
pub(crate) fn write_via_tmp(
    path: &Path,
    tmp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
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
    drop(f);
    std::fs::rename(tmp, path)
        .map_err(|e| LeanError::State(format!("rename into {}: {e}", path.display())))?;
    Ok(())
}
