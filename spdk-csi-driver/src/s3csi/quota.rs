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
//! costs only what the tenant has written, so the sum of the ceilings on
//! a node may exceed the node's disk. Has written, not holds: nothing
//! discards, so a block the tenant frees stays allocated in the backing
//! file until unpublish (measured: write-and-delete cycles held their
//! high-water mark and evicted pods on an 8 GiB root). That is the same
//! bargain `emptyDir` sizeLimit made. The ceiling bounds one tenant's
//! blast radius; it is not a reservation.
//!
//! THE SYNCER'S RESERVE (2026-10-26, kind S18). The syncer keeps its
//! bookkeeping in `.flint-sync/` inside this same tree: the intent
//! journal, the baseline, the conflict record, the drain's attestation.
//! A tenant that filled its ceiling therefore stopped every barrier at
//! its first journal write, and at the pod's delete the drain met ENOSPC
//! on `intent.json` too: nothing it wrote last was published, the worker
//! stood Terminating for its whole grace, and the image stayed mounted on
//! the node. The two cannot be told apart by uid — the syncer runs as
//! the workspace's — so they are told apart by GROUP: the image reserves
//! 1/16 of its blocks (`-m 6.25`) for `RESERVE_GID` (`resgid=`, every
//! mount), which only the worker pod carries (`worker::build_pod`). The
//! image is sized up by the same 1/16, so the tenant still gets the
//! ceiling it declared (less the filesystem's own overhead, as before).
//! The syncer holds tree CONTENT it writes to what the tenant could
//! still write (`FLINT_SYNC_TREE_RESERVE`), so only its bookkeeping
//! reaches the reserve. 1/16 is 1 KiB for every inode the image has (one
//! per 16 KiB), several times what a baseline entry costs. Inodes are not
//! reserved: a tenant that spends every inode still stops the syncer.
//!
//! OFF by default (chart `workers.quota`, 2026-09-15): the image costs
//! speed a plain directory does not — buffered writes 0.57x on local
//! NVMe, fsync-each writes ~0.6x on any disk, two journals per fsync
//! (docs/plans/flint-lean-loop-image-perf-2026-09-15.md). A node that runs
//! with it off enforces `sizeLimitGib` with nothing.
//!
//! Everything here is Linux. The non-Linux build keeps the signatures so
//! the crate's unit tests still run on macOS, and every call refuses.

use std::path::{Path, PathBuf};

/// The image backing one volume's tree, beside its state file.
pub fn image_path(dir: &Path) -> PathBuf {
    dir.join("tree.img")
}

/// The group the image's reserved blocks belong to: the lean worker
/// pod's only supplementary group, and no tenant's.
pub const RESERVE_GID: u32 = 65530;

/// The share of the image reserved for `RESERVE_GID`, as `mkfs.ext4 -m`
/// takes it: 1/16.
pub const RESERVE_PCT: &str = "6.25";

/// The image's apparent size for a `gib` ceiling: grown by the reserve, so
/// the tenant's share is the ceiling, rounded up to a whole MiB.
pub fn image_bytes(gib: u64) -> u64 {
    (gib << 30).saturating_mul(16).div_ceil(15).next_multiple_of(1 << 20)
}

