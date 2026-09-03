//! Which project a publish is for, and whether this pod may have it
//! (design §3.4 steps 4-5).
//!
//! The pod names a CR in ITS OWN namespace (kubelet-asserted, never the
//! pod author's word). The CR is the policy object: bucket, prefix,
//! endpoint, presentation defaults, and the `consumers` list of
//! ServiceAccounts allowed to mount it. `consumers` absent ⇒ deny.

use kube::api::Api;
use kube::Client;
use serde_json::Value;

use super::attrs::Selector;
use super::policy::{Consumers, CredentialMode};
use crate::lean_operator::crd::{FlintLeanWorkspace, FlintLeanWorkspaceSpec};
use crate::passthrough::spec::MountSpec;

#[derive(Debug, Clone)]
pub enum Resolved {
    Passthrough { spec: MountSpec },
    Lean { spec: FlintLeanWorkspaceSpec, phase: Option<String> },
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub consumers: Consumers,
    pub credential_mode: CredentialMode,
}

/// Why a publish is refused. `Transient` is the only retryable one;
/// the others are final until the tenant changes something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotFound(String),
    Invalid(String),
    Forbidden(String),
    Transient(String),
}

impl Refusal {
    pub fn message(&self) -> &str {
        match self {
            Refusal::NotFound(m) | Refusal::Invalid(m) | Refusal::Forbidden(m) | Refusal::Transient(m) => m,
        }
    }
}

impl Resolved {
    pub fn policy(&self) -> Result<Policy, Refusal> {
        let (consumers, identity) = match self {
            Resolved::Passthrough { spec } => (spec.consumers.clone(), spec.identity.clone()),
            Resolved::Lean { spec, .. } => (spec.consumers.clone(), spec.identity.clone()),
        };
        let mode = identity.map(|i| i.mode).unwrap_or_default();
        let credential_mode = CredentialMode::parse(&mode).map_err(Refusal::Invalid)?;
        Ok(Policy { consumers: consumers.unwrap_or_default(), credential_mode })
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Resolved::Passthrough { .. } => "passthrough",
            Resolved::Lean { .. } => "lean",
        }
    }
}

/// Pure: a passthrough CR's JSON (as the API returned it) into a
/// validated spec. `None` = the CR does not exist.
pub fn decide_passthrough(cr: Option<Value>, ns: &str, name: &str) -> Result<Resolved, Refusal> {
    let Some(cr) = cr else {
        return Err(Refusal::NotFound(format!(
            "FlintPassthroughMount {ns}/{name} does not exist — the pod's volumeAttributes name it, \
             and the CR must live in the pod's own namespace"
        )));
    };
    let spec: MountSpec = match cr.get("spec").cloned() {
        Some(s) => serde_json::from_value(s)
            .map_err(|e| Refusal::Invalid(format!("FlintPassthroughMount {ns}/{name} spec is invalid: {e}")))?,
        None => return Err(Refusal::Invalid(format!("FlintPassthroughMount {ns}/{name} has no spec"))),
    };
    spec.validate()
        .map_err(|e| Refusal::Invalid(format!("FlintPassthroughMount {ns}/{name}: {e}")))?;
    Ok(Resolved::Passthrough { spec })
}

pub fn decide_lean(cr: Option<FlintLeanWorkspace>, ns: &str, name: &str) -> Result<Resolved, Refusal> {
    let Some(cr) = cr else {
        return Err(Refusal::NotFound(format!(
            "FlintLeanWorkspace {ns}/{name} does not exist — the pod's volumeAttributes name it, \
             and the CR must live in the pod's own namespace"
        )));
    };
    let phase = cr.status.as_ref().and_then(|s| s.phase.clone());
    if phase.as_deref() == Some("Refused") {
        let why = cr.status.as_ref().and_then(|s| s.message.clone()).unwrap_or_default();
        return Err(Refusal::Invalid(format!(
            "FlintLeanWorkspace {ns}/{name} is Refused by its operator — {why}"
        )));
    }
    Ok(Resolved::Lean { spec: cr.spec, phase })
}

