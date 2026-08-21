//! The `FlintShare` API (flint.io/v1alpha1) — the operator's contract
//! with its users.
//!
//! # Shape rules (all of them load-bearing)
//!
//! - **Volume-shaped, not release-shaped.** The spec never names a
//!   Deployment, a PVC or a Service: those are today's reconcile
//!   outputs, not the API. If the multi-volume topology
//!   (docs/plans/multi-volume-hub-design.md) is ever adopted, the
//!   reconcile changes and this file does not.
//! - **`settings` is an all-`Option` mirror with ZERO schema
//!   defaults.** A CRD's structural defaulting MATERIALIZES defaults
//!   into stored objects at admission: deriving the schema from
//!   [`TierKnobs`] with its serde defaults would write today's numbers
//!   into every CR as if the user had pinned them, and a later
//!   server-side re-pricing (an explicit expectation of the economics
//!   gate) would leave the fleet running the old values —
//!   stale-values-by-construction, the exact class this operator
//!   exists to kill. Unset knobs must stay unset so the SERVER's
//!   default applies. `Option<T>` + `skip_serializing_if` is what
//!   keeps schemars from emitting a `default` key at all, and
//!   `crd_settings_have_no_defaults` pins it.
//! - **Nothing from k8s-openapi is embedded.** Conditions and resource
//!   requirements are small local mirrors, so the crate does not take
//!   k8s-openapi's `schemars` feature (JsonSchema impls for every
//!   Kubernetes type, on a crate that already builds four binaries).
//!   The mirrors use the upstream field names, so `kubectl` prints
//!   them the usual way.
//! - **Identity is immutable, by CEL.** Changing `bucket`/`keyPrefix`
//!   under a live hub is split-brain by construction: the running pod
//!   holds an epoch on the old prefix and the operator would render a
//!   config for a different one.

use std::collections::BTreeMap;

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A mountable NFS share backed by one hub — optionally tiered to one
/// bucket prefix.
///
/// "Share" is deliberate: `FlintVolume` would collide with the
/// CSI/SPDK volume vocabulary this project already uses, and the
/// object-store backing is an option, not the identity (a tier-off
/// share is a perfectly good share).
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "flint.io",
    version = "v1alpha1",
    kind = "FlintShare",
    plural = "flintshares",
    singular = "flintshare",
    shortname = "fsh",
    namespaced,
    status = "FlintShareStatus",
    derive = "PartialEq",
    doc = "A mountable NFS share served by one flint-lite hub, optionally tiered to an S3 prefix",
    printcolumn = r#"{"name":"PHASE","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"ADDRESS","type":"string","jsonPath":".status.address"}"#,
    printcolumn = r#"{"name":"BUCKET","type":"string","jsonPath":".spec.bucket"}"#,
    printcolumn = r#"{"name":"PREFIX","type":"string","jsonPath":".spec.keyPrefix"}"#,
    printcolumn = r#"{"name":"CONFLICT","type":"string","jsonPath":".status.conflictWith.name"}"#,
    // The front door's index. A share's Kubernetes name is derived
    // (`fs-<project-id>`), so this is what makes the mapping legible
    // from the cluster side without decoding names by eye.
    printcolumn = r#"{"name":"PROJECT","type":"string","jsonPath":".metadata.labels['flint.io/project-id']","priority":1}"#,
    printcolumn = r#"{"name":"AGE","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
// Identity immutability. Written as "was it set before?" rather than a
// plain `self == oldSelf` because both fields are optional: a
// transition rule on an absent field would not fire, and turning a
// tier-off share into a tiered one (or re-pointing a tiered one) is
// precisely the split-brain we refuse.
#[x_kube(validation = Rule::new(
    "!has(oldSelf.bucket) ? !has(self.bucket) : (has(self.bucket) && self.bucket == oldSelf.bucket)")
    .message("spec.bucket is immutable — create a new FlintShare instead"))]
#[x_kube(validation = Rule::new(
    "(has(oldSelf.keyPrefix) ? oldSelf.keyPrefix : '') == (has(self.keyPrefix) ? self.keyPrefix : '')")
    .message("spec.keyPrefix is immutable — create a new FlintShare instead"))]
// Prefix syntax, refused at admission rather than discovered as a
// mis-scoped hub: a prefix that does not end in "/" silently shares a
// subtree with its siblings ("tenant-a" also matches "tenant-agency/"),
// and ".flint/" under a prefix is reserved for tier control objects.
#[x_kube(validation = Rule::new(
    "!has(self.keyPrefix) || self.keyPrefix == '' || self.keyPrefix.endsWith('/')")
    .message("spec.keyPrefix must end with '/' — a prefix without it also matches sibling names"))]
#[x_kube(validation = Rule::new("!has(self.keyPrefix) || !self.keyPrefix.startsWith('/')")
    .message("spec.keyPrefix must not start with '/'"))]
#[x_kube(validation = Rule::new("!has(self.keyPrefix) || !self.keyPrefix.contains('.flint/')")
    .message("'.flint/' under a prefix is reserved for tier control objects"))]
// The tier knobs are meaningless without a bucket, and silently
// ignoring them is how a user concludes the operator is broken.
#[x_kube(validation = Rule::new("!has(self.settings) || has(self.bucket)")
    .message("spec.settings needs spec.bucket — the tier is off without one"))]
