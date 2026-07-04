//! Bounded unmount: an `umount` whose backing device is dead blocks in
//! uninterruptible sleep, and one such hang inside a NodeUnstage froze the
//! entire node plugin — kubelet then got connection-refused for every
//! volume on the node (found by the 1.2.0 release gate, 2026-06-12).
//!
//! Three layers, each catching what the previous can't:
//! 1. `timeout -k` escalation: SIGTERM at the deadline, SIGKILL 5 s later —
//!    kills the common `TASK_KILLABLE` sleep that ignores TERM.
//! 2. The wait runs in `spawn_blocking` with a tokio-level deadline on the
//!    await: GNU `timeout` waits for its child even after signalling, so a
//!    hard-D-state child (SIGKILL undeliverable) would hang the wrapper
//!    itself. Abandoning the await parks one blocking-pool thread on the
//!    corpse but keeps the server serving; the kernel errors the stuck I/O
//!    when the initiator's `ctrl_loss_tmo` expires and the thread frees.
//! 3. Callers fall back to lazy (`-l`) unmount, which detaches the mount
//!    from the VFS without waiting for in-flight I/O.

use std::time::Duration;

/// argv for layer 1 (separated from the spawn for testability).
fn umount_argv(target: &str, lazy: bool, deadline_secs: u64) -> Vec<String> {
    let mut argv = vec![
        "-k".to_string(),
        "5".to_string(),
        deadline_secs.to_string(),
        "umount".to_string(),
    ];
    if lazy {
        argv.push("-l".to_string());
    }
    argv.push(target.to_string());
    argv
}

/// Unmount `target`, bounded as described in the module docs. Returns true
/// only on a confirmed clean unmount; a timeout at any layer is false (the
/// caller's mountpoint re-check and kubelet's retry own the outcome).
pub async fn bounded_umount(target: &str, lazy: bool, deadline_secs: u64) -> bool {
    let argv = umount_argv(target, lazy, deadline_secs);
    let join = tokio::task::spawn_blocking(move || {
        std::process::Command::new("timeout")
            .args(&argv)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    // +10 s: room for the TERM→KILL escalation (deadline + 5) plus reaping.
    match tokio::time::timeout(Duration::from_secs(deadline_secs + 10), join).await {
        Ok(Ok(success)) => success,
        Ok(Err(join_err)) => {
            eprintln!("⚠️ [MOUNT_UTIL] umount task failed to join: {}", join_err);
            false
        }
        Err(_) => {
            eprintln!(
                "⚠️ [MOUNT_UTIL] umount of {} unresponsive past {}s (likely D-state on a dead \
                 device) — abandoning the wait; the kernel clears it at ctrl_loss_tmo",
                target,
                deadline_secs + 10
            );
            false
        }
    }
}

/// Bounded global `sync`. After a LAZY unmount the filesystem is detached
/// from the VFS but still alive with dirty pages — tearing its backing
/// device down at that point loses every acked-but-unflushed write
/// (observed live 2026-06-12: a busy NFS-server export went lazy at
/// unstage and the volume came back with its server-side files rolled
/// back). `sync(2)` still reaches a lazily-detached filesystem, so one
/// bounded call between the lazy unmount and the device teardown flushes
/// whatever the backing device can still accept; on a dead device it
/// times out harmlessly (the data is unreachable either way).
pub async fn bounded_sync(deadline_secs: u64) -> bool {
    let join = tokio::task::spawn_blocking(move || {
        std::process::Command::new("timeout")
            .args(["-k", "5", &deadline_secs.to_string(), "sync"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    match tokio::time::timeout(Duration::from_secs(deadline_secs + 10), join).await {
        Ok(Ok(success)) => success,
        Ok(Err(join_err)) => {
            eprintln!("⚠️ [MOUNT_UTIL] sync task failed to join: {}", join_err);
            false
        }
        Err(_) => {
            eprintln!(
                "⚠️ [MOUNT_UTIL] sync unresponsive past {}s (dead backing device?) — abandoning",
                deadline_secs + 10
            );
            false
        }
    }
}

/// Verdict for a `timeout N mountpoint -q <path>` probe during teardown:
/// does the path need an unmount attempt?
///
/// Exit 0 = is a mountpoint → yes. Exit 1 = cleanly NOT a mountpoint →
/// no. ANYTHING else — 124 (`timeout` fired: mountpoint(1) blocked, the
/// signature of a dead hard NFS mount), killed-by-signal (`None`), or a
/// spawn failure mapped to `None` by the caller — means UNKNOWN, and
/// unknown MUST be treated as mounted: skipping the unmount on a live
/// dead mount returns success to kubelet, which then EBUSY-loops on the
/// pod directory forever and degrades the whole node (identity Phase-3
/// drill A finding, 2026-07-04). The `umount -l` this triggers is
/// non-blocking and fails harmlessly on a genuinely unmounted path.
pub fn mountpoint_probe_says_unmount(exit_code: Option<i32>) -> bool {
    !matches!(exit_code, Some(1))
}

/// Is this findmnt FSTYPE an NFS client mount? Audit finding L3: a
/// shared (RWX/ROX) volume publishes as an NFS mount on the consumer —
/// there is no node-side block device, so NodeExpand must be a clean
/// no-op (the nvme-resize path would otherwise chew on a `host:/export`
/// source string, error out, and leave kubelet retrying the resize
/// forever). Covers `nfs`, `nfs4`, and any future `nfs*` flavor.
pub fn fstype_is_nfs(fstype: &str) -> bool {
    fstype.trim().to_ascii_lowercase().starts_with("nfs")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drill-A truth table: only a clean "not a mountpoint" (exit 1)
    /// skips the unmount. Timeout 124 was previously read as "not
    /// mounted" — the exact misread that stranded a dead NFS mount.
    #[test]
    fn mountpoint_probe_truth_table() {
        assert!(mountpoint_probe_says_unmount(Some(0)), "mountpoint → unmount");
        assert!(!mountpoint_probe_says_unmount(Some(1)), "clean not-a-mountpoint → skip");
        assert!(mountpoint_probe_says_unmount(Some(124)), "probe TIMED OUT (dead NFS) → unmount");
        assert!(mountpoint_probe_says_unmount(Some(32)), "probe errored → unmount");
        assert!(mountpoint_probe_says_unmount(None), "killed by signal / spawn error → unmount");
    }

    /// L3 arm gate: only NFS-flavored fstypes take the NodeExpand no-op;
    /// block filesystems keep the resize path.
    #[test]
    fn fstype_nfs_detection() {
        assert!(fstype_is_nfs("nfs"));
        assert!(fstype_is_nfs("nfs4"));
        assert!(fstype_is_nfs(" nfs4\n"));
        assert!(!fstype_is_nfs("ext4"));
        assert!(!fstype_is_nfs("xfs"));
        assert!(!fstype_is_nfs(""));
    }

    #[test]
    fn argv_escalates_and_targets() {
        assert_eq!(
            umount_argv("/mnt/x", false, 10),
            vec!["-k", "5", "10", "umount", "/mnt/x"]
        );
        assert_eq!(
            umount_argv("/mnt/x", true, 10),
            vec!["-k", "5", "10", "umount", "-l", "/mnt/x"]
        );
    }

    #[tokio::test]
    async fn bounded_umount_returns_false_fast_on_nonexistent_target() {
        // umount of a non-mount exits non-zero immediately — well inside
        // the deadline; this also exercises the spawn_blocking plumbing.
        let start = std::time::Instant::now();
        assert!(!bounded_umount("/definitely/not/a/mountpoint", false, 5).await);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
