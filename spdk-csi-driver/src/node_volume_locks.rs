// node_volume_locks.rs — Contract R2: one mutating owner per volume per
// layer, node-local half. A per-volume async lock held across the whole
// probe→mutate window closes the node-local TOCTOU (a consumer appearing
// between a chokepoint probe and the destructive RPC), which today is
// handled only by post-hoc compensation.
//
// Feasible as a process-global because every node-side actor — the CSI
// gRPC handlers, the node agent's HTTP handlers, and the background loops —
// shares one process (main.rs wires them over one Arc<SpdkCsiDriver>).
//
// NESTING RULE (load-bearing — read before adding an acquire site): the
// driver calls its OWN node agent over HTTP (call_node_agent to self), so
// an HTTP handler reached from a locked CSI path must NEVER acquire. Lock
// only at non-nesting entry points: NodeStage/NodeUnstage (main.rs),
// repair_data_path (the background repair entry), and force_unstage (driven
// by the controller's DeleteVolume, never from a locked path). The lock is
// NOT reentrant; a nested acquire on the same volume deadlocks until the
// acquire timeout fires.
//
// Kill switch: FLINT_VOLUME_LOCK=disabled — this sits on the CSI critical
// path, and a bug here wedges every attach on the node, so operators get a
// standing off-switch (the FLINT_F36C_GATE pattern). Hold times are bounded
// by the wave-1 RPC deadlines; the acquire wait is bounded below kubelet's
// ~2-minute CSI deadline so a stuck holder surfaces as a retryable error,
// not a consumed deadline.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn enabled() -> bool {
    !std::env::var("FLINT_VOLUME_LOCK").is_ok_and(|v| v.eq_ignore_ascii_case("disabled"))
}

fn acquire_budget() -> Duration {
    Duration::from_secs(
        std::env::var("FLINT_VOLUME_LOCK_ACQUIRE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(90),
    )
}

fn registry() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A held guard (None when the lock is disabled — callers treat both as
/// "proceed"). Entries persist in the registry for the volume's lifetime on
/// this node; the per-entry cost is one Arc'd mutex and nodes serve a small
/// number of volumes.
pub struct VolumeGuard {
    _guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

/// Serialize on `key` (callers pass the identity-normalized storage id so
/// the user-PV and backing-PV spellings of one volume contend on ONE lock).
/// Err = the acquire budget elapsed — surface as a retryable failure.
pub async fn acquire(key: &str) -> Result<VolumeGuard, String> {
    if !enabled() {
        return Ok(VolumeGuard { _guard: None });
    }
    let entry = {
        let mut map = registry().lock().expect("volume-lock registry poisoned");
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    match tokio::time::timeout(acquire_budget(), entry.lock_owned()).await {
        Ok(guard) => Ok(VolumeGuard { _guard: Some(guard) }),
        Err(_) => Err(format!(
            "volume lock for {} not acquired within {}s — another operation holds it \
             (bounded by RPC deadlines; retry)",
            key,
            acquire_budget().as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lock_serializes_and_acquire_times_out_bounded() {
        let g1 = acquire("test-vol-serialize").await.expect("first acquire");
        // Second acquire on the same key must wait; with a tiny budget it
        // times out with a retryable error rather than hanging.
        std::env::set_var("FLINT_VOLUME_LOCK_ACQUIRE_SECS", "1");
        let started = std::time::Instant::now();
        let second_err = acquire("test-vol-serialize")
            .await
            .err()
            .expect("second acquire must not succeed while held");
        assert!(started.elapsed() < Duration::from_secs(10), "bounded wait");
        assert!(second_err.contains("retry"));
        drop(g1);
        // Released → immediate success.
        let g3 = acquire("test-vol-serialize").await;
        assert!(g3.is_ok());
        std::env::remove_var("FLINT_VOLUME_LOCK_ACQUIRE_SECS");
    }

    #[tokio::test]
    async fn different_volumes_do_not_contend() {
        let _a = acquire("test-vol-a").await.expect("a");
        let b = acquire("test-vol-b").await;
        assert!(b.is_ok(), "independent keys are independent locks");
    }
}
