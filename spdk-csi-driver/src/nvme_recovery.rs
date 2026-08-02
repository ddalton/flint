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
    /// Seconds until QUEUED I/O starts failing while reconnect continues;
    /// 0 disables. See [`ReconnectPolicy::fast_io_fail_sysfs`] for why this
    /// is applied through sysfs rather than as a connect flag.
    pub fast_io_fail_tmo_secs: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        // 30 min of reconnecting at 5s intervals (~360 attempts), with I/O
        // failing after 20s. 20s matches LegTransportPolicy: long enough to
        // ride a target restart + export reconcile (#1), short enough that a
        // consumer faults instead of parking in D-state.
        Self { ctrl_loss_tmo_secs: 1800, reconnect_delay_secs: 5, fast_io_fail_tmo_secs: 20 }
    }
}

impl ReconnectPolicy {
    /// Reads `FLINT_NVME_CTRL_LOSS_TMO` (seconds, or `-1` for infinite),
    /// `FLINT_NVME_RECONNECT_DELAY` and `FLINT_NVME_FAST_IO_FAIL` (seconds,
    /// `0` to disable); unset/garbage → the defaults.
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
        let fast_io_fail_tmo_secs = get("FLINT_NVME_FAST_IO_FAIL")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(d.fast_io_fail_tmo_secs);
        Self { ctrl_loss_tmo_secs, reconnect_delay_secs, fast_io_fail_tmo_secs }
    }

    /// The `nvme connect` argument fragment for this policy.
    ///
    /// NOTE fast-io-fail is deliberately NOT here. `nvme connect` in the
    /// shipped image (nvme-cli 2.8) exposes only `--keep-alive-tmo` and
    /// `--ctrl-loss-tmo` — verified by running `nvme connect --help` inside
    /// the driver container on a live node. Passing an unknown flag would
    /// fail EVERY attach, so it goes through sysfs after connect instead.
    pub fn connect_args(&self) -> Vec<String> {
        vec![
            "--ctrl-loss-tmo".to_string(),
            self.ctrl_loss_tmo_secs.to_string(),
            "--reconnect-delay".to_string(),
            self.reconnect_delay_secs.to_string(),
        ]
    }

    /// The value to write to `/sys/class/nvme/<ctrl>/fast_io_fail_tmo`, or
    /// `None` when it must not be written.
    ///
    /// WHY THIS EXISTS (runay, 2026-08-02). F42 bounded queued I/O on
    /// `LegTransportPolicy` — the SPDK-side attach used for raid LEGS. The
    /// KERNEL initiator that a consumer's filesystem sits on comes from this
    /// policy, and it emitted no fast-io-fail at all. Measured on a live
    /// node: `ctrl_loss_tmo=1800 fast_io_fail_tmo=off`. So when spdk-tgt
    /// died under a mounted volume the controller went to `connecting` and
    /// QUEUED I/O for thirty minutes rather than failing it — `umount` parked
    /// in `blkdev_issue_flush` in D state, the pod could not terminate, its
    /// RWO volume could not detach, and `reboot` itself hung in `ksys_sync`
    /// on the same superblock. Leg I/O was bounded; consumer I/O was not.
    ///
    /// Refuses the combinations the kernel rejects, because a rejected write
    /// is silent and would leave the old `off` in place while looking fixed:
    /// the timeout must be positive, and with a finite `ctrl_loss_tmo` it
    /// must not exceed it (failing I/O later than the controller gives up is
    /// meaningless).
    pub fn fast_io_fail_sysfs(&self) -> Option<String> {
        if self.fast_io_fail_tmo_secs == 0 {
            return None;
        }
        if self.ctrl_loss_tmo_secs >= 0
            && self.fast_io_fail_tmo_secs > self.ctrl_loss_tmo_secs as u64
        {
            return None;
        }
        Some(self.fast_io_fail_tmo_secs.to_string())
    }
}

