//! The operator owns its CRD.
//!
//! # Why this is not the chart's job
//!
//! Helm installs `crds/` once and NEVER touches it on upgrade. A
//! chart-only CRD therefore freezes at whatever schema the cluster
//! first installed, and a structural schema PRUNES unknown fields
//! silently at admission: a knob added in a later release would be
//! accepted by `kubectl apply`, dropped by the API server, and take
//! its server default at the hub — with `kubectl get flintshare -o
//! yaml` showing it gone and nothing anywhere saying why. That is the
//! exact silent-default failure this CRD exists to retire; shipping it
//! back in through the packaging would be a joke at our own expense.
//!
//! So the operator applies its own compiled-in CRD at startup, before
//! the controller runs. The chart still ships a copy under `crds/` as
//! install-time bootstrap (so `kubectl apply -f` users have a schema
//! before the operator's first breath), but the operator is the thing
//! that keeps it current.
//!
//! # The guard
//!
//! A bare apply has one bad direction: during a fleet upgrade an OLD
//! operator that restarts would stomp the NEW schema back down and
//! prune everyone's new knobs. So the CRD carries
//! `chert.us/crd-schema-version`, and the operator refuses to apply
//! over a version higher than its own — it degrades loudly instead,
//! which is the correct behaviour for "someone is mid-upgrade and I am
//! the old one".

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client, CustomResourceExt};
use tracing::{error, info, warn};

use super::crd::FlintShare;
use super::reconcile::FIELD_MANAGER;

pub const SCHEMA_VERSION_ANNOTATION: &str = "chert.us/crd-schema-version";

/// Bump on EVERY schema change (a new spec field, a new CEL rule, a
/// new printer column). It is what tells a mixed-version fleet which
/// operator's CRD is the newer one; leaving it behind after adding a
/// field means an older operator can prune that field back out.
///
/// The equal-version case deliberately RE-APPLIES (drift repair), so
/// forgetting a bump is not a no-op — it is an older operator quietly
/// deleting the newer one's fields on its next startup.
///
/// - `1` — as shipped in 1.28.0.
/// - `2` — `spec.service.advertiseAddress` and its CEL rule, plus
///   `status.serverId`.
/// - `3` — `spec.persistence.reprovisionOnShrink`, and the
///   `Reprovisioning` phase. The phase matters as much as the field:
///   `status.phase` is an ENUM in the schema, so an operator on `2`
///   would refuse to store a value its CRD does not list.
/// - `4` — `spec.persistence.autoExpand` and its two CEL rules.
/// - `5` — the `Terminating` phase. Same reasoning as `3`: the phase
///   is an ENUM in the schema, so an operator on `4` would refuse to
///   store it.
/// - `6` — `status.conflictWith` and the CONFLICT printer column.
pub const SCHEMA_VERSION: u32 = 6;

/// The CRD this binary would install, annotated with its schema
/// version. Identical to what `crdgen` prints and what the chart
/// ships — one artifact, three consumers.
pub fn desired_crd() -> CustomResourceDefinition {
    // `crd::crd()`, not `FlintShare::crd()`: the raw derive output is
    // not structural and the API server refuses it.
    let mut crd = super::crd::crd();
    crd.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(
            SCHEMA_VERSION_ANNOTATION.to_string(),
            SCHEMA_VERSION.to_string(),
        );
    crd
}

pub fn served_version(crd: &CustomResourceDefinition) -> Option<u32> {
    crd.metadata
        .annotations
        .as_ref()?
        .get(SCHEMA_VERSION_ANNOTATION)?
        .parse()
        .ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrdDecision {
    /// Nothing is installed, ours is newer, or it is the same version —
    /// apply. Re-applying an equal version is not a no-op worth
    /// skipping: it REPAIRS a schema someone edited by hand (a stripped
    /// property prunes that field on every apply, silently, and nothing
    /// else in the system would ever put it back). Once per process
    /// start, so it costs nothing.
    Apply,
    /// The cluster serves a NEWER schema than this binary knows. Do not
    /// touch it: a rolling operator upgrade would otherwise flip the
    /// fleet's schema back and forth, pruning fields each way.
    RefuseNewer { served: u32 },
    /// A CRD exists with no version annotation — someone else's, or
    /// one installed before this mechanism. Apply (adopting it), but
    /// say so: this is the one case where we overwrite a schema we did
    /// not stamp.
    AdoptUnstamped,
}

pub fn decide(existing: Option<&CustomResourceDefinition>, mine: u32) -> CrdDecision {
    match existing {
        None => CrdDecision::Apply,
        Some(crd) => match served_version(crd) {
            None => CrdDecision::AdoptUnstamped,
            Some(v) if v > mine => CrdDecision::RefuseNewer { served: v },
            Some(_) => CrdDecision::Apply,
        },
    }
}

/// Result of the startup bootstrap, so `main` can refuse to start (or
/// start degraded) with a reason a human can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The served schema is ours (or newer, which is a superset).
    Ready,
    /// The controller can run, but the schema is not the one this
    /// binary expects — some spec fields may be pruned at admission.
    Degraded(String),
}

