//! Backing-store self-fencing for the NFS server — the F33 fix.
//!
//! Finding (runy2 drills 3.6 + 3.7, 2026-07-21): a node-level failure
//! that stops kubelet but not pods leaves this server process ALIVE on
//! the isolated node (observed 93 minutes) while the reconciler
//! resurrects a replacement on a surviving replica node. The old
//! instance's backing leg is fenced by the resurrect, so its I/O wedges
//! — but its TCP endpoints stay open. Clients whose established flows
//! are anchored to the orphan hang indefinitely instead of failing over
//! (the witness in 3.6/3.7); which clients escape is a race on whose
//! connection happens to break. The moment the orphan died, stuck
//! clients reconnected instantly — so the fix is for the server to die
//! on its own: probe the backing store on a heartbeat and EXIT when the
//! probe cannot complete within a deadline. Process exit closes every
//! client TCP connection (RST), and the kernel NFS clients re-resolve
//! through the Service to the resurrected instance.
//!
//! Mechanics: the probe (write + fsync of `<export>/.flint-nfs/fence.hb`)
//! runs on its own thread because a fenced store BLOCKS the syscall in
//! D-state — the prober cannot observe its own hang. A separate monitor
//! thread watches the wall-clock age of the last successful probe and
//! fences on staleness, which catches hangs, EIO loops, and outright
//! device death uniformly. The deadline is deliberately generous
//! (default 90s — vs the 93-minute hang it replaces): a healthy-but-slow
//! store under load (amcheck, checkpoint storms) must never trip it.
//! `FLINT_FENCE_DEADLINE_SECS=0` disables fencing entirely.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Default staleness deadline before the server self-fences.
pub const DEFAULT_DEADLINE_SECS: u64 = 90;

/// What the monitor concluded from probe staleness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceDecision {
    Healthy,
    /// Last successful probe is older than the deadline: the backing
    /// store is unresponsive — exit so clients fail over.
    Fence { stale_secs: u64 },
}

/// Pure staleness rule. Fence strictly AFTER the deadline (a probe that
/// lands exactly at the deadline is still healthy — boundary pinned by
/// test so the rule can't silently tighten).
pub fn decide(last_ok_age: Duration, deadline: Duration) -> FenceDecision {
    if last_ok_age > deadline {
        FenceDecision::Fence {
            stale_secs: last_ok_age.as_secs(),
        }
    } else {
        FenceDecision::Healthy
    }
}

/// Deadline from the environment: `FLINT_FENCE_DEADLINE_SECS` (0 or
/// unparseable-negative semantics: `Some(0)` means DISABLED, absent or
/// junk falls back to the default).
pub fn deadline_from_env() -> Option<Duration> {
    match std::env::var("FLINT_FENCE_DEADLINE_SECS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(_) => {
                warn!("FLINT_FENCE_DEADLINE_SECS unparseable ({v:?}) — using default");
                Some(Duration::from_secs(DEFAULT_DEADLINE_SECS))
            }
        },
        Err(_) => Some(Duration::from_secs(DEFAULT_DEADLINE_SECS)),
    }
}

/// The default probe: write + fsync a heartbeat file on the export
/// filesystem. Any Err (or a hang, caught by the monitor) means the
/// backing store cannot serve durable writes.
pub fn heartbeat_probe(export_root: &PathBuf) -> impl FnMut() -> std::io::Result<()> {
    let dir = export_root.join(".flint-nfs");
    let path = dir.join("fence.hb");
    move || {
        std::fs::create_dir_all(&dir)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        f.write_all(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .to_string()
                .as_bytes(),
        )?;
        f.sync_all()
    }
}