#[x_kube(validation = Rule::new("!has(self.credentialsSecretRef) || has(self.bucket)")
    .message("spec.credentialsSecretRef needs spec.bucket — the tier is off without one"))]
// Hibernation DELETES the PVC. Without a bucket that PVC is the only
// copy of the data, so this is not a tuning mistake to discover at 3am
// — it is refused at admission.
#[x_kube(validation = Rule::new(
    "!has(self.idle) || !has(self.idle.hibernateAfterSecs) || has(self.bucket)")
    .message("spec.idle.hibernateAfterSecs needs spec.bucket — hibernation deletes the PVC, and without a bucket that PVC is the only copy of the data"))]
// Auto-expand sizes the claim from the MANIFEST, so a share with no
// bucket has nothing to size against — and a rule with no ceiling grows
// a disk with no agreed limit. Both are refused at admission rather
// than discovered on a bill.
#[x_kube(validation = Rule::new(
    "!has(self.persistence.autoExpand) || !has(self.persistence.autoExpand.enabled) || !self.persistence.autoExpand.enabled || has(self.bucket)")
    .message("spec.persistence.autoExpand needs spec.bucket — the size comes from the bucket's manifest"))]
#[x_kube(validation = Rule::new(
    "!has(self.persistence.autoExpand) || !has(self.persistence.autoExpand.enabled) || !self.persistence.autoExpand.enabled || has(self.persistence.autoExpand.maxSize)")
    .message("spec.persistence.autoExpand.maxSize is required when autoExpand is enabled — growth cannot be undone without a reprovision"))]
// `advertiseAddress` is copied into `status.address` verbatim and
// mounted by consumers, so a malformed one is a mount failure in
// somebody else's cluster with nothing local to look at. Require the
// port explicitly: an NFS client given a bare host silently uses 2049,
// which is exactly wrong for the port-per-project shape this exists to
// serve. IPv6 must be bracketed, so the last colon always separates
// host from port.
//
// NO BACKSLASHES IN THE PATTERNS. `\[` is not a valid escape in a CEL
// STRING LITERAL, so a regex-escaped bracket fails to parse before RE2
// ever sees it — and the API server then refuses the WHOLE CRD, taking
// every other rule with it. The opening bracket is checked with
// startsWith instead; a closing bracket needs no escape in RE2 outside
// a character class.
#[x_kube(validation = Rule::new(
    "!has(self.service) || !has(self.service.advertiseAddress) || self.service.advertiseAddress.matches('^[^:]+:[0-9]+$') || (self.service.advertiseAddress.startsWith('[') && self.service.advertiseAddress.matches('^.+]:[0-9]+$'))")
    .message("spec.service.advertiseAddress must be host:port — e.g. hub.example.internal:2049, 10.0.4.7:2049, or [2001:db8::1]:2049 for IPv6"))]
pub struct FlintShareSpec {
    /// Bucket this share publishes to. Must already exist, with
    /// VERSIONING ON (delete-marker recovery assumes it) — the hub
    /// never creates buckets and the operator never touches their
    /// contents.
    ///
    /// **Absent = tier off**: a plain NFS hub whose PVC is the only
    /// copy of the data. Immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,

    /// Prefix under the bucket, e.g. `tenant-a/`. ONE PREFIX = ONE
    /// SHARE = ONE HUB — the operator refuses a second share on the
    /// same subtree (see `lite_operator::conflict`). Immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,

    /// Custom S3 endpoint (MinIO and friends; forces path-style
    /// addressing). Absent = real S3.
    ///
    /// Mutable on purpose — an endpoint can legitimately move — but it
    /// participates in share uniqueness, so a change can make this
    /// share a conflict LOSER and scale it to zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// `AWS_REGION` for the SDK. Absent = ambient (env / IRSA / IMDS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Name of an EXISTING Secret in this namespace, injected wholesale
    /// as env (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` [+
    /// `AWS_SESSION_TOKEN`]). Absent = ambient credentials (IRSA /
    /// instance role), which is the better answer on EKS.
    ///
    /// The operator watches this Secret: a rotation rolls the hub
    /// deliberately instead of waiting for the hub to fence itself out
    /// on a failed heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_secret_ref: Option<String>,

    /// DR / bucket adoption: when the tier state is FRESH and the
    /// bucket holds content, import the namespace before serving.
    /// Absent = the server default (on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_on_start: Option<bool>,

    /// The hub's disk. Sized for the WORKING SET when the tier is on
    /// (durable data lives in the bucket), for the WHOLE DATASET when
    /// it is off.
    pub persistence: PersistenceSpec,

    /// How consumers reach the hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceSpec>,

    /// Hub image override, verbatim (`repo/name:tag`). Absent = the
    /// operator's fleet-wide default, which is how a fleet upgrade is
    /// one operator rollout instead of N edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// `RUST_LOG` for the hub. Absent = `info`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,

    /// Container resources. Requests/limits as plain quantity strings
    /// (`{"memory": "1Gi"}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceSpec>,

    /// Node selector for the hub pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tier tuning. Every knob absent = the SERVER's default, and the
    /// server's defaults are the economics gate's assumptions, not
    /// neutral tuning — so absence is the right answer unless you have
    /// measured a reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<TierSettings>,

    /// `Active` (default) serves; `Suspended` scales the hub to zero
    /// and KEEPS the PVC — the share stops costing compute and stays
    /// instantly wakeable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Lifecycle>,

