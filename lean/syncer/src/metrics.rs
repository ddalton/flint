//! `/metrics` (D15, §8 Q5) — Prometheus exposition rendered from the
//! SAME struct `gauges.json` is written from.
//!
//! One renderer over one struct cannot drift; two computations would,
//! and the drift would be invisible until an operator made a decision
//! on the number that was wrong. That is why [`render`] takes a
//! [`Gauges`] and nothing else — it cannot consult the store, the
//! stage, or the clock, so a scrape costs exactly zero bucket requests
//! by construction rather than by care (leg B8's oracle counts them).
//!
//! **The label set is fixed at `{workspace, namespace}` and that is a
//! rule, not a default.** A per-path or per-file metric would multiply
//! series by the workspace's inventory — 250,000 files is the shipped
//! cap — and the fleet is 3,000 workspaces. The parity test enforces
//! the label keys exactly.
//!
//! **String-valued gauges are rendered as numeric enums**, not as
//! labels, for the same reason: `reason="parked-412"` would add a label key
//! and open the door to the next one. The mapping is in each metric's
//! HELP line, where a human reading the exposition can see it.

use super::gauges::Gauges;

/// The only labels any series carries.
#[derive(Debug, Clone)]
pub struct Labels {
    pub workspace: String,
    pub namespace: String,
}

impl Labels {
    fn render(&self) -> String {
        format!(
            "{{workspace=\"{}\",namespace=\"{}\"}}",
            escape(&self.workspace),
            escape(&self.namespace)
        )
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
}

/// `none` → 0, and one code per `Withheld` variant.
fn withheld_code(s: Option<&str>) -> u64 {
    match s {
        None => 0,
        Some("parked-412") => 1,
        Some(_) => 99,
    }
}

/// One code per boundary source (`gauges::LastBoundary::source`).
fn source_code(s: &str) -> u64 {
    match s {
        "sentinel" => 1,
        "sentinel-deferred" => 2,
        "cadence" => 3,
        "drain" => 4,
        _ => 99,
    }
}

/// Every metric this syncer exposes: the metric name, its HELP text,
/// and how it is read out of the gauges. The table is the contract the
/// parity test checks — a gauges field with no row here fails the
/// build's tests rather than silently going unexposed.
fn series(g: &Gauges) -> Vec<(&'static str, &'static str, u64)> {
    vec![
        (
            "flint_lean_rpo_seconds",
            "seconds since the last boundary was installed. Elapsed time, not exposure: an \
             idle healthy workspace has nothing at risk and a growing value — pair it with \
             flint_lean_withheld_reason before alerting",
            g.rpo_secs,
        ),
        (
            "flint_lean_withheld_reason",
            "why visibility is withheld: 0=none 1=parked-412",
            withheld_code(g.withheld_reason.as_deref()),
        ),
        (
            "flint_lean_sentinel_budget_remaining",
            "work units left in this hour's sentinel budget (D3.1). Zero means honors \
             defer to the floor — the workspace is running at exactly cadence behavior",
            g.sentinel_budget_remaining,
        ),
        (
            "flint_lean_last_boundary_source",
            "which clock installed the last boundary: 1=sentinel 2=sentinel-deferred \
             3=cadence 4=drain, 0=none yet",
            g.last_boundary.as_ref().map(|b| source_code(&b.source)).unwrap_or(0),
        ),
        (
            "flint_lean_last_boundary_seq",
            "the manifest seq the last boundary installed",
            g.last_boundary.as_ref().map(|b| b.seq).unwrap_or(0),
        ),
        (
            "flint_lean_last_boundary_timestamp_seconds",
            "unix time of the last boundary",
            g.last_boundary.as_ref().map(|b| b.unix).unwrap_or(0),
        ),
        (
            "flint_lean_auth_paused_since_timestamp_seconds",
            "unix time of the first renewal the store refused with 401/403 since the last \
             successful one; 0 = not paused. A credential, token or clock fault — never \
             contention and never a deposal, and retrying does not fix it. It is exported \
             as a timestamp rather than a duration because a paused holder cannot renew, \
             and a holder that cannot renew is what a challenger reads as DEAD: alert on \
             time() - this > the takeover threshold, which is the window in which a live \
             writer gets deposed (design 6.3)",
            g.auth_paused_since_unix.unwrap_or(0),
        ),
        (
            "flint_lean_updated_timestamp_seconds",
            "unix time these gauges were last refreshed. Refreshed on EVERY tick, news or \
             not, so an idle-but-healthy workspace is distinguishable from a dead one",
            g.updated_unix,
        ),
        (
            "flint_lean_last_durable_timestamp_seconds",
            "unix time of the last durable write (gauges.last_durable_unix)",
            g.last_durable_unix,
        ),
    ]
}

/// Render the exposition. Pure, synchronous, store-free.
pub fn render(g: &Gauges, labels: &Labels) -> String {
    let l = labels.render();
    let mut out = String::new();
    for (name, help, value) in series(g) {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name}{l} {value}\n"));
    }
    out
}

/// Which gauges field each metric reports, for the parity test. Kept
/// beside `series` so adding a metric without saying what it covers is
/// a compile-time-visible omission rather than a silent gap.
pub const COVERED_FIELDS: &[&str] = &[
    "rpo_secs",
    "withheld_reason",
    "sentinel_budget_remaining",
    "last_boundary",
    "updated_unix",
    "last_durable_unix",
    "auth_paused_since_unix",
];

/// What the syncer recorded about its own exposition attempt. Written
/// to the state directory at startup and echoed to the operator: a bind
/// collision has to be VISIBLE, because the design's answer to it is to
/// keep running, and a degradation nobody can see is indistinguishable
/// from a feature nobody enabled.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MetricsPosture {
    pub enabled: bool,
    pub port: u16,
    /// False while `enabled` is true ⇒ the port was taken (the agent
    /// container is the likely occupant) and the listener is not up.
    pub bound: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub const POSTURE_FILE: &str = "metrics.json";

impl super::Syncer {
    pub fn save_metrics_posture(&self, p: &MetricsPosture) -> super::LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(p)
            .map_err(|e| super::LeanError::State(format!("metrics posture: {e}")))?;
        super::control::write_atomic(&self.cfg.state_dir().join(POSTURE_FILE), &bytes)
    }

    pub fn load_metrics_posture(&self) -> Option<MetricsPosture> {
        serde_json::from_slice(&std::fs::read(self.cfg.state_dir().join(POSTURE_FILE)).ok()?).ok()
    }
}
