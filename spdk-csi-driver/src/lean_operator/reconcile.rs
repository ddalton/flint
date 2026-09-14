//! The lean workspace reconcile: claim stamping with BOTH adopt arms,
//! the operator-principal bootstrap, and the operator-side MPU sweep.
//!
//! The claim (plan P1 + §2.4) is a bucket cell carrying the durable,
//! USER-DECLARED project identity — never the CR UID, because CR
//! delete/recreate over the same data is a designed lifecycle (DR,
//! GitOps re-apply, cross-cluster moves). The two arms, each of which
//! the naive other-arm implementation fails:
//!
//! - **adopt-own**: a standing claim with the SAME declared identity is
//!   adopted silently (a UID-keyed claim would refuse its own data
//!   after every re-apply);
//! - **refuse-foreign**: a standing claim with a DIFFERENT identity
//!   parks the CR in Refused (an always-adopt implementation silently
//!   attaches a new tenant to a reused prefix — the prefix-reuse
//!   adoption bug class).
//!
//! Bucket-admin ops run here under the OPERATOR principal: `bootstrap`
//! (versioning/lifecycle posture) and the MPU sweep (`list_uploads` is
//! bucket-wide on the wire and a correctly project-scoped proxy DENIES
//! it to syncers — plan §2.4).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::tier::store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError};

use super::boundary;
use super::crd::{FlintLeanWorkspaceSpec, LeanCondition};

/// The claim cell, one per subtree prefix.
pub fn claim_key(prefix: &str) -> String {
    format!("{}/.flint/lean/claim", prefix.trim_end_matches('/'))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimDoc {
    pub project_id: String,
    pub created_unix: u64,
    /// Which operator/cluster stamped it (audit only, never identity).
    pub stamped_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// Fresh prefix: the claim was created.
    Created,
    /// A standing claim with the SAME declared identity: adopted.
    AdoptedOwn,
    /// A standing claim with a DIFFERENT identity: refused — never
    /// adopted on the fly.
    RefusedForeign { standing: String },
}

pub async fn ensure_claim(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    project_id: &str,
    stamped_by: &str,
) -> Result<ClaimOutcome, StoreError> {
    let key = claim_key(prefix);
    let doc = ClaimDoc {
        project_id: project_id.to_string(),
        created_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        stamped_by: stamped_by.to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&doc).expect("claim serializes");
    let crc = crc64_nvme(&bytes);
    let stamps = GenerationStamps {
        generation: 1,
        epoch: 0,
        flush_uuid: "lean-operator-claim".into(),
        boundary_source: None,
        posix: None,
    };
    match store
        .put_whole(&key, bytes.into(), &PutCondition::IfNoneMatchAny, &stamps, crc)
        .await
    {
        Ok(_) => Ok(ClaimOutcome::Created),
        Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {
            let (_, body) = store.get_whole(&key, None).await?;
            let standing: ClaimDoc = serde_json::from_slice(&body).map_err(|e| {
                StoreError::Other(format!("claim cell at {key} is unparseable: {e}"))
            })?;
            if standing.project_id == project_id {
                Ok(ClaimOutcome::AdoptedOwn)
            } else {
                Ok(ClaimOutcome::RefusedForeign { standing: standing.project_id })
            }
        }
        Err(e) => Err(e),
    }
}

/// The operator-side MPU sweep: abort in-progress multipart assemblies
/// under the prefix older than `min_age_secs` (a crashed syncer's
/// half-uploaded compose bills until aborted; the lifecycle rule is
/// the backstop, this is the fast path). Returns aborted count.
pub async fn sweep_stale_uploads(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    min_age_secs: u64,
) -> Result<usize, StoreError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut aborted = 0;
    for up in store.list_uploads(prefix).await? {
        // A store that cannot date its uploads gets a leak, never an
        // aborted LIVE publish: `unwrap_or(true)` swept every in-progress
        // compose on such a backend on every reconcile, and a compose
        // longer than the reconcile interval never completed (protocol
        // review 2026-09-12, atomicity-9 / audit csi-6). Real S3 always
        // returns `Initiated`; the lifecycle rule is the backstop there.
        let old_enough = up
            .initiated_unix
            .map(|t| now.saturating_sub(t) >= min_age_secs)
            .unwrap_or(false);
        if old_enough {
            store.abort_upload(&up.key, &up.upload_id).await?;
            aborted += 1;
        }
    }
    Ok(aborted)
}

