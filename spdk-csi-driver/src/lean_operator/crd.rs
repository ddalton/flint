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
}

pub fn crd() -> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition {
    use kube::CustomResourceExt;
    FlintLeanWorkspace::crd()
}
