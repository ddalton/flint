//! Phase 4: the operator's half of the boundary contract (lean
//! boundary-verbs plan §2.6, D8, D10).
//!
//! Everything here answers one of two questions, and the split matters:
//!
//! - **Is this spec coherent?** — pure functions over the CR, no bucket
//!   and no cluster. A gated workspace without a lag bound, a retention
//!   shorter than the staging window it must outlive, a drain that
//!   cannot finish inside a spot reclaim. These refuse at reconcile,
//!   before a byte is staged.
//! - **Is this BUCKET able to carry gated mode?** — the versioning
//!   conformance probe and the lifecycle read. Both are re-run on the
//!   operator's cadence rather than once, because proxies upgrade and a
//!   bucket's lifecycle rules change under you: a once-at-install probe
//!   is a claim about the past, not a posture.
//!
//! The refusals are deliberately conditions rather than webhook
//! rejections. A CR that goes from acceptable to unacceptable — because
//! someone else added a 1-day noncurrent rule over the prefix — was
//! never re-admitted by a webhook, and that is exactly the case D8
//! exists to catch.

use flint_store::{LifecycleView, ObjectStore, RetentionOutcome};

use super::crd::{FlintLeanWorkspaceSpec, LeanCondition};

/// The routine ceiling a preStop drain is really sized against on this
/// fleet (D10, and the standing pure-spot directive): EC2 spot gives
/// ~2 minutes of reclaim notice, and native-sidecar ordering spends the
/// agent's share of that budget first. A gated workspace whose drain
/// arithmetic does not fit inside it is not "tight", it is a workspace
/// whose backlog caps are set to a size it can never drain — so the
/// caps, not the grace, are what the refusal names.
pub const SPOT_RECLAIM_CEILING_SECS: u64 = 120;

/// The bounded-retry budget the SIGTERM arm spends before releasing the
/// lease (3 attempts, 2 s apart — `bin/flint_sync.rs`).
const DRAIN_RETRY_SECS: u64 = 6;

/// Slack over the arithmetic. The drain also scans, CASes and settles
/// owed acks; none of that is proportional to the backlog.
const DRAIN_SLACK_SECS: u64 = 15;

/// Conservative proxy-shaped planning rates, deliberately the SAME
/// constants the startupProbe derivation uses (`inject.rs`) — one place
/// to re-measure through a real proxy, and a derivation that cannot
/// disagree with itself about how fast this fleet moves bytes.
const SECS_PER_GIB: u64 = 15;
const FILES_PER_SEC: u64 = 500;

/// How long this workspace's final drain can take, in the worst case
/// its own knobs permit (D10 rule 3).
///
/// The two modes are sized from different facts, and pretending
/// otherwise would produce a number with no meaning:
///
/// - **gated** — the backlog caps bound the staged set BY CONSTRUCTION
///   (a forced citation fires at the cap), so the drain's upper bound is
///   exactly those caps at the planning rates. This is the number the
///   spot ceiling is checked against.
/// - **cadence/hybrid** — nothing stages, so the drain repeats at most
///   one floor's barrier. A workspace whose barrier does not fit inside
///   its own floor is already failing its RPO contract for reasons that
///   have nothing to do with SIGTERM, so the floor IS the estimate.
pub fn drain_need_secs(spec: &FlintLeanWorkspaceSpec) -> u64 {
    let base = if spec.boundary_mode == "gated" {
        let gib = spec.staged_backlog_cap_bytes.div_ceil(1 << 30);
        gib * SECS_PER_GIB + spec.staged_backlog_cap_objects.div_ceil(FILES_PER_SEC)
    } else {
        spec.floor_secs
    };
    base + DRAIN_RETRY_SECS + DRAIN_SLACK_SECS
}

/// The grace period the webhook stamps on an injected pod. Never below
/// the 30 s the pod would otherwise inherit — the hazard D10 names is
/// that today's injected sidecar sets NO `terminationGracePeriodSeconds`
/// at all, so every workspace drains inside a number nobody chose.
pub fn derived_grace_secs(spec: &FlintLeanWorkspaceSpec) -> u64 {
    drain_need_secs(spec).max(30)
}

