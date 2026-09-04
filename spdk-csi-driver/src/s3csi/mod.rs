//! `s3.csi.chert.us` — the CSI node driver that replaces the flint-passthrough
//! and flint-lean sidecar-injection webhooks.
//!
//! Design of record: `docs/plans/csi-node-mount-design.md`. The one-line
//! version: a privileged node DaemonSet (this module, one process per
//! node) serves CSI *ephemeral inline* volumes. For each tenant pod that
//! declares one, it resolves the pod's project (a `FlintPassthroughMount`
//! or `FlintLeanWorkspace` in the POD'S namespace), authorizes the pod's
//! kubelet-asserted ServiceAccount against the CR's consumer list, creates
//! one unprivileged worker pod in a system namespace, and either
//!
//! - passthrough: opens `/dev/fuse`, performs the `mount(2)` itself and
//!   hands the fd to the worker over `SCM_RIGHTS`, where an unchanged
//!   `mount-s3` serves it (`fuse.rs`, `worker.rs`); or
//! - lean: gives the worker a plugin-owned tree over which the unchanged
//!   `flint-sync run` checks out and publishes (`worker.rs`, `node.rs`).
//!
//! The tenant pod is given nothing: no sidecar, no credential, no
//! privilege, no label — it can be admitted under PodSecurity
//! `restricted`. Credentials reach the worker through the loopback
//! container-credentials door the worker serves from files this plugin
//! writes host-side (`creds.rs`), minted by the broker (`broker/`) from
//! the kubelet-minted, pod-bound ServiceAccount token kubelet delivers
//! with every publish.
//!
//! What lives where (§3.6, said plainly): this process is privileged
//! (it mounts); the worker pods are not; the tenant pods are not. Istio's
//! CNI move concentrated privilege the same way; it did not remove it.

pub mod attrs;
pub mod creds;
pub mod fuse;
pub mod node;
pub mod policy;
pub mod quota;
pub mod resolve;
pub mod state;
pub mod worker;
pub mod broker;

/// The CSIDriver name. Also the audience of the pod-bound ServiceAccount
/// token kubelet mints for each publish (`CSIDriver.spec.tokenRequests`).
pub const DRIVER_NAME: &str = "s3.csi.chert.us";

/// Where kubelet keeps pods on the node (the DaemonSet mounts it at the
/// same path, `Bidirectional`), overridable for rigs.
pub fn kubelet_root() -> std::path::PathBuf {
    std::env::var("FLINT_S3CSI_KUBELET_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/kubelet"))
}

/// The plugin's own directory: `<kubelet>/plugins/s3.csi.chert.us/`.
pub fn plugin_root() -> std::path::PathBuf {
    std::env::var("FLINT_S3CSI_PLUGIN_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| kubelet_root().join("plugins").join(DRIVER_NAME))
}
