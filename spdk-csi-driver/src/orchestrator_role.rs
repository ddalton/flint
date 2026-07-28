//! F53 — which *process* may run the controller-side orchestrators.
//!
//! The per-volume claim registry that serializes catch-up / cutover /
//! hot-rejoin (`volume_claims::global()`) is **in-process**. Two processes
//! that both run the orchestrator block arbitrate nothing against each
//! other: each one's `try_claim` always succeeds against its own empty
//! registry, so their admission windows destroy each other's state
//! mid-flight. That is [F50](../docs/f50-hotrejoin-window-concurrency.md).
//!
//! F50's fix removed the vestigial `spdk-controller-operator` Deployment,
//! but the shape survived in one more place the write-up missed: the
//! **dashboard backend** runs the same `csi-driver` binary with
//! `CSI_MODE=controller` hard-coded in the chart, so it stood up its own
//! cutover *and* hot-rejoin orchestrators — observed live on runaj, in the
//! dashboard pod's own log, on a cluster where the operator was already
//! gone.
//!
//! The lesson is that inferring "may run orchestrators" from `CSI_MODE` was
//! always the bug. `CSI_MODE` says which **gRPC services** this process
//! serves; it says nothing about which process owns cluster-wide
//! singletons. Those are different questions and they now have different
//! answers.
//!
//! The permission is therefore explicit — `FLINT_ORCHESTRATORS` — and the
//! chart sets it on exactly one Deployment. When it is unset we fall back
//! to the historical CSI_MODE rule *minus* any process that has declared
//! itself a dashboard, so a hand-rolled or dev deployment (`CSI_MODE=all`,
//! single pod, no chart) keeps working unchanged while the one shipped
//! second-controller stops.
//!
//! This is a narrowing, not the complete answer. Any *fourth* process that
//! sets `CSI_MODE=controller` and no `FLINT_ORCHESTRATORS` still runs
//! orchestrators. The complete answer is kube-Lease leader election among
//! the orchestrator block's owners, which is tracked as F50's deferred
//! follow-up — this module is where that lease check belongs when it lands.

use std::sync::Once;

/// The decision plus a human-readable reason, so the process can say why it
/// is or is not running orchestrators instead of leaving an operator to
/// infer it from missing log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleDecision {
    pub enabled: bool,
    pub reason: String,
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "enabled" | "true" | "1" | "yes" | "on" => Some(true),
        "disabled" | "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Pure decision function — every input explicit so it is testable without
/// touching the process environment.
///
/// * `mode` — the resolved `CSI_MODE`.
/// * `flint_orchestrators` — `FLINT_ORCHESTRATORS`, the explicit grant.
/// * `enable_dashboard` — `ENABLE_DASHBOARD`; a process serving the
///   dashboard is not the CSI controller even when it says
///   `CSI_MODE=controller` to get the controller-side query surface.
pub fn decide(
    mode: &str,
    flint_orchestrators: Option<&str>,
    enable_dashboard: Option<&str>,
) -> RoleDecision {
    if let Some(raw) = flint_orchestrators {
        match parse_bool(raw) {
            Some(true) => {
                return RoleDecision {
                    enabled: true,
                    reason: "FLINT_ORCHESTRATORS is set to enabled — this process owns the \
                             controller-side orchestrators"
                        .to_string(),
                }
            }
            Some(false) => {
                return RoleDecision {
                    enabled: false,
                    reason: "FLINT_ORCHESTRATORS is set to disabled — another process owns the \
                             controller-side orchestrators"
                        .to_string(),
                }
            }
            // An unparseable value must not silently mean "enabled": that is
            // how a typo turns into a second controller. Fall through to the
            // default rule and say so.
            None => {}
        }
    }

    let is_dashboard = enable_dashboard.and_then(parse_bool).unwrap_or(false);
    if is_dashboard {
        return RoleDecision {
            enabled: false,
            reason: format!(
                "ENABLE_DASHBOARD is set — the dashboard backend serves the controller-side \
                 query surface (CSI_MODE={mode}) but is NOT the CSI controller, so it must not \
                 run orchestrators (F53)"
            ),
        };
    }

    let controller_role = mode == "controller" || mode == "all";
    if controller_role {
        RoleDecision {
            enabled: true,
            reason: format!(
                "CSI_MODE={mode} and FLINT_ORCHESTRATORS is unset — falling back to the \
                 historical controller-role rule. Set FLINT_ORCHESTRATORS explicitly if any \
                 other pod also runs the controller role (F50/F53)."
            ),
        }
    } else {
        RoleDecision {
            enabled: false,
            reason: format!("CSI_MODE={mode} is not a controller role"),
        }
    }
}