/// A refusal: the reason goes in the condition, the message goes to a
/// human who has to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
    pub message: String,
}

fn refuse(reason: &str, message: String) -> Refusal {
    Refusal { reason: reason.to_string(), message }
}

/// Spec-only validation (no bucket, no cluster). `Ok(())` ⇒ the knobs
/// are coherent with each other.
pub fn validate_spec(spec: &FlintLeanWorkspaceSpec) -> Result<(), Refusal> {
    if !matches!(spec.boundary_mode.as_str(), "cadence" | "hybrid" | "gated") {
        return Err(refuse(
            "InvalidBoundaryMode",
            format!(
                "boundaryMode {:?} is not cadence|hybrid|gated — the sidecar would exit 2 at \
                 startup and the pod would crash-loop with a full checkout behind it",
                spec.boundary_mode
            ),
        ));
    }
    if !matches!(spec.sentinels.as_str(), "auto" | "off" | "force") {
        return Err(refuse(
            "InvalidSentinelMode",
            format!("sentinels {:?} is not auto|off|force", spec.sentinels),
        ));
    }
    if spec.boundary_mode != "gated" {
        return Ok(());
    }

    let Some(lag) = spec.visibility_lag_bound_secs else {
        return Err(refuse(
            "LagBoundRequired",
            "boundaryMode: gated requires visibilityLagBoundSecs — unbounded citation \
             staleness is refused by construction, not by convention"
                .into(),
        ));
    };
    if lag == 0 {
        return Err(refuse(
            "LagBoundRequired",
            "visibilityLagBoundSecs must be positive; 0 would force a citation every tick, \
             which is cadence mode with extra requests"
                .into(),
        ));
    }

    // D8's K=2 cross-validation. It reads like a check on the lag bound
    // and is really a check on the RETENTION knob: at the 30-day default
    // it cannot fire until a lag bound exceeds ~15 days, which no fleet
    // will set. It binds when someone LOWERS retention — a 1-day
    // retention against an hour-scale lag bound is the case worth
    // catching, because the backstop then reaps CITED versions.
    let window = 2 * (lag + spec.floor_secs);
    let retention = spec.noncurrent_retention_days.saturating_mul(86_400);
    if retention <= window {
        return Err(refuse(
            "RetentionTooShort",
            format!(
                "noncurrentRetentionDays={} ({}s) must exceed 2x(visibilityLagBoundSecs {} + \
                 floorSecs {}) = {}s: gated staging makes the CITED version noncurrent, so a \
                 retention shorter than one staging window reaps live published data",
                spec.noncurrent_retention_days,
                retention,
                lag,
                spec.floor_secs,
                window
            ),
        ));
    }

    let need = drain_need_secs(spec);
    if need > SPOT_RECLAIM_CEILING_SECS {
        return Err(refuse(
            "GraceTooShort",
            format!(
                "the derived preStop drain needs {need}s at the configured backlog caps \
                 (stagedBacklogCapBytes={}, stagedBacklogCapObjects={}), which exceeds the \
                 {SPOT_RECLAIM_CEILING_SECS}s spot-reclaim ceiling this fleet actually gets — \
                 lower the caps so a reclaimed pod can drain what it staged",
                spec.staged_backlog_cap_bytes, spec.staged_backlog_cap_objects
            ),
        ));
    }
    Ok(())
}

/// Does `rule` put a clock on versions under `files_prefix` that is
/// shorter than the retention this workspace requires?
///
/// Overlap in EITHER direction counts. A rule scoped above us covers
/// every key we own; a rule scoped below us covers some of them — and
/// "only some of the published data was destroyed" is not a passing
/// verdict.
fn covers(rule: &LifecycleView, files_prefix: &str) -> bool {
    rule.prefix.is_empty()
        || files_prefix.starts_with(&rule.prefix)
        || rule.prefix.starts_with(files_prefix)
}

