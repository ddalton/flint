//! F36c — degraded-assembly freshness gate (docs/f36c-assembly-freshness-gate.md).
//!
//! Pure decision logic for `create_raid_from_replicas`: given the recorded
//! last-writer set and evidence about each missing writer's availability,
//! decide whether to assemble, defer this tick, or serve with a surfaced
//! acked-tail risk. Everything impure — node lookup, attach-reason
//! classification input, PV-annotation persistence — stays in driver.rs so
//! this module is unit-testable without a cluster.
//!
//! The invariant this protects (drill 3.6 run 3, 2026-07-21): for a
//! synchronous raid1 every acked write lives on every leg of the LAST
//! serving assembly. Assembling without one of those legs while it is only
//! TRANSIENTLY unavailable serves a trailing lineage — the 6-write-tail
//! loss. The counter-pressure (drill 2.4): a PERMANENTLY lost writer must
//! never manufacture an outage — serve the reachable survivor and surface
//! the bounded risk.

/// Availability evidence for a writer-set leg that did not attach this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum LegAvailability {
    /// Attach failed with a claim-shaped error: the previous assembly's
    /// raid (live or phantom) still holds the lvol. The strongest transient
    /// signal — guard-b's phantom-raid hygiene clears it — and independent
    /// corroboration that the leg WAS in the last writer set.
    ClaimBlocked,
    /// The leg's node is Ready per the API server. F33 caveat: Ready does
    /// not guarantee a live tgt — which is one reason the defer is bounded.
    NodeReady,
    /// NotReady, with seconds since the Ready condition last transitioned.
    NodeNotReady { not_ready_secs: u64 },
    /// The Node object is gone (terminated / deleted).
    NodeGone,
}

/// A recorded (or claim-corroborated) writer leg that did not attach.
#[derive(Debug, Clone)]
pub struct MissingWriter {
    pub lvol_uuid: String,
    pub node_name: String,
    pub availability: LegAvailability,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// Every writer-set leg attached (or the gate is inert): assemble.
    Proceed,
    /// A writer-set leg is transiently unavailable: refuse this assembly
    /// tick (NodeStage error → kubelet retries; guard-b clears claims
    /// meanwhile).
    Defer,
    /// Every missing writer is permanently gone, or the defer deadline
    /// passed without progress: serve the reachable legs and surface the
    /// acked-tail risk. Never hang (the 2.4 obligation).
    ServeWithRisk,
}

#[derive(Debug, Clone)]
pub struct GateConfig {
    /// Kill switch: FLINT_F36C_GATE=disabled restores pre-F36c
    /// serve-anything assembly.
    pub enabled: bool,
    /// Wall-clock defer bound in seconds (FLINT_F36C_DEFER_SECS). Sized to
    /// the claim-clear worst case — run 3 showed clearing can require a
    /// node reboot (~90-120s) — NOT to a tick count: NodeStage retry
    /// cadence belongs to kubelet backoff.
    pub defer_secs: u64,
    /// NotReady duration past which a node counts as permanently lost
    /// (FLINT_F36C_NODE_GONE_SECS; default mirrors the 6-minute
    /// attach/detach forced-detach horizon).
    pub node_gone_secs: u64,
}

impl Default for GateConfig {
    fn default() -> Self {
        GateConfig {
            enabled: true,
            defer_secs: 180,
            node_gone_secs: 360,
        }
    }
}

