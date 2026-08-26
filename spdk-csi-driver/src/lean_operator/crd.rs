//! The FlintLeanWorkspace CRD.
//!
//! One CR = one lean workspace subtree: a durable project identity, a
//! bucket/prefix address, a durability profile, and budgets. There is
//! deliberately NO Deployment/PVC/Service in its wake — an idle lean
//! workspace is bucket objects and nothing else (the scale-to-zero
//! argument). Pods opt in to injection with the label
//! `flint.io/lean-workspace: <name>`.

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const INJECT_LABEL: &str = "flint.io/lean-workspace";

#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "flint.io",
    version = "v1alpha1",
    kind = "FlintLeanWorkspace",
    plural = "flintleanworkspaces",
    singular = "flintleanworkspace",
    shortname = "flw",
    namespaced,
    status = "FlintLeanWorkspaceStatus",
    derive = "PartialEq",
    doc = "A lean checkout/publish workspace: full local checkout at pod start, snapshot publishes at the flush floor, zero long-running per-workspace resources",
    printcolumn = r#"{"name":"PHASE","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"PROJECT","type":"string","jsonPath":".spec.projectId"}"#,
    printcolumn = r#"{"name":"BUCKET","type":"string","jsonPath":".spec.bucket"}"#,
    printcolumn = r#"{"name":"PREFIX","type":"string","jsonPath":".spec.keyPrefix"}"#,
    printcolumn = r#"{"name":"MODE","type":"string","jsonPath":".status.observedBoundaryMode"}"#,
    printcolumn = r#"{"name":"CITED-SEQ","type":"integer","jsonPath":".status.citedSeq"}"#,
    printcolumn = r#"{"name":"LAG","type":"integer","jsonPath":".status.visibilityLagSecs"}"#,
    printcolumn = r#"{"name":"STAGED","type":"integer","jsonPath":".status.stagedUncited"}"#,
    printcolumn = r#"{"name":"AGE","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FlintLeanWorkspaceSpec {
    /// The durable, user-declared project identity the claim cell
    /// carries (plan P1). Stable across CR delete/recreate — NEVER the
    /// CR UID. Equal identity on a standing claim ⇒ adopt (DR, GitOps,
    /// cross-cluster moves); different ⇒ the CR is Refused.
    pub project_id: String,

    /// Bucket and subtree prefix (the proxy's tenancy boundary —
    /// project-granular per plan §9 Q6).
    pub bucket: String,
    pub key_prefix: String,

    /// S3 endpoint override (the deployment proxy; MinIO rigs). None =
    /// ambient AWS endpoint resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Secret with the SIDECAR's proxy credentials, keys AWS_* VERBATIM
    /// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, ...). The
    /// OPERATOR never uses this — bucket-admin ops run under the
    /// operator principal (plan §2.4 principal split).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_secret_ref: Option<String>,

    /// Publish cadence floor, seconds: the durability (RPO) contract.
    #[serde(default = "default_floor_secs")]
    pub floor_secs: u64,

    /// Checkout budgets. Files defaults to the 0b-measured v1 cap
    /// (docs/plans/flint-lean-0b-measurements.md); 0 = unlimited.
    #[serde(default)]
    pub max_bytes: u64,
    #[serde(default = "default_max_files")]
    pub max_files: u64,

    /// Bounded upload/checkout concurrency (0b lever; default 16).
    #[serde(default = "default_fanout")]
    pub fanout: u64,

    /// emptyDir sizeLimit for the workspace volume, GiB. 0 = no limit.
    #[serde(default = "default_size_limit_gib")]
    pub size_limit_gib: u64,

    /// Where the workspace mounts in every container.
    #[serde(default = "default_mount_path")]
    pub mount_path: String,

    /// Sidecar image override; None = the operator's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Expected inventory, used to DERIVE the sidecar's startupProbe
    /// budget (plan §2.4: probes are derived, never fleet constants —
    /// the hub's 600 s default killed a 20 GiB checkout at the only
    /// measured rate). Unset ⇒ the budgets assume the caps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_files: Option<u64>,

    // ── boundary verbs (docs/plans/flint-lean-boundary-verbs-plan.md
    //    §2.6). Every default below is today's behavior, so an existing
    //    CR that names none of them is byte-identical after upgrade. ──
    /// Citation policy (D6): `cadence` | `hybrid` | `gated`.
    ///
    /// - `cadence` — exactly pre-boundary behavior; the escape hatch.
    /// - `hybrid` (DEFAULT) — cadence ∪ boundary sentinels, whichever
    ///   comes first. No trade: a workspace whose agent never touches
    ///   `.flint/publish` behaves identically to `cadence`.
    /// - `gated` — OPT-IN. Durability and visibility split: uploads
    ///   land every floor tick as uncited object versions, and the
    ///   manifest advances only at coherent points. Buys coherent
    ///   views for manifest-resolving readers. COSTS, all of which
    ///   apply the moment you set it: (1) automatic-recovery RPO is
    ///   the last BOUNDARY, not the last floor — on a pure-spot fleet
    ///   pod replacement is routine, and uncited work then needs
    ///   `flint-sync recover-staged`, an operator action; (2) uncited
    ///   generations are the CURRENT version of real `files/` keys, so
    ///   any reader that does not resolve through the manifest — an
    ///   import tool, `aws s3 cp`, a foreign system, a human — sees
    ///   mid-logical-change bytes; (3) uncited = invisible to every
    ///   import, DR checkout, GitOps re-apply and cross-cluster move;
    ///   (4) the bucket must pass the versioning conformance probe
    ///   (versioning=Enabled, `x-amz-version-id` on PUT, version-scoped
    ///   GET/HEAD/DELETE, `ListObjectVersions`) — a proxy that strips
    ///   the version header gets gated REFUSED, never degraded into;
    ///   (5) `visibilityLagBoundSecs` is REQUIRED.
    ///
    /// CHANGING THIS ON A LIVE WORKSPACE REQUIRES POD RECREATION: the
    /// sidecar's config is env stamped at pod creation by the webhook
    /// and there is no re-read path. Recreation destroys the emptyDir
    /// pending record, which turns the whole uncited window into
    /// recovery candidates — the gated→cadence escape hatch is
    /// therefore an operator procedure, not an edit.
    #[serde(default = "default_boundary_mode")]
    pub boundary_mode: String,

    /// Sentinel posture (D0.4): `auto` | `off` | `force`. `auto` runs
    /// the verbs unless the pre-flight finds pre-existing `.flint/`
    /// data in the workspace (an app that already owns that name);
    /// `force` accepts consuming such files.
    #[serde(default = "default_sentinels")]
    pub sentinels: String,

    /// Latency guard: sentinels arriving inside the interval coalesce
    /// into one barrier whose ack covers every nonce. Inert on a
    /// workspace whose agent never touches a sentinel.
    #[serde(default = "default_sentinel_min_interval_secs")]
    pub sentinel_min_interval_secs: u64,

    /// Work-metered hourly cap (D3.1) — UNITS, not calls. A honor
    /// charges `max(1, ceil(published_bytes / 64 MiB))`, and 0 for a
    /// no-diff honor, so sentinel-driven bytes are bounded at
    /// budget × 64 MiB/hour however hot the agent's loop is. Exhausted
    /// ⇒ honors defer to the floor tick and the ack is stamped
    /// `sentinel-deferred`: the workspace degrades to exactly cadence
    /// behavior, never to a refusal.
    #[serde(default = "default_sentinel_hourly_budget")]
    pub sentinel_hourly_budget: u64,

    /// Gated only, and REQUIRED there: the hard cap on citation
    /// staleness. Unbounded staleness is refused by construction
    /// rather than by convention — a gated CR without this is
    /// `BoundaryModeAccepted=False`. A citation forced by this cap is
    /// stamped `forced-lag-cap` in the ack AND on the manifest object,
    /// so "was that view coherent?" is answerable from the bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility_lag_bound_secs: Option<u64>,

    /// Gated only: the scan-to-scan stability window that counts as
    /// quiescence — the cheapest coherent point there is, and the one
    /// that fires for an agent that never learns the verbs.
    #[serde(default = "default_quiesce_bound_secs")]
    pub quiesce_bound_secs: u64,

    /// Gated only: forced-citation sources that bound the preStop
    /// drain by construction. If backlog-cap becomes the DOMINANT
    /// citation source, the cap is pacing the workspace instead of
    /// bounding it — lower the lag bound or teach the agent the verbs.
    #[serde(default = "default_staged_backlog_cap_objects")]
    pub staged_backlog_cap_objects: u64,
    #[serde(default = "default_staged_backlog_cap_bytes")]
    pub staged_backlog_cap_bytes: u64,

    /// Gated only: the noncurrent-version retention the operator
    /// provisions on `<prefix>/files/` — the crash-window backstop
    /// BEHIND flint's exact per-citation version GC, not the reaper.
    ///
    /// Read the inversion before lowering it: gated staging makes the
    /// CITED version noncurrent the moment a newer generation stages,
    /// so a `NoncurrentVersionExpiration` rule over `files/` runs a
    /// clock against live cited data and never against the newest
    /// uncited bytes. A shorter fleet rule covering this prefix
    /// REFUSES gated mode with the offending rule Id named. Cross-
    /// validated against `2 × (visibilityLagBoundSecs + floorSecs)`.
    #[serde(default = "default_noncurrent_retention_days")]
    pub noncurrent_retention_days: u64,

    /// The UDS control door (§2.5, Phase 5): a Unix socket at
    /// `<mountPath>/.flint-sync/ctl.sock` serving `POST /v1/boundary`,
    /// `POST /v1/sync` and `GET /v1/status`.
    ///
    /// Pure sugar over the file protocol — a socket request lands in
    /// the same pending record a `.flint/publish` touch would, so it
    /// obeys the same min-interval, budget and ack rules — with one
    /// thing the files cannot give: a SYNCHRONOUS answer, instead of
    /// polling `.flint/publish.ack`. Pod-internal only; there is no TCP
    /// listener and no auth, because the trust boundary is the pod,
    /// exactly as it already is for the sentinel files.
    ///
    /// Off by default: the file protocol is the only guaranteed
    /// interface, and a bind failure degrades to a log line rather than
    /// failing the workspace.
    #[serde(default)]
    pub uds_door: bool,

    /// Opt-in Prometheus exposition on the pod network (D15). Off by
    /// default: a workspace with metrics disabled is fully operable —
    /// `gauges.json`, the heartbeat echo and `flint-sync status` are
    /// the authority for every operational decision, and `/metrics` is
    /// additive. A bind collision degrades to a condition, never a
    /// crash.
    #[serde(default)]
    pub metrics: MetricsSpec,
}

