//! One door for every by-path open in the NFSv4 data path.
//!
//! In NFS the CLIENT resolves symbolic links, never the server. LOOKUP
//! returns the link's own filehandle, the client READLINKs it and
//! re-resolves against its own namespace (RFC 8881 §16.10.5); an OPEN
//! that lands on a link is answered `NFS4ERR_SYMLINK` (§18.16.3). A
//! server that dereferences a link on the client's behalf is not being
//! helpful — it is executing a path the client never asked for, chosen
//! by whoever wrote the link.
//!
//! That is a privilege boundary, because the hub's process is not the
//! client's process. On a flint-lite hub the export root is
//! `/data/exports` and the state database is `/data/state/state.db` —
//! a sibling on the same PVC. So
//!
//! ```text
//!   ln -s /data/state/state.db s && cat s
//! ```
//!
//! would have handed any mount every filehandle, lock and session in the
//! volume, and
//!
//! ```text
//!   ln -s /var/run/secrets/eks.amazonaws.com/serviceaccount/token t && cat t
//! ```
//!
//! the hub's own IRSA token, and with it the bucket the tier publishes
//! to. Neither target is inside the export, and neither needed a bug in
//! the containment check to reach it: containment canonicalizes the
//! PARENT and re-appends the leaf raw (filehandle.rs), exactly as the
//! RFC requires, so the escape rode the one component that layer must
//! deliberately leave un-followed.
//!
//! The guard is `O_NOFOLLOW` on that final component. It is
//! kernel-enforced and atomic, so unlike a `symlink_metadata` pre-check
//! it cannot be raced by swapping the name between the check and the
//! open. It costs a conforming client nothing — such a client never asks
//! the server to open a link — and a non-conforming one gets `ELOOP`,
//! which `io_error_to_nfs4` renders as `NFS4ERR_SYMLINK` so it knows to
//! READLINK instead.
//!
//! Every caller here receives a path that already came out of the
//! filehandle layer, so its parent is canonicalized and proven inside
//! the export before arriving. This module closes the leaf.
//!
//! ## Not yet: `openat2(RESOLVE_BENEATH)`
//!
//! On Linux ≥5.6, resolving relative to a pinned O_PATH dirfd with
//! `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` would make the *whole*
//! walk kernel-enforced, closing the residual race where a directory
//! component is swapped for a symlink between the containment check and
//! the open. That window is narrow (it needs an already-authenticated
//! client racing the server) and the syscall is unavailable on macOS,
//! where most of this suite runs — so it is a documented upgrade, not a
//! silent gap. The leaf hole, which needs no race at all, is closed here.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// `O_NOFOLLOW` for the platform, or 0 where the concept does not exist.
#[cfg(unix)]
fn nofollow() -> i32 {
    libc::O_NOFOLLOW
}

/// Open `path` with `opts`, refusing to follow a symlink at the final
/// component.
///
/// The returned error for a symlink leaf is `ELOOP` on both Linux and
/// macOS; callers that answer NFS render it via `io_error_to_nfs4`.
pub fn open(opts: &OpenOptions, path: &Path) -> io::Result<File> {
    let mut o = opts.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // custom_flags REPLACES rather than ORs, so this must be the
        // only custom_flags call on the options it is given. No caller
        // in the data path sets others (O_DIRECT lives in the DS lane,
        // which does not resolve client-named paths).
        o.custom_flags(nofollow());
    }
    o.open(path)
}

/// Async twin of [`open`], for the handlers already on tokio's fs API.
pub async fn open_async(opts: &tokio::fs::OpenOptions, path: &Path) -> io::Result<File> {
    let mut o = opts.clone();
    #[cfg(unix)]
    {
        // tokio's OpenOptions carries its own inherent custom_flags.
        o.custom_flags(nofollow());
    }
    // Handed back as a std File: every caller here either caches it in
    // the fd cache (which holds std files) or drops it, and returning
    // one type from both doors keeps them from drifting.
    Ok(o.open(path).await?.into_std().await)
}