/// One full operator pass for a workspace: claim (both arms), bucket
/// posture, MPU sweep. Returns (phase, message, standing_id).
pub async fn verify_workspace(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    project_id: &str,
    stamped_by: &str,
) -> Result<(String, String, Option<String>), StoreError> {
    match ensure_claim(store, prefix, project_id, stamped_by).await? {
        ClaimOutcome::RefusedForeign { standing } => {
            return Ok((
                "Refused".into(),
                format!(
                    "prefix {prefix} is claimed by project {standing:?}; refusing — delete the \
                     standing claim explicitly if this reuse is intended"
                ),
                Some(standing),
            ));
        }
        ClaimOutcome::Created => {
            let report = store.bootstrap(prefix).await?;
            if !report.ok() {
                return Ok((
                    "Error".into(),
                    format!("bucket posture: {}", report.errors.join("; ")),
                    None,
                ));
            }
            let swept = sweep_stale_uploads(store, prefix, 3600).await?;
            Ok(("Claimed".into(), format!("claim created; {swept} stale uploads swept"), None))
        }
        ClaimOutcome::AdoptedOwn => {
            let swept = sweep_stale_uploads(store, prefix, 3600).await?;
            Ok(("Adopted".into(), format!("standing claim adopted; {swept} stale uploads swept"), None))
        }
    }
}

/// Everything one operator pass observed about a workspace: the claim
/// verdict, the boundary conditions, and what the RUNNING syncer says
/// about itself.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceReport {
    pub phase: String,
    pub message: String,
    pub standing_project_id: Option<String>,
    pub conditions: Vec<LeanCondition>,
    pub observed_syncer_version: Option<String>,
    pub cited_seq: Option<u64>,
    /// Writers with a heartbeat within the last five minutes. The lease
    /// is held per barrier (design 2026-09-13 §4), so the cell says who
    /// ran the LAST boundary and nothing about who is alive; the
    /// heartbeats do, and two writers on one workspace show as two.
    pub observed_writers: Option<u64>,
}

