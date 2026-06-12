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

#[cfg(test)]
mod tests {
    use super::*;

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