/// F42 (runac 2026-07-22): SPDK-initiator transport bounds for every fabric
/// attach whose bdev can serve I/O (raid legs, remote volumes, rejoin/copy
/// plumbing). Two concerns the old hardcoded `ctrlr_loss_timeout_sec: -1`
/// conflated:
///
/// * **identity survival** — the controller must keep reconnecting across a
///   target bounce (chaos drill 1u/B3: dropping the bdev cascades into the
///   ublk chain / raid teardown). `ctrlr_loss_timeout_sec: -1` stays.
/// * **bounded I/O** — queued I/O must FAIL after a bound so a dead leg
///   faults out of its raid and the survivor keeps serving. Without it, a
///   terminated storage node stalls every consumer write indefinitely and
///   the whole heal chain (monitor_raid_health → record_stale_replicas →
///   replace → catch-up) stays blind: the raid never sees an error, so it
///   reports online 2/2 forever (F42 — found live on runac; the data-plane
///   R5 violation). `fast_io_fail_timeout_sec` is exactly this split: legal
///   alongside infinite ctrlr-loss, fails queued I/O while reconnect
///   continues, never drops the bdev.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegTransportPolicy {
    /// -1 = reconnect forever (keep the bdev identity alive; B3).
    pub ctrlr_loss_timeout_sec: i64,
    pub reconnect_delay_sec: u64,
    /// Seconds until queued I/O starts failing while reconnect continues;
    /// 0 disables (pre-F42 behavior: I/O queues unboundedly).
    pub fast_io_fail_timeout_sec: u64,
}

impl Default for LegTransportPolicy {
    fn default() -> Self {
        // 20s: long enough to ride a target restart + export reconcile
        // (the #1 fast loss-detector re-exports within seconds), short
        // enough that a raid leg on a dead node faults before consumers
        // hit kernel-level hung-task territory.
        Self { ctrlr_loss_timeout_sec: -1, reconnect_delay_sec: 2, fast_io_fail_timeout_sec: 20 }
    }
}

impl LegTransportPolicy {
    /// Reads `FLINT_SPDK_FAST_IO_FAIL_SECS` (seconds; `0` disables the
    /// bound). Values below `reconnect_delay_sec` (except 0) are invalid
    /// per SPDK and fall back to the default. ctrlr-loss and delay are
    /// deliberate constants: -1 is load-bearing for B3, and every legal
    /// fast-io-fail works against it.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Env-lookup seam so the parsing is unit-testable without touching the
    /// process environment.
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(get: F) -> Self {
        let d = Self::default();
        let fast_io_fail_timeout_sec = get("FLINT_SPDK_FAST_IO_FAIL_SECS")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&v| v == 0 || v >= d.reconnect_delay_sec)
            .unwrap_or(d.fast_io_fail_timeout_sec);
        Self { fast_io_fail_timeout_sec, ..d }
    }

    /// Stamp the transport bounds onto a `bdev_nvme_attach_controller`
    /// `params` object (leaves every other field alone; omits
    /// fast_io_fail when disabled).
    pub fn apply(&self, attach_params: &mut serde_json::Value) {
        attach_params["ctrlr_loss_timeout_sec"] = serde_json::json!(self.ctrlr_loss_timeout_sec);
        attach_params["reconnect_delay_sec"] = serde_json::json!(self.reconnect_delay_sec);
        if self.fast_io_fail_timeout_sec > 0 {
            attach_params["fast_io_fail_timeout_sec"] =
                serde_json::json!(self.fast_io_fail_timeout_sec);
        }
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

/// P4 (runal/runak/runai 3.6e, 2026-07-28): global SPDK-initiator timeouts
/// that bound *dead-target detection*. A terminated instance is a TCP
/// blackhole — no RST — so the qpair never sees a transport error and never
/// enters the reset path where [`LegTransportPolicy`]'s `fast_io_fail` clock
/// runs. The raid kept the dead base configured for 116–176s (runak clean
/// 3.6e: degrade logged T0+176s, stale +176s, swap +186s — everything AFTER
/// the failure is fast) and the RWX ledger stalled 150–177s. RWO 2.5 never
/// stalled only because that shutdown produced an RST.
///
/// These are `bdev_nvme_set_options` fields — GLOBAL, and SPDK returns
/// -EPERM once any controller is attached, so they must be applied at
/// target bring-up before the first attach (agent startup, and the
/// baseline-collapse recovery that detects a tgt restart).
///
/// * `transport_ack_timeout` (exponent: 2^n ms) becomes TCP_USER_TIMEOUT on
///   the qpair socket — the KERNEL errors a blackholed connection once
///   retransmitted data goes unacked that long. 13 → ~8.2s.
/// * `timeout_us` + `action_on_timeout=reset` is the command-level watchdog
///   for the complementary failure (peer kernel ACKs but the target is
///   wedged). A spurious trip costs one reset/reconnect cycle, not data.
/// * `tcp_connect_timeout_ms` bounds each reconnect attempt so the retry
///   loop stays live against a blackholed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadTargetTimeouts {
    /// 2^n milliseconds of unacked TCP retransmission → socket error.
    /// 0 omits the field (kernel default: ~15 retries, 15+ minutes).
    pub transport_ack_timeout_exp: u8,
    /// Command timeout with action_on_timeout=reset; 0 omits both fields.
    pub io_timeout_secs: u64,
    /// Per-attempt connect bound for the reconnect loop; 0 omits.
    pub tcp_connect_timeout_ms: u64,
}

