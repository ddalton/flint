//! A per-volume ceiling on the lean tree (design §5, §10.2 S18).
//!
//! `sizeLimitGib` on `FlintLeanWorkspace` promised a ceiling and, under
//! the webhook delivery, got one for free: the tree was an `emptyDir`,
//! so kubelet's own accounting evicted a pod that overran it. Under the
//! CSI delivery the tree is a plugin-owned DIRECTORY on the node's root
//! filesystem, and the field described nothing at all — a runaway
//! workspace's only limit was the node's disk, which it shares with the
//! kubelet, the container runtime and every other pod on the machine.
//! A field that names a limit and enforces none is worse than no field.
//!
//! The ceiling is now a filesystem. One sparse ext4 image per volume,
//! loop-mounted at the tree, so overrunning it is `ENOSPC` inside the
//! tenant's own `write(2)` — what any application already expects from
//! a full disk — and the bound holds by construction rather than by
//! anyone remembering to check. It is per-volume, so one workspace
//! cannot spend another's budget, and it is reclaimed at unpublish.
//!
//! Sparse matters: the image is `sizeLimitGib` in APPARENT size and
//! costs only what the tenant actually writes, so the sum of the
//! ceilings on a node may exceed the node's disk. That is the same
//! bargain `emptyDir` sizeLimit made. The ceiling bounds one tenant's
//! blast radius; it is not a reservation.
//!
//! Everything here is Linux. The non-Linux build keeps the signatures so
//! the crate's unit tests still run on macOS, and every call refuses.

use std::path::{Path, PathBuf};

/// The image backing one volume's tree, beside its state file.
pub fn image_path(dir: &Path) -> PathBuf {
    dir.join("tree.img")
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::process::Command;

    fn run(what: &str, cmd: &mut Command) -> Result<(), String> {
        match cmd.output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!(
                "{what} failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim().chars().take(300).collect::<String>()
            )),
            Err(e) => Err(format!("{what} could not run: {e} (is the plugin image missing e2fsprogs or util-linux?)")),
        }
    }

    /// Build the tree's ceiling, idempotently: an already-mounted tree is
    /// a retried `NodePublishVolume`, not an error.
    ///
    /// Order matters. The image is formatted only when this call created
    /// it — reformatting an image that survived a plugin restart would
    /// erase a live workspace — and the ownership is applied AFTER the
    /// mount, because a `chown` before it lands on the directory the
    /// mount then hides.
    pub fn ensure(dir: &Path, tree: &Path, gib: u64, uid: u32, gid: u32) -> Result<PathBuf, String> {
        let img = image_path(dir);
        if super::super::fuse::is_mountpoint(tree).unwrap_or(false) {
            return Ok(img);
        }
        let fresh = !img.exists();
        if fresh {
            let f = std::fs::File::create(&img).map_err(|e| format!("create {}: {e}", img.display()))?;
            f.set_len(gib << 30).map_err(|e| format!("size {} to {gib} GiB: {e}", img.display()))?;
            drop(f);
            // lazy init keeps mkfs off the critical path of a publish:
            // the kernel finishes the tables in the background, and a
            // fresh image has nothing to recover.
            run(
                "mkfs.ext4",
                Command::new("mkfs.ext4")
                    .args(["-F", "-q", "-m", "0", "-E", "lazy_itable_init=1,lazy_journal_init=1"])
                    .arg(&img),
            )
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&img);
            })?;
        }
        std::fs::create_dir_all(tree).map_err(|e| format!("tree {}: {e}", tree.display()))?;
        run("mount -o loop", Command::new("mount").args(["-o", "loop,noatime"]).arg(&img).arg(tree)).inspect_err(
            |_| {
                if fresh {
                    let _ = std::fs::remove_file(&img);
                }
            },
        )?;
        chown_tree(tree, uid, gid)?;
        Ok(img)
    }

    /// Ownership of the MOUNTED root: the syncer's uid, world-writable
    /// with the sticky bit so an app uid the CR does not name can still
    /// create files and only its owner can remove them (§3.5 step 6).
    pub fn chown_tree(tree: &Path, uid: u32, gid: u32) -> Result<(), String> {
        std::os::unix::fs::chown(tree, Some(uid), Some(gid))
            .and_then(|_| std::fs::set_permissions(tree, std::os::unix::fs::PermissionsExt::from_mode(0o1777)))
            .map_err(|e| format!("own {}: {e}", tree.display()))
    }

    /// Unmount the ceiling and reclaim its blocks. Best effort by
    /// design: this runs on the teardown path, where a partially built
    /// volume is normal and a missing image is not a failure.
    pub fn teardown(tree: &Path, img: &Path) -> Result<(), String> {
        for _ in 0..8 {
            if !super::super::fuse::is_mountpoint(tree).unwrap_or(false) {
                break;
            }
            super::super::fuse::unmount(tree, true).map_err(|e| format!("umount {}: {e}", tree.display()))?;
        }
        if super::super::fuse::is_mountpoint(tree).unwrap_or(false) {
            return Err(format!("{} is still a mount point after 8 unmounts", tree.display()));
        }
        match std::fs::remove_file(img) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", img.display())),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;
    fn unsupported<T>(what: &str) -> Result<T, String> {
        Err(format!("{what}: the lean tree quota is a loop-mounted image, which is Linux-only"))
    }
    pub fn ensure(_: &Path, _: &Path, _: u64, _: u32, _: u32) -> Result<PathBuf, String> {
        unsupported("ensure")
    }
    pub fn chown_tree(_: &Path, _: u32, _: u32) -> Result<(), String> {
        unsupported("chown_tree")
    }
    pub fn teardown(_: &Path, _: &Path) -> Result<(), String> {
        unsupported("teardown")
    }
}

pub use imp::{chown_tree, ensure, teardown};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_image_sits_beside_the_state_file_not_inside_the_tree() {
        // Inside the tree it would be a file the TENANT can see, and
        // deleting it would be deleting the filesystem it lives on.
        let dir = Path::new("/var/lib/kubelet/plugins/s3.csi.chert.us/volumes/csi-abc");
        let img = image_path(dir);
        assert_eq!(img, dir.join("tree.img"));
        assert!(!img.starts_with(dir.join("tree")), "the image must not live inside the tree it backs");
    }
}
