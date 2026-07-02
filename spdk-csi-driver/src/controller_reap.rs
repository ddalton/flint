//! Dead NVMe-oF controller reaping (Tier-2 phase 7b-0, spike finding).
//!
//! After a replica node's spdk-tgt is recreated and its export re-fenced,
//! a consumer-side `bdev_nvme` controller for the dead leg reconnect-loops
//! against the rebuilt subsystem — INVALID HOST rejections at ~100/s flood
//! the target's logs past rotation within seconds
//! (`tier2-spike-2026-06-12.md`, operational findings). Nothing converges
//! it short of a full restage. This pass detaches such controllers from
//! the node's periodic monitor tick.
//!
//! Safety model, mirroring the orphan sweep's discipline:
//!
//! * **Strict ownership parsing.** Only controllers whose name carries the
//!   mangled flint volume-NQN prefix (`nvme_nqn_2024-11_com_flint_volume_`,
//!   the shape `connect_to_nvmeof_target` produces) are ever candidates.
//!   Anything else — local PCIe controllers, operator experiments — is
//!   invisible.
//! * **A controller serving any raid base is never a candidate**, whatever
//!   its state: its namespace bdevs (`{name}n<nsid>`) are matched against
//!   every present raid's configured bases. A raid member mid path-flap
//!   belongs to the raid layer (reset/reconnect), not to the reaper; raid
//!   teardown stays NodeUnstage's job.
//! * **Only positively dead states condemn.** A controller is a candidate
//!   only when it reports at least one path and no path is `enabled`.
//!   `deleting` means a detach is already in flight — skipped, it will
//!   disappear on its own. An empty path list proves nothing and is
//!   skipped.
//! * **Three strikes.** A candidate is reaped only after being condemned
//!   on `strike_threshold` consecutive cycles (60 s apart), riding out
//!   transient resets and replica-node reboots that heal on their own.
//!   Leaving the candidate set resets the count. A healthy catch-up
//!   copy's controller is `enabled` and never condemned; a failed one is
//!   safe to reap — the chase re-attaches idempotently on its next cycle.

use std::collections::{HashMap, HashSet};

/// One controller from `bdev_nvme_get_controllers`.
#[derive(Debug, Clone)]
pub struct ControllerEntry {
    pub name: String,
    /// `ctrlrs[].state` strings: `enabled` / `failed` / `resetting` /
    /// `reconnect_is_delayed` / `disabled` / `deleting` (multipath yields
    /// one entry per path).
    pub states: Vec<String>,
}

/// Everything the planner needs, gathered by the node agent.
#[derive(Debug, Clone, Default)]
pub struct ReapInput {
    pub controllers: Vec<ControllerEntry>,
    /// Bdev names configured as bases of any raid present on this node
    /// (`bdev_raid_get_bdevs` `base_bdevs_list[].name`, failed slots'
    /// nulls excluded).
    pub raid_base_bdevs: HashSet<String>,
}

/// The controller-name prefix `connect_to_nvmeof_target` produces for any
/// flint volume NQN: "nvme_" + NQN with ':' and '.' mangled to '_'.
/// Derived from the same constant the export paths use so the shapes can
/// never drift apart silently.
pub fn flint_controller_prefix() -> String {
    format!(
        "nvme_{}",
        "nqn.2024-11.com.flint:volume:"
            .replace(':', "_")
            .replace('.', "_")
    )
}

/// True when `bdev` is a namespace bdev of controller `ctrlr_name`
/// (`{name}n<nsid>`).
fn is_namespace_of(bdev: &str, ctrlr_name: &str) -> bool {
    bdev.strip_prefix(ctrlr_name)
        .and_then(|rest| rest.strip_prefix('n'))
        .is_some_and(|nsid| !nsid.is_empty() && nsid.bytes().all(|b| b.is_ascii_digit()))
}

