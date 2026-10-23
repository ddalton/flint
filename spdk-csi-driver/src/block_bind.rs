//! Raw block volumes (`volumeMode: Block`). NodeStage binds the device it
//! connected -- the kernel NVMe-oF namespace or the ublk disk, whichever
//! backend made it -- onto a file in the staging directory, and
//! NodePublish binds that file onto kubelet's target FILE. Publish never
//! names a device itself: it used to compute `/dev/ublkb<id>` whatever the
//! backend, so on the default nvmeof backend every Block PVC failed
//! ("ublk device ... not found", bench4 2026-10-21), and it created the
//! target as a directory, which a device cannot be bound onto.
//!
//! A path holds the device when it stats as a block device with the
//! device's number: the file Flint creates is a regular file until a bind
//! covers it, so that is the bind, and no mount table is read.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::mount_util::{bounded_mount, bounded_umount, MountOutcome};

/// The file in a block volume's staging directory that NodeStage binds the
/// device onto.
pub const STAGED_DEVICE: &str = "flint-block-device";

pub fn staged_device(staging_target_path: &str) -> PathBuf {
    Path::new(staging_target_path).join(STAGED_DEVICE)
}

/// What a bind target is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    Missing,
    /// The old publish made the target a directory.
    Directory,
    /// A file with no device bound onto it.
    Unbound,
    /// A block device, by its device number: a bind is in place.
    Device(u64),
}

pub fn held(path: &Path) -> std::io::Result<Held> {
    match std::fs::metadata(path) {
        Ok(m) if m.file_type().is_block_device() => Ok(Held::Device(m.rdev())),
        Ok(m) if m.is_dir() => Ok(Held::Directory),
        Ok(_) => Ok(Held::Unbound),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Held::Missing),
        Err(e) => Err(e),
    }
}

/// The next step that brings a target holding `held` to holding the device
/// numbered `want`.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Done,
    /// Another device is bound here (a stale bind): unbind it first.
    Unbind,
    RemoveDir,
    CreateFile,
    Bind,
}

pub fn next_step(held: Held, want: u64) -> Step {
    match held {
        Held::Device(rdev) if rdev == want => Step::Done,
        Held::Device(_) => Step::Unbind,
        Held::Directory => Step::RemoveDir,
        Held::Missing => Step::CreateFile,
        Held::Unbound => Step::Bind,
    }
}

/// Binds the block device at `device` (a device node, or a path a device is
/// bound onto) onto the file `target`, read-only when asked. Idempotent: a
/// target already holding this device is left as it is.
pub async fn bind_device(device: &str, target: &Path, readonly: bool) -> Result<(), String> {
    let want = match held(Path::new(device)) {
        Ok(Held::Device(rdev)) => rdev,
        Ok(other) => return Err(format!("{device} is not a block device ({other:?})")),
        Err(e) => return Err(format!("stat {device}: {e}")),
    };
    let shown = target.display().to_string();
    for _ in 0..6 {
        let now = held(target).map_err(|e| format!("stat {shown}: {e}"))?;
        match next_step(now, want) {
            Step::Done => return Ok(()),
            Step::Unbind => unbind_once(&shown).await?,
            Step::RemoveDir => std::fs::remove_dir(target)
                .map_err(|e| format!("{shown} is a directory and cannot be removed: {e}"))?,
            Step::CreateFile => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create {}: {e}", parent.display()))?;
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(target)
                    .map_err(|e| format!("create {shown}: {e}"))?;
            }
            Step::Bind => {
                let mut argv = vec!["--bind".to_string()];
                if readonly {
                    argv.extend(["-o".to_string(), "ro".to_string()]);
                }
                argv.extend([device.to_string(), shown.clone()]);
                match bounded_mount(argv, 30).await {
                    MountOutcome::Ok => {}
                    MountOutcome::Failed(e) => return Err(format!("bind {device} onto {shown}: {e}")),
                    MountOutcome::TimedOut => return Err(format!("bind {device} onto {shown} timed out")),
                }
            }
        }
    }
    Err(format!("{shown} does not hold {device} after 6 steps"))
}

/// Unbinds whatever device is bound onto `target` and removes the file.
/// A missing target is done.
pub async fn unbind_device(target: &Path) -> Result<(), String> {
    let shown = target.display().to_string();
    for _ in 0..6 {
        match held(target).map_err(|e| format!("stat {shown}: {e}"))? {
            Held::Missing => return Ok(()),
            Held::Device(_) => unbind_once(&shown).await?,
            Held::Directory => {
                return match std::fs::remove_dir(target) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        Err(format!("remove {shown}: {e}"))
                    }
                    _ => Ok(()),
                }
            }
            Held::Unbound => {
                return match std::fs::remove_file(target) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        Err(format!("remove {shown}: {e}"))
                    }
                    _ => Ok(()),
                }
            }
        }
    }
    Err(format!("{shown} still holds a device after 6 unbinds"))
}