/// The first enabled rule whose noncurrent expiration is shorter than
/// `want_days` and whose scope overlaps `files_prefix`.
pub fn shorter_covering_rule<'a>(
    rules: &'a [LifecycleView],
    files_prefix: &str,
    want_days: u64,
) -> Option<&'a LifecycleView> {
    rules.iter().find(|r| {
        r.enabled
            && covers(r, files_prefix)
            && r.noncurrent_days.is_some_and(|d| d < want_days)
    })
}

/// What the bucket said about this workspace's gated posture.
#[derive(Debug, Clone, Default)]
pub struct BucketVerdict {
    /// `None` ⇒ accepted.
    pub refusal: Option<Refusal>,
    /// The retention rule installed (or found) on `<prefix>/files/`.
    pub retention: Option<RetentionOutcome>,
    /// Why retention could not be provisioned, when it could not.
    pub retention_error: Option<String>,
    /// True when the conformance probe ran and passed.
    pub probe_passed: bool,
}

/// The probe key the OPERATOR writes. Deliberately distinct from the
/// sidecar's: the two probe on independent clocks and share a bucket,
/// and a shared key would let two conformant probes fail each other's
/// `If-None-Match` write — a conformance failure manufactured by
/// conformance checking.
pub fn operator_probe_key(prefix: &str) -> String {
    format!("{}/.flint/lean/probe/versioning-operator", prefix.trim_end_matches('/'))
}

/// The bucket-side assessment (gated only). Runs the shared version
/// probe, reads the live lifecycle rules, and provisions the noncurrent
/// backstop.
///
/// Ordering is deliberate: the shorter-rule check runs BEFORE
/// provisioning. Installing our own 30-day rule next to somebody's
/// 1-day rule would make the status read "provisioned" while the
/// destroyer is still armed — S3 applies the shortest matching rule,
/// not ours.
pub async fn assess_bucket(
    store: &dyn ObjectStore,
    prefix: &str,
    spec: &FlintLeanWorkspaceSpec,
) -> BucketVerdict {
    let mut v = BucketVerdict::default();
    if spec.boundary_mode != "gated" {
        return v;
    }
    let files_prefix = format!("{}/files/", prefix.trim_end_matches('/'));

    if let Err(msg) =
        flint_store::probe::probe_version_surface(store, &operator_probe_key(prefix)).await
    {
        v.refusal = Some(refuse(
            "VersionSurfaceProbeFailed",
            format!(
                "versioning conformance probe FAILED: {msg}. Gated mode is refused rather than \
                 degraded — falling back to etag semantics on a key whose current version is \
                 uncited IS the torn view the mode exists to prevent"
            ),
        ));
        return v;
    }
    v.probe_passed = true;

    match store.lifecycle_rules().await {
        Ok(rules) => {
            tracing::debug!(
                "lean boundary: {} lifecycle rule(s) read for {files_prefix}: {:?}",
                rules.len(),
                rules
            );
            if let Some(bad) =
                shorter_covering_rule(&rules, &files_prefix, spec.noncurrent_retention_days)
            {
                v.refusal = Some(refuse(
                    "ShorterNoncurrentRule",
                    format!(
                        "lifecycle rule {:?} (prefix {:?}) expires noncurrent versions after \
                         {} days, shorter than noncurrentRetentionDays={} and covering \
                         {files_prefix}: under gated staging the CITED version is the \
                         noncurrent one, so that rule destroys published data",
                        bad.id,
                        bad.prefix,
                        bad.noncurrent_days.unwrap_or_default(),
                        spec.noncurrent_retention_days
                    ),
                ));
                return v;
            }
        }
        Err(e) => {
            // Unreadable rules are NOT "no rules": accepting gated on an
            // unreadable posture accepts an unknown destroyer.
            v.refusal = Some(refuse(
                "LifecycleUnreadable",
                format!(
                    "cannot read the bucket's lifecycle rules ({e}); gated mode needs a \
                     positive answer that nothing shorter than {} days reaps noncurrent \
                     versions under {files_prefix}",
                    spec.noncurrent_retention_days
                ),
            ));
            return v;
        }
    }

    match store
        .ensure_noncurrent_retention(&files_prefix, spec.noncurrent_retention_days)
        .await
    {
        Ok(o) => v.retention = Some(o),
        // A missing backstop is a degradation, not a torn view: exact
        // per-citation GC is the reaper and it still runs. Surfaced as
        // VersionRetentionProvisioned=False, never as a gated refusal.
        Err(e) => v.retention_error = Some(e.to_string()),
    }
    v
}

