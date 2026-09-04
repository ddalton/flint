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
//! it to sidecars — plan §2.4).

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
/// under the prefix older than `min_age_secs` (a crashed sidecar's
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
        let old_enough = up
            .initiated_unix
            .map(|t| now.saturating_sub(t) >= min_age_secs)
            .unwrap_or(true);
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
/// verdict, the boundary conditions, and what the RUNNING sidecar says
/// about itself.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceReport {
    pub phase: String,
    pub message: String,
    pub standing_project_id: Option<String>,
    pub conditions: Vec<LeanCondition>,
    pub observed_boundary_mode: Option<String>,
    pub observed_sidecar_version: Option<String>,
    pub cited_seq: Option<u64>,
    pub visibility_lag_secs: Option<u64>,
    pub staged_uncited: Option<u64>,
    /// Uncited work with no live lease — the DR signature (D9). The
    /// binary raises an event on this; the condition carries it in
    /// status either way.
    pub stranded_candidates: Option<usize>,
}

/// One operator pass over a workspace.
///
/// **Two cadences, and the split is the point.** `posture` runs the
/// expensive half — the claim CAS, the MPU sweep, the versioning
/// conformance probe, the live lifecycle read and the backstop
/// provisioning — a dozen-odd requests that answer questions which
/// change on the timescale of a proxy upgrade or an admin edit. The
/// cheap half is the OBSERVATION: one epoch read that carries the
/// sidecar's echo, and with it `citedSeq`, the visibility lag and the
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
    let mut accepted = match boundary::validate_spec(spec) {
        Ok(()) => Ok(()),
        Err(e) => Err(e),
    };

    // 2. The bucket, when the spec asks for something the bucket has to
    //    be able to carry. Re-run every pass on purpose: proxies
    //    upgrade and lifecycle rules change under you, so a once-at-
    //    install verdict is a claim about the past.
    if accepted.is_ok() && posture {
        let v = boundary::assess_bucket(store.as_ref(), prefix, spec).await;
        if let Some(refusal) = v.refusal {
            accepted = Err(refusal);
        }
        match (&v.retention, &v.retention_error) {
            (Some(o), _) => boundary::set_condition(
                &mut r.conditions,
                boundary::condition(
                    "VersionRetentionProvisioned",
                    "True",
                    if o.created { "Created" } else { "AlreadyPresent" },
                    Some(format!(
                        "rule {:?} expires noncurrent versions after {} days under {}/files/",
                        o.rule_id, o.noncurrent_days, prefix
                    )),
                    generation,
                ),
            ),
            (None, Some(e)) => boundary::set_condition(
                &mut r.conditions,
                boundary::condition(
                    "VersionRetentionProvisioned",
                    "False",
                    "NotProvisioned",
                    Some(format!(
                        "the noncurrent backstop could not be installed ({e}); flint's exact \
                         per-citation version GC still reclaims, so this is a lost crash-window \
                         backstop rather than a torn view"
                    )),
                    generation,
                ),
            ),
            (None, None) => {}
        }
    }
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
                "BoundaryModeAccepted",
                "False",
                &e.reason,
                Some(e.message.clone()),
                generation,
            ),
        ),
        (Ok(()), true) => boundary::set_condition(
            &mut r.conditions,
            boundary::condition(
                "BoundaryModeAccepted",
                "True",
                "Accepted",
                Some(format!("boundaryMode={} accepted", spec.boundary_mode)),
                generation,
            ),
        ),
        (Ok(()), false) => {}
    }

    // 3. What the RUNNING sidecar says. One read of a cell the operator
    //    already has a reason to look at.
    let cell = store.epoch_read(&format!("{prefix}/.flint/lean/epoch")).await?;
    let echo: Option<flint_store::LeaseEcho> = cell
        .as_ref()
        .and_then(|c| c.echo.as_deref())
        .and_then(|e| serde_json::from_str(e).ok());
    let released = cell.as_ref().map(|c| c.released).unwrap_or(true);
    boundary::set_condition(
        &mut r.conditions,
        boundary::boundary_mode_active(spec, echo.as_ref(), released, generation),
    );
    if let Some(e) = &echo {
        r.observed_boundary_mode = Some(e.active_boundary_mode.clone());
        r.observed_sidecar_version = Some(e.sidecar_version.clone());
        r.cited_seq = Some(e.last_cited_seq);
        r.staged_uncited = Some(e.staged_uncited_count);
        r.visibility_lag_secs = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_sub(e.last_cited_unix),
        );
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
                    "the sidecar is serving /metrics".into()
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

    // 4. D9's DR signature: durable work no manifest cites, and nobody
    //    holding the lease to cite it. This is exactly the window in
    //    which conflicts.jsonl and the CR may no longer exist, so the
    //    bucket-side summary is the only witness.
    // "No live lease" cannot mean "no cell": the pure-spot failure that
    // strands work is a pod that DIED, and a dead holder leaves its
    // cell behind unreleased. Liveness is judged against the STORE's
    // clock (A8's rule), on the same threshold the sidecar's own
    // takeover uses — six quiet polls at ten seconds, doubled for
    // slack. Without this the DR-signature condition can only fire
    // after a CLEAN shutdown, which is the one case that never strands
    // anything: the drain cites everything before it releases.
    const LEASE_DEAD_SECS: u64 = 120;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let live_lease = cell.as_ref().is_some_and(|c| {
        !c.released
            && c.last_renew_unix.is_none_or(|t| now.saturating_sub(t) < LEASE_DEAD_SECS)
    });
    // Only read when it can mean something: with a live holder this is
    // a BACKLOG the sidecar is expected to cite, and the GET would be
    // one request per workspace per pass for an answer nobody acts on.
    let stranded = if live_lease {
        None
    } else {
        read_orphans(store, prefix).await
    };
    if let Some(n) = stranded {
        r.stranded_candidates = Some(n);
        boundary::set_condition(
            &mut r.conditions,
            boundary::condition(
                "StagedWorkRecovered",
                if n == 0 { "True" } else { "False" },
                if n == 0 { "NothingStranded" } else { "UncitedWithNoLease" },
                if n == 0 {
                    None
                } else {
                    // The recipe must name a pod that EXISTS and that the
                    // reader can reach. Under the CSI delivery the binary
                    // lives only in the worker pod in `flint-workers` — a
                    // tenant cannot exec there (the §3.6 admission policy
                    // admits the node SA and the kubelet, nobody else), so
                    // "in a pod on this workspace" sent operators somewhere
                    // there is nothing to run (design §3.2).
                    // Say what was OBSERVED, which is a lease that has
                    // stopped advancing — not "no sidecar", which is an
                    // inference this code cannot make. A syncer whose
                    // credentials the store is refusing (401/403) is
                    // alive, is serving its tenant, and cannot renew:
                    // the renewal is the request being refused, so it
                    // has no way to say so through the bucket (design
                    // §6.3). Telling an operator the sidecar is gone
                    // sends them to restart something that is running
                    // fine, and away from the credential that expired.
                    // The operator holds no `pods` RBAC, so it cannot
                    // check — but it can decline to guess, and name the
                    // gauge that answers it.
                    Some(format!(
                        "{n} durable object(s) are staged and uncited and this workspace's \
                         lease has stopped advancing: invisible to every manifest-resolving \
                         reader, including import, DR checkout, GitOps re-apply and \
                         cross-cluster move. The holder may be GONE, or alive and unable to \
                         renew — a 401/403 from the store pauses a syncer without stopping \
                         it, and it cannot report that through the bucket. Check \
                         `flint_lean_auth_paused_since_timestamp_seconds` (non-zero = \
                         credentials refused since then) or `flint-sync status` on the worker \
                         before concluding the sidecar died. To re-cite the objects as one \
                         flagged boundary run `flint-sync recover-staged` in the worker pod \
                         serving this workspace (`kubectl -n flint-workers exec <worker> -- \
                         flint-sync recover-staged`); with no worker running, start a pod \
                         that mounts this workspace and re-run it there"
                    ))
                },
                generation,
            ),
        );
    }
    Ok(r)
}

