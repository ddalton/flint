//! The FlintLeanWorkspace CRD.
//!
//! One CR = one lean workspace subtree: a durable project identity, a
//! bucket/prefix address, a durability profile, and budgets. There is
//! deliberately NO Deployment/PVC/Service in its wake — an idle lean
//! workspace is bucket objects and nothing else (the scale-to-zero
//! argument). A pod asks for a workspace by NAMING THE CR in an inline
//! CSI volume — `driver: s3.csi.chert.us`, `volumeAttributes:
//! { chert.us/workspace: <name> }` — and kubelet's NodePublishVolume is
//! what delivers the tree. Before v1.45.0 a mutating webhook injected a
//! syncer for pods carrying the label `chert.us/lean-workspace`; that
//! webhook and its label are gone (`fcac038f`).

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "chert.us",
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
    printcolumn = r#"{"name":"CITED-SEQ","type":"integer","jsonPath":".status.citedSeq"}"#,
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

    /// AWS region of `bucket`. None = the node plugin's default
    /// (`FLINT_S3CSI_REGION`, chart `node.region`), ONE value for the
    /// whole node. A bucket in any other region answers the syncer's
    /// first request `301 PermanentRedirect` and the worker crash-loops
    /// (runcu 2026-09-12: a us-west-1 bucket under a us-east-1 plugin).
    /// A passthrough mount has named its region since its first release;
    /// a workspace needs to for the same reason. A static-identity
    /// Secret's own `AWS_REGION` still wins over both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Secret with the SYNCER's proxy credentials, keys AWS_* VERBATIM
    /// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, ...). The
    /// OPERATOR never uses this — bucket-admin ops run under the
    /// operator principal (plan §2.4 principal split).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_secret_ref: Option<String>,

    /// CSI delivery (`s3.csi.chert.us`, docs/plans/csi-node-mount-design.md):
    /// which ServiceAccounts in this namespace may mount the workspace,
    /// read-write (`serviceAccounts`) or read-only
    /// (`readOnlyServiceAccounts`). ABSENT = DENY — never "any pod in this
    /// namespace".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumers: Option<crate::s3csi::policy::MountConsumers>,
    /// CSI delivery: how the syncer gets its credential (design §4.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<crate::s3csi::policy::Identity>,
    /// CSI delivery: the uid/gid the syncer runs as and the tree is
    /// owned by. REQUIRED under CSI for lean (design §3.5 step 6): a
    /// syncer at a uid other than the app's cannot read the app's 0600
    /// files and would silently skip them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<i64>,

    /// Publish cadence floor, seconds: the durability (RPO) contract.
    #[serde(default = "default_floor_secs")]
    pub floor_secs: u64,

    /// Checkout budgets. Files defaults to the 0b-measured v1 cap
    /// (docs/plans/flint-lean-0b-measurements.md); 0 = unlimited.
    #[serde(default)]
    pub max_bytes: u64,
    #[serde(default = "default_max_files")]
    pub max_files: u64,

    /// Bounded upload/checkout concurrency (measured: useful ceiling is
    /// RTT / 375us, independent of file count; ~32 for same-region S3).
    #[serde(default = "default_fanout")]
    pub fanout: u64,

    /// Ceiling on bytes in flight across the checkout fan-out window, MiB.
    /// `fanout` bounds how MANY objects are fetched at once but not how
    /// big they are, and each is held whole in RAM before it reaches
    /// disk, so peak RSS is the product of the two. Sized against the
    /// syncer's memory limit, not its CPU.
    #[serde(default = "default_fetch_inflight_mb")]
    pub fetch_inflight_mb: u64,

    /// Driver tasks for the checkout fan-out. `fanout` is how many
    /// fetches are in flight; this is how many tasks drive them, i.e.
    /// how many cores the per-request work can use. 0 = auto (the
    /// syncer's cores, at most 8). One driver was the ~3,300 files/s
    /// small-file plateau whatever `fanout` said.
    #[serde(default = "default_fetch_drivers")]
    pub fetch_drivers: u64,

    /// Parts of ONE object uploaded concurrently on publish (default 8;
    /// 1 = sequential). Distinct from `fanout`, which spreads uploads
    /// ACROSS objects: this only moves a tree whose critical path is a
    /// single large object — the checkpoint shape. Measured (runcu
    /// 2026-09-12, `door-drill-2026-09-12.md`, n=3): publishing a 4 GiB
    /// object 8-wide instead of 1 took a checkpoint publish from
    /// 50.7–58.6 s to 13.8–14.4 s (3.6x), the point the NIC saturates;
    /// 16 was no better. It shipped opt-in in v1.51.0 because an upload
    /// part is held whole in RAM and the window had no byte bound, so
    /// peak RSS was `min(large_objects, fanout) x this x 64 MiB` — 16 GiB
    /// at 8 across many large objects. `uploadInflightMb` is that bound:
    /// peak upload RSS is now `min(uploadInflightMb, the bytes actually
    /// live)` plus the fan-out's small bodies, whatever this says, and
    /// this only decides how fast one large object goes.
    #[serde(default = "default_upload_part_parallelism")]
    pub upload_part_parallelism: u64,

    /// Ceiling on bytes in flight across the upload window, MiB — the
    /// write side's `fetchInflightMb`. `fanout` bounds how MANY objects
    /// upload at once and `uploadPartParallelism` how many parts of
    /// each, but not how big they are, and every part and every whole
    /// body is read into RAM before its PUT, so peak upload RSS was the
    /// product of the three. A single part or body larger than the whole
    /// window still uploads, alone. Sized against the syncer's memory
    /// limit, not its CPU; 0 = no bound.
    #[serde(default = "default_upload_inflight_mb")]
    pub upload_inflight_mb: u64,

    /// Write the syncer's protocol event trace: one JSON line on the
    /// worker's stderr per protocol step (each consume, upload outcome,
    /// claim, merge, CAS, garbage-collector delete, queue write, release,
    /// fence, ack and sync), in the format pinned by
    /// `lean/e2e/writers-live/README.md` §2. For explaining what several
    /// writers of one workspace did to each other after the fact, when
    /// the interleaving may not happen again. Off by default: on, it is a
    /// line per step on the worker's log.
    #[serde(default)]
    pub event_trace: bool,

    /// Route every GET and HEAD through the syncer's raw HTTP/1.1 read
    /// path (SigV4 by hand, pooled keep-alive connections, none of the
    /// SDK's per-request machinery) instead of the AWS SDK. Writes
    /// always use the SDK. Integrity does not depend on the client:
    /// every reader verifies each fetch against the manifest's CRC-64.
    #[serde(default)]
    pub raw_reads: bool,

    /// Ceiling on the workspace tree, GiB. 0 = no limit.
    ///
    /// Under the CSI delivery this is a sparse ext4 image loop-mounted
    /// at the tree, so an overrun is ENOSPC in the app's own write; it
    /// was an emptyDir sizeLimit under the sidecar-injection webhook flint
    /// used before v1.45.0, which is where the
    /// name comes from. Sparse: it costs what is written, not what is
    /// declared, so the ceilings on a node may sum to more than the
    /// node's disk. It bounds one workspace's blast radius; it is not a
    /// reservation.
    #[serde(default = "default_size_limit_gib")]
    pub size_limit_gib: u64,

    /// Where the workspace mounts in every container.
    #[serde(default = "default_mount_path")]
    pub mount_path: String,

    /// Syncer image override, and DEAD — it has no reader.
    ///
    /// The webhook that read it went in v1.45.0. The worker image now
    /// comes from `FLINT_S3CSI_LEAN_IMAGE` on the node plugin
    /// (`s3csi/node.rs`), which the flint-s3-csi chart pins, and the
    /// only construction of `WorkerInputs.image` takes it from there.
    /// Setting this on a CR changes nothing. Kept rather than removed
    /// so a spec carrying it still parses — the node plugin REFUSES a
    /// spec it cannot parse — but it should go, with the same
    /// treatment `FlintPassthroughMount.spec.image` needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Expected inventory, used to DERIVE the syncer's startupProbe
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
    // 32, not 16. The blocking checkout on a small-file tree is ~95%
    // round trips, so this multiplies directly against the wall clock
    // the agent waits on. Not higher yet: the per-entry local write
    // chain is blocking `std::fs` driven from ONE runtime task, so past
    // ~32 the extra width queues on that task instead of the wire.
    32
}
fn default_fetch_inflight_mb() -> u64 {
    // 512 → 128 (2026-09-13). The deployed door drill OOM-killed the
    // worker on a 6 x 1 GiB checkout: the plugin-wide worker limit is
    // 1Gi and the checkout peaked at 1105 MiB RSS with 512 MiB in
    // flight. At 128 the same checkout peaks at 403-437 MiB (n=3) in
    // the same wall time (24.9-25.7 s vs 24.1-25.0 s); `mixed` 229 vs
    // 298 MiB, equal time. The window past 128 buys nothing on the NIC.
    128
}
fn default_fetch_drivers() -> u64 {
    0
}
fn default_upload_part_parallelism() -> u64 {
    // 1 → 8 (2026-09-13), the knee of the runcu measurement (see the
    // field doc), made safe by `uploadInflightMb`: the width no longer
    // multiplies into peak RSS.
    8
}
fn default_upload_inflight_mb() -> u64 {
    // 256: four 64 MiB parts in flight, and with `fetchInflightMb`'s 128
    // and the binary's own footprint still under the plugin-wide 1Gi
    // worker limit that OOM-killed a 512 MiB read window. Loopback A/B
    // 2026-09-13, 4 x 256 MiB objects published parts 8-wide (n=3,
    // interleaved): peak RSS 280-311 MiB at 256 against 1048-1049 MiB
    // with the bound effectively off (8192) — the predicted
    // 4 objects x 4 parts x 64 MiB — for the same 46 requests and the
    // same bytes; the serial-parts v1.51.0 default peaked at 278 MiB.
    256
}
fn default_size_limit_gib() -> u64 {
    20
}
fn default_mount_path() -> String {
    "/workspace".into()
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

    /// `SpecAccepted`, `SyncerObserved`, `SentinelVerbsActive`,
    /// `MetricsExposed` (§2.6), `AccessIsolation` (whether the bucket, or
    /// only the mount and the syncer, holds a read-only pod to reads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<LeanCondition>>,

    // ── observed, from the lease cell's echo of the last barrier ────
    //    Every field below reports what the RUNNING binary says, never
    //    what the spec asked for. That distinction is the whole point:
    //    an old syncer reads a FIXED env list, so a knob it predates is
    //    ignored in silence.
    /// The syncer binary's version — the mixed-fleet tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_syncer_version: Option<String>,
    /// The last manifest seq the syncer cited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_seq: Option<u64>,
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

    const RETIRED_KNOBS: &[&str] = &[
        "boundaryMode",
        "visibilityLagBoundSecs",
        "quiesceBoundSecs",
        "stagedBacklogCapObjects",
        "stagedBacklogCapBytes",
        "noncurrentRetentionDays",
    ];

    /// Every §2.6 knob must reach the schema, and every default must be
    /// today's behavior: an existing CR that names none of them is
    /// byte-identical after the upgrade.
    #[test]
    fn boundary_knobs_default_to_todays_behavior() {
        let spec: FlintLeanWorkspaceSpec = serde_json::from_value(serde_json::json!({
            "projectId": "team-a/p", "bucket": "b", "keyPrefix": "t/p",
        }))
        .unwrap();
        assert_eq!(spec.sentinels, "auto");
        assert_eq!(spec.sentinel_min_interval_secs, 5);
        assert_eq!(spec.sentinel_hourly_budget, 60);
        assert!(!spec.metrics.enabled, "metrics must be off by default — D15 is opt-in");

        // And they are really in the published schema, not just in Rust.
        let v = serde_json::to_value(crd()).unwrap();
        let props = &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"];
        for k in ["sentinels", "sentinelMinIntervalSecs", "sentinelHourlyBudget", "metrics"] {
            assert!(props.get(k).is_some(), "{k} never reached the CRD schema");
        }
        // The retired knobs must be GONE from the schema, not merely
        // ignored: a field the schema still carries is a field a CR
        // can set and nothing will honour.
        for k in RETIRED_KNOBS {
            assert!(props.get(*k).is_none(), "{k} is still in the CRD schema");
        }
    }
}