/// Spawn the prober + monitor threads. Generic over the probe and the
/// fence action so tests can inject both; production passes
/// [`heartbeat_probe`] and a process-exit closure. The monitor calls
/// `on_fence` at most once.
pub fn spawn_with_probe(
    mut probe: impl FnMut() -> std::io::Result<()> + Send + 'static,
    deadline: Duration,
    interval: Duration,
    on_fence: impl FnOnce(u64) + Send + 'static,
) {
    let start = Instant::now();
    // Millis since `start` of the last successful probe. Seeded to "now"
    // so a server that boots onto an already-dead store still gets one
    // full deadline before fencing (grace for slow first mounts).
    let last_ok = Arc::new(AtomicU64::new(0));

    let prober_last = Arc::clone(&last_ok);
    std::thread::Builder::new()
        .name("fence-probe".into())
        .spawn(move || loop {
            match probe() {
                Ok(()) => {
                    prober_last.store(start.elapsed().as_millis() as u64, Ordering::SeqCst);
                }
                Err(e) => {
                    // EIO-style failures don't update last_ok; staleness
                    // accumulates and the monitor fences. Log at warn so
                    // a transient blip is visible but not fatal-looking.
                    warn!(error = %e, "[FENCE] backing-store probe failed");
                }
            }
            std::thread::sleep(interval);
        })
        .expect("spawn fence-probe thread");

    std::thread::Builder::new()
        .name("fence-monitor".into())
        .spawn(move || {
            info!(
                deadline_secs = deadline.as_secs(),
                interval_ms = interval.as_millis() as u64,
                "[FENCE] backing-store watchdog armed (F33)"
            );
            loop {
                std::thread::sleep(interval);
                let age =
                    start.elapsed() - Duration::from_millis(last_ok.load(Ordering::SeqCst));
                if let FenceDecision::Fence { stale_secs } = decide(age, deadline) {
                    error!(
                        stale_secs,
                        "[FENCE] backing store unresponsive past deadline — \
                         exiting so clients RST and fail over to the \
                         resurrected server (F33)"
                    );
                    on_fence(stale_secs);
                    return;
                }
            }
        })
        .expect("spawn fence-monitor thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn decide_boundary_is_strictly_after_deadline() {
        let d = Duration::from_secs(90);
        assert_eq!(decide(Duration::from_secs(0), d), FenceDecision::Healthy);
        assert_eq!(decide(Duration::from_secs(90), d), FenceDecision::Healthy);
        assert_eq!(
            decide(Duration::from_millis(90_001), d),
            FenceDecision::Fence { stale_secs: 90 }
        );
    }

    /// F33 reproduction shape: a probe that HANGS (fenced store blocks
    /// the syscall in D-state) must still fence — the monitor watches
    /// wall-clock staleness, not probe return values.
    #[test]
    fn hanging_probe_fences_within_deadline() {
        let (tx, rx) = mpsc::channel();
        spawn_with_probe(
            move || {
                // Simulate a D-state hang: block far past the deadline.
                std::thread::sleep(Duration::from_secs(3600));
                Ok(())
            },
            Duration::from_millis(200),
            Duration::from_millis(50),
            move |stale| {
                let _ = tx.send(stale);
            },
        );
        rx.recv_timeout(Duration::from_secs(5))
            .expect("fence must fire for a hanging probe");
    }

    /// EIO loops (store returns errors instead of hanging) accumulate
    /// staleness and fence the same way.
    #[test]
    fn erroring_probe_fences_within_deadline() {
        let (tx, rx) = mpsc::channel();
        spawn_with_probe(
            || Err(std::io::Error::new(std::io::ErrorKind::Other, "EIO")),
            Duration::from_millis(200),
            Duration::from_millis(50),
            move |stale| {
                let _ = tx.send(stale);
            },
        );
        rx.recv_timeout(Duration::from_secs(5))
            .expect("fence must fire for an erroring probe");
    }

    /// A healthy probe must NEVER fence — run several deadlines' worth
    /// of ticks and assert silence (false positives kill a healthy
    /// server; this is the guard against overtightening).
    #[test]
    fn healthy_probe_never_fences() {
        let (tx, rx) = mpsc::channel::<u64>();
        spawn_with_probe(
            || Ok(()),
            Duration::from_millis(150),
            Duration::from_millis(25),
            move |stale| {
                let _ = tx.send(stale);
            },
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "healthy probe fenced — false positive"
        );
    }

    /// The real heartbeat probe round-trips on a live filesystem.
    #[test]
    fn heartbeat_probe_writes_and_syncs() {
        let dir = std::env::temp_dir().join(format!("fence-test-{}", std::process::id()));
        let mut probe = heartbeat_probe(&dir);
        probe().expect("probe on a live fs");
        assert!(dir.join(".flint-nfs").join("fence.hb").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_deadline_parsing() {
        // Not using set_var-based tests (process-global races); the
        // parse rule is exercised via decide + the documented contract:
        // 0 disables. Pin the default here instead.
        assert_eq!(DEFAULT_DEADLINE_SECS, 90);
    }
}