    /// What CR deletion does to the PVC. Absent = `Retain`.
    /// The BUCKET is never touched, under any policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclaim: Option<Reclaim>,

    /// Adoption: bind this share to an EXISTING PVC instead of
    /// creating one. This is the migration path off the chart (the
    /// chart's PVC is `flint-lite-data`) — see the operator plan's
    /// step 2b. The operator never deletes a claim it adopted unless
    /// `reclaim: Delete` says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_claim: Option<String>,

    /// Whether a config or credential change rolls the hub
    /// immediately. Absent = `Immediate`. `Manual` renders the new
    /// ConfigMap but leaves the running pod alone (status reports
    /// `ConfigCurrent=False`) so the bounce can be scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,

    /// startupProbe budget, in 10-second periods, for the tiered
    /// hub's pre-listener work (epoch claim may WAIT OUT a dead
    /// holder's lease; a DR import walks the whole bucket). Absent =
    /// 60 (ten minutes). Misreading this window as failure is how an
    /// operator kills a takeover at 55 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_failure_threshold: Option<i32>,

    /// Seconds the kubelet waits after SIGTERM before SIGKILL. The hub
    /// spends that window draining, flushing every dirty file, writing
    /// the DR manifest and releasing the epoch — which is what makes
    /// the NEXT start instant instead of a lease wait. Absent = 120.
    /// A hub killed at the deadline is still correct (it leaves the
    /// epoch held so no successor serves a stale bucket), just slow to
    /// wake with its last writes unpublished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_grace_period_seconds: Option<i64>,

    /// The hub's own HTTP surface: `/health`, `/status`, and optionally
    /// the file API. Absent = off, which is the shipped posture for
    /// every share that has not opted in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitoring: Option<MonitoringSpec>,

    /// Wind the share down when nobody is using it. Absent = off, and
    /// each rung is opt-in on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<IdleSpec>,
}

/// The idle ladder: Suspend, then Hibernate.
///
/// **Absent is OFF, per rung.** Defaulting this on would auto-suspend
/// every existing share in a fleet — including tier-off ones whose
/// consumers mount `status.address` as a plain PV and have never heard
/// of the wake annotation. Their mounts would hang, and nothing in
/// their world would know to wake anything.
///
/// The two rungs are not variations of one setting. Suspend scales the
/// hub to zero and KEEPS the PVC: cheap, reversible in seconds, and safe
/// for any share. Hibernate DELETES the PVC, at which point the bucket
/// is the only copy — so it requires a bucket, and the operator verifies
/// a clean flush at drain time before it acts.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("!has(self.hibernateAfterSecs) || has(self.suspendAfterSecs)")
    .message("spec.idle.hibernateAfterSecs requires spec.idle.suspendAfterSecs — hibernate is the lower rung of the same ladder"))]
#[x_kube(validation = Rule::new(
    "!has(self.hibernateAfterSecs) || !has(self.suspendAfterSecs) || self.hibernateAfterSecs >= self.suspendAfterSecs")
    .message("spec.idle.hibernateAfterSecs must be >= spec.idle.suspendAfterSecs"))]
pub struct IdleSpec {
    /// Seconds of no client activity before the hub is scaled to zero.
    /// The PVC is kept, so waking is a pod start.
    ///
    /// Seconds, not a duration string: `15m` and `15M` differ by a
    /// factor of 60 in some parsers and by an error in others, and this
    /// number decides when someone's project goes away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_after_secs: Option<u64>,

    /// Seconds of no client activity before the PVC is DELETED and the
    /// bucket becomes the only copy. Requires `spec.bucket`, and the
    /// operator verifies a clean flush at drain time before acting.
    ///
    /// The CR is never deleted by this — only the disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hibernate_after_secs: Option<u64>,

    /// May the ladder suspend this share while an NFS client still
    /// holds a lease?
    ///
    /// **Read the name literally, and note that the protective value is
    /// `false`.** `suspendWithSessions: false` REFUSES to suspend while
    /// any client holds a lease, even a quiet one — set it for shares
    /// whose consumers cannot tolerate a reconnect, and for any share
    /// mounted from another cluster, where a partition makes live
    /// agents look idle.
    ///
    /// Absent (and `true`) means the ladder suspends on quiet
    /// regardless of leases. That is the default because an idle NFSv4
    /// mount renews its lease forever, so refusing by default would pin
    /// every mounted share awake permanently — which is the state this
    /// ladder exists to end.
    ///
    /// The residual is worth knowing: leases EXPIRE, so a long enough
    /// partition drops the count to zero on its own and the guard stops
    /// guarding. It narrows the window; it does not close it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_with_sessions: Option<bool>,
}

/// The hub's HTTP surface.
///
/// Served on its own port, ClusterIP-only, and NEVER added to the
/// consumer-facing Service — that Service carries NFS and may be a
/// LoadBalancer, which would put a read-write file API on the internet.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringSpec {
    /// Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Absent = 8080.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,

    /// Browse and edit files over HTTP without mounting the share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_api: Option<FileApiSpec>,
}