/// The loop mount's options. `nodev,nosuid` because checkout restores
/// the manifest's mode bits, setuid and setgid included, and the tenant
/// bind is cloned from this mount (`fuse::bind_mount` adds them again).
/// `resgid` on every mount, `ensure`'s and `remount`'s: ext4 keeps the
/// group in the mount, not in the image (`RESERVE_GID`).
pub const LOOP_OPTS: &str = "loop,nodev,nosuid,noatime,resgid=65530";

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
            f.set_len(image_bytes(gib)).map_err(|e| format!("size {} for {gib} GiB: {e}", img.display()))?;
            drop(f);
            // lazy init keeps mkfs off the critical path of a publish:
            // the kernel finishes the tables in the background, and a
            // fresh image has nothing to recover.
            run(
                "mkfs.ext4",
                Command::new("mkfs.ext4")
                    .args(["-F", "-q", "-m", RESERVE_PCT, "-E", "lazy_itable_init=1,lazy_journal_init=1"])
                    .arg(&img),
            )
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&img);
            })?;
        }
        std::fs::create_dir_all(tree).map_err(|e| format!("tree {}: {e}", tree.display()))?;
        run("mount -o loop", Command::new("mount").args(["-o", LOOP_OPTS]).arg(&img).arg(tree)).inspect_err(
            |_| {
                if fresh {
                    let _ = std::fs::remove_file(&img);
                }
            },
        )?;
        chown_tree(tree, uid, gid)?;
        // mkfs.ext4 leaves a root-owned `lost+found` (mode 0700) in the
        // filesystem root. That root IS the tenant's workspace, and the
        // syncer walks it as the app's uid: it hits a directory it
        // cannot enter and every barrier dies `EACCES`, so nothing is
        // ever published. Measured on the kind rig — a plain-directory
        // tree never had this, and a leg that only WRITES into the
        // workspace never walks it, so the quota's own leg missed it.
        //
        // Removing it is safe here: fsck recreates `lost+found` if it
        // ever needs one, and this image lives and dies with one volume.
        // It would also be an entry in the tenant's namespace that they
        // did not create and cannot read.
        if let Err(e) = std::fs::remove_dir(tree.join("lost+found")) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("remove lost+found from {}: {e}", tree.display()));
            }
        }
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

    /// Unmount the ceiling, keeping the image: the tree's bytes stay
    /// where they are. This is the half `preserve_undrained` uses, and
    /// what lets the volume directory be renamed (a directory with a
    /// mount under it cannot be).
    pub fn unmount_tree(tree: &Path) -> Result<(), String> {
        for _ in 0..8 {
            if !super::super::fuse::is_mountpoint(tree).unwrap_or(false) {
                break;
            }
            super::super::fuse::unmount(tree, true).map_err(|e| format!("umount {}: {e}", tree.display()))?;
        }
        if super::super::fuse::is_mountpoint(tree).unwrap_or(false) {
            return Err(format!("{} is still a mount point after 8 unmounts", tree.display()));
        }
        Ok(())
    }

    /// Mount an EXISTING image back at its tree, with no mkfs and no
    /// chown: the recovery for a published volume whose loop mount is
    /// gone (a target lost under a live pod). A missing image is an
    /// error, never something to create here.
    pub fn remount(dir: &Path, tree: &Path) -> Result<(), String> {
        let img = image_path(dir);
        if !img.is_file() {
            return Err(format!("{} does not exist; nothing to remount", img.display()));
        }
        if super::super::fuse::is_mountpoint(tree).unwrap_or(false) {
            return Ok(());
        }
        std::fs::create_dir_all(tree).map_err(|e| format!("tree {}: {e}", tree.display()))?;
        run("mount -o loop", Command::new("mount").args(["-o", LOOP_OPTS]).arg(&img).arg(tree))
    }

    /// Unmount the ceiling and reclaim its blocks. Best effort by
    /// design: this runs on the teardown path, where a partially built
    /// volume is normal and a missing image is not a failure.
    pub fn teardown(tree: &Path, img: &Path) -> Result<(), String> {
        unmount_tree(tree)?;
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
    pub fn unmount_tree(_: &Path) -> Result<(), String> {
        unsupported("unmount_tree")
    }
    pub fn remount(_: &Path, _: &Path) -> Result<(), String> {
        unsupported("remount")
    }
    pub fn teardown(_: &Path, _: &Path) -> Result<(), String> {
        unsupported("teardown")
    }
}