impl Default for DeadTargetTimeouts {
    fn default() -> Self {
        // 13 → 8192ms: an in-VPC RTT is sub-ms, so 8s of unacked
        // retransmission is unambiguous death, and 8s + fast_io_fail(20s)
        // lands the whole failure inside the P4 ≤60s stall budget. 30s
        // command timeout: an order of magnitude above any legitimate
        // lvol/raid I/O, well under the pre-fix 150s+ blindness.
        Self { transport_ack_timeout_exp: 13, io_timeout_secs: 30, tcp_connect_timeout_ms: 10_000 }
    }
}

impl DeadTargetTimeouts {
    /// `FLINT_SPDK_TRANSPORT_ACK_TIMEOUT_EXP` (0–31; 0 disables),
    /// `FLINT_SPDK_IO_TIMEOUT_SECS` (0 disables),
    /// `FLINT_SPDK_TCP_CONNECT_TIMEOUT_MS` (0 disables); garbage → default.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup<F: Fn(&str) -> Option<String>>(get: F) -> Self {
        let d = Self::default();
        Self {
            transport_ack_timeout_exp: get("FLINT_SPDK_TRANSPORT_ACK_TIMEOUT_EXP")
                .and_then(|v| v.trim().parse::<u8>().ok())
                .filter(|&v| v <= 31)
                .unwrap_or(d.transport_ack_timeout_exp),
            io_timeout_secs: get("FLINT_SPDK_IO_TIMEOUT_SECS")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(d.io_timeout_secs),
            tcp_connect_timeout_ms: get("FLINT_SPDK_TCP_CONNECT_TIMEOUT_MS")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(d.tcp_connect_timeout_ms),
        }
    }

    /// The `bdev_nvme_set_options` params object. `None` when every knob is
    /// disabled (nothing to send). The RPC merges over current options
    /// server-side, so only the configured fields are included.
    pub fn set_options_params(&self) -> Option<serde_json::Value> {
        let mut params = serde_json::Map::new();
        if self.transport_ack_timeout_exp > 0 {
            params.insert(
                "transport_ack_timeout".into(),
                serde_json::json!(self.transport_ack_timeout_exp),
            );
        }
        if self.io_timeout_secs > 0 {
            params.insert("timeout_us".into(), serde_json::json!(self.io_timeout_secs * 1_000_000));
            params.insert("action_on_timeout".into(), serde_json::json!("reset"));
        }
        if self.tcp_connect_timeout_ms > 0 {
            params
                .insert("tcp_connect_timeout_ms".into(), serde_json::json!(self.tcp_connect_timeout_ms));
        }
        if params.is_empty() { None } else { Some(serde_json::Value::Object(params)) }
    }
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

    /// The regression for the runay D-state wedge: queued I/O on the KERNEL
    /// initiator was unbounded (`fast_io_fail_tmo=off` measured live) while
    /// the SPDK-side legs were bounded by F42. A consumer's umount then
    /// parked in `blkdev_issue_flush` for the full 1800s ctrl_loss_tmo.
    #[test]
    fn the_kernel_initiator_bounds_queued_io_by_default() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.fast_io_fail_tmo_secs, 20);
        assert_eq!(
            p.fast_io_fail_sysfs().as_deref(),
            Some("20"),
            "a consumer's controller must fail I/O rather than queue it forever"
        );
        assert!(
            p.fast_io_fail_tmo_secs < p.ctrl_loss_tmo_secs as u64,
            "I/O must fail well before the controller gives up entirely"
        );
    }

    /// nvme-cli 2.8 in the shipped image has no fast-io-fail flag (verified
    /// with `nvme connect --help` on a live node), so passing one would fail
    /// EVERY attach. It must travel via sysfs, never in the argv.
    #[test]
    fn fast_io_fail_never_leaks_into_the_connect_arguments() {
        let p = ReconnectPolicy::default();
        for a in p.connect_args() {
            assert!(
                !a.contains("fast"),
                "nvme connect has no fast-io-fail option; `{a}` would break every attach"
            );
        }
    }

    /// A rejected sysfs write is silent, so a value the kernel would refuse
    /// must be refused here instead — otherwise the controller keeps `off`
    /// while everything looks configured.
    #[test]
    fn fast_io_fail_refuses_what_the_kernel_would_reject() {
        let disabled = ReconnectPolicy { fast_io_fail_tmo_secs: 0, ..Default::default() };
        assert_eq!(disabled.fast_io_fail_sysfs(), None, "0 means disabled");

        let too_late = ReconnectPolicy {
            ctrl_loss_tmo_secs: 10,
            fast_io_fail_tmo_secs: 20,
            ..Default::default()
        };
        assert_eq!(
            too_late.fast_io_fail_sysfs(),
            None,
            "failing I/O after the controller has already given up is meaningless"
        );

        let infinite = ReconnectPolicy { ctrl_loss_tmo_secs: -1, ..Default::default() };
        assert_eq!(
            infinite.fast_io_fail_sysfs().as_deref(),
            Some("20"),
            "bounded I/O alongside infinite reconnect is the whole point of the split"
        );
    }

    #[test]
    fn fast_io_fail_is_tunable_and_can_be_switched_off() {
        let env = |k: &str| match k {
            "FLINT_NVME_FAST_IO_FAIL" => Some("45".to_string()),
            _ => None,
        };
        assert_eq!(ReconnectPolicy::from_lookup(env).fast_io_fail_sysfs().as_deref(), Some("45"));

        let off = |k: &str| match k {
            "FLINT_NVME_FAST_IO_FAIL" => Some("0".to_string()),
            _ => None,
        };
        assert_eq!(ReconnectPolicy::from_lookup(off).fast_io_fail_sysfs(), None);
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
    fn leg_transport_defaults_bound_io_but_never_the_identity() {
        let p = LegTransportPolicy::default();
        assert_eq!(p.ctrlr_loss_timeout_sec, -1, "B3: identity must survive forever");
        assert_eq!(p.reconnect_delay_sec, 2);
        assert_eq!(p.fast_io_fail_timeout_sec, 20, "F42: I/O must be bounded by default");
    }

    #[test]
    fn leg_transport_apply_stamps_params_without_clobbering() {
        let mut params = serde_json::json!({
            "name": "nvme_x", "subnqn": "nqn.y", "trtype": "TCP"
        });
        LegTransportPolicy::default().apply(&mut params);
        assert_eq!(params["ctrlr_loss_timeout_sec"], -1);
        assert_eq!(params["reconnect_delay_sec"], 2);
        assert_eq!(params["fast_io_fail_timeout_sec"], 20);
        assert_eq!(params["name"], "nvme_x", "existing fields untouched");
        // Disabled bound → the param must be ABSENT (pre-F42 behavior),
        // not zero: SPDK rejects fast_io_fail < reconnect_delay.
        let mut params = serde_json::json!({ "name": "nvme_x" });
        let off = LegTransportPolicy { fast_io_fail_timeout_sec: 0, ..Default::default() };
        off.apply(&mut params);
        assert!(params.get("fast_io_fail_timeout_sec").is_none());
        assert_eq!(params["ctrlr_loss_timeout_sec"], -1);
    }

    #[test]
    fn leg_transport_env_override_and_validation() {
        // Operator tunes the bound.
        let p = LegTransportPolicy::from_lookup(|k| {
            (k == "FLINT_SPDK_FAST_IO_FAIL_SECS").then(|| "45".to_string())
        });
        assert_eq!(p.fast_io_fail_timeout_sec, 45);
        // 0 = explicit opt-out (unbounded queueing).
        let p = LegTransportPolicy::from_lookup(|k| {
            (k == "FLINT_SPDK_FAST_IO_FAIL_SECS").then(|| "0".to_string())
        });
        assert_eq!(p.fast_io_fail_timeout_sec, 0);
        // Below reconnect_delay (SPDK-invalid) and garbage → default.
        for bad in ["1", "abc", "-3"] {
            let p = LegTransportPolicy::from_lookup(|k| {
                (k == "FLINT_SPDK_FAST_IO_FAIL_SECS").then(|| bad.to_string())
            });
            assert_eq!(p, LegTransportPolicy::default(), "{bad:?} must fall back");
        }
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

    #[test]
    fn dead_target_defaults_land_inside_the_p4_stall_budget() {
        let t = DeadTargetTimeouts::default();
        // 2^13 ms ≈ 8.2s blackhole bound; +fast_io_fail(20s) ≈ 30s < 60s.
        assert_eq!(t.transport_ack_timeout_exp, 13);
        let p = t.set_options_params().expect("defaults produce params");
        assert_eq!(p["transport_ack_timeout"], 13);
        assert_eq!(p["timeout_us"], 30_000_000_u64);
        assert_eq!(p["action_on_timeout"], "reset");
        assert_eq!(p["tcp_connect_timeout_ms"], 10_000_u64);
    }

    #[test]
    fn dead_target_disabled_knobs_omit_their_fields() {
        // Each 0 removes its field(s); the RPC merges server-side, so an
        // omitted field means "keep the target's current value", never 0.
        let t = DeadTargetTimeouts {
            transport_ack_timeout_exp: 0,
            io_timeout_secs: 0,
            tcp_connect_timeout_ms: 5_000,
        };
        let p = t.set_options_params().unwrap();
        assert!(p.get("transport_ack_timeout").is_none());
        assert!(p.get("timeout_us").is_none());
        assert!(p.get("action_on_timeout").is_none(), "reset without a timeout is meaningless");
        assert_eq!(p["tcp_connect_timeout_ms"], 5_000_u64);
        // Everything off → nothing to send at all.
        let off = DeadTargetTimeouts {
            transport_ack_timeout_exp: 0,
            io_timeout_secs: 0,
            tcp_connect_timeout_ms: 0,
        };
        assert!(off.set_options_params().is_none());
    }

    #[test]
    fn dead_target_env_overrides_and_validation() {
        let p = DeadTargetTimeouts::from_lookup(|k| match k {
            "FLINT_SPDK_TRANSPORT_ACK_TIMEOUT_EXP" => Some("14".to_string()),
            "FLINT_SPDK_IO_TIMEOUT_SECS" => Some("0".to_string()),
            "FLINT_SPDK_TCP_CONNECT_TIMEOUT_MS" => Some("3000".to_string()),
            _ => None,
        });
        assert_eq!(p.transport_ack_timeout_exp, 14);
        assert_eq!(p.io_timeout_secs, 0); // explicit opt-out honored
        assert_eq!(p.tcp_connect_timeout_ms, 3000);
        // Exponent above 31 (SPDK clamps at its TCP max anyway) and garbage
        // fall back to the default rather than sending nonsense.
        for bad in ["32", "abc", "-1"] {
            let p = DeadTargetTimeouts::from_lookup(|k| {
                (k == "FLINT_SPDK_TRANSPORT_ACK_TIMEOUT_EXP").then(|| bad.to_string())
            });
            assert_eq!(p, DeadTargetTimeouts::default(), "{bad:?} must fall back");
        }
    }
}