/// The HTTP file API.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileApiSpec {
    /// Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Name of an EXISTING Secret in this namespace holding the bearer
    /// token under key `token`. The operator projects it into the hub
    /// at a fixed path, so rotating the token is a Secret edit rather
    /// than a pod-spec change.
    ///
    /// Absent = the hub falls back to `FLINT_FILE_API_TOKEN` in its
    /// environment. With NEITHER source the hub logs why and does not
    /// serve the routes at all — there is no token-optional mode for a
    /// surface that can rewrite every file in the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret_ref: Option<String>,

    /// Largest single upload, in bytes. Absent = 5Gi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_upload_bytes: Option<i64>,

    /// Largest single download, in bytes. Absent = 5Gi. A browse click
    /// on a cold file pulls it out of S3 — real, billed egress — so
    /// this is the cap that bounds one careless click. Larger files are
    /// fetched with Range requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_bytes: Option<i64>,

    /// Seconds a download waits for an evicted file to hydrate before
    /// answering 503 with a Retry-After. Absent = 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_wait_secs: Option<i64>,
}

/// The hub's disk.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceSpec {
    /// Requested size, e.g. `20Gi`. Required — capacity is a decision,
    /// not a default: with the tier on this is the working-set budget
    /// that decides how much of the tree stays hot.
    pub size: String,

    /// StorageClass. Absent = the cluster default class. Any CSI
    /// driver's RWO volume works; the hub writes plain files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,

    /// Let a SHRINK of `size` reprovision the disk instead of being
    /// refused. Absent = off, and off is the safe default: Kubernetes
    /// cannot shrink a claim, so the only way to honour a smaller size
    /// is to destroy the volume and make a new one.
    ///
    /// This is never a silent operation. It runs the same
    /// verify-then-delete the hibernate rung uses — the hub must first
    /// prove the bucket can rebuild the tree (`rpoClean`) — and it is
    /// REFUSED outright for a share with no `bucket` (whose PVC is the
    /// only copy) and for an adopted `existingClaim` (which the
    /// operator did not create and does not get to delete).
    ///
    /// The cost is a wake: the new disk is empty, so the hub imports
    /// the bucket and hydrates on demand, and every mounted client sees
    /// a new `serverId` and must remount. Turn it on deliberately, for
    /// a share whose disk you actually intend to resize downward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reprovision_on_shrink: Option<bool>,

    /// Grow the claim to fit the project, instead of making someone
    /// guess `size` up front. Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_expand: Option<AutoExpandSpec>,
}

/// Size the disk from what the bucket actually holds.
///
/// The hub publishes two numbers once it has read a manifest: the
/// project's total logical bytes, and its largest single object. This
/// turns them into a claim size, so `size` becomes a STARTING point
/// rather than a guess that has to be right.
///
/// The operator never writes `spec` — the target lives in an
/// operator-owned annotation, and `size` stays exactly what the user
/// set. Editing `size` discards the target and starts over from the
/// new number, so the user's edit always wins.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AutoExpandSpec {
    /// Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Headroom over the project's logical size, in percent. Absent =
    /// 100 (twice the project). Growth is one-way and a PVC cannot be
    /// shrunk, so this buys quiet at the cost of disk — 0 is legal and
    /// means "exactly the project", which will expand again on the
    /// next write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_percent: Option<u32>,

    /// Hard ceiling. REQUIRED when enabled: expansion cannot be undone
    /// without a reprovision, so an unbounded rule is a bill nobody
    /// agreed to. The claim stops here and the share says so in
    /// `PersistenceCurrent` rather than growing quietly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<String>,
}

/// How consumers reach the hub.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    /// Absent = `ClusterIP`. NFS is one long-lived TCP flow: prefer
    /// flat/peered networks to a cloud LB, and mind LB idle timeouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<ServiceType>,

    /// Absent = 2049.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,

    /// Fixed nodePort when `type: NodePort`. Absent = Kubernetes picks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_port: Option<i32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,

    /// What `status.address` should say, verbatim, as `host:port`.
    ///
    /// **This is the only way a consumer OUTSIDE this cluster gets a
    /// mountable address.** Derived addresses do not travel: a
    /// ClusterIP is not routable from another cluster, and a NodePort
    /// Service still resolves to the in-cluster DNS name — so a
    /// workload-cluster client that reads `status.address` and mounts
    /// it gets a name it cannot resolve. Only `type: LoadBalancer`
    /// derives something routable on its own, and NFS is one
    /// long-lived TCP flow, so a flat or peered network beats a cloud
    /// LB and its idle timeouts.
    ///
    /// Set this to whatever the consumer should actually dial — a
    /// peered-VPC address, an internal L4 endpoint's `host:port`, a
    /// DNS name that resolves on both sides. The operator does NOT
    /// change the Service it creates; this changes only what it
    /// ADVERTISES, so the in-cluster path keeps working unchanged.
    ///
    /// IPv6 goes in brackets: `[2001:db8::1]:2049`.
    ///
    /// Deliberately MUTABLE — unlike `bucket`/`keyPrefix`, an endpoint
    /// can legitimately move. It is not part of the share's identity.
    /// Clients already mounted keep pointing at the old address until
    /// they remount; nothing recalls them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise_address: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ServiceType {
    ClusterIP,
    NodePort,
    LoadBalancer,
}

/// Container resources, as quantity strings — a deliberate mirror of
/// the useful half of `ResourceRequirements` (see the module doc on
/// not taking k8s-openapi's schemars feature).
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BTreeMap<String, String>>,
}