pub use imp::{chown_tree, ensure, remount, teardown, unmount_tree};

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

    #[test]
    fn the_loop_mount_is_nodev_and_nosuid() {
        let opts: Vec<&str> = LOOP_OPTS.split(',').collect();
        for o in ["loop", "nodev", "nosuid"] {
            assert!(opts.contains(&o), "{o} missing from {LOOP_OPTS}");
        }
    }

    #[test]
    fn the_reserve_is_the_syncers_group_and_comes_on_top_of_the_ceiling() {
        let resgid = format!("resgid={RESERVE_GID}");
        assert!(LOOP_OPTS.split(',').any(|o| o == resgid), "{resgid} missing from {LOOP_OPTS}");
        let pct: f64 = RESERVE_PCT.parse().unwrap();
        for gib in [1u64, 2, 10, 100] {
            let img = image_bytes(gib);
            assert_eq!(img % (1 << 20), 0, "{gib} GiB: not a whole MiB");
            let tenant = img as f64 * (1.0 - pct / 100.0);
            assert!(tenant >= (gib << 30) as f64, "{gib} GiB: the reserve came out of the ceiling ({tenant} bytes left)");
            assert!(tenant < ((gib << 30) + (2 << 20)) as f64, "{gib} GiB: the tenant gets more than it declared ({tenant})");
        }
    }

    /// Both mounts, `ensure`'s and `remount`'s, as the kernel reports
    /// them. Needs root, mkfs.ext4 and a loop device; it runs in a
    /// private mount namespace on its own thread (the `mount` child
    /// inherits it), so nothing reaches the machine's mount table.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_tree_is_mounted_nodev_and_nosuid() {
        use nix::mount::{mount, umount2, MntFlags, MsFlags};
        if !nix::unistd::geteuid().is_root() {
            eprintln!("SKIPPED: needs root for a loop mount");
            return;
        }
        // SAFETY: unshare(2) of this test thread's own mount namespace.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, 0, "unshare: {}", std::io::Error::last_os_error());
        mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>).unwrap();

        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        let tree = dir.join("tree");
        let opts = || {
            let info = std::fs::read_to_string("/proc/thread-self/mountinfo").unwrap();
            let want = tree.to_string_lossy().into_owned();
            info.lines()
                .filter(|l| l.split(' ').nth(4) == Some(want.as_str()))
                .last()
                .and_then(|l| l.split(' ').nth(5).map(str::to_string))
                .unwrap_or_else(|| panic!("{want} is not a mount point:\n{info}"))
        };
        let check = |opts: String| {
            let has = |o: &str| opts.split(',').any(|x| x == o);
            assert!(has("nodev") && has("nosuid"), "{opts}");
        };

        ensure(&dir, &tree, 1, 1000, 1000).unwrap();
        check(opts());
        umount2(&tree, MntFlags::empty()).unwrap();
        remount(&dir, &tree).unwrap();
        check(opts());
        let _ = umount2(&tree, MntFlags::MNT_DETACH);
    }

    /// S18: the tenant fills its ceiling and is refused there; the
    /// syncer's group still writes; the tenant still cannot. Both as uid
    /// 1000, told apart by the supplementary group alone (`setpriv`).
    /// Needs root, mkfs.ext4, setpriv and a loop device.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_full_tree_leaves_the_reserve_to_the_syncers_group() {
        use nix::mount::{mount, umount2, MntFlags, MsFlags};
        use std::os::unix::fs::PermissionsExt;
        if !nix::unistd::geteuid().is_root() {
            eprintln!("SKIPPED: needs root for a loop mount");
            return;
        }
        // SAFETY: unshare(2) of this test thread's own mount namespace.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, 0, "unshare: {}", std::io::Error::last_os_error());
        mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>).unwrap();

        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tree = dir.join("tree");
        ensure(&dir, &tree, 1, 1000, 1000).unwrap();
        let as_1000 = |groups: &str, script: String| {
            std::process::Command::new("setpriv")
                .args(["--reuid=1000", "--regid=1000", groups, "--", "sh", "-c", &script])
                .output()
                .expect("setpriv")
        };
        let t_ = tree.display();

        let fill = as_1000("--clear-groups", format!("dd if=/dev/zero of={t_}/fill bs=1M 2>&1"));
        let said = String::from_utf8_lossy(&fill.stdout).into_owned();
        assert!(said.contains("No space left on device"), "the tenant's fill did not stop at a full disk: {said}");
        let filled = std::fs::metadata(tree.join("fill")).unwrap().len();
        assert!(filled < 1 << 30, "the tenant wrote past the 1 GiB it declared: {filled}");
        assert!(filled > (1u64 << 30) * 9 / 10, "the reserve came out of the tenant's ceiling: {filled}");

        let mine = as_1000(&format!("--groups={RESERVE_GID}"), format!("head -c 4194304 /dev/zero > {t_}/intent.json"));
        assert!(mine.status.success(), "the syncer's group could not write into a full tree: {}", String::from_utf8_lossy(&mine.stderr));
        assert_eq!(std::fs::metadata(tree.join("intent.json")).unwrap().len(), 4 << 20);

        let more = as_1000("--clear-groups", format!("head -c 65536 /dev/zero > {t_}/more"));
        assert!(!more.status.success(), "the reserve let the tenant past its ceiling");
        let _ = umount2(&tree, MntFlags::MNT_DETACH);
    }
}