pub async fn ensure_crd(client: &Client) -> Result<Outcome, kube::Error> {
    let api: Api<CustomResourceDefinition> = Api::all(client.clone());
    let name = FlintShare::crd_name().to_string();
    let existing = match api.get(&name).await {
        Ok(c) => Some(c),
        Err(kube::Error::Api(e)) if e.code == 404 => None,
        Err(e) => return Err(e),
    };

    match decide(existing.as_ref(), SCHEMA_VERSION) {
        CrdDecision::RefuseNewer { served } => {
            // Not an error: it means a newer operator is rolling out.
            // Ours keeps reconciling — the newer schema is a superset —
            // but it must not write the old one back.
            warn!(
                "CRD {name} serves schema version {served}, newer than this operator's \
                 {SCHEMA_VERSION}; leaving it alone (a newer operator is rolling out). \
                 Fields this binary does not know are passed through untouched."
            );
            return Ok(Outcome::Ready);
        }
        CrdDecision::AdoptUnstamped => warn!(
            "CRD {name} exists without a {SCHEMA_VERSION_ANNOTATION} annotation — adopting it \
             and stamping version {SCHEMA_VERSION}"
        ),
        CrdDecision::Apply => {}
    }

    match api
        .patch(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(desired_crd()),
        )
        .await
    {
        Ok(_) => {
            info!("CRD {name} applied at schema version {SCHEMA_VERSION}");
            Ok(Outcome::Ready)
        }
        Err(e) => {
            let stale = existing.as_ref().and_then(served_version);
            let msg = format!(
                "could not apply CRD {name} ({e}). The operator will keep reconciling against \
                 the schema the cluster already serves (version {}), but any FlintShare field \
                 newer than it is SILENTLY PRUNED at admission — grant the operator \
                 customresourcedefinitions create/patch, or apply the CRD by hand from `crdgen`.",
                stale.map(|v| v.to_string()).unwrap_or_else(|| "unknown".into())
            );
            if existing.is_none() {
                // Nothing to watch and no way to install it: failing
                // here is honest, and the pod's restart loop carries the
                // message.
                error!("{msg}");
                return Err(e);
            }
            error!("{msg}");
            Ok(Outcome::Degraded(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use std::collections::BTreeMap;

    fn crd_at(version: Option<&str>) -> CustomResourceDefinition {
        let mut c = FlintShare::crd();
        c.metadata = ObjectMeta {
            annotations: version.map(|v| {
                BTreeMap::from([(SCHEMA_VERSION_ANNOTATION.to_string(), v.to_string())])
            }),
            ..c.metadata
        };
        c
    }

    #[test]
    fn the_crd_we_would_install_is_stamped() {
        let crd = desired_crd();
        assert_eq!(served_version(&crd), Some(SCHEMA_VERSION));
        assert_eq!(crd.metadata.name.as_deref(), Some("flintshares.chert.us"));
    }

    /// The upgrade direction that must work: a new operator raises the
    /// schema so the knobs it knows about stop being pruned.
    #[test]
    fn a_newer_operator_upgrades_the_schema() {
        assert_eq!(decide(Some(&crd_at(Some("1"))), 2), CrdDecision::Apply);
        assert_eq!(decide(None, 1), CrdDecision::Apply);
    }

    /// The direction that must NOT: an old operator restarting during a
    /// rollout would otherwise prune every field the new schema added,
    /// for as long as it lived.
    #[test]
    fn an_older_operator_refuses_to_stomp_a_newer_schema() {
        assert_eq!(
            decide(Some(&crd_at(Some("5"))), 1),
            CrdDecision::RefuseNewer { served: 5 }
        );
    }

    /// Same version still applies: a schema someone edited by hand (a
    /// property stripped, a rule dropped) silently prunes that field on
    /// every admission, and nothing else would ever restore it.
    #[test]
    fn an_equal_version_is_reapplied_so_hand_edits_are_repaired() {
        assert_eq!(decide(Some(&crd_at(Some("1"))), 1), CrdDecision::Apply);
    }

    /// A CRD installed by the chart (or by hand) carries no stamp; we
    /// adopt it rather than refuse to start, but never silently.
    #[test]
    fn an_unstamped_crd_is_adopted_loudly() {
        assert_eq!(decide(Some(&crd_at(None)), 1), CrdDecision::AdoptUnstamped);
        assert_eq!(
            decide(Some(&crd_at(Some("not-a-number"))), 1),
            CrdDecision::AdoptUnstamped
        );
    }
}