/// Lifecycle. `Hibernated` (delete the PVC, wake by DR import + warm
/// fill) is deliberately NOT here yet: it needs the final-flush-on-
/// SIGTERM guarantee, which is unverified — an unflushed hibernate
/// loses the RPO window permanently, since the PVC goes with it.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub enum Lifecycle {
    #[default]
    Active,
    Suspended,
}

/// What CR deletion does to the PVC.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub enum Reclaim {
    /// Keep the PVC. The default, and the reason the PVC carries no
    /// ownerReference: owner GC would collect it regardless of what
    /// this field says.
    #[default]
    Retain,
    /// Delete the PVC with the CR. The bucket is still untouched — for
    /// a tiered share the durable copy survives; for a tier-off share
    /// this destroys the data.
    ///
    /// **An adopted claim (`spec.existingClaim`) is never deleted, even
    /// with this set.** The operator does not delete volumes it did not
    /// create; the share is removed, the claim is left, and a
    /// `ReclaimRefused` event says so. Hibernation has always refused
    /// on the same grounds.
    Delete,
}

/// Whether an input change rolls the hub now.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    #[default]
    Immediate,
    Manual,
}

/// The tuning knobs, mirrored from
/// [`crate::pnfs::config::TierKnobs`] as all-`Option` with no
/// defaults.
///
/// Adding a knob to `TierKnobs` without adding it here fails
/// `crd_settings_mirror_matches_tier_knobs`. Adding a `#[serde(default
/// = ...)]` here (or dropping a `skip_serializing_if`) fails
/// `crd_settings_have_no_defaults` — that is not a style rule, it is
/// the unset-takes-server-default contract.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct TierSettings {
    /// Per-file flush-interval floor, seconds — the cap on a hot
    /// file's request bill (server default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flush_floor_secs: Option<u64>,
    /// Quiescence guard: files touched more recently are skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiesce_secs: Option<u64>,
    /// Flush loop cadence, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_secs: Option<u64>,
    /// Below this size a generation publishes as ONE conditional PUT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whole_put_max_bytes: Option<u64>,
    /// Multipart part-grid floor (clamped up to the backend minimum).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_floor_bytes: Option<u64>,
    /// Epoch heartbeat interval, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_heartbeat_secs: Option<u64>,
    /// Missed heartbeats before a successor may judge this holder dead
    /// (lease TTL ≈ heartbeat × misses — and the startupProbe budget
    /// must cover it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_lease_misses: Option<u32>,
    /// Admission headroom: writes answer NFS4ERR_NOSPC while
    /// `avail − reserve` cannot cover them (NOSPC before hard-full —
    /// F55: EIO makes postgres PANIC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_bytes: Option<u64>,
    /// Eviction-trigger watermark, percent used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark_pct: Option<u8>,
    /// Ballast next to state.db, released at critical fullness so the
    /// durable bookkeeping keeps committing. 0 disables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ballast_bytes: Option<u64>,
    /// In-RPC park bound while a hydration runs, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_hold_secs: Option<u64>,
    /// Concurrent demand restores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_concurrency: Option<usize>,
    /// Ranged GETs per restore (the cold-read fan-out). Peak restore
    /// buffering ≈ hydrateConcurrency × this × 8 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_fetch_parallel: Option<usize>,
    /// After a DR/adopt import, bulk-restore the tree instead of
    /// waiting for demand touches. The wake-up half of a cold share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_warm_after_import: Option<bool>,
    /// Concurrent warm-fill restores (a pool of its own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_warm_concurrency: Option<usize>,
}

/// Which way the winner's prefix sits relative to this share's — and
/// therefore whether a redirect is even possible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ConflictRelation {
    /// The winner owns EXACTLY this prefix. Mount its export root.
    Same,
    /// The winner owns a prefix ABOVE this one, so it already serves
    /// these bytes: mount its export root and use `subPath`.
    Ancestor,
    /// The winner owns a prefix BELOW this one. There is nothing to
    /// redirect to — this share asked for MORE than the winner serves,
    /// and no hub covers the difference.
    Descendant,
}

/// The share that owns this bucket subtree, as a machine-readable
/// field rather than a sentence to regex out of a condition message.
///
/// # Why the address is conditional
///
/// A hub's NFS export has no per-client authentication: whoever can
/// reach `status.address` can read the tree. Publishing a winner's
/// address into a LOSER's status therefore hands out a mount target,
/// and arbitration is fleet-wide across namespaces — so doing it
/// unconditionally would let a typo'd prefix in one namespace be
/// answered with a pointer at another tenant's live data.
///
/// The rule is that this field may only tell a reader something they
/// could already have read for themselves: the address is set ONLY
/// when the winner is in the SAME namespace, where anyone able to read
/// this CR can read the winner's too. Across namespaces the winner is
/// still NAMED — that much is already in the condition message — and
/// resolving the name to an address is left to a caller that holds
/// the wider read, which is the point at which it becomes an
/// authorization decision instead of a side effect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConflictWith {
    pub namespace: String,
    pub name: String,
    /// The key prefix the winner owns.
    pub prefix: String,
    pub relation: ConflictRelation,
    /// This share's prefix relative to the winner's export root —
    /// present only for `Ancestor`, and only when the winner's prefix
    /// ends at a path boundary, so it is always a usable path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
    /// Where the winner serves. Same namespace only — see above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// Observed state. Everything here is derived — the operator never