/// D9's durable summary, if the sidecar has written one. Absence is not
/// evidence of absence — a cadence/hybrid workspace never writes one —
/// so this returns `None` rather than `Some(0)` when the doc is missing.
async fn read_orphans(store: &Arc<dyn ObjectStore>, prefix: &str) -> Option<usize> {
    let key = format!("{prefix}/.flint/lean/orphans.json");
    let (_, body) = store.get_whole(&key, None).await.ok()?;
    let doc: serde_json::Value = serde_json::from_slice(&body).ok()?;
    Some(doc.get("candidates")?.as_array()?.len())
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
    async fn full_pass_accepts_a_default_workspace_and_reports_no_sidecar() {
        let s = store();
        let r = full_pass(&s, &spec_of(serde_json::json!({})), "op", Some(1), true).await.unwrap();
        assert_eq!(r.phase, "Claimed");
        assert_eq!(cond(&r, "BoundaryModeAccepted").unwrap().status, "True");
        let active = cond(&r, "BoundaryModeActive").unwrap();
        assert_eq!(active.status, "Unknown");
        assert_eq!(active.reason, "NoLiveSidecar");
        assert!(r.observed_boundary_mode.is_none());
        // Nothing gated was asked for, so nothing gated was provisioned.
        assert!(cond(&r, "VersionRetentionProvisioned").is_none());
        assert!(s.lifecycle_rules().await.unwrap().is_empty());
    }

    /// An incoherent spec must be refused BEFORE the operator touches
    /// the bucket on its behalf: probing versioning and writing
    /// lifecycle rules for a CR that cannot start is work done for a
    /// workspace that will never run.
    #[tokio::test]
    async fn an_incoherent_gated_spec_is_refused_before_the_bucket_is_touched() {
        let s = store();
        let r = full_pass(&s, &spec_of(serde_json::json!({"boundaryMode": "gated"})), "op", None, true)
            .await
            .unwrap();
        let c = cond(&r, "BoundaryModeAccepted").unwrap();
        assert_eq!(c.status, "False");
        assert_eq!(c.reason, "LagBoundRequired");
        assert!(
            s.lifecycle_rules().await.unwrap().is_empty(),
            "the operator provisioned for a spec it just refused"
        );
        assert!(cond(&r, "VersionRetentionProvisioned").is_none());
    }

    /// The gated happy path, end to end through the operator: accepted,
    /// probed, and the backstop installed on `<prefix>/files/`.
    #[tokio::test]
    async fn a_conformant_gated_workspace_is_accepted_and_provisioned() {
        let s = store();
        let spec = spec_of(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));
        let r = full_pass(&s, &spec, "op", Some(2), true).await.unwrap();
        assert_eq!(cond(&r, "BoundaryModeAccepted").unwrap().status, "True");
        let ret = cond(&r, "VersionRetentionProvisioned").unwrap();
        assert_eq!(ret.status, "True");
        assert!(ret.message.as_ref().unwrap().contains("30 days"));
        assert_eq!(s.lifecycle_rules().await.unwrap()[0].prefix, "t/p1/files/");
    }

    /// The mixed-version hole (§2.6): the CR says gated, the binary in
    /// the pod predates the knob, reads a FIXED env list, ignores it,
    /// and runs fused cadence. Nothing else in the system can see that.
    #[tokio::test]
    async fn a_stale_sidecar_binary_flips_boundary_mode_active() {
        let s = store();
        let key = "t/p1/.flint/lean/epoch";
        let lease = s.epoch_acquire(key, "holder-1", None).await.unwrap();
        let echo = serde_json::to_string(&flint_store::LeaseEcho {
            sidecar_version: "0.0.9".into(),
            protocol: 1,
            active_boundary_mode: "hybrid".into(),
            last_cited_seq: 12,
            last_cited_unix: 1_700_000_000,
            staged_uncited_count: 0,
            sentinel_verbs_active: true,
            metrics_bound: None,
        })
        .unwrap();
        s.epoch_renew(key, &lease, Some(&echo)).await.unwrap();

        let spec = spec_of(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));
        let r = full_pass(&s, &spec, "op", Some(3), true).await.unwrap();
        let c = cond(&r, "BoundaryModeActive").unwrap();
        assert_eq!(c.status, "False");
        assert_eq!(c.reason, "ModeMismatch");
        assert!(c.message.as_ref().unwrap().contains("0.0.9"));
        assert_eq!(r.observed_boundary_mode.as_deref(), Some("hybrid"));
        assert_eq!(r.cited_seq, Some(12));
        assert_eq!(cond(&r, "SentinelVerbsActive").unwrap().status, "True");
    }

    /// D9's DR signature. This is the window in which conflicts.jsonl
    /// and the CR itself may no longer exist, so the bucket-side summary
    /// is the only witness that durable work is invisible.
    #[tokio::test]
    async fn stranded_uncited_work_is_reported_only_when_no_lease_holds() {
        let s = store();
        let doc = serde_json::json!({
            "written_unix": 1_756_000_000u64,
            "epoch": 4,
            "boundary_mode": "gated",
            "candidates": [
                {"path": "ckpt.bin", "version_id": "v7", "size": 10,
                 "generation": 1, "epoch": 4, "staged_unix": 1_756_000_000u64},
            ],
        });
        let body = bytes::Bytes::from(serde_json::to_vec(&doc).unwrap());
        let crc = crc64_nvme(&body);
        s.put_whole(
            "t/p1/.flint/lean/orphans.json",
            body,
            &PutCondition::IfNoneMatchAny,
            &GenerationStamps {
                generation: 0,
                epoch: 0,
                flush_uuid: "test".into(),
                boundary_source: None,
                posix: None,
            },
            crc,
        )
        .await
        .unwrap();

        let spec = spec_of(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));
        let r = full_pass(&s, &spec, "op", None, true).await.unwrap();
        let c = cond(&r, "StagedWorkRecovered").expect("stranded work was not surfaced");
        assert_eq!(c.status, "False");
        assert_eq!(r.stranded_candidates, Some(1));
        let msg = c.message.as_ref().unwrap();
        assert!(msg.contains("recover-staged"), "the condition must name the verb that fixes it");
        // ...and a place the verb can actually be run. Under the CSI
        // delivery flint-sync exists only in the worker pod; a recipe
        // saying "in a pod on this workspace" points an operator at a
        // tenant pod that does not carry the binary (design §3.2).
        assert!(
            msg.contains("flint-workers"),
            "the recipe must name the namespace the binary is reachable in, not a tenant pod: {msg}"
        );

        // With a live sidecar this is a BACKLOG, not an orphan set: the
        // holder is expected to cite it, and paging on it would page on
        // gated mode working as designed.
        let key = "t/p1/.flint/lean/epoch";
        s.epoch_acquire(key, "holder-1", None).await.unwrap();
        let r = full_pass(&s, &spec, "op", None, true).await.unwrap();
        assert!(cond(&r, "StagedWorkRecovered").is_none());
        assert!(r.stranded_candidates.is_none());
    }

    /// The trap the kind drill caught, kept as a test: an observation
    /// pass consults no bucket, so it must never write
    /// `BoundaryModeAccepted=True`. Doing so clears a bucket-side
    /// refusal about two minutes after the posture pass raised it, and
    /// the operator is then looking at green while the destroyer — a
    /// customer's 1-day noncurrent rule — is still armed.
    #[tokio::test]
    async fn an_observation_pass_never_clears_a_bucket_side_refusal() {
        let mem = Arc::new(MemoryStore::new());
        let s: Arc<dyn ObjectStore> = mem.clone();
        mem.plant_lifecycle_rule(flint_store::LifecycleView {
            id: "corp-cost-policy".into(),
            enabled: true,
            prefix: "t/".into(),
            noncurrent_days: Some(1),
            expired_delete_marker: false,
        });
        let spec = spec_of(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));

        let posture = full_pass(&s, &spec, "op", Some(1), true).await.unwrap();
        assert_eq!(cond(&posture, "BoundaryModeAccepted").unwrap().reason, "ShorterNoncurrentRule");

        // …and now the cheap pass, whose spec check passes cleanly.
        let observed = full_pass(&s, &spec, "op", Some(1), false).await.unwrap();
        assert!(
            cond(&observed, "BoundaryModeAccepted").is_none(),
            "the observation pass re-asserted acceptance it never re-checked: {:?}",
            cond(&observed, "BoundaryModeAccepted")
        );

        // A spec-level refusal IS authoritative on a fast pass, though:
        // an incoherent knob must not wait out a posture cadence.
        let broken = spec_of(serde_json::json!({"boundaryMode": "gated"}));
        let observed = full_pass(&s, &broken, "op", Some(2), false).await.unwrap();
        assert_eq!(cond(&observed, "BoundaryModeAccepted").unwrap().reason, "LagBoundRequired");
    }

    /// The two cadences, and the property that makes them safe to
    /// separate: the OBSERVATION pass must still see what the sidecar
    /// is doing, and must not spend the posture's dozen requests to do
    /// it. §2.6 promises `LAG` as a printer column, and a lag column
    /// refreshed on the posture cadence is not a lag column.
    #[tokio::test]
    async fn the_observation_pass_reads_the_echo_without_the_posture_work() {
        let s = store();
        let key = "t/p1/.flint/lean/epoch";
        let lease = s.epoch_acquire(key, "holder-1", None).await.unwrap();
        let echo = serde_json::to_string(&flint_store::LeaseEcho {
            sidecar_version: "0.1.0".into(),
            protocol: 1,
            active_boundary_mode: "gated".into(),
            last_cited_seq: 42,
            last_cited_unix: 1_700_000_000,
            staged_uncited_count: 9,
            sentinel_verbs_active: true,
            metrics_bound: None,
        })
        .unwrap();
        s.epoch_renew(key, &lease, Some(&echo)).await.unwrap();

        let spec = spec_of(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));
        let r = full_pass(&s, &spec, "op", None, false).await.unwrap();
        assert_eq!(r.cited_seq, Some(42), "the fast pass did not read the echo");
        assert_eq!(r.staged_uncited, Some(9));
        assert_eq!(cond(&r, "BoundaryModeActive").unwrap().status, "True");
        // …and it did NOT do the posture's work: no claim was stamped,
        // no probe ran, no lifecycle rule was written.
        assert!(r.phase.is_empty(), "the observation pass overwrote the phase");
        assert!(cond(&r, "VersionRetentionProvisioned").is_none());
        assert!(
            s.lifecycle_rules().await.unwrap().is_empty(),
            "the observation pass provisioned lifecycle rules"
        );
        assert!(
            s.get_whole("t/p1/.flint/lean/claim", None).await.is_err(),
            "the observation pass stamped a claim"
        );
    }

    /// The failure that actually strands work on this fleet is a pod
    /// that DIED, and a dead holder leaves its cell behind unreleased.
    /// A condition that fires only on a clean release fires only in the
    /// one case that never strands anything — the drain cites
    /// everything before releasing.
    #[tokio::test]
    async fn a_dead_holders_cell_does_not_count_as_a_live_lease() {
        let mem = Arc::new(MemoryStore::new());
        let s: Arc<dyn ObjectStore> = mem.clone();
        let doc = serde_json::json!({
            "written_unix": 1u64, "epoch": 4, "boundary_mode": "gated",
            "candidates": [{"path": "ckpt.bin", "version_id": "v7", "size": 10,
                            "generation": 1, "epoch": 4, "staged_unix": 1u64}],
        });
        let body = bytes::Bytes::from(serde_json::to_vec(&doc).unwrap());
        let crc = crc64_nvme(&body);
        let stamps = GenerationStamps {
            generation: 0,
            epoch: 0,
            flush_uuid: "test".into(),
            boundary_source: None,
            posix: None,
        };
        s.put_whole(
            "t/p1/.flint/lean/orphans.json",
            body,
            &PutCondition::IfNoneMatchAny,
            &stamps,
            crc,
        )
        .await
        .unwrap();

        // An UNRELEASED cell whose last renewal is older than the
        // takeover threshold: a killed pod, which is routine here.
        s.epoch_acquire("t/p1/.flint/lean/epoch", "dead-holder", None).await.unwrap();
        mem.backdate_epoch("t/p1/.flint/lean/epoch", 600);

        let spec = spec_of(serde_json::json!({
            "boundaryMode": "gated", "visibilityLagBoundSecs": 300,
        }));
        let r = full_pass(&s, &spec, "op", None, true).await.unwrap();
        let c = cond(&r, "StagedWorkRecovered")
            .expect("a dead holder's cell suppressed the DR signature");
        assert_eq!(c.status, "False");
        assert_eq!(r.stranded_candidates, Some(1));
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
