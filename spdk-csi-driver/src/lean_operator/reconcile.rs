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

    #[tokio::test]
    async fn verify_full_pass_claims_and_sweeps() {
        let s = store();
        let (phase, _, _) = verify_workspace(&s, "t/p2", "proj2", "op").await.unwrap();
        assert_eq!(phase, "Claimed");
        let (phase, _, _) = verify_workspace(&s, "t/p2", "proj2", "op").await.unwrap();
        assert_eq!(phase, "Adopted");
    }
}
