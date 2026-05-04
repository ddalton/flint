//! Startup preflight check for VolumeSnapshot CRDs.
//!
//! VolumeSnapshot/Content/Class CRDs and the snapshot-controller
//! Deployment are cluster-singleton, cluster-scoped components installed
//! by the cluster admin (not by Flint's chart — bundling them would
//! conflict with other CSI drivers in the same cluster, since CRDs are
//! cluster-wide objects shared across drivers).
//!
//! When the CRDs are missing, the cluster's snapshot-controller pod
//! itself crash-loops on startup with "no matches for kind
//! VolumeSnapshotContent". Users debugging this often look at the Flint
//! controller pod first because that's where snapshot UX surfaces. This
//! preflight runs once at controller startup, logs a clear message
//! pointing at the right install commands, and continues running. It
//! never fails the Flint pod — non-snapshot CSI operations
//! (CreateVolume, NodePublishVolume, ...) work fine without snapshot
//! CRDs and most users never use snapshots.

use kube::{api::Api, Client};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;

/// Required snapshot-related CRDs. Maintained from the canonical set
/// installed by `kubernetes-csi/external-snapshotter` client/config/crd.
const REQUIRED_CRDS: &[&str] = &[
    "volumesnapshotclasses.snapshot.storage.k8s.io",
    "volumesnapshots.snapshot.storage.k8s.io",
    "volumesnapshotcontents.snapshot.storage.k8s.io",
];

/// Pinned external-snapshotter release surfaced in the install
/// instructions. Bump when the project pins a newer supported release;
/// the version in the kubectl-apply URL must match what we test against.
pub const EXTERNAL_SNAPSHOTTER_RELEASE: &str = "v8.2.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotPreflightResult {
    /// All required CRDs found.
    Ready,
    /// One or more required CRDs are missing — the operator needs to
    /// run the install commands surfaced in the rendered message.
    CrdsMissing(Vec<&'static str>),
    /// CRD lookup failed for a reason other than "not found"
    /// (e.g. RBAC denial, transient API-server error). Reported but
    /// non-fatal — non-snapshot RPCs still work and we don't claim the
    /// CRDs are missing when we don't actually know.
    LookupFailed(String),
}

pub async fn check_snapshot_crds(client: &Client) -> SnapshotPreflightResult {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let mut missing: Vec<&'static str> = Vec::new();
    for &name in REQUIRED_CRDS {
        match crds.get(name).await {
            Ok(_) => {}
            Err(kube::Error::Api(api_err)) if api_err.code == 404 => {
                missing.push(name);
            }
            Err(e) => {
                return SnapshotPreflightResult::LookupFailed(e.to_string());
            }
        }
    }
    if missing.is_empty() {
        SnapshotPreflightResult::Ready
    } else {
        SnapshotPreflightResult::CrdsMissing(missing)
    }
}

pub fn render_preflight_message(result: &SnapshotPreflightResult) -> String {
    match result {
        SnapshotPreflightResult::Ready => {
            "✅ [PREFLIGHT] Snapshot CRDs present — snapshot RPCs enabled".to_string()
        }
        SnapshotPreflightResult::CrdsMissing(missing) => {
            let mut s = String::new();
            s.push_str("⚠️  [PREFLIGHT] Snapshot CRDs not installed in this cluster.\n");
            s.push_str("    Snapshot RPCs will return FAILED_PRECONDITION until installed.\n");
            s.push_str("    Non-snapshot operations are unaffected.\n");
            s.push_str("    To install (one-time, cluster-wide):\n");
            s.push_str(&format!(
                "       kubectl apply -k https://github.com/kubernetes-csi/external-snapshotter/client/config/crd?ref={}\n",
                EXTERNAL_SNAPSHOTTER_RELEASE
            ));
            s.push_str(&format!(
                "       kubectl apply -k https://github.com/kubernetes-csi/external-snapshotter/deploy/kubernetes/snapshot-controller?ref={}\n",
                EXTERNAL_SNAPSHOTTER_RELEASE
            ));
            s.push_str("    Missing CRDs:");
            for name in missing {
                s.push_str(&format!("\n       - {}", name));
            }
            s
        }
        SnapshotPreflightResult::LookupFailed(msg) => {
            format!(
                "⚠️  [PREFLIGHT] Could not verify snapshot CRDs: {}. \
                 Snapshot RPCs may fail; non-snapshot operations unaffected.",
                msg
            )
        }
    }
}