impl GateConfig {
    pub fn from_env() -> Self {
        let d = GateConfig::default();
        GateConfig {
            enabled: std::env::var("FLINT_F36C_GATE")
                .map(|v| v != "disabled")
                .unwrap_or(d.enabled),
            defer_secs: std::env::var("FLINT_F36C_DEFER_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.defer_secs),
            node_gone_secs: std::env::var("FLINT_F36C_NODE_GONE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.node_gone_secs),
        }
    }
}

impl LegAvailability {
    /// Transient = the fresher data is plausibly coming back: wait for it.
    /// Permanent = waiting protects nothing: serve and surface.
    fn is_transient(&self, cfg: &GateConfig) -> bool {
        match self {
            LegAvailability::ClaimBlocked | LegAvailability::NodeReady => true,
            LegAvailability::NodeNotReady { not_ready_secs } => {
                *not_ready_secs < cfg.node_gone_secs
            }
            LegAvailability::NodeGone => false,
        }
    }
}

/// The gate. `missing` is the set of recorded/corroborated writer legs that
/// did not attach this tick; `deadline_passed` is the persisted wall-clock
/// defer bound (driver.rs owns the annotation).
pub fn evaluate(missing: &[MissingWriter], deadline_passed: bool, cfg: &GateConfig) -> GateDecision {
    if !cfg.enabled || missing.is_empty() {
        return GateDecision::Proceed;
    }
    let any_transient = missing.iter().any(|m| m.availability.is_transient(cfg));
    if any_transient && !deadline_passed {
        GateDecision::Defer
    } else {
        GateDecision::ServeWithRisk
    }
}

/// Claim-shaped attach failures — corroboration that the leg was in the
/// previous serving assembly, independent of record freshness. Conservative
/// matching: a false negative only means the gate falls back to
/// node-condition evidence; a false positive defers on a leg that is not
/// coming back — bounded by the deadline either way. "Operation not
/// permitted" is the raid module's exclusive-claim EPERM shape (see
/// attach_replica_base's stale-export note).
pub fn is_claim_blocked(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("claim")
        || r.contains("cannot be opened")
        || r.contains("operation not permitted")
        || r.contains("resource busy")
}

/// One-line operator-facing description of the missing writers.
pub fn describe_missing(missing: &[MissingWriter]) -> String {
    missing
        .iter()
        .map(|m| {
            let a = match &m.availability {
                LegAvailability::ClaimBlocked => "claim-blocked".to_string(),
                LegAvailability::NodeReady => "node Ready".to_string(),
                LegAvailability::NodeNotReady { not_ready_secs } => {
                    format!("node NotReady {}s", not_ready_secs)
                }
                LegAvailability::NodeGone => "node gone".to_string(),
            };
            format!("{} on {} ({})", m.lvol_uuid, m.node_name, a)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Defer-deadline marker (persisted as a PV annotation by driver.rs) ────
//
// Format: "<deadline_rfc3339>|<uuid1>,<uuid2>" with uuids sorted. The uuid
// list re-arms the deadline whenever the missing set CHANGES — partial
// progress (one of two writers came back) is new evidence and earns a
// fresh bound.

pub fn encode_defer_marker(deadline_rfc3339: &str, missing_uuids: &[String]) -> String {
    let mut uuids = missing_uuids.to_vec();
    uuids.sort();
    format!("{}|{}", deadline_rfc3339, uuids.join(","))
}

pub fn parse_defer_marker(marker: &str) -> Option<(String, Vec<String>)> {
    let (deadline, uuids) = marker.split_once('|')?;
    if deadline.is_empty() {
        return None;
    }
    let uuids: Vec<String> = uuids
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    Some((deadline.to_string(), uuids))
}

/// Deadline = now + defer_secs, RFC3339.
pub fn deadline_from(now_rfc3339: &str, defer_secs: u64) -> String {
    match chrono::DateTime::parse_from_rfc3339(now_rfc3339) {
        Ok(t) => (t + chrono::Duration::seconds(defer_secs as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        // Unparseable "now" cannot happen with now_rfc3339(); degrade to a
        // marker that reads as already-passed so the gate stays bounded.
        Err(_) => now_rfc3339.to_string(),
    }
}

/// True once `now` reaches the deadline. An unparseable deadline reads as
/// passed — the gate must bound the outage, never extend it on bad data.
pub fn deadline_passed(deadline_rfc3339: &str, now_rfc3339: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(deadline_rfc3339),
        chrono::DateTime::parse_from_rfc3339(now_rfc3339),
    ) {
        (Ok(deadline), Ok(now)) => now >= deadline,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GateConfig {
        GateConfig::default()
    }

    fn missing(availability: LegAvailability) -> MissingWriter {
        MissingWriter {
            lvol_uuid: "uuid-w".to_string(),
            node_name: "aws-1".to_string(),
            availability,
        }
    }

    #[test]
    fn no_missing_writers_proceeds() {
        assert_eq!(evaluate(&[], false, &cfg()), GateDecision::Proceed);
        assert_eq!(evaluate(&[], true, &cfg()), GateDecision::Proceed);
    }

    #[test]
    fn disabled_gate_proceeds() {
        let mut c = cfg();
        c.enabled = false;
        let m = [missing(LegAvailability::ClaimBlocked)];
        assert_eq!(evaluate(&m, false, &c), GateDecision::Proceed);
    }

    #[test]
    fn claim_blocked_defers_the_run3_shape() {
        // Drill 3.6 run 3: fresher leg claim-blocked by the dead node's
        // phantom raid, trailing leg attached — must NOT serve.
        let m = [missing(LegAvailability::ClaimBlocked)];
        assert_eq!(evaluate(&m, false, &cfg()), GateDecision::Defer);
    }

    #[test]
    fn node_ready_defers() {
        let m = [missing(LegAvailability::NodeReady)];
        assert_eq!(evaluate(&m, false, &cfg()), GateDecision::Defer);
    }

    #[test]
    fn fresh_not_ready_defers_but_gone_horizon_serves() {
        let m = [missing(LegAvailability::NodeNotReady { not_ready_secs: 30 })];
        assert_eq!(evaluate(&m, false, &cfg()), GateDecision::Defer);
        let m = [missing(LegAvailability::NodeNotReady { not_ready_secs: 361 })];
        assert_eq!(evaluate(&m, false, &cfg()), GateDecision::ServeWithRisk);
    }

    #[test]
    fn node_gone_serves_immediately_the_24_shape() {
        // Drill 2.4: permanent loss never manufactures an outage — no defer
        // round-trip, serve the survivor on the first tick.
        let m = [missing(LegAvailability::NodeGone)];
        assert_eq!(evaluate(&m, false, &cfg()), GateDecision::ServeWithRisk);
    }

    #[test]
    fn deadline_expiry_falls_through_to_serve() {
        let m = [missing(LegAvailability::ClaimBlocked)];
        assert_eq!(evaluate(&m, true, &cfg()), GateDecision::ServeWithRisk);
    }

    #[test]
    fn mixed_transient_and_permanent_defers_for_the_transient_leg() {
        let m = [
            missing(LegAvailability::NodeGone),
            missing(LegAvailability::ClaimBlocked),
        ];
        assert_eq!(evaluate(&m, false, &cfg()), GateDecision::Defer);
    }

    #[test]
    fn claim_classifier_shapes() {
        assert!(is_claim_blocked(
            "NVMe-oF connection failed: bdev is already claimed by raid"
        ));
        assert!(is_claim_blocked("bdev cannot be opened, error=-1"));
        assert!(is_claim_blocked("Operation not permitted"));
        assert!(is_claim_blocked("Device or resource busy"));
        assert!(!is_claim_blocked("NVMe-oF connection failed: timeout"));
        assert!(!is_claim_blocked("Local lvol not found: no bdev"));
    }

    #[test]
    fn defer_marker_roundtrip() {
        let uuids = vec!["b".to_string(), "a".to_string()];
        let marker = encode_defer_marker("2026-07-21T10:00:00Z", &uuids);
        let (deadline, parsed) = parse_defer_marker(&marker).unwrap();
        assert_eq!(deadline, "2026-07-21T10:00:00Z");
        assert_eq!(parsed, vec!["a".to_string(), "b".to_string()]);
        assert!(parse_defer_marker("garbage-no-separator").is_none());
        assert!(parse_defer_marker("|a,b").is_none());
    }

    #[test]
    fn deadline_math() {
        let deadline = deadline_from("2026-07-21T10:00:00Z", 180);
        assert_eq!(deadline, "2026-07-21T10:03:00Z");
        assert!(!deadline_passed(&deadline, "2026-07-21T10:02:59Z"));
        assert!(deadline_passed(&deadline, "2026-07-21T10:03:00Z"));
        // Corrupt deadline reads as passed — bounded, never an indefinite
        // outage.
        assert!(deadline_passed("not-a-time", "2026-07-21T10:00:00Z"));
    }
}