async fn unbind_once(target: &str) -> Result<(), String> {
    if bounded_umount(target, false, 10).await || bounded_umount(target, true, 10).await {
        Ok(())
    } else {
        Err(format!("unmount {target} failed"))
    }
}

/// The size in bytes of the block device at `path`.
pub fn device_bytes(path: &Path) -> std::io::Result<u64> {
    use std::io::Seek;
    std::fs::File::open(path)?.seek(std::io::SeekFrom::End(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_converges_on_the_wanted_device() {
        assert_eq!(next_step(Held::Device(7), 7), Step::Done);
        // a stale bind of another device is never kept
        assert_eq!(next_step(Held::Device(8), 7), Step::Unbind);
        // the old publish's directory target
        assert_eq!(next_step(Held::Directory, 7), Step::RemoveDir);
        assert_eq!(next_step(Held::Missing, 7), Step::CreateFile);
        assert_eq!(next_step(Held::Unbound, 7), Step::Bind);
    }

    #[test]
    fn held_reads_the_path() {
        let dir = std::env::temp_dir().join(format!("flint-block-bind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(STAGED_DEVICE);
        assert_eq!(held(&file).unwrap(), Held::Missing);
        std::fs::write(&file, b"").unwrap();
        assert_eq!(held(&file).unwrap(), Held::Unbound);
        assert_eq!(held(&dir).unwrap(), Held::Directory);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(staged_device("/k/staging/pv-1"), PathBuf::from("/k/staging/pv-1/flint-block-device"));
    }

    // A device node is a device whether or not it is bound: /dev/null is a
    // character device, never taken for a block device.
    #[test]
    fn a_character_device_is_not_a_block_device() {
        assert_eq!(held(Path::new("/dev/null")).unwrap(), Held::Unbound);
    }

    #[tokio::test]
    async fn binding_refuses_a_source_that_is_not_a_block_device() {
        let err = bind_device("/dev/null", Path::new("/nonexistent/target"), false).await.unwrap_err();
        assert!(err.contains("is not a block device"), "{err}");
        let err = bind_device("/nonexistent/dev", Path::new("/nonexistent/target"), false).await.unwrap_err();
        assert!(err.contains("not a block device") || err.contains("stat"), "{err}");
    }

    // Real binds need root and a block device to spare:
    //   FLINT_TEST_BLOCK_DEVICE=/dev/loopN cargo test --lib block_bind -- --ignored
    // Stage binds the device onto the staging file, publish binds the staging
    // file onto the target (a bind of a bind), both converge when repeated,
    // and unbinding removes both files.
    #[tokio::test]
    #[ignore]
    async fn stage_then_publish_binds_the_device_through() {
        let dev = std::env::var("FLINT_TEST_BLOCK_DEVICE").expect("FLINT_TEST_BLOCK_DEVICE");
        let Held::Device(rdev) = held(Path::new(&dev)).unwrap() else { panic!("{dev} is not a block device") };
        let dir = std::env::temp_dir().join(format!("flint-block-real-{}", std::process::id()));
        let staged = staged_device(dir.join("staging").to_str().unwrap());
        let target = dir.join("publish").join("pod-uid");
        std::fs::create_dir_all(&target).unwrap(); // the old publish's directory target
        for _ in 0..2 {
            bind_device(&dev, &staged, false).await.unwrap();
            bind_device(staged.to_str().unwrap(), &target, true).await.unwrap();
        }
        assert_eq!(held(&staged).unwrap(), Held::Device(rdev));
        assert_eq!(held(&target).unwrap(), Held::Device(rdev));
        assert_eq!(device_bytes(&target).unwrap(), device_bytes(Path::new(&dev)).unwrap());
        unbind_device(&target).await.unwrap();
        unbind_device(&staged).await.unwrap();
        assert_eq!(held(&target).unwrap(), Held::Missing);
        assert_eq!(held(&staged).unwrap(), Held::Missing);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn unbinding_a_missing_or_unbound_target_cleans_up() {
        let dir = std::env::temp_dir().join(format!("flint-block-unbind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(STAGED_DEVICE);
        unbind_device(&file).await.unwrap();
        std::fs::write(&file, b"").unwrap();
        unbind_device(&file).await.unwrap();
        assert!(!file.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