pub fn log_preflight_result(result: &SnapshotPreflightResult) {
    eprintln!("{}", render_preflight_message(result));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_message_is_single_line() {
        let msg = render_preflight_message(&SnapshotPreflightResult::Ready);
        assert!(
            !msg.contains('\n'),
            "Ready message should be one line, got {:?}",
            msg
        );
        assert!(msg.contains("PREFLIGHT"));
        assert!(msg.to_lowercase().contains("snapshot"));
    }

    #[test]
    fn missing_message_includes_install_commands_and_pinned_version() {
        let msg = render_preflight_message(&SnapshotPreflightResult::CrdsMissing(vec![
            "volumesnapshots.snapshot.storage.k8s.io",
        ]));
        assert!(
            msg.contains("kubectl apply -k"),
            "must point at kubectl-apply command: {}",
            msg
        );
        assert!(msg.contains("external-snapshotter/client/config/crd"));
        assert!(msg.contains("external-snapshotter/deploy/kubernetes/snapshot-controller"));
        assert!(
            msg.contains(EXTERNAL_SNAPSHOTTER_RELEASE),
            "must pin a version so users get reproducible installs: {}",
            msg
        );
    }

    #[test]
    fn missing_message_clarifies_non_snapshot_rpcs_still_work() {
        let msg = render_preflight_message(&SnapshotPreflightResult::CrdsMissing(
            REQUIRED_CRDS.to_vec(),
        ));
        // Critical UX: a user reading this log shouldn't think the
        // entire driver is broken when only snapshot RPCs are gated.
        assert!(
            msg.to_lowercase().contains("non-snapshot")
                || msg.to_lowercase().contains("unaffected"),
            "message should reassure that non-snapshot ops still work: {}",
            msg
        );
    }

    #[test]
    fn missing_message_lists_each_missing_crd() {
        let result = SnapshotPreflightResult::CrdsMissing(REQUIRED_CRDS.to_vec());
        let msg = render_preflight_message(&result);
        for &name in REQUIRED_CRDS {
            assert!(
                msg.contains(name),
                "missing CRD {} should appear in message:\n{}",
                name,
                msg
            );
        }
    }

    #[test]
    fn lookup_failed_message_does_not_falsely_claim_crds_missing() {
        let msg = render_preflight_message(&SnapshotPreflightResult::LookupFailed(
            "forbidden: User cannot list customresourcedefinitions".to_string(),
        ));
        assert!(msg.contains("forbidden"), "must surface the underlying error");
        // We don't *know* the CRDs are absent when lookup fails — must
        // not claim it. A false "CRDs missing" message would send the
        // operator down the wrong debugging path.
        assert!(
            !msg.contains("not installed"),
            "LookupFailed must not claim CRDs are missing: {}",
            msg
        );
    }

    #[test]
    fn required_crds_list_matches_external_snapshotter_v1_set() {
        // Pinned: the canonical set from external-snapshotter v8 CRDs.
        // If this changes upstream, EXTERNAL_SNAPSHOTTER_RELEASE must be
        // bumped together with this list.
        assert_eq!(REQUIRED_CRDS.len(), 3);
        assert!(REQUIRED_CRDS.contains(&"volumesnapshots.snapshot.storage.k8s.io"));
        assert!(REQUIRED_CRDS.contains(&"volumesnapshotcontents.snapshot.storage.k8s.io"));
        assert!(REQUIRED_CRDS.contains(&"volumesnapshotclasses.snapshot.storage.k8s.io"));
    }
}