/// One operator pass over a workspace.
///
/// **Two cadences, and the split is the point.** `posture` runs the
/// expensive half — the claim CAS, the MPU sweep, the versioning
/// conformance probe, the live lifecycle read and the backstop
/// provisioning — a dozen-odd requests that answer questions which
/// change on the timescale of a proxy upgrade or an admin edit. The
/// cheap half is the OBSERVATION: one epoch read that carries the
/// syncer's echo, and with it `citedSeq`, the visibility lag and the
/// staged-uncited count.
///
/// They cannot share a cadence. §2.6 promises `LAG` as a printer
/// column — live per-workspace visibility lag with no metrics stack —
/// and a lag column refreshed every thirty minutes is not a lag column.
/// Running the posture at the observation's rate instead would multiply
/// the fleet's operator traffic by an order of magnitude to re-ask
/// whether a bucket's lifecycle rules changed in the last two minutes.
///
/// Ordering is load-bearing. The claim runs first because a refused
/// prefix is not ours to assess: probing versioning and provisioning
/// lifecycle rules on another project's subtree would be the operator
/// acting on data it just refused to adopt.
pub async fn full_pass(
    store: &Arc<dyn ObjectStore>,
    spec: &FlintLeanWorkspaceSpec,
    stamped_by: &str,
    generation: Option<i64>,
    posture: bool,
) -> Result<WorkspaceReport, StoreError> {
    let prefix = spec.key_prefix.trim_end_matches('/');
    let (phase, message, standing) = if posture {
        verify_workspace(store, prefix, &spec.project_id, stamped_by).await?
    } else {
        // The observation pass asserts nothing about the claim, so it
        // reports nothing about it: the caller keeps the standing phase
        // rather than overwriting it with a guess.
        (String::new(), String::new(), None)
    };
    let mut r = WorkspaceReport {
        phase: phase.clone(),
        message,
        standing_project_id: standing,
        ..Default::default()
    };
    if phase == "Refused" {
        return Ok(r);
    }

    // 1. The spec, on its own terms.
    let accepted = match boundary::validate_spec(spec) {
        Ok(()) => Ok(()),
        Err(e) => Err(e),
    };

    // The spec verdict is pure, so a REFUSAL is authoritative on every
    // pass: a knob edited into an incoherent state must not wait out a
    // posture cadence to be refused.
    //
    // Acceptance is NOT symmetric, and this is the trap the kind drill
    // caught. An observation pass consults no bucket, so "the spec is
    // fine" is not "the workspace is accepted": writing True here would
    // clear a bucket-side refusal — a customer's 1-day noncurrent rule,
    // a proxy that strips version ids — roughly two minutes after the
    // posture pass raised it, leaving the operator looking at green
    // while the destroyer is still armed. When the fast pass has
    // nothing to add, it says nothing and the standing condition
    // stands.
    match (&accepted, posture) {
        (Err(e), _) => boundary::set_condition(
            &mut r.conditions,
            boundary::condition(
                "SpecAccepted",
                "False",
                &e.reason,
                Some(e.message.clone()),
                generation,
            ),
        ),
        (Ok(()), true) => boundary::set_condition(
            &mut r.conditions,
            boundary::condition(
                "SpecAccepted",
                "True",
                "Accepted",
                Some("the spec's knobs are coherent".to_string()),
                generation,
            ),
        ),
        (Ok(()), false) => {}
    }

    // 3. What the RUNNING syncer says. One read of a cell the operator
    //    already has a reason to look at.
    let cell = store.epoch_read(&format!("{prefix}/.flint/lean/epoch")).await?;
    // NOT `.ok()`. That mapped a parse FAILURE onto `None`, and `None`
    // reports as "NoEcho — a syncer older than the boundary-verbs
    // protocol". A schema skew therefore surfaced as a benign old
    // binary: an error returning a legal value.
    let raw_echo = cell.as_ref().and_then(|c| c.echo.as_deref());
    let (echo, echo_unparseable): (Option<flint_store::LeaseEcho>, bool) = match raw_echo {
        None => (None, false),
        Some(e) => match serde_json::from_str(e) {
            Ok(v) => (Some(v), false),
            Err(_) => (None, true),
        },
    };
    let released = cell.as_ref().map(|c| c.released).unwrap_or(true);
    boundary::set_condition(
        &mut r.conditions,
        boundary::syncer_observed(echo.as_ref(), released, generation, echo_unparseable),
    );
    // 4. Who is ALIVE: one LIST of the writers' heartbeats. The cell is
    //    at rest between barriers, so its `released` says nothing about
    //    liveness any more; a heartbeat fresher than five minutes does.
    r.observed_writers = Some(
        boundary::live_writers(
            store.as_ref(),
            prefix,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
        .await?,
    );
    if let Some(e) = &echo {
        r.observed_syncer_version = Some(e.syncer_version.clone());
        r.cited_seq = Some(e.last_cited_seq);
        boundary::set_condition(
            &mut r.conditions,
            boundary::condition(
                "SentinelVerbsActive",
                if e.sentinel_verbs_active { "True" } else { "False" },
                if e.sentinel_verbs_active { "Active" } else { "PreflightDisabled" },
                Some(if e.sentinel_verbs_active {
                    "the workspace consumes .flint/publish and .flint/sync".into()
                } else {
                    "boundary verbs are OFF in this workspace — the pre-flight found \
                     pre-existing .flint/ data, or sentinels are set to off. The agent's \
                     .flint/capabilities.json carries the reason"
                        .to_string()
                }),
                generation,
            ),
        );
    }

    if let Some(bound) = echo.as_ref().and_then(|e| e.metrics_bound) {
        boundary::set_condition(
            &mut r.conditions,
            boundary::condition(
                "MetricsExposed",
                if bound { "True" } else { "False" },
                if bound { "Listening" } else { "PortUnavailable" },
                Some(if bound {
                    "the syncer is serving /metrics".into()
                } else {
                    "exposition is enabled but the port was taken (the agent container is \
                     the likely occupant). The workspace is fully operable — gauges.json, \
                     the heartbeat echo and `flint-sync status` remain authoritative — but \
                     nothing is scraping it"
                        .to_string()
                }),
                generation,
            ),
        );
    }

    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::store::memory::MemoryStore;

    fn store() -> Arc<dyn ObjectStore> {
        Arc::new(MemoryStore::new())
    }

    /// Both adopt arms, plus the leg each naive implementation fails.
    #[tokio::test]
    async fn claim_adopt_own_and_refuse_foreign() {
        let s = store();
        // Fresh prefix: created.
        let r = ensure_claim(&s, "t/p1", "team-a/proj1", "op").await.unwrap();
        assert_eq!(r, ClaimOutcome::Created);

        // The DR/GitOps leg: SAME declared identity re-applied ⇒ adopt.
        // (A UID-keyed claim — the naive arm — would refuse here.)
        let r = ensure_claim(&s, "t/p1", "team-a/proj1", "op2").await.unwrap();
        assert_eq!(r, ClaimOutcome::AdoptedOwn);

        // The prefix-reuse leg: DIFFERENT identity ⇒ refuse, never
        // adopt on the fly. (An always-adopt arm silently attaches the
        // new tenant to the old tenant's data.)
        let r = ensure_claim(&s, "t/p1", "team-b/other", "op").await.unwrap();
        assert_eq!(r, ClaimOutcome::RefusedForeign { standing: "team-a/proj1".into() });

        // And the refusal is what verify_workspace surfaces.
        let (phase, msg, standing) =
            verify_workspace(&s, "t/p1", "team-b/other", "op").await.unwrap();
        assert_eq!(phase, "Refused");
        assert!(msg.contains("team-a/proj1"));
        assert_eq!(standing.as_deref(), Some("team-a/proj1"));
    }

    fn spec_of(extra: serde_json::Value) -> FlintLeanWorkspaceSpec {
        let mut v = serde_json::json!({
            "projectId": "team-a/p1", "bucket": "b", "keyPrefix": "t/p1",
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        serde_json::from_value(v).unwrap()
    }

    fn cond<'a>(r: &'a WorkspaceReport, t: &str) -> Option<&'a LeanCondition> {
        r.conditions.iter().find(|c| c.r#type == t)
    }

    /// The pass every existing workspace takes: accepted, and honest
    /// that nobody is running. An idle lean workspace at rest is bucket
    /// objects and nothing else — that is the design, and the status
    /// must not read as a fault.
    #[tokio::test]
    async fn full_pass_accepts_a_default_workspace_and_reports_no_syncer() {
        let s = store();
        let r = full_pass(&s, &spec_of(serde_json::json!({})), "op", Some(1), true).await.unwrap();
        assert_eq!(r.phase, "Claimed");
        assert_eq!(cond(&r, "SpecAccepted").unwrap().status, "True");
        let observed = cond(&r, "SyncerObserved").unwrap();
        assert_eq!(observed.status, "Unknown");
        assert_eq!(observed.reason, "NoLiveSyncer");
        assert!(r.observed_syncer_version.is_none());
        // The operator never touches the bucket's lifecycle rules.
        assert!(s.lifecycle_rules().await.unwrap().is_empty());
    }

    /// The mixed-fleet tell (§2.6): the binary in the pod names itself
    /// through the lease echo, and nothing else in the system can see
    /// which version is serving a workspace.
    #[tokio::test]
    async fn a_running_syncer_is_observed_through_its_echo() {
        let s = store();
        let key = "t/p1/.flint/lean/epoch";
        let lease = s.epoch_acquire(key, "holder-1", None).await.unwrap();
        let echo = serde_json::to_string(&flint_store::LeaseEcho {
            syncer_version: "0.0.9".into(),
            protocol: 1,
            last_cited_seq: 12,
            last_cited_unix: 1_700_000_000,
            sentinel_verbs_active: true,
            metrics_bound: None,
        })
        .unwrap();
        s.epoch_renew(key, &lease, Some(&echo)).await.unwrap();

        let r = full_pass(&s, &spec_of(serde_json::json!({})), "op", Some(3), true).await.unwrap();
        let c = cond(&r, "SyncerObserved").unwrap();
        assert_eq!(c.status, "True");
        assert_eq!(c.reason, "Running");
        assert!(c.message.as_ref().unwrap().contains("0.0.9"));
        assert_eq!(r.observed_syncer_version.as_deref(), Some("0.0.9"));
        assert_eq!(r.cited_seq, Some(12));
        assert_eq!(cond(&r, "SentinelVerbsActive").unwrap().status, "True");
    }

    /// The spec verdict is pure, so a REFUSAL is authoritative on the
    /// fast pass too: an incoherent knob must not wait out a posture
    /// cadence to be refused. Acceptance is not symmetric — a fast pass
    /// that has nothing to add says nothing, so the standing condition
    /// stands.
    #[tokio::test]
    async fn a_spec_refusal_is_authoritative_on_the_fast_pass() {
        let s = store();
        let broken = spec_of(serde_json::json!({"sentinels": "sometimes"}));
        let observed = full_pass(&s, &broken, "op", Some(2), false).await.unwrap();
        assert_eq!(cond(&observed, "SpecAccepted").unwrap().reason, "InvalidSentinelMode");

        let fine = spec_of(serde_json::json!({}));
        let observed = full_pass(&s, &fine, "op", Some(3), false).await.unwrap();
        assert!(
            cond(&observed, "SpecAccepted").is_none(),
            "the observation pass asserted acceptance it never re-checked: {:?}",
            cond(&observed, "SpecAccepted")
        );
    }

    /// The two cadences, and the property that makes them safe to
    /// separate: the OBSERVATION pass must still see what the syncer
    /// is doing, and must not spend the posture's dozen requests to do
    /// it. §2.6 promises `CITED-SEQ` as a printer column, and a column
    /// refreshed on the posture cadence is not that column.
    #[tokio::test]
    async fn the_observation_pass_reads_the_echo_without_the_posture_work() {
        let s = store();
        let key = "t/p1/.flint/lean/epoch";
        let lease = s.epoch_acquire(key, "holder-1", None).await.unwrap();
        let echo = serde_json::to_string(&flint_store::LeaseEcho {
            syncer_version: "0.1.0".into(),
            protocol: 1,
            last_cited_seq: 42,
            last_cited_unix: 1_700_000_000,
            sentinel_verbs_active: true,
            metrics_bound: None,
        })
        .unwrap();
        s.epoch_renew(key, &lease, Some(&echo)).await.unwrap();

        let r = full_pass(&s, &spec_of(serde_json::json!({})), "op", None, false).await.unwrap();
        assert_eq!(r.cited_seq, Some(42), "the fast pass did not read the echo");
        assert_eq!(cond(&r, "SyncerObserved").unwrap().status, "True");
        // …and it did NOT do the posture's work: no claim was stamped.
        assert!(r.phase.is_empty(), "the observation pass overwrote the phase");
        assert!(
            s.get_whole("t/p1/.flint/lean/claim", None).await.is_err(),
            "the observation pass stamped a claim"
        );
    }

    #[tokio::test]
    async fn verify_full_pass_claims_and_sweeps() {
        let s = store();
        let (phase, _, _) = verify_workspace(&s, "t/p2", "proj2", "op").await.unwrap();
        assert_eq!(phase, "Claimed");
        let (phase, _, _) = verify_workspace(&s, "t/p2", "proj2", "op").await.unwrap();
        assert_eq!(phase, "Adopted");
    }
}