/// `BoundaryModeActive` (§2.6): spec vs the RUNNING binary, read from
/// the lease-heartbeat echo.
///
/// `Unknown` is a real answer here and is used honestly. No lease means
/// no sidecar — an idle lean workspace at rest is bucket objects and
/// nothing else, which is the design, not a fault — and an old binary
/// writes no echo at all. Neither is evidence that the mode is wrong;
/// what would be evidence is an echo that disagrees.
pub fn boundary_mode_active(
    spec: &FlintLeanWorkspaceSpec,
    echo: Option<&flint_store::LeaseEcho>,
    lease_released: bool,
    generation: Option<i64>,
) -> LeanCondition {
    let (status, reason, message) = match echo {
        Some(e) if e.active_boundary_mode == spec.boundary_mode => (
            "True",
            "Matches",
            format!(
                "sidecar {} (protocol {}) is running boundaryMode={}",
                e.sidecar_version, e.protocol, e.active_boundary_mode
            ),
        ),
        Some(e) => (
            "False",
            "ModeMismatch",
            format!(
                "spec asks for boundaryMode={} but sidecar {} reports {}: the sidecar reads a \
                 FIXED env list, so a binary older than the knob ignores it in silence — \
                 upgrade the sidecar image, then recreate the pod",
                spec.boundary_mode, e.sidecar_version, e.active_boundary_mode
            ),
        ),
        None if lease_released => (
            "Unknown",
            "NoLiveSidecar",
            "no sidecar holds the lease (the workspace is at rest, which is the design)".into(),
        ),
        None => (
            "Unknown",
            "NoEcho",
            "the lease holder writes no observed-state echo — a sidecar older than the \
             boundary-verbs protocol, or a backend that cannot carry it"
                .into(),
        ),
    };
    LeanCondition {
        r#type: "BoundaryModeActive".into(),
        status: status.into(),
        reason: reason.into(),
        message: Some(message),
        last_transition_time: now_rfc3339(),
        observed_generation: generation,
    }
}