/// Read-only open, refusing a symlink leaf. The `File::open` shorthand.
pub fn open_read(path: &Path) -> io::Result<File> {
    open(OpenOptions::new().read(true), path)
}

/// Is `path` itself a symbolic link?
///
/// For producing the RFC-correct status BEFORE a handler mutates
/// anything (OPEN admits space and stamps ownership on the way to its
/// create). This is advisory only — it can be raced, and the guarantee
/// is [`open`]'s `O_NOFOLLOW`. Never use it as the sole guard.
pub fn leaf_is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Did this error come from refusing to follow a symlink?
pub fn is_symlink_refusal(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        return e.raw_os_error() == Some(libc::ELOOP);
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The whole point: a link pointing outside the export cannot be
    /// opened through it, in either direction. Reading it would leak the
    /// target; writing it would corrupt the target.
    #[test]
    fn a_symlink_leaf_is_never_followed() {
        let dir = tempfile::TempDir::new().unwrap();
        let outside = dir.path().join("credentials");
        std::fs::write(&outside, b"AKIA-secret").unwrap();

        let export = dir.path().join("exports");
        std::fs::create_dir(&export).unwrap();
        let link = export.join("innocent.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        // Read.
        let err = open_read(&link).unwrap_err();
        assert!(is_symlink_refusal(&err), "expected ELOOP, got {err:?}");

        // Write, including the create form an OPEN(UNCHECKED4) uses —
        // O_CREAT without O_EXCL on an existing symlink is exactly the
        // case that used to silently truncate the target.
        let err = open(
            OpenOptions::new().read(true).write(true).create(true),
            &link,
        )
        .unwrap_err();
        assert!(is_symlink_refusal(&err), "expected ELOOP, got {err:?}");

        // And the target is untouched.
        assert_eq!(std::fs::read(&outside).unwrap(), b"AKIA-secret");
    }

    /// A dangling link is refused the same way — the O_CREAT form must
    /// not quietly create the target the link names. This is the arm a
    /// pre-check based on `try_exists` misses, since it follows.
    #[test]
    fn a_dangling_symlink_does_not_create_its_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("does-not-exist-yet");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = open(OpenOptions::new().write(true).create(true), &link).unwrap_err();
        assert!(is_symlink_refusal(&err), "expected ELOOP, got {err:?}");
        assert!(!target.exists(), "the refusal must not create the target");
    }

    /// Ordinary files are unaffected — this guard must be invisible to
    /// every legitimate open, or it would be reverted the first time it
    /// broke a workload.
    #[test]
    fn ordinary_files_open_normally() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("real.bin");
        std::fs::write(&p, b"hello").unwrap();

        let mut f = open(OpenOptions::new().read(true).write(true), &p).unwrap();
        f.write_all(b"!").unwrap();
        assert!(open_read(&p).is_ok());
        assert!(!leaf_is_symlink(&p));

        // Creating a fresh name still works.
        let fresh = dir.path().join("fresh.bin");
        assert!(open(OpenOptions::new().write(true).create(true), &fresh).is_ok());
        assert!(fresh.exists());
    }

    /// A symlinked DIRECTORY on the way in is still followed: only the
    /// leaf is refused. Legitimate in-export relative links keep
    /// working, which is why this is O_NOFOLLOW and not a blanket ban.
    #[test]
    fn a_symlinked_parent_directory_still_resolves() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real-dir");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("f.txt"), b"data").unwrap();
        let linked = dir.path().join("linked-dir");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        assert!(open_read(&linked.join("f.txt")).is_ok());
    }

    #[tokio::test]
    async fn the_async_door_refuses_the_same_leaf() {
        let dir = tempfile::TempDir::new().unwrap();
        let outside = dir.path().join("secret");
        std::fs::write(&outside, b"token").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let err = open_async(
            tokio::fs::OpenOptions::new().read(true).write(true).create(true),
            &link,
        )
        .await
        .unwrap_err();
        assert!(is_symlink_refusal(&err), "expected ELOOP, got {err:?}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"token");
    }
}