fn decide_from_env(mode: &str) -> RoleDecision {
    decide(
        mode,
        std::env::var("FLINT_ORCHESTRATORS").ok().as_deref(),
        std::env::var("ENABLE_DASHBOARD").ok().as_deref(),
    )
}

/// Whether this process may run the controller-side orchestrator block
/// (epoch scheduler, catch-up, cutover, hot-rejoin, NFS server reconciler).
/// Logs the decision once, with its reason.
pub fn orchestrators_enabled(mode: &str) -> bool {
    static LOGGED: Once = Once::new();
    let decision = decide_from_env(mode);
    LOGGED.call_once(|| {
        if decision.enabled {
            println!("🎛️ [ORCHESTRATORS] ENABLED — {}", decision.reason);
        } else {
            println!(
                "🎛️ [ORCHESTRATORS] DISABLED — {}. This process runs no epoch scheduler, \
                 catch-up, cutover, hot-rejoin or NFS reconciler.",
                decision.reason
            );
        }
    });
    decision.enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_grant_wins_over_every_inference() {
        // Even a node-mode process runs them if told to — the grant is the
        // authority, so a future single-purpose orchestrator pod needs no
        // CSI_MODE gymnastics.
        assert!(decide("node", Some("enabled"), None).enabled);
        // ...and the controller does NOT if told not to.
        assert!(!decide("controller", Some("disabled"), None).enabled);
        // The grant also beats the dashboard veto, in both directions.
        assert!(decide("controller", Some("true"), Some("true")).enabled);
        assert!(!decide("all", Some("false"), None).enabled);
    }

    #[test]
    fn the_dashboard_backend_never_runs_orchestrators() {
        // THE F53 REGRESSION TEST. This is the exact env of the shipped
        // dashboard Deployment: CSI_MODE=controller + ENABLE_DASHBOARD=true,
        // no explicit grant. Live on runaj it started cutover and hot-rejoin
        // against its own empty claim registry.
        let d = decide("controller", None, Some("true"));
        assert!(!d.enabled, "dashboard backend must not run orchestrators");
        assert!(d.reason.contains("F53"), "reason should name the finding");
    }

    #[test]
    fn unset_grant_keeps_the_historical_controller_rule() {
        // A dev/kind single pod (CSI_MODE=all, no chart, no dashboard) must
        // keep self-healing exactly as before this change.
        assert!(decide("all", None, None).enabled);
        assert!(decide("controller", None, None).enabled);
        // ...and a plain node pod still never did.
        assert!(!decide("node", None, None).enabled);
        assert!(!decide("", None, None).enabled);
    }

    #[test]
    fn a_dashboard_flag_that_is_explicitly_false_is_not_a_veto() {
        assert!(decide("controller", None, Some("false")).enabled);
    }

    #[test]
    fn an_unparseable_grant_falls_through_instead_of_meaning_enabled() {
        // A typo ("enabld") must not hand a second process the orchestrators.
        // It falls through to the default rule, so the dashboard veto still
        // applies and a plain controller still runs them.
        assert!(!decide("controller", Some("enabld"), Some("true")).enabled);
        assert!(decide("controller", Some("enabld"), None).enabled);
        assert!(!decide("node", Some("wat"), None).enabled);
    }

    #[test]
    fn accepts_the_usual_spellings_and_ignores_case_and_padding() {
        for yes in ["enabled", "ENABLED", " True ", "1", "yes", "on"] {
            assert!(decide("node", Some(yes), None).enabled, "{yes:?}");
        }
        for no in ["disabled", "DISABLED", " False ", "0", "no", "off"] {
            assert!(!decide("controller", Some(no), None).enabled, "{no:?}");
        }
    }
}
