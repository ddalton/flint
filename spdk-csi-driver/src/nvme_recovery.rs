//! Graceful recovery from an spdk-tgt hard stop/restart.
//!
//! When the NVMe-oF *target* server (spdk-tgt) hard-stops and restarts, its
//! lvstore auto-reloads but its subsystems (the exports) are gone, and every
//! consumer's kernel initiator controller is left dead against a vanished
//! target. Three coordinated mechanisms make this recover gracefully; the
//! pure logic + policy lives here (unit-tested), the I/O side-effects live in
//! `node_agent.rs`:
//!
//! * **#1 export reconcile-on-loss** — the node tracks the NQNs it exports;
//!   when SPDK is missing any of them (target lost/restarted) the periodic
//!   target reconcile is run *immediately* rather than waiting out its tick,
//!   so the subsystems reappear fast and the client can reconnect. See
//!   [`missing_exports`].
//! * **#2 survivable reconnect** — the kernel `nvme connect` is issued with an
//!   explicit `ctrl-loss-tmo` + `reconnect-delay` so the initiator keeps a
//!   controller reconnecting across a target bounce and auto-restores I/O
//!   when #1 brings the subsystem back — instead of the kernel default
//!   giving up. See [`ReconnectPolicy`].
//! * **#3 disconnect-before-reuse** — NodeStage only treats an existing
//!   controller as usable when it is `live`; a stale/dead one is disconnected
//!   and reconnected fresh instead of remounting the dead device (which
//!   otherwise CrashLoops the consumer). See [`controller_state_is_live`].

use std::collections::HashSet;

/// #2: kernel NVMe-oF initiator reconnect policy (`nvme connect` options).
///
/// `ctrl_loss_tmo_secs` is how long the kernel keeps a controller
/// reconnecting before giving up and failing I/O with EIO (`-1` = never give
/// up). `reconnect_delay_secs` is the retry interval. The default is
/// long-but-finite: long enough to ride out an spdk-tgt restart + export
/// reconcile (#1) transparently, finite so a genuinely-dead volume still
/// eventually clears (the D-state that `mount_util` bounds relies on this).
/// Both tunable via env for operators who want infinite (internal pNFS that
/// must always recover) or a shorter bound (app RWO wanting faster EIO).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub ctrl_loss_tmo_secs: i64,
    pub reconnect_delay_secs: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        // 30 min of reconnecting at 5s intervals (~360 attempts).
        Self { ctrl_loss_tmo_secs: 1800, reconnect_delay_secs: 5 }
    }
}

impl ReconnectPolicy {
    /// Reads `FLINT_NVME_CTRL_LOSS_TMO` (seconds, or `-1` for infinite) and
    /// `FLINT_NVME_RECONNECT_DELAY` (seconds); unset/garbage → the defaults.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Env-lookup seam so the parsing is unit-testable without touching the
    /// process environment.
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(get: F) -> Self {
        let d = Self::default();
        let ctrl_loss_tmo_secs = get("FLINT_NVME_CTRL_LOSS_TMO")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|&v| v >= -1)
            .unwrap_or(d.ctrl_loss_tmo_secs);
        let reconnect_delay_secs = get("FLINT_NVME_RECONNECT_DELAY")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&v| v >= 1)
            .unwrap_or(d.reconnect_delay_secs);
        Self { ctrl_loss_tmo_secs, reconnect_delay_secs }
    }

    /// The `nvme connect` argument fragment for this policy.
    pub fn connect_args(&self) -> Vec<String> {
        vec![
            "--ctrl-loss-tmo".to_string(),
            self.ctrl_loss_tmo_secs.to_string(),
            "--reconnect-delay".to_string(),
            self.reconnect_delay_secs.to_string(),
        ]
    }
}

/// #3: a kernel NVMe controller (`/sys/class/nvme/nvmeX/state`) is safe to
/// REUSE for a mount only when it is `live`. Every other state —
/// `connecting`, `resetting`, `deleting`, `new`, `dead` — is stale for
/// NodeStage: the device node may still exist but I/O to it is wedged, so the
/// controller must be disconnected and reconnected fresh rather than
/// remounted. Deterministic and case-insensitive.
pub fn controller_state_is_live(state: &str) -> bool {
    state.trim().eq_ignore_ascii_case("live")
}