/// Upsert a condition, preserving `lastTransitionTime` unless the status
/// actually changed — so the timestamp means what it says instead of
/// "when we last reconciled".
pub fn set_condition(conds: &mut Vec<LeanCondition>, new: LeanCondition) {
    match conds.iter_mut().find(|c| c.r#type == new.r#type) {
        Some(old) => {
            let last = if old.status == new.status {
                old.last_transition_time.clone()
            } else {
                new.last_transition_time.clone()
            };
            *old = LeanCondition { last_transition_time: last, ..new };
        }
        None => conds.push(new),
    }
    conds.sort_by(|a, b| a.r#type.cmp(&b.r#type));
}

pub fn condition(
    r#type: &str,
    status: &str,
    reason: &str,
    message: impl Into<Option<String>>,
    generation: Option<i64>,
) -> LeanCondition {
    LeanCondition {
        r#type: r#type.to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        message: message.into(),
        last_transition_time: now_rfc3339(),
        observed_generation: generation,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lean_operator::crd::FlintLeanWorkspaceSpec;
    use flint_store::memory::MemoryStore;

    fn spec(extra: serde_json::Value) -> FlintLeanWorkspaceSpec {
        let mut v = serde_json::json!({
            "projectId": "team-a/p", "bucket": "b", "keyPrefix": "t/p",
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        serde_json::from_value(v).unwrap()
    }

    /// The default CR — the one every existing workspace already is —
    /// must stay acceptable, or Phase 4 is an upgrade that refuses the
    /// fleet it ships to.
    #[test]
    fn todays_default_spec_is_accepted() {
        assert_eq!(validate_spec(&spec(serde_json::json!({}))), Ok(()));
        assert_eq!(validate_spec(&spec(serde_json::json!({"boundaryMode": "cadence"}))), Ok(()));
    }

    /// §2.4.1: unbounded citation staleness is refused by construction.
    /// The control is the same spec WITH a lag bound — so the refusal
    /// tracks the missing knob, not something else about gated.
    #[test]
    fn gated_without_a_lag_bound_is_refused() {
        let e = validate_spec(&spec(serde_json::json!({"boundaryMode": "gated"}))).unwrap_err();
        assert_eq!(e.reason, "LagBoundRequired");
        assert_eq!(
            validate_spec(&spec(serde_json::json!({
                "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
            }))),
            Ok(())
        );
    }

    /// D8's K=2 cross-validation, in the direction it actually binds:
    /// somebody LOWERS retention. Gated staging makes the CITED version
    /// noncurrent, so a retention shorter than one staging window runs
    /// the backstop's clock against live published data.
    #[test]
    fn a_retention_shorter_than_one_staging_window_is_refused() {
        let hostile = spec(serde_json::json!({
            "boundaryMode": "gated",
            "visibilityLagBoundSecs": 3600,
            "noncurrentRetentionDays": 0,
        }));
        let e = validate_spec(&hostile).unwrap_err();
        assert_eq!(e.reason, "RetentionTooShort");
        assert!(e.message.contains("7320s"), "the message must show the window: {}", e.message);
        // Control: one day IS longer than 2x(3600+60), and is accepted.
        assert_eq!(
            validate_spec(&spec(serde_json::json!({
                "boundaryMode": "gated",
                "visibilityLagBoundSecs": 3600,
                "noncurrentRetentionDays": 1,
            }))),
            Ok(())
        );
    }

    /// D10 sized against the ceiling this fleet really gets. The
    /// control is the DEFAULT caps: if anyone raises those defaults past
    /// what a spot reclaim can drain, this test fails and says so.
    #[test]
    fn an_undrainable_backlog_is_refused_against_the_spot_ceiling() {
        let ok = spec(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));
        assert!(
            drain_need_secs(&ok) <= SPOT_RECLAIM_CEILING_SECS,
            "the DEFAULT gated backlog caps no longer drain inside a spot reclaim: {}s",
            drain_need_secs(&ok)
        );
        assert_eq!(validate_spec(&ok), Ok(()));

        let greedy = spec(serde_json::json!({
            "boundaryMode": "gated",
            "visibilityLagBoundSecs": 300,
            "stagedBacklogCapBytes": 20u64 << 30,
        }));
        let e = validate_spec(&greedy).unwrap_err();
        assert_eq!(e.reason, "GraceTooShort");
        assert!(
            e.message.contains("stagedBacklogCapBytes"),
            "the refusal must name the knob to lower: {}",
            e.message
        );
    }

    /// The derived grace is what the webhook stamps, and it must never
    /// be below the 30 s a pod inherits when nobody sets it — the exact
    /// hazard D10 names in today's injected sidecar.
    #[test]
    fn derived_grace_never_drops_below_the_inherited_default() {
        let tiny = spec(serde_json::json!({"floorSecs": 1}));
        assert!(derived_grace_secs(&tiny) >= 30);
        let gated = spec(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));
        assert!(
            derived_grace_secs(&gated) > derived_grace_secs(&tiny),
            "a staging workspace must get MORE grace than a cadence one"
        );
    }

    fn rule(id: &str, prefix: &str, days: Option<u64>) -> LifecycleView {
        LifecycleView {
            id: id.into(),
            enabled: true,
            prefix: prefix.into(),
            noncurrent_days: days,
            expired_delete_marker: false,
        }
    }

    /// Overlap in EITHER direction is a hazard: a rule scoped above us
    /// covers every key we own, one scoped below us covers some of them.
    /// "Only some of the published data was destroyed" is not a pass.
    #[test]
    fn a_shorter_covering_rule_is_found_in_both_scope_directions() {
        let files = "tenants/p1/files/";
        let above = rule("fleet-wide", "", Some(1));
        let exact = rule("scoped", "tenants/p1/files/", Some(7));
        let below = rule("deep", "tenants/p1/files/models/", Some(2));
        let elsewhere = rule("other-tenant", "tenants/p2/", Some(1));
        let no_noncurrent = rule("mpu-abort", "", None);
        let disabled = LifecycleView { enabled: false, ..rule("off", "", Some(1)) };

        for r in [&above, &exact, &below] {
            assert!(
                shorter_covering_rule(std::slice::from_ref(r), files, 30).is_some(),
                "rule {:?} covers {files} and is shorter than 30 days",
                r.id
            );
        }
        for r in [&elsewhere, &no_noncurrent, &disabled] {
            assert!(
                shorter_covering_rule(std::slice::from_ref(r), files, 30).is_none(),
                "rule {:?} is not a destroyer of this prefix and must not refuse the CR",
                r.id
            );
        }
        // And a LONGER rule is fine — the backstop is allowed to be
        // conservative, only shorter is fatal.
        assert!(shorter_covering_rule(&[rule("long", "", Some(90))], files, 30).is_none());
    }

    fn gated_spec() -> FlintLeanWorkspaceSpec {
        spec(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }))
    }

    /// A conformant bucket: the probe passes and the backstop is
    /// installed on `<prefix>/files/`, not on the bare prefix — a rule
    /// over the whole prefix would also cover the manifest and epoch
    /// cells, which are not versioned staging and must not be aged.
    #[tokio::test]
    async fn a_conformant_bucket_gets_the_backstop_provisioned() {
        let store = MemoryStore::new();
        let v = assess_bucket(&store, "tenants/p1", &gated_spec()).await;
        assert!(v.refusal.is_none(), "conformant bucket refused: {:?}", v.refusal);
        assert!(v.probe_passed);
        let r = v.retention.expect("no retention provisioned");
        assert_eq!(r.noncurrent_days, 30);
        assert!(r.created);
        let rules = store.lifecycle_rules().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].prefix, "tenants/p1/files/");

        // Idempotent: a second pass finds it and writes nothing. The
        // read-merge-append path must never run blind.
        let again = assess_bucket(&store, "tenants/p1", &gated_spec()).await;
        assert!(!again.retention.unwrap().created);
        assert_eq!(store.lifecycle_rules().await.unwrap().len(), 1);
    }

    /// The ordering claim, tested: a destroyer already installed by the
    /// customer's own fleet policy must be found BEFORE we provision.
    /// Installing our 30-day rule alongside their 1-day rule would make
    /// the status read "provisioned" while S3 keeps applying the
    /// shortest matching rule.
    #[tokio::test]
    async fn a_customers_shorter_rule_refuses_gated_before_we_provision() {
        let store = MemoryStore::new();
        store.plant_lifecycle_rule(rule("corp-cost-policy", "tenants/", Some(1)));
        let v = assess_bucket(&store, "tenants/p1", &gated_spec()).await;
        let r = v.refusal.expect("a 1-day noncurrent rule over the prefix was accepted");
        assert_eq!(r.reason, "ShorterNoncurrentRule");
        assert!(
            r.message.contains("corp-cost-policy"),
            "the offending rule Id must be named: {}",
            r.message
        );
        assert!(v.retention.is_none(), "we provisioned next to the destroyer");
        assert_eq!(
            store.lifecycle_rules().await.unwrap().len(),
            1,
            "the refusal wrote a rule anyway"
        );
    }

    /// A proxy that strips `x-amz-version-id` is refused, and the CR is
    /// left in the default mode rather than degraded into etag
    /// semantics. Leg B24(a)'s control arm, in a unit test.
    #[tokio::test]
    async fn a_version_stripping_proxy_refuses_gated() {
        let store = MemoryStore::new();
        store.strip_version_ids(true);
        let v = assess_bucket(&store, "tenants/p1", &gated_spec()).await;
        let r = v.refusal.expect("a version-stripping proxy was accepted");
        assert_eq!(r.reason, "VersionSurfaceProbeFailed");
        assert!(r.message.contains("x-amz-version-id"));
        assert!(!v.probe_passed);
        // Control: the SAME store passes once the header comes back —
        // the refusal tracks the proxy, not a typo in the fixture.
        store.strip_version_ids(false);
        assert!(assess_bucket(&store, "tenants/p1", &gated_spec()).await.refusal.is_none());
    }

    /// An unprovisionable backstop is a DEGRADATION, not a torn view:
    /// flint's exact per-citation version GC is the reaper and still
    /// runs. Refusing gated here would page an operator for the loss of
    /// a crash-window backstop.
    #[tokio::test]
    async fn an_unwritable_lifecycle_degrades_rather_than_refusing() {
        let store = MemoryStore::new();
        store.fail_lifecycle_writes(true);
        let v = assess_bucket(&store, "tenants/p1", &gated_spec()).await;
        assert!(v.refusal.is_none(), "a missing BACKSTOP refused gated mode");
        assert!(v.retention.is_none());
        assert!(v.retention_error.is_some(), "the degradation was silent");
    }

    /// Unreadable rules are not "no rules". Accepting gated on an
    /// unreadable posture accepts an unknown destroyer — and the D8
    /// hazard is precisely a rule flint never wrote.
    #[tokio::test]
    async fn an_unreadable_lifecycle_refuses_gated() {
        struct Blind(MemoryStore);
        #[async_trait::async_trait]
        impl ObjectStore for Blind {
            async fn put_whole(
                &self,
                key: &str,
                body: bytes::Bytes,
                condition: &flint_store::PutCondition,
                stamps: &flint_store::GenerationStamps,
                crc64: u64,
            ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
                self.0.put_whole(key, body, condition, stamps, crc64).await
            }
            async fn compose_generation(
                &self,
                spec: &flint_store::ComposeSpec<'_>,
            ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
                self.0.compose_generation(spec).await
            }
            async fn get_whole(
                &self,
                key: &str,
                if_match: Option<&str>,
            ) -> flint_store::StoreResult<(flint_store::ObjectMeta, bytes::Bytes)> {
                self.0.get_whole(key, if_match).await
            }
            async fn get_range(
                &self,
                key: &str,
                offset: u64,
                len: u64,
                if_match: &str,
            ) -> flint_store::StoreResult<bytes::Bytes> {
                self.0.get_range(key, offset, len, if_match).await
            }
            async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
                self.0.head(key).await
            }
            async fn list(
                &self,
                prefix: &str,
            ) -> flint_store::StoreResult<Vec<flint_store::ListedObject>> {
                self.0.list(prefix).await
            }
            async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
                self.0.delete(key).await
            }
            async fn head_version(
                &self,
                key: &str,
                v: &str,
            ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
                self.0.head_version(key, v).await
            }
            async fn get_version(
                &self,
                key: &str,
                v: &str,
            ) -> flint_store::StoreResult<(flint_store::ObjectMeta, bytes::Bytes)> {
                self.0.get_version(key, v).await
            }
            async fn delete_version(&self, key: &str, v: &str) -> flint_store::StoreResult<()> {
                self.0.delete_version(key, v).await
            }
            async fn list_versions(
                &self,
                prefix: &str,
            ) -> flint_store::StoreResult<Vec<flint_store::ListedVersion>> {
                self.0.list_versions(prefix).await
            }
            async fn list_uploads(
                &self,
                prefix: &str,
            ) -> flint_store::StoreResult<Vec<flint_store::PendingUpload>> {
                self.0.list_uploads(prefix).await
            }
            async fn abort_upload(&self, key: &str, id: &str) -> flint_store::StoreResult<()> {
                self.0.abort_upload(key, id).await
            }
            async fn bootstrap(
                &self,
                prefix: &str,
            ) -> flint_store::StoreResult<flint_store::BootstrapReport> {
                self.0.bootstrap(prefix).await
            }
            async fn epoch_read(
                &self,
                key: &str,
            ) -> flint_store::StoreResult<Option<flint_store::EpochState>> {
                self.0.epoch_read(key).await
            }
            async fn epoch_acquire(
                &self,
                key: &str,
                holder: &str,
                s: Option<&flint_store::EpochState>,
            ) -> flint_store::StoreResult<flint_store::EpochLease> {
                self.0.epoch_acquire(key, holder, s).await
            }
            async fn epoch_renew(
                &self,
                key: &str,
                lease: &flint_store::EpochLease,
                echo: Option<&str>,
            ) -> flint_store::StoreResult<flint_store::EpochLease> {
                self.0.epoch_renew(key, lease, echo).await
            }
            async fn epoch_release(
                &self,
                key: &str,
                lease: &flint_store::EpochLease,
            ) -> flint_store::StoreResult<()> {
                self.0.epoch_release(key, lease).await
            }
            fn min_part_size(&self) -> u64 {
                self.0.min_part_size()
            }
            fn max_parts(&self) -> usize {
                self.0.max_parts()
            }
            // ...and `lifecycle_rules` is left at the trait default,
            // which REFUSES. That is the whole point of the default.
        }
        let v = assess_bucket(&Blind(MemoryStore::new()), "tenants/p1", &gated_spec()).await;
        let r = v.refusal.expect("gated was accepted on an unreadable lifecycle posture");
        assert_eq!(r.reason, "LifecycleUnreadable");
        assert!(v.probe_passed, "the probe should have run before the lifecycle read");
    }

    fn echo(mode: &str, version: &str) -> flint_store::LeaseEcho {
        flint_store::LeaseEcho {
            sidecar_version: version.into(),
            protocol: 1,
            active_boundary_mode: mode.into(),
            last_cited_seq: 7,
            last_cited_unix: 1_756_000_000,
            staged_uncited_count: 3,
            sentinel_verbs_active: true,
            metrics_bound: None,
        }
    }

    /// The mixed-version hole this condition exists to close: an old
    /// sidecar reads a FIXED env list, so `gated` reaching it is ignored
    /// in silence and the workspace runs fused cadence. `Unknown` is
    /// used honestly — no sidecar is the design at rest, not a fault.
    #[test]
    fn boundary_mode_active_separates_mismatch_from_absence() {
        let s = gated_spec();
        let matched = boundary_mode_active(&s, Some(&echo("gated", "0.1.0")), false, Some(4));
        assert_eq!(matched.status, "True");

        let stale = boundary_mode_active(&s, Some(&echo("hybrid", "0.0.9")), false, Some(4));
        assert_eq!(stale.status, "False");
        assert_eq!(stale.reason, "ModeMismatch");
        assert!(stale.message.unwrap().contains("0.0.9"), "name the binary that is wrong");

        assert_eq!(boundary_mode_active(&s, None, true, None).reason, "NoLiveSidecar");
        assert_eq!(boundary_mode_active(&s, None, false, None).reason, "NoEcho");
        for c in [
            boundary_mode_active(&s, None, true, None),
            boundary_mode_active(&s, None, false, None),
        ] {
            assert_eq!(c.status, "Unknown", "absence is not evidence of a wrong mode");
        }
    }

    /// A condition's timestamp must mean "when this changed", not "when
    /// we last reconciled" — an operator reading a 30-minute-old
    /// transition on a flapping workspace is reading a lie.
    #[test]
    fn condition_transition_time_survives_a_no_change_reconcile() {
        let mut conds = vec![];
        set_condition(&mut conds, condition("BoundaryModeAccepted", "True", "Ok", None, Some(1)));
        let first = conds[0].last_transition_time.clone();
        // The second stamp is EXPLICITLY different: `now_rfc3339` is
        // second-granular, so two calls inside one second produce the
        // same string and the assertion below would hold with the
        // preservation rule removed.
        set_condition(
            &mut conds,
            LeanCondition {
                last_transition_time: "2098-01-01T00:00:00Z".into(),
                ..condition("BoundaryModeAccepted", "True", "Ok", None, Some(2))
            },
        );
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].last_transition_time, first);
        assert_eq!(conds[0].observed_generation, Some(2), "the generation still advances");

        set_condition(
            &mut conds,
            LeanCondition {
                last_transition_time: "2099-01-01T00:00:00Z".into(),
                ..condition("BoundaryModeAccepted", "False", "LagBoundRequired", None, Some(3))
            },
        );
        assert_eq!(conds[0].last_transition_time, "2099-01-01T00:00:00Z");
    }
}
