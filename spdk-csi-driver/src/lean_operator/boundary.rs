//! Phase 4: the operator's half of the boundary contract (lean
//! boundary-verbs plan §2.6, D10).
//!
//! Two questions, both answered without a bucket: **is this spec
//! coherent?** (a pure function over the CR) and **what is the RUNNING
//! syncer saying about itself?** (the lease-heartbeat echo, read from
//! the cell the operator already looks at). The refusals are conditions
//! rather than webhook rejections, so a CR that goes from acceptable to
//! unacceptable is caught on the next pass rather than never.

use super::crd::{FlintLeanWorkspaceSpec, LeanCondition};

/// The bounded-retry budget the SIGTERM arm spends before releasing the
/// lease (3 attempts, 2 s apart — `bin/flint_sync.rs`).
const DRAIN_RETRY_SECS: u64 = 6;

/// Slack over the arithmetic. The drain also scans, CASes and settles
/// owed acks; none of that is proportional to the backlog.
const DRAIN_SLACK_SECS: u64 = 15;

/// How long this workspace's final drain can take, in the worst case
/// its own knobs permit (D10 rule 3). Nothing stages between boundaries,
/// so the drain repeats at most one floor's barrier: a workspace whose
/// barrier does not fit inside its own floor is already failing its RPO
/// contract for reasons that have nothing to do with SIGTERM, so the
/// floor IS the estimate.
pub fn drain_need_secs(spec: &FlintLeanWorkspaceSpec) -> u64 {
    spec.floor_secs + DRAIN_RETRY_SECS + DRAIN_SLACK_SECS
}

/// The grace period stamped on the worker pod. Never below the 30 s the
/// pod would otherwise inherit — the hazard D10 names is a worker that
/// sets NO `terminationGracePeriodSeconds` at all, so every workspace
/// drains inside a number nobody chose.
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
    if !matches!(spec.sentinels.as_str(), "auto" | "off" | "force") {
        return Err(refuse(
            "InvalidSentinelMode",
            format!("sentinels {:?} is not auto|off|force", spec.sentinels),
        ));
    }
    Ok(())
}

/// `SyncerObserved` (§2.6): what the RUNNING binary says about itself,
/// read from the lease-heartbeat echo.
///
/// `Unknown` is a real answer here and is used honestly. No lease means
/// no syncer — an idle lean workspace at rest is bucket objects and
/// nothing else, which is the design, not a fault — and an old binary
/// writes no echo at all. Neither is evidence of a fault; what would be
/// evidence is an echo that cannot be read.
///
/// `echo_unparseable` is the THIRD case and it is not cosmetic. The
/// reader used to collapse a parse failure into `None` with `.ok()`, and
/// `None` reports as `NoEcho` — "a syncer older than the boundary-verbs
/// protocol". So a malformed or schema-mismatched echo, the exact thing a
/// field rename or a version skew produces, was reported as a benign old
/// binary. An error must not return a legal value.
pub fn syncer_observed(
    echo: Option<&flint_store::LeaseEcho>,
    lease_released: bool,
    generation: Option<i64>,
    echo_unparseable: bool,
) -> LeanCondition {
    let (status, reason, message) = match echo {
        // The echo survives the handoff on purpose: the cell is at rest
        // between barriers (the lease is held per barrier), so a
        // released cell WITH an echo is the normal reading of a live
        // workspace — it names the binary that ran the last boundary.
        Some(e) => (
            "True",
            "Running",
            format!(
                "syncer {} (protocol {}) ran the last boundary (seq {}); the fence is {}",
                e.syncer_version,
                e.protocol,
                e.last_cited_seq,
                if lease_released { "at rest" } else { "held for a commit section" }
            ),
        ),
        // RELEASED IS CHECKED FIRST. A released lease is at rest whatever
        // the last holder happened to leave in the cell, so a stale
        // unparseable echo must not turn an idle workspace into a fault.
        // Ordering pinned by an_unparseable_echo_is_not_reported_as_an_absent_one.
        None if lease_released => (
            "Unknown",
            "NoLiveSyncer",
            "no syncer holds the lease (the workspace is at rest, which is the design)".into(),
        ),
        None if echo_unparseable => (
            "Unknown",
            "EchoUnparseable",
            "the lease holder wrote an observed-state echo this operator \
             could not parse — a schema skew between syncer and operator, \
             NOT an absent or older syncer"
                .into(),
        ),
        None => (
            "Unknown",
            "NoEcho",
            "the lease holder writes no observed-state echo — a syncer older than the \
             boundary-verbs protocol, or a backend that cannot carry it"
                .into(),
        ),
    };
    LeanCondition {
        r#type: "SyncerObserved".into(),
        status: status.into(),
        reason: reason.into(),
        message: Some(message),
        last_transition_time: now_rfc3339(),
        observed_generation: generation,
    }
}

/// Writers with a heartbeat fresher than `WRITER_STALE_SECS` by the
/// store's clock — one LIST of `<prefix>/.flint/lean/writers/`. The
/// comparison is the store's Last-Modified against this process's
/// clock, so the threshold is minutes, not beats: a node clock behind
/// the store's under-counts, never over-counts.
pub const WRITER_STALE_SECS: u64 = 300;