/// Decide which controllers to detach this cycle. Mutates `strikes` (the
/// persistent per-name condemnation counts): current candidates increment,
/// everything else is forgotten, and only names at `strike_threshold` or
/// beyond are returned for detachment.
pub fn plan_reap(
    input: &ReapInput,
    strikes: &mut HashMap<String, u32>,
    strike_threshold: u32,
) -> Vec<String> {
    let prefix = flint_controller_prefix();

    let candidates: Vec<&ControllerEntry> = input
        .controllers
        .iter()
        .filter(|c| c.name.starts_with(&prefix))
        // positively dead: at least one path, none enabled, none already
        // being deleted
        .filter(|c| {
            !c.states.is_empty()
                && !c.states.iter().any(|s| s == "enabled" || s == "deleting")
        })
        // never touch a controller serving a raid base
        .filter(|c| {
            !input
                .raid_base_bdevs
                .iter()
                .any(|b| is_namespace_of(b, &c.name))
        })
        .collect();

    let current: HashSet<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
    strikes.retain(|name, _| current.contains(name.as_str()));

    let mut reap = Vec::new();
    for c in candidates {
        let count = strikes.entry(c.name.clone()).or_insert(0);
        *count += 1;
        if *count >= strike_threshold {
            reap.push(c.name.clone());
        }
    }
    reap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrlr(name: &str, states: &[&str]) -> ControllerEntry {
        ControllerEntry {
            name: name.to_string(),
            states: states.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn dead_name() -> String {
        format!("{}pvc-abc_1", flint_controller_prefix())
    }

    fn input(controllers: Vec<ControllerEntry>, bases: &[&str]) -> ReapInput {
        ReapInput {
            controllers,
            raid_base_bdevs: bases.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn prefix_matches_export_shape() {
        // Must equal replica_sync::expected_remote_base_bdev's stem.
        let expected = crate::replica_sync::expected_remote_base_bdev("v", 0);
        assert!(expected.starts_with(&flint_controller_prefix()));
        assert_eq!(flint_controller_prefix(), "nvme_nqn_2024-11_com_flint_volume_");
    }

    #[test]
    fn dead_controller_reaped_after_three_strikes() {
        let mut strikes = HashMap::new();
        let inp = input(vec![ctrlr(&dead_name(), &["failed"])], &[]);
        assert!(plan_reap(&inp, &mut strikes, 3).is_empty());
        assert!(plan_reap(&inp, &mut strikes, 3).is_empty());
        assert_eq!(plan_reap(&inp, &mut strikes, 3), vec![dead_name()]);
    }

    #[test]
    fn reconnect_loop_states_condemn() {
        let mut strikes = HashMap::new();
        let inp = input(
            vec![ctrlr(&dead_name(), &["resetting", "reconnect_is_delayed"])],
            &[],
        );
        assert_eq!(plan_reap(&inp, &mut strikes, 1), vec![dead_name()]);
    }

    #[test]
    fn healthy_tick_resets_strikes() {
        let mut strikes = HashMap::new();
        let dead = input(vec![ctrlr(&dead_name(), &["failed"])], &[]);
        let healthy = input(vec![ctrlr(&dead_name(), &["enabled"])], &[]);
        plan_reap(&dead, &mut strikes, 3);
        plan_reap(&dead, &mut strikes, 3);
        // recovers: candidate leaves the set, count forgotten
        assert!(plan_reap(&healthy, &mut strikes, 3).is_empty());
        assert!(strikes.is_empty());
        // dies again: counting restarts from one
        assert!(plan_reap(&dead, &mut strikes, 3).is_empty());
    }

    #[test]
    fn any_enabled_path_is_healthy() {
        let mut strikes = HashMap::new();
        let inp = input(vec![ctrlr(&dead_name(), &["failed", "enabled"])], &[]);
        assert!(plan_reap(&inp, &mut strikes, 1).is_empty());
        assert!(strikes.is_empty());
    }

    #[test]
    fn raid_base_controller_never_condemned() {
        let mut strikes = HashMap::new();
        let name = dead_name();
        let base = format!("{}n1", name);
        let inp = input(vec![ctrlr(&name, &["failed"])], &[base.as_str()]);
        assert!(plan_reap(&inp, &mut strikes, 1).is_empty());
        assert!(strikes.is_empty());
    }

    #[test]
    fn namespace_matching_is_exact() {
        // "…n1" of a LONGER controller name must not shield a shorter one,
        // and non-numeric tails are not namespaces.
        assert!(is_namespace_of("nvme_xn1", "nvme_x"));
        assert!(is_namespace_of("nvme_xn12", "nvme_x"));
        assert!(!is_namespace_of("nvme_xyn1", "nvme_x"));
        assert!(!is_namespace_of("nvme_xnope", "nvme_x"));
        assert!(!is_namespace_of("nvme_xn", "nvme_x"));
    }

    #[test]
    fn foreign_and_deleting_and_empty_are_invisible() {
        let mut strikes = HashMap::new();
        let inp = input(
            vec![
                ctrlr("nvme0", &["failed"]),                    // not flint-shaped
                ctrlr(&dead_name(), &["deleting"]),             // detach in flight
                ctrlr(&format!("{}x_2", flint_controller_prefix()), &[]), // no paths
            ],
            &[],
        );
        assert!(plan_reap(&inp, &mut strikes, 1).is_empty());
        assert!(strikes.is_empty());
    }

    #[test]
    fn strikes_survive_only_for_persisting_candidates() {
        let mut strikes = HashMap::new();
        let a = format!("{}a_0", flint_controller_prefix());
        let b = format!("{}b_0", flint_controller_prefix());
        let both = input(vec![ctrlr(&a, &["failed"]), ctrlr(&b, &["failed"])], &[]);
        let only_a = input(vec![ctrlr(&a, &["failed"])], &[]);
        plan_reap(&both, &mut strikes, 3);
        plan_reap(&only_a, &mut strikes, 3);
        assert_eq!(strikes.get(&a), Some(&2));
        assert_eq!(strikes.get(&b), None);
    }
}