/// The authorization step. Names the SA and the field in every refusal:
/// this message is the tenant's `FailedMount` event.
pub fn authorize(policy: &Policy, service_account: &str, ns: &str, kind: &str, name: &str) -> Result<(), Refusal> {
    if policy.consumers.allows(service_account) {
        return Ok(());
    }
    if policy.consumers.service_accounts.is_empty() {
        return Err(Refusal::Forbidden(format!(
            "{kind} {ns}/{name} has no spec.consumers.serviceAccounts — under the CSI delivery an absent \
             list denies every pod, ServiceAccount {ns}/{service_account} included; list the ServiceAccounts \
             that may mount it (\"*\" for any in the namespace)"
        )));
    }
    Err(Refusal::Forbidden(format!(
        "ServiceAccount {ns}/{service_account} is not in spec.consumers.serviceAccounts of {kind} {ns}/{name}"
    )))
}

/// Fetch the CR the selector names, in the pod's namespace.
pub async fn fetch(client: &Client, sel: &Selector, ns: &str) -> Result<Resolved, Refusal> {
    match sel {
        Selector::Mount(name) => {
            let cr = crate::passthrough::spec::get_mount(client, ns, name)
                .await
                .map_err(|e| Refusal::Transient(format!("FlintPassthroughMount {ns}/{name} lookup: {e}")))?;
            decide_passthrough(cr, ns, name)
        }
        Selector::Workspace(name) => {
            let api: Api<FlintLeanWorkspace> = Api::namespaced(client.clone(), ns);
            let cr = api
                .get_opt(name)
                .await
                .map_err(|e| Refusal::Transient(format!("FlintLeanWorkspace {ns}/{name} lookup: {e}")))?;
            decide_lean(cr, ns, name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pt(spec: Value) -> Value {
        json!({ "apiVersion": "flint.io/v1alpha1", "kind": "FlintPassthroughMount",
                "metadata": { "name": "d", "namespace": "team-a" }, "spec": spec })
    }

    #[test]
    fn missing_cr_is_not_found_naming_the_namespace() {
        let e = decide_passthrough(None, "team-a", "d").unwrap_err();
        assert!(matches!(e, Refusal::NotFound(_)));
        assert!(e.message().contains("team-a/d"));
    }

    #[test]
    fn invalid_spec_is_final() {
        let e = decide_passthrough(Some(pt(json!({ "bucket": "" }))), "team-a", "d").unwrap_err();
        assert!(matches!(e, Refusal::Invalid(_)), "{e:?}");
        let e = decide_passthrough(Some(pt(json!({ "bucket": "b", "mountOptions": ["-o", "x"] }))), "team-a", "d").unwrap_err();
        assert!(matches!(e, Refusal::Invalid(_)), "{e:?}");
    }

    #[test]
    fn consumers_absent_denies_and_names_the_field() {
        let r = decide_passthrough(Some(pt(json!({ "bucket": "b" }))), "team-a", "d").unwrap();
        let p = r.policy().unwrap();
        assert_eq!(p.credential_mode, CredentialMode::Broker);
        let e = authorize(&p, "alice", "team-a", "FlintPassthroughMount", "d").unwrap_err();
        assert!(matches!(e, Refusal::Forbidden(_)));
        assert!(e.message().contains("spec.consumers.serviceAccounts"), "{e:?}");
    }

    #[test]
    fn listed_sa_passes_others_named_in_the_refusal() {
        let r = decide_passthrough(
            Some(pt(json!({ "bucket": "b", "consumers": { "serviceAccounts": ["alice"] }, "identity": { "mode": "static" } }))),
            "team-a",
            "d",
        )
        .unwrap();
        let p = r.policy().unwrap();
        assert_eq!(p.credential_mode, CredentialMode::Static);
        authorize(&p, "alice", "team-a", "FlintPassthroughMount", "d").unwrap();
        let e = authorize(&p, "bob", "team-a", "FlintPassthroughMount", "d").unwrap_err();
        assert!(e.message().contains("team-a/bob"), "{e:?}");
    }

    #[test]
    fn unknown_identity_mode_is_invalid() {
        let r = decide_passthrough(
            Some(pt(json!({ "bucket": "b", "consumers": { "serviceAccounts": ["*"] }, "identity": { "mode": "knox" } }))),
            "team-a",
            "d",
        )
        .unwrap();
        assert!(matches!(r.policy().unwrap_err(), Refusal::Invalid(_)));
    }
}