pub async fn live_writers(
    store: &dyn flint_store::ObjectStore,
    prefix: &str,
    now: u64,
) -> Result<u64, flint_store::StoreError> {
    let listed = store.list(&format!("{prefix}/.flint/lean/writers/")).await?;
    Ok(listed
        .iter()
        .filter(|o| o.last_modified_unix.map(|t| now.saturating_sub(t) <= WRITER_STALE_SECS).unwrap_or(true))
        .count() as u64)
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
        assert_eq!(validate_spec(&spec(serde_json::json!({"sentinels": "off"}))), Ok(()));
        assert_eq!(
            validate_spec(&spec(serde_json::json!({"sentinels": "sometimes"}))).unwrap_err().reason,
            "InvalidSentinelMode"
        );
    }

    /// The derived grace is what the worker pod carries, and it must never
    /// be below the 30 s a pod inherits when nobody sets it — the exact
    /// hazard D10 names.
    #[test]
    fn derived_grace_never_drops_below_the_inherited_default() {
        let tiny = spec(serde_json::json!({"floorSecs": 1}));
        assert!(derived_grace_secs(&tiny) >= 30);
        let slow = spec(serde_json::json!({"floorSecs": 120}));
        assert!(
            derived_grace_secs(&slow) > derived_grace_secs(&tiny),
            "a longer floor must get MORE grace: the drain repeats one barrier"
        );
    }

    fn echo(version: &str) -> flint_store::LeaseEcho {
        flint_store::LeaseEcho {
            syncer_version: version.into(),
            protocol: 1,
            last_cited_seq: 7,
            last_cited_unix: 1_756_000_000,
            sentinel_verbs_active: true,
            metrics_bound: None,
        }
    }

    /// The mixed-version tell: the echo names the BINARY that is serving
    /// the workspace, and `Unknown` is used honestly — no syncer is the
    /// design at rest, not a fault.
    #[test]
    fn syncer_observed_separates_presence_from_absence() {
        let running = syncer_observed(Some(&echo("0.1.0")), false, Some(4), false);
        assert_eq!(running.status, "True");
        assert_eq!(running.reason, "Running");
        assert!(running.message.unwrap().contains("0.1.0"), "name the binary that is running");

        assert_eq!(syncer_observed(None, true, None, false).reason, "NoLiveSyncer");
        assert_eq!(syncer_observed(None, false, None, false).reason, "NoEcho");
        for c in [syncer_observed(None, true, None, false), syncer_observed(None, false, None, false)] {
            assert_eq!(c.status, "Unknown", "absence is not evidence of a fault");
        }
    }

    /// The THIRD case, and the one the reader used to erase. An echo that
    /// is PRESENT but unparseable is a schema skew between syncer and
    /// operator. It used to arrive here as `None` — because the reader
    /// said `.ok()` — and report as `NoEcho`, i.e. "an older syncer that
    /// writes no echo at all". Two different faults, one message, and the
    /// wrong one: an error returning a legal value.
    ///
    /// The assertion that matters is the INEQUALITY. Asserting only that
    /// the reason is "EchoUnparseable" would still pass if `NoEcho` were
    /// renamed to match it, which is exactly the collapse being pinned.
    #[test]
    fn an_unparseable_echo_is_not_reported_as_an_absent_one() {
        let garbled = syncer_observed(None, false, None, true);
        let absent = syncer_observed(None, false, None, false);

        assert_eq!(garbled.reason, "EchoUnparseable");
        assert_ne!(
            garbled.reason, absent.reason,
            "a parse failure must not be indistinguishable from no echo"
        );
        assert_ne!(garbled.message, absent.message);
        // Still Unknown: a skew is not evidence of a fault in the workspace.
        assert_eq!(garbled.status, "Unknown");
        // A released lease still reads as at-rest; unparseable only
        // describes a HELD lease whose holder wrote something bad.
        assert_eq!(
            syncer_observed(None, true, None, true).reason,
            "NoLiveSyncer",
            "a released lease is at rest whatever the stale echo says"
        );
    }

    /// A condition's timestamp must mean "when this changed", not "when
    /// we last reconciled" — an operator reading a 30-minute-old
    /// transition on a flapping workspace is reading a lie.
    #[test]
    fn condition_transition_time_survives_a_no_change_reconcile() {
        let mut conds = vec![];
        set_condition(&mut conds, condition("SpecAccepted", "True", "Ok", None, Some(1)));
        let first = conds[0].last_transition_time.clone();
        // The second stamp is EXPLICITLY different: `now_rfc3339` is
        // second-granular, so two calls inside one second produce the
        // same string and the assertion below would hold with the
        // preservation rule removed.
        set_condition(
            &mut conds,
            LeanCondition {
                last_transition_time: "2098-01-01T00:00:00Z".into(),
                ..condition("SpecAccepted", "True", "Ok", None, Some(2))
            },
        );
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].last_transition_time, first);
        assert_eq!(conds[0].observed_generation, Some(2), "the generation still advances");

        set_condition(
            &mut conds,
            LeanCondition {
                last_transition_time: "2099-01-01T00:00:00Z".into(),
                ..condition("SpecAccepted", "False", "InvalidSentinelMode", None, Some(3))
            },
        );
        assert_eq!(conds[0].last_transition_time, "2099-01-01T00:00:00Z");
    }
}