/// #1: which of the NQNs this node believes it exports are not fully served
/// by SPDK — i.e. absent, OR present but INCOMPLETE. `satisfied` must
/// contain only NQNs whose subsystem is usable end to end (see
/// [`subsystem_is_satisfied`]); a subsystem re-created after an spdk-tgt
/// restart but still missing its namespace (the lvol bdev hadn't reloaded
/// when the re-export ran) is NOT satisfied, so it is returned here and the
/// convergent re-export runs again until it completes. Order unspecified.
pub fn missing_exports(registered: &HashSet<String>, satisfied: &HashSet<String>) -> Vec<String> {
    registered.difference(satisfied).cloned().collect()
}

/// #1: an SPDK subsystem is only a usable target when it has at least one
/// namespace (the block device) AND at least one listener. A subsystem that
/// exists with neither — the partial state a post-restart re-export leaves
/// if the lvol bdev wasn't ready for `add_ns` — must NOT count as present,
/// or the loss-detector stops one convergence short and the client hangs
/// `connecting` against an empty target.
pub fn subsystem_is_satisfied(has_namespaces: bool, has_listeners: bool) -> bool {
    has_namespaces && has_listeners
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_policy_defaults_and_args() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.ctrl_loss_tmo_secs, 1800);
        assert_eq!(p.reconnect_delay_secs, 5);
        assert_eq!(
            p.connect_args(),
            vec!["--ctrl-loss-tmo", "1800", "--reconnect-delay", "5"]
        );
    }

    #[test]
    fn reconnect_policy_env_overrides() {
        let env = |k: &str| match k {
            "FLINT_NVME_CTRL_LOSS_TMO" => Some("-1".to_string()),
            "FLINT_NVME_RECONNECT_DELAY" => Some("10".to_string()),
            _ => None,
        };
        let p = ReconnectPolicy::from_lookup(env);
        assert_eq!(p.ctrl_loss_tmo_secs, -1); // infinite: never give up
        assert_eq!(p.reconnect_delay_secs, 10);
        assert_eq!(p.connect_args()[1], "-1");
    }

    #[test]
    fn reconnect_policy_rejects_garbage_and_out_of_range() {
        let env = |k: &str| match k {
            "FLINT_NVME_CTRL_LOSS_TMO" => Some("-5".to_string()), // < -1 invalid
            "FLINT_NVME_RECONNECT_DELAY" => Some("0".to_string()), // < 1 invalid
            _ => None,
        };
        let p = ReconnectPolicy::from_lookup(env);
        assert_eq!(p, ReconnectPolicy::default());
        // Non-numeric also falls back.
        let p2 = ReconnectPolicy::from_lookup(|_| Some("abc".to_string()));
        assert_eq!(p2, ReconnectPolicy::default());
    }

    #[test]
    fn only_live_is_reusable() {
        assert!(controller_state_is_live("live"));
        assert!(controller_state_is_live("  live\n"));
        assert!(controller_state_is_live("LIVE"));
        for stale in ["connecting", "resetting", "deleting", "new", "dead", ""] {
            assert!(!controller_state_is_live(stale), "{stale:?} must not be reusable");
        }
    }

    #[test]
    fn missing_exports_detects_target_loss() {
        let reg: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // All satisfied → nothing missing.
        let satisfied = reg.clone();
        assert!(missing_exports(&reg, &satisfied).is_empty());
        // spdk-tgt restarted, lost everything but the discovery subsystem.
        let satisfied: HashSet<String> =
            ["nqn.2014-08.org.nvmexpress.discovery"].iter().map(|s| s.to_string()).collect();
        let mut missing = missing_exports(&reg, &satisfied);
        missing.sort();
        assert_eq!(missing, vec!["a", "b", "c"]);
        // Partial loss.
        let satisfied: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(missing_exports(&reg, &satisfied), vec!["c".to_string()]);
    }

    #[test]
    fn incomplete_subsystem_is_not_satisfied() {
        // Present + namespace + listener → satisfied.
        assert!(subsystem_is_satisfied(true, true));
        // The post-restart partial states that must trigger another
        // convergent re-export (client would otherwise hang connecting):
        assert!(!subsystem_is_satisfied(false, true)); // no namespace (bdev not ready)
        assert!(!subsystem_is_satisfied(true, false)); // no listener
        assert!(!subsystem_is_satisfied(false, false));
    }
}