/// D15's exposition knobs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSpec {
    #[serde(default)]
    pub enabled: bool,
    /// Deliberately not 8080/9090/9100: the agent container is the
    /// likely occupant of the usual ports, and losing the bind is a
    /// degraded condition rather than a failure — but a default that
    /// collides on most pods would make the degraded path the normal
    /// one.
    #[serde(default = "default_metrics_port")]
    pub port: u32,
}

impl Default for MetricsSpec {
    fn default() -> Self {
        MetricsSpec { enabled: false, port: default_metrics_port() }
    }
}

fn default_floor_secs() -> u64 {
    60
}
fn default_max_files() -> u64 {
    250_000
}
fn default_fanout() -> u64 {
    16
}
fn default_size_limit_gib() -> u64 {
    20
}
fn default_mount_path() -> String {
    "/workspace".into()
}
fn default_boundary_mode() -> String {
    "hybrid".into()
}
fn default_sentinels() -> String {
    "auto".into()
}
fn default_sentinel_min_interval_secs() -> u64 {
    5
}
fn default_sentinel_hourly_budget() -> u64 {
    60
}
fn default_quiesce_bound_secs() -> u64 {
    30
}
fn default_staged_backlog_cap_objects() -> u64 {
    5_000
}
fn default_staged_backlog_cap_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}
fn default_noncurrent_retention_days() -> u64 {
    30
}
fn default_metrics_port() -> u32 {
    9847
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlintLeanWorkspaceStatus {
    /// Pending | Claimed | Adopted | Refused | Error
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The claim cell's standing identity when Refused (the operator
    /// never adopts a foreign claim on the fly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_project_id: Option<String>,
    /// Unix time of the last successful operator pass (claim verified,
    /// bootstrap posture checked, MPU sweep run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_unix: Option<u64>,

    /// `BoundaryModeAccepted`, `VersionRetentionProvisioned`,
    /// `SentinelVerbsActive`, `BoundaryModeActive` (§2.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<LeanCondition>>,

    // ── observed, from the live sidecar's lease-heartbeat echo ───────
    //    Every field below reports what the RUNNING binary says, never
    //    what the spec asked for. That distinction is the whole point:
    //    an old sidecar reads a FIXED env list, so a `gated` spec
    //    reaching a pre-boundary binary is ignored in silence.
    /// The mode the sidecar is actually running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_boundary_mode: Option<String>,
    /// The sidecar binary's version — the mixed-fleet tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_sidecar_version: Option<String>,
    /// The last manifest seq the sidecar cited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_seq: Option<u64>,
    /// Seconds since that citation — gated mode's coherence lag, as a
    /// printer column, with no metrics stack in the picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility_lag_secs: Option<u64>,
    /// Durable-but-invisible objects standing right now. This is the
    /// number that would need `recover-staged` if the pod were
    /// replaced this second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_uncited: Option<u64>,
}

/// A metav1.Condition mirror (same field names, same semantics) — the
/// lite operator's `ShareCondition`, kept separate on purpose: the two
/// controllers share no types by design (`mod.rs`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeanCondition {
    pub r#type: String,
    /// `"True"` | `"False"` | `"Unknown"`.
    pub status: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// RFC3339. Bumped only when `status` actually changes, so it means
    /// what it says.
    pub last_transition_time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

pub fn crd() -> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition {
    use kube::CustomResourceExt;
    FlintLeanWorkspace::crd()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The check a cluster would otherwise do for us at install time,
    /// with an error message about junctors — and it takes the WHOLE
    /// CRD down, not the offending field, so every knob in §2.6 would
    /// vanish together. schemars emits `anyOf: [<typed branch>, {null}]`
    /// for an `Option<T>` whose `T` carries its own doc comment, and
    /// Kubernetes refuses `type`/`description`/`default`/`nullable`
    /// inside a logical junctor.
    ///
    /// This is a live control, not a formality: `MetricsSpec` and
    /// `LeanCondition` are the first named types this schema has ever
    /// carried.
    #[test]
    fn crd_is_structural() {
        fn walk(v: &Value, path: &str, bad: &mut Vec<String>) {
            if let Value::Object(m) = v {
                for junctor in ["anyOf", "oneOf", "allOf", "not"] {
                    if m.contains_key(junctor) {
                        bad.push(format!("{path}: {junctor}"));
                    }
                }
                if let Some(Value::Array(items)) = m.get("enum") {
                    if items.iter().any(Value::is_null) {
                        bad.push(format!("{path}: null in enum"));
                    }
                }
                for (k, child) in m {
                    walk(child, &format!("{path}.{k}"), bad);
                }
            } else if let Value::Array(items) = v {
                for (i, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}[{i}]"), bad);
                }
            }
        }
        // The scanner must be able to SEE a junctor, or this test is a
        // green light that means nothing. kube's `KubeSchema` derive
        // currently flattens the `anyOf: [T, null]` that plain schemars
        // emits for an `Option<T>` — which is why no field shape here
        // trips it today, and exactly why the guard stays: the day that
        // derive changes, or somebody adds a shape it does not flatten,
        // the failure is an install-time error about junctors in a
        // cluster rather than a red test here.
        let mut probe = vec![];
        walk(
            &serde_json::json!({"properties": {"x": {"anyOf": [{"type": "string"}]}}}),
            "probe",
            &mut probe,
        );
        assert_eq!(probe.len(), 1, "the structural scanner cannot see a junctor");

        let v = serde_json::to_value(crd()).unwrap();
        let mut bad = vec![];
        walk(&v, "crd", &mut bad);
        assert!(bad.is_empty(), "CRD is not structural — the API server refuses it: {bad:?}");
    }

    /// Every §2.6 knob must reach the schema, and every default must be
    /// today's behavior: an existing CR that names none of them is
    /// byte-identical after the upgrade.
    #[test]
    fn boundary_knobs_default_to_todays_behavior() {
        let spec: FlintLeanWorkspaceSpec = serde_json::from_value(serde_json::json!({
            "projectId": "team-a/p", "bucket": "b", "keyPrefix": "t/p",
        }))
        .unwrap();
        assert_eq!(spec.boundary_mode, "hybrid");
        assert_eq!(spec.sentinels, "auto");
        assert_eq!(spec.sentinel_min_interval_secs, 5);
        assert_eq!(spec.sentinel_hourly_budget, 60);
        assert_eq!(spec.visibility_lag_bound_secs, None);
        assert_eq!(spec.quiesce_bound_secs, 30);
        assert_eq!(spec.noncurrent_retention_days, 30);
        assert!(!spec.metrics.enabled, "metrics must be off by default — D15 is opt-in");

        // And they are really in the published schema, not just in Rust.
        let v = serde_json::to_value(crd()).unwrap();
        let props = &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"];
        for k in [
            "boundaryMode",
            "sentinels",
            "sentinelMinIntervalSecs",
            "sentinelHourlyBudget",
            "visibilityLagBoundSecs",
            "quiesceBoundSecs",
            "stagedBacklogCapObjects",
            "stagedBacklogCapBytes",
            "noncurrentRetentionDays",
            "metrics",
        ] {
            assert!(props.get(k).is_some(), "{k} never reached the CRD schema");
        }
        // The doc-comment carries the trade a user must read before
        // setting gated; a knob whose cost is only in a plan file is a
        // knob nobody reads the cost of.
        let doc = props["boundaryMode"]["description"].as_str().unwrap();
        for phrase in ["recover-staged", "POD RECREATION", "aws s3 cp"] {
            assert!(doc.contains(phrase), "boundaryMode doc-comment never states {phrase:?}");
        }
    }
}