/// asks the user to write status, and never reads it back as input.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct FlintShareStatus {
    /// One-word summary for `kubectl get`. The detail is in
    /// `conditions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,

    /// What consumers mount (`host:port`), once there is something to
    /// mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,

    /// The `.metadata.generation` this status describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// The PVC actually in use — canonical or adopted. Recorded
    /// because an adopted share's children do NOT follow the
    /// CR-derived naming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_name: Option<String>,

    /// The hub's persisted NFS server identity, as last observed.
    ///
    /// **A change here means every existing mount is stale.** The id is
    /// stable across ordinary restarts, because it lives on the PVC
    /// with the rest of the NFS state — but a hibernate deletes that
    /// PVC, so a woken share comes back with a new one and the
    /// stateids clients still hold refer to a server generation that
    /// no longer exists. A front door that records this value alongside
    /// a brokered mount can tell the difference between "the hub
    /// bounced, carry on" and "remount before you trust that handle",
    /// without polling the hub itself.
    ///
    /// `None` = not observed yet, or a hub too old to report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,

    /// Set while `Conflict` is True: who owns this bucket subtree, and
    /// whether this share's bytes are already being served by them.
    ///
    /// The `Conflict` condition's message has always named the winner
    /// in prose. This is the same fact in a shape a front door can act
    /// on without a regex — and it carries the two things the sentence
    /// never did: whether a redirect is possible at all, and where to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_with: Option<ConflictWith>,

    /// Standard conditions (`Ready`, `ConfigCurrent`, `Conflict`,
    /// `AdoptionBlocked`), upstream field-for-field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<ShareCondition>>,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    /// Children applied, no hub pod running yet.
    #[default]
    Pending,
    /// The pod is up but pre-listener: claiming the epoch (possibly
    /// waiting out a dead holder's lease) and/or importing. This is
    /// PROGRESS, not failure — the single most important thing this
    /// operator must not misread.
    Starting,
    /// Serving.
    Ready,
    /// `lifecycle: Suspended` — scaled to zero, PVC kept. An ADMIN
    /// decision: a wake request does not override it.
    Suspended,
    /// Scaled to zero by the idle ladder, PVC kept. Distinct from
    /// `Suspended` on purpose: the front door has to be able to tell
    /// "will wake on request" from "an admin said no", and one phase
    /// for both makes that impossible.
    IdleSuspended,
    /// Scaled to zero AND the PVC deleted — the bucket is the only
    /// copy. Waking is a full DR import.
    Hibernated,
    /// The disk is being rebuilt at a smaller size, because
    /// `persistence.reprovisionOnShrink` is on and `persistence.size`
    /// went down. The share is briefly down and comes back on a NEW,
    /// empty claim, so a consumer must expect a fresh `serverId` and a
    /// DR import — the same contract as waking from `Hibernated`.
    Reprovisioning,
    /// Refused: another share owns this bucket subtree, or adoption is
    /// blocked. See conditions.
    Failed,
    /// The CR is being deleted and the finalizer is honoring `reclaim`.
    ///
    /// Reported because a front door cannot infer it: the finalizer
    /// keeps the object in the API (and in the operator's reflector)
    /// with its last status intact, so an ensure-live loop written as
    /// `GET → 404? create it` gets a **200**, skips the create, polls,
    /// and reads whatever phase the share last had. Owner GC has not
    /// run yet either, so the Deployment and Service are still up and
    /// the mount would even succeed — right up until they are
    /// collected under a hard NFS mount. `status.address` is cleared
    /// on entry to this phase for the same reason.
    Terminating,
}

