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
//! labels, for the same reason: `mode="gated"` would add a label key
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

/// `cadence` → 0, `hybrid` → 1, `gated` → 2.
fn mode_code(s: &str) -> u64 {
    match s {
        "cadence" => 0,
        "hybrid" => 1,
        "gated" => 2,
        _ => 99,
    }
}

/// `none` → 0, and one code per `Withheld` variant.
fn withheld_code(s: Option<&str>) -> u64 {
    match s {
        None => 0,
        Some("quiesce-pending") => 1,
        Some("awaiting-boundary") => 2,
        Some("parked-412") => 3,
        Some("cas-conflict") => 4,
        Some("version-probe-failed") => 5,
        Some(_) => 99,
    }
}

/// One code per citation source (`gauges::LastBoundary::source`).
fn source_code(s: &str) -> u64 {
    match s {
        "sentinel" => 1,
        "quiescence" => 2,
        "forced-lag-cap" => 3,
        "forced-backlog-cap" => 4,
        "cadence" => 5,
        "drain" => 6,
        "recovered" => 7,
        "repair" => 8,
        _ => 99,
    }
}

/// Every metric this sidecar exposes: the metric name, its HELP text,
/// and how it is read out of the gauges. The table is the contract the
/// parity test checks — a gauges field with no row here fails the
/// build's tests rather than silently going unexposed.
fn series(g: &Gauges) -> Vec<(&'static str, &'static str, u64)> {
    vec![
        (
            "flint_lean_fenced",
            "1 when this sidecar has been deposed and stopped publishing (gauges.state)",
            u64::from(g.state == "fenced"),
        ),
        (
            "flint_lean_boundary_mode",
            "citation policy in force: 0=cadence 1=hybrid 2=gated (gauges.boundary_mode)",
            mode_code(&g.boundary_mode),
        ),
        (
            "flint_lean_rpo_seconds",
            "seconds since the last DURABLE write. Elapsed time, not exposure: an idle \
             healthy workspace has nothing at risk and a growing value — pair it with \
             flint_lean_staged_uncited_objects before alerting",
            g.rpo_secs,
        ),
        (
            "flint_lean_visibility_lag_seconds",
            "seconds since the last CITATION. In cadence/hybrid this equals the RPO by \
             construction; in gated it is the number visibilityLagBoundSecs caps",
            g.visibility_lag_secs,
        ),
        (
            "flint_lean_staged_uncited_objects",
            "durable objects no manifest cites yet. This is gated mode's whole exposure: \
             what a pod replacement would strand for `flint-sync recover-staged`",
            g.staged_uncited_count,
        ),
        (
            "flint_lean_staged_uncited_bytes",
            "bytes staged and uncited",
            g.staged_uncited_bytes,
        ),
        (
            "flint_lean_cited_noncurrent_age_max_seconds",
            "how long the OLDEST still-cited version has been noncurrent. Gated staging \
             makes the cited version noncurrent, so the retention backstop runs a clock \
             against live cited data — noncurrentRetentionDays is the number this must \
             never reach",
            g.cited_noncurrent_age_max_secs,
        ),
        (
            "flint_lean_withheld_reason",
            "why visibility is withheld: 0=none 1=quiesce-pending 2=awaiting-boundary \
             3=parked-412 4=cas-conflict 5=version-probe-failed",
            withheld_code(g.withheld_reason.as_deref()),
        ),
        (
            "flint_lean_sentinel_budget_remaining",
            "work units left in this hour's sentinel budget (D3.1). Zero means honors \
             defer to the floor — the workspace is running at exactly cadence behavior",
            g.sentinel_budget_remaining,
        ),
        (
            "flint_lean_forced_citations_total",
            "citations forced by a cap rather than taken at a declared coherent point. A \
             workspace that forces every citation has no coherence contract left",
            g.forced_citation_count,
        ),
        (
            "flint_lean_last_boundary_source",
            "which coherent point installed the last citation: 1=sentinel 2=quiescence \
             3=forced-lag-cap 4=forced-backlog-cap 5=cadence 6=drain 7=recovered 8=repair, \
             0=none yet",
            g.last_boundary.as_ref().map(|b| source_code(&b.source)).unwrap_or(0),
        ),
        (
            "flint_lean_last_boundary_seq",
            "the manifest seq the last citation installed",
            g.last_boundary.as_ref().map(|b| b.seq).unwrap_or(0),
        ),
        (
            "flint_lean_last_boundary_timestamp_seconds",
            "unix time of the last citation",
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
    "state",
    "boundary_mode",
    "rpo_secs",
    "visibility_lag_secs",
    "staged_uncited_count",
    "staged_uncited_bytes",
    "cited_noncurrent_age_max_secs",
    "withheld_reason",
    "sentinel_budget_remaining",
    "forced_citation_count",
    "last_boundary",
    "updated_unix",
    "last_durable_unix",
    "auth_paused_since_unix",
];

/// What the sidecar recorded about its own exposition attempt. Written
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

impl super::Sidecar {
    pub fn save_metrics_posture(&self, p: &MetricsPosture) -> super::LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(p)
            .map_err(|e| super::LeanError::State(format!("metrics posture: {e}")))?;
        super::control::write_atomic(&self.cfg.state_dir().join(POSTURE_FILE), &bytes)
    }

    pub fn load_metrics_posture(&self) -> Option<MetricsPosture> {
        serde_json::from_slice(&std::fs::read(self.cfg.state_dir().join(POSTURE_FILE)).ok()?).ok()
    }
}