/// A metav1.Condition mirror (same field names, same semantics).
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShareCondition {
    pub r#type: String,
    /// `"True"` | `"False"` | `"Unknown"`.
    pub status: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// RFC3339. Only bumped when `status` actually changes, so it
    /// means what it says.
    pub last_transition_time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// The CRD, made STRUCTURAL.
///
/// Kubernetes refuses a CRD whose schema sets `type`, `description`,
/// `default` or `nullable` inside a logical junctor (`anyOf`/`oneOf`/
/// `allOf`/`not`) — and that is exactly what schemars emits for an
/// `Option<T>` whose `T` carries its own doc comment: `anyOf: [<the
/// enum, with a description>, {enum: [null], nullable: true}]`. The
/// raw `FlintShare::crd()` is therefore REJECTED AT INSTALL with a
/// validation error about junctors, which is a miserable thing to
/// discover in a cluster.
///
/// Flattening is lossless here because every junctor we emit has the
/// same shape — one real branch plus a null branch — so the real
/// branch merges into the parent and `nullable: true` carries the
/// rest. `crd_is_structural` walks the result and fails if any junctor
/// (or a `null` inside an `enum`, which the API server also rejects)
/// survives.
pub fn crd() -> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition
{
    use kube::CustomResourceExt;
    let mut v = serde_json::to_value(FlintShare::crd()).expect("CRD serializes");
    structuralize(&mut v);
    serde_json::from_value(v).expect("CRD round-trips")
}

fn structuralize(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            // anyOf: [real, null-branch]  ⇒  real + nullable: true
            if let Some(Value::Array(branches)) = map.get("anyOf").cloned() {
                let is_null_branch = |b: &Value| {
                    b.get("type").and_then(Value::as_str) == Some("null")
                        || b.get("enum")
                            .and_then(Value::as_array)
                            .is_some_and(|e| e.iter().all(Value::is_null))
                };
                if branches.len() == 2 && branches.iter().any(is_null_branch) {
                    if let Some(Value::Object(real)) =
                        branches.iter().find(|b| !is_null_branch(b)).cloned()
                    {
                        map.remove("anyOf");
                        for (k, val) in real {
                            // The FIELD's own description wins over the
                            // type's — it is the one written for this
                            // use of the type.
                            if k == "description" && map.contains_key("description") {
                                continue;
                            }
                            map.insert(k, val);
                        }
                        map.insert("nullable".into(), Value::Bool(true));
                    }
                }
            }
            // `enum: [A, B, null]` — the API server rejects a null enum
            // member; `nullable` already says the field may be absent.
            if let Some(Value::Array(items)) = map.get_mut("enum") {
                if items.iter().any(Value::is_null) {
                    items.retain(|i| !i.is_null());
                    map.insert("nullable".into(), Value::Bool(true));
                }
            }
            for (_, child) in map.iter_mut() {
                structuralize(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(structuralize),
        _ => {}
    }
}

impl FlintShareSpec {
    /// `keyPrefix` with the absent case resolved. Empty means "the
    /// whole bucket", which is legal and is exactly why the conflict
    /// predicate treats it as an ancestor of every other prefix.
    pub fn prefix(&self) -> &str {
        self.key_prefix.as_deref().unwrap_or("")
    }

    /// Endpoint normalized for comparison: absent and empty are the
    /// same store (real S3), and a trailing slash is not a different
    /// server.
    pub fn endpoint_key(&self) -> String {
        self.endpoint
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string()
    }

    /// Is the S3 tier on? One question, one answer, everywhere.
    pub fn tiered(&self) -> bool {
        self.bucket.as_deref().is_some_and(|b| !b.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;
    use serde_json::Value;

    fn crd_json() -> Value {
        serde_json::to_value(super::crd()).unwrap()
    }

    /// The check a cluster would otherwise do for us, at install time,
    /// with an error message about junctors. Structural means: no
    /// `anyOf`/`oneOf`/`allOf`/`not` anywhere (this schema needs none),
    /// no `null` inside an `enum`, and every property node typed.
    #[test]
    fn crd_is_structural() {
        fn walk(v: &Value, path: &str, bad: &mut Vec<String>) {
            if let Value::Object(m) = v {
                for junctor in ["anyOf", "oneOf", "allOf", "not"] {
                    if m.contains_key(junctor) {
                        bad.push(format!("{path}.{junctor}"));
                    }
                }
                if let Some(Value::Array(e)) = m.get("enum") {
                    if e.iter().any(Value::is_null) {
                        bad.push(format!("{path}.enum contains null"));
                    }
                }
                if let Some(Value::Object(props)) = m.get("properties") {
                    for (name, child) in props {
                        if !child.get("type").is_some()
                            && child.get("x-kubernetes-preserve-unknown-fields").is_none()
                            && child.get("x-kubernetes-int-or-string").is_none()
                        {
                            bad.push(format!("{path}.properties.{name} has no type"));
                        }
                        walk(child, &format!("{path}.{name}"), bad);
                    }
                }
                if let Some(items) = m.get("items") {
                    walk(items, &format!("{path}[]"), bad);
                }
            }
        }
        let crd = crd_json();
        let mut bad = Vec::new();
        walk(
            &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"],
            "openAPIV3Schema",
            &mut bad,
        );
        assert!(
            bad.is_empty(),
            "the API server would refuse this CRD as non-structural: {bad:?}"
        );
    }

    /// The flattening must keep the enum's VALUES — a nullable field
    /// whose constraint was dropped would accept `lifecycle: Banana`.
    #[test]
    fn flattening_keeps_the_enum_constraints() {
        let crd = crd_json();
        let props = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"];
        for (field, want) in [
            ("lifecycle", vec!["Active", "Suspended"]),
            ("reclaim", vec!["Retain", "Delete"]),
            ("restartPolicy", vec!["Immediate", "Manual"]),
        ] {
            let got: Vec<&str> = props[field]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{field} lost its enum: {}", props[field]))
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(got, want, "{field}");
            assert_eq!(props[field]["type"], "string", "{field}");
            assert_eq!(props[field]["nullable"], true, "{field}");
        }
        let ty = &props["service"]["properties"]["type"];
        assert_eq!(
            ty["enum"].as_array().unwrap().len(),
            3,
            "service.type kept its enum: {ty}"
        );
    }

    /// The schema of `spec.settings`, as the API server would store it.
    fn settings_schema() -> Value {
        let crd = crd_json();
        crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
            ["settings"]
            .clone()
    }

    fn spec_with(settings: TierSettings) -> FlintShareSpec {
        FlintShareSpec {
            bucket: Some("b".into()),
            key_prefix: Some("p/".into()),
            endpoint: None,
            region: None,
            credentials_secret_ref: None,
            import_on_start: None,
            persistence: PersistenceSpec {
                size: "20Gi".into(),
                storage_class_name: None,

                reprovision_on_shrink: None,
                    auto_expand: None,
            },
            service: None,
            image: None,
            log_level: None,
            resources: None,
            node_selector: None,
            settings: Some(settings),
            lifecycle: None,
            reclaim: None,
            existing_claim: None,
            restart_policy: None,
            startup_failure_threshold: None,
            termination_grace_period_seconds: None,
            monitoring: None,
            idle: None,
        }
    }

    /// The parity test the plan calls for: every server knob is
    /// expressible, and nothing else is. A knob added to `TierKnobs`
    /// with no mirror here would be UNREACHABLE through the CR (the
    /// silent-default class the CRD exists to retire); a mirror field
    /// with no server knob would be silently ignored by the parser,
    /// which is the same bug wearing the other hat.
    #[test]
    fn crd_settings_mirror_matches_tier_knobs() {
        let knobs = crate::pnfs::config::TierKnobs::default();
        let server: std::collections::BTreeSet<String> = serde_json::to_value(&knobs)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();

        let props = settings_schema()["properties"].clone();
        let crd: std::collections::BTreeSet<String> =
            props.as_object().unwrap().keys().cloned().collect();

        assert_eq!(
            server, crd,
            "FlintShare spec.settings drifted from TierKnobs — every server knob needs a mirror \
             field (and vice versa)"
        );
    }

    /// The whole point of the all-`Option` mirror. A `default` key
    /// anywhere under `spec.settings` means the API server writes that
    /// value into every stored CR at admission, and the operator can
    /// no longer tell "user pinned 60" from "nobody said" — after
    /// which a server-side re-pricing never reaches the fleet.
    #[test]
    fn crd_settings_have_no_defaults() {
        fn find_defaults(v: &Value, path: &str, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    for (k, child) in m {
                        if k == "default" {
                            out.push(path.to_string());
                        }
                        find_defaults(child, &format!("{path}.{k}"), out);
                    }
                }
                Value::Array(a) => {
                    for (i, child) in a.iter().enumerate() {
                        find_defaults(child, &format!("{path}[{i}]"), out);
                    }
                }
                _ => {}
            }
        }

        let mut found = Vec::new();
        find_defaults(&settings_schema(), "spec.settings", &mut found);
        assert!(
            found.is_empty(),
            "spec.settings must carry NO schema defaults (CRD structural defaulting would \
             materialize them into stored objects); found at {found:?}"
        );

        // And the whole schema, for the same reason one level up: a
        // default on any spec field is a value the operator can never
        // re-decide later.
        let crd = crd_json();
        let mut all = Vec::new();
        find_defaults(
            &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"],
            "spec",
            &mut all,
        );
        assert!(all.is_empty(), "FlintShare spec carries schema defaults at {all:?}");
    }

    /// Absence must survive the round trip: a sparse `settings` block
    /// serializes back to exactly the knobs the user set, which is what
    /// makes "unset = server default" true in the rendered mds.yaml.
    #[test]
    fn sparse_settings_round_trip_to_only_what_was_set() {
        let spec = spec_with(TierSettings {
            watermark_pct: Some(90),
            ..Default::default()
        });
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["settings"], serde_json::json!({"watermarkPct": 90}));

        // ... and the empty case is an empty map, not fifteen nulls.
        let spec = spec_with(TierSettings::default());
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["settings"], serde_json::json!({}));
    }

    /// Identity immutability and prefix syntax are refused by the API
    /// server, not by a reconcile that has already rendered a config
    /// for the wrong prefix. This asserts the rules are IN the CRD;
    /// the kind e2e proves the API server enforces them.
    #[test]
    fn crd_carries_the_identity_and_prefix_rules() {
        let crd = serde_json::to_string(&FlintShare::crd()).unwrap();
        assert!(crd.contains("x-kubernetes-validations"), "no CEL rules emitted");
        for needle in [
            "spec.bucket is immutable",
            "spec.keyPrefix is immutable",
            "must end with '/'",
            "reserved for tier control objects",
        ] {
            assert!(crd.contains(needle), "CRD lost the rule: {needle}");
        }
    }

    /// Print columns and the short name are the fleet dashboard —
    /// `kubectl get fsh` has to answer "which of these is unhappy, and
    /// what does it own" without a describe.
    #[test]
    fn crd_prints_the_fleet_columns() {
        let crd = crd_json();
        let cols = &crd["spec"]["versions"][0]["additionalPrinterColumns"];
        let names: Vec<&str> = cols
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["PHASE", "ADDRESS", "BUCKET", "PREFIX", "CONFLICT", "PROJECT", "AGE"]
        );
        // PROJECT is priority 1 — `kubectl get flintshares` stays
        // narrow and `-o wide` shows the front door's index.
        let project = cols
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "PROJECT")
            .expect("PROJECT column");
        assert_eq!(project["priority"], 1);
        // Bracket notation, because the label key contains dots and a
        // backslash escape is not valid JSON.
        assert_eq!(
            project["jsonPath"],
            ".metadata.labels['flint.io/project-id']"
        );
        assert_eq!(crd["spec"]["names"]["shortNames"][0], "fsh");
        assert_eq!(crd["spec"]["scope"], "Namespaced");
        assert!(crd["spec"]["versions"][0]["subresources"]["status"].is_object());
    }

    #[test]
    fn spec_helpers_normalize_identity() {
        let mut spec = spec_with(TierSettings::default());
        spec.endpoint = Some("http://minio:9000/".into());
        assert_eq!(spec.endpoint_key(), "http://minio:9000");
        spec.endpoint = None;
        assert_eq!(spec.endpoint_key(), "");
        assert!(spec.tiered());
        spec.bucket = None;
        assert!(!spec.tiered(), "no bucket is a tier-off share, not a broken one");
        spec.key_prefix = None;
        assert_eq!(spec.prefix(), "");
    }
}
