// Minimal State SPDK CSI Driver - Library Modules

// The dashboard's warp route chain (spdk_dashboard_backend_minimal) nests one
// filter type per `.or(route)`; at 20+ routes the trait solver overflows the
// default limit of 128 on rustc <=1.92 (E0275 at the /api/nodes addition).
#![recursion_limit = "256"]

pub mod identity;  // Canonical volume identity: VolumeRef + naming + parsers (identity-unification Phase 0)
pub mod spdk_native;
pub mod nvmeof_utils;
pub mod nvmeof_export;  // Convergent NVMe-oF export (phase 0 idempotency fix)
pub mod nvme_recovery;  // Graceful recovery from spdk-tgt hard stop/restart (#1 reconcile-on-loss, #2 survivable reconnect, #3 disconnect-before-reuse)
pub mod replica_sync;  // Persistent per-replica sync state on the PV (incremental-rebuild phase 1)
pub mod epoch_scheduler;  // Common-epoch snapshot scheduler (incremental-rebuild phase 2)
pub mod catchup;  // Replica catch-up orchestrator / warm standby + reassembly admission (incremental-rebuild phases 3/4)
pub mod replica_replace;  // U11: replica re-placement after permanent node loss (pre-pass of the catch-up task)
pub mod cutover;  // Reassembly cutover: RWX NFS-pod bounce + RWO policy knob (incremental-rebuild phase 4)
pub mod mount_opts;  // Driver-default / operator-mountOptions merge. runax 2026-08-02: class mountOptions could not override ANY default the driver emitted — the RWX path never read them at all, and the pNFS path emitted both values and bet on the kernel's last-one-wins parse.
pub mod driver;
pub mod minimal_models;
pub mod minimal_disk_service;
pub mod node_agent;
pub mod mount_util;  // Bounded unmount (D-state hang containment, 2026-06-12)
pub mod ublk_ctrl;  // UBLK_U_CMD_DEL_DEV escape hatch (DEAD-device reclaim, runy2 2026-07-21)
pub mod orphan_sweep;  // §10-14 node-local reaping of lvols/exports keyed by absent PVs
pub mod controller_reap;  // Dead NVMe-oF controller reaping (Tier-2 7b-0 spike finding)
pub mod hot_rejoin;  // Tier-2 7b-1/7b-2: hot rejoin into a live raid (skip_rebuild window + localization + trigger loop)
pub mod volume_claims;  // Tier-2 7b-2: per-volume single-operation claim shared by catch-up/cutover/hot-rejoin
pub mod maint_roll;  // The csi-node roll landmine fix: drain-before-restart roller (docs/maintenance-drain-csi-node-roll.md; formal maintenance tranche)
pub mod expand;  // Replicated volume expansion fan-out core (v1.21.0; extracted for the F56 sim tier)
pub mod orchestrator_role;  // F53: which PROCESS may run the orchestrators (volume_claims is in-process, so two that do serialize nothing)
pub mod orchestrator_lease; // the MECHANISM behind that decision: kube-Lease leader election among the granted candidates
pub mod freshness_gate;  // F36c: last-writer-set degraded-assembly gate (docs/f36c-assembly-freshness-gate.md)
pub mod guarded_destroy;  // Contract R3: the destruction chokepoint (docs/attach-detach-robustness-contract.md)
pub mod leg_size_guard;  // F43 doc item #8: leg-size precondition on raid admission (silent-shrink guard; kill switch FLINT_LEG_SIZE_GUARD)
pub mod node_volume_locks;  // Contract R2: per-volume node-local lock (probe→mutate serialization; kill switch FLINT_VOLUME_LOCK)
pub mod spdk_dashboard_backend_minimal;
pub mod dashboard_auth;  // Bearer-token auth for the dashboard backend (frontend Phase 0)
pub mod snapshot;  // Volume snapshot support (isolated module)
pub mod capacity_cache;  // Capacity caching for scalability
pub mod raid;  // RAID support for multi-replica volumes
pub mod nfs;  // NFSv4.2 server for RWX volume support
pub mod rwx_nfs;  // ReadWriteMany (RWX) support via NFS pods (isolated module - zero regression)
pub mod pnfs;  // pNFS (Parallel NFS) support - metadata/data server separation (experimental)
pub mod pnfs_csi;  // CSI driver-side client to the pNFS MDS (used by main.rs when layout: pnfs)
pub mod pnfs_block_session;  // csi-node nvme session management for pnfs-block volumes (§5)
pub mod reserved_devices;  // Device reservation for direct SPDK access (device plugin use)
pub mod state_backend;  // Persistence trait + impls for NFSv4/pNFS server state (Phase B)
pub mod tier;  // S3 cold tier (L2) — design of record: docs/plans/s3-tier-l2-design-review.md
pub mod lite_operator;  // flint-lite operator: the FlintShare CRD + its reconcile (docs/plans/flint-lite-operator-plan.md)
pub mod lean_operator;  // flint-lean operator: FlintLeanWorkspace CRD + claim/adopt + sidecar injection (docs/plans/flint-lean-plan.md §2.4); separate controller, shared image
pub mod lite_gateway;  // flint-hub-gateway: one door in front of every hub's file API (docs/flint-hub-gateway.md)

/// Install the process-wide rustls crypto provider. **Call this first
/// in every binary that opens a TLS connection through rustls** — in
/// practice, everything that builds a `kube::Client`.
///
/// # The bug this exists to prevent (it shipped, in 1.26.0 and 1.27.0)
///
/// rustls 0.23 picks a provider from ITS OWN crate features, and
/// refuses to guess when more than one is enabled. Until v1.26.0 only
/// `ring` was in the tree (via hyper-rustls), so the default resolved
/// silently. v1.26.0 added the AWS SDK for the S3 tier, which brings
/// `aws-lc-rs` — and from that commit on, the FIRST rustls client
/// config any binary built PANICKED:
///
/// ```text
/// Could not automatically determine the process-level CryptoProvider
/// from Rustls crate features.
/// ```
///
/// `Client::try_default()` is the second statement in the CSI driver's
/// `main`, so the driver could not start at all. Nothing caught it:
/// the S3 tier is unaffected (the AWS SDK passes its provider
/// EXPLICITLY and never consults the process default — which is why
/// every tier drill and the real-S3 L4 run stayed green), the hub
/// binary does not use kube, and the kind lanes exercised by the two
/// releases were the lite ones.
///
/// Installing a default is safe for the SDK's users too, precisely
/// because the SDK ignores it.
pub fn install_crypto_provider() {
    // Err = someone already installed one. That is a fine outcome:
    // this function's contract is "a provider is installed after this
    // returns", not "I installed it".
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[cfg(test)]
mod crypto_provider_tests {
    /// The call that panicked in the field. `ClientConfig::builder()`
    /// resolves the process-level provider, so this reproduces the
    /// exact failure — and passes only because the installer ran.
    #[test]
    fn a_rustls_client_config_can_actually_be_built() {
        super::install_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "no process-level rustls provider — every kube client would panic"
        );
        let _ = rustls::ClientConfig::builder();
        // Idempotent: a second call must not panic or unset anything.
        super::install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    /// The guard with teeth. A unit test can only prove the installer
    /// WORKS; what shipped broken was a binary that never called it.
    /// So this reads the entry points themselves — the same
    /// source-is-the-schema idiom as the chart knob-list test.
    ///
    /// It enumerates `[[bin]]` from Cargo.toml rather than naming
    /// files. The hand-written list this replaced covered two of the
    /// nine binaries that actually build, so the guard's own doc
    /// comment ("what shipped broken was a binary that never called
    /// it") described a gap the guard still had: the next binary to
    /// grow a kube client would have shipped unchecked, which is the
    /// exact shape of the 1.26.0/1.27.0 regression.
    #[test]
    fn every_binary_that_builds_a_kube_client_installs_the_provider() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");

        // Hand-rolled rather than pulling in a toml dev-dependency:
        // the shape is `[[bin]]` then `path = "..."`, and commented-out
        // targets (a real case — `controller_operator.rs`) are not
        // compiled and so cannot panic. Ignoring them is the point.
        let mut paths = Vec::new();
        let mut in_bin = false;
        for line in manifest.lines() {
            let t = line.trim();
            if t.starts_with('#') {
                continue;
            }
            if t.starts_with("[[") || t.starts_with('[') {
                in_bin = t == "[[bin]]";
                continue;
            }
            if in_bin {
                if let Some(rest) = t.strip_prefix("path") {
                    if let Some(v) = rest.split('=').nth(1) {
                        paths.push(v.trim().trim_matches('"').to_string());
                    }
                }
            }
        }
        assert!(
            paths.len() >= 8,
            "parsed only {} [[bin]] targets from Cargo.toml — the parser has drifted from the \
             manifest and this guard is no longer guarding anything",
            paths.len()
        );

        let mut checked = 0;
        for path in &paths {
            let src = match std::fs::read_to_string(root.join(path)) {
                Ok(s) => s,
                // A [[bin]] whose file is missing is a broken manifest,
                // but that is cargo's complaint to make, not ours.
                Err(_) => continue,
            };
            if !src.contains("Client::try_default") {
                continue;
            }
            checked += 1;
            let install = src.find("install_crypto_provider()").unwrap_or_else(|| {
                panic!(
                    "{path} builds a kube client but never calls install_crypto_provider() — \
                     it will PANIC at startup on 'could not automatically determine the \
                     process-level CryptoProvider' (the 1.26.0/1.27.0 regression)"
                )
            });
            let client = src.find("Client::try_default").expect("checked above");
            assert!(
                install < client,
                "{path} installs the crypto provider AFTER building its kube client — the panic \
                 happens during construction, so the order is the whole fix"
            );
        }
        assert!(
            checked >= 2,
            "no [[bin]] target appears to build a kube client — either the walk broke or the \
             detection string did; a guard that checks nothing passes silently"
        );
    }
}

// Include generated CSI protobuf types (generated by tonic-build)
pub mod csi {
    #![allow(non_camel_case_types)]
    #![allow(clippy::all)]
    // tonic-build generates files based on package name: csi.v1 -> csi.v1.rs
    include!(concat!(env!("OUT_DIR"), "/csi.v1.rs"));
}

// Export minimal models instead of CRD models
pub use minimal_models::*;

/// CSI topology segment key advertised by every node (NodeGetInfo) and
/// consumed by the controller's single-replica placement. The segment
/// value is the k8s node name (== NODE_ID == the names get_all_nodes()
/// returns), so a `preferred` topology from a WaitForFirstConsumer bind
/// names the node the consuming pod scheduled onto. Placement honors it
/// so a single-replica volume's lvol lands on the same node as its pod
/// (data locality; and it lets anti-affinity-spread pods — e.g. sharded
/// MDS shards — pull their state.db disks onto distinct nodes). Absent /
/// unparseable topology degrades to the historical max-free placement.
pub const TOPOLOGY_NODE_KEY: &str = "topology.flint.csi.storage.io/node";

/// Extract the ordered list of preferred node names from a CreateVolume
/// request's `accessibility_requirements`. Reads the [`TOPOLOGY_NODE_KEY`]
/// segment out of each `preferred` topology, preserving the CO's order
/// (the CO lists the selected node first for WaitForFirstConsumer binds).
/// Returns empty when there is no requirement or no recognizable segment —
/// which the placement path treats as "no hint, use max-free".
pub fn preferred_nodes_from_topology(
    accessibility: Option<&csi::TopologyRequirement>,
) -> Vec<String> {
    accessibility
        .map(|tr| {
            tr.preferred
                .iter()
                .filter_map(|t| t.segments.get(TOPOLOGY_NODE_KEY).cloned())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod topology_tests {
    use super::*;
    use csi::{Topology, TopologyRequirement};
    use std::collections::HashMap;

    fn topo(node: &str) -> Topology {
        let mut segments = HashMap::new();
        segments.insert(TOPOLOGY_NODE_KEY.to_string(), node.to_string());
        Topology { segments }
    }

    #[test]
    fn none_requirement_yields_empty() {
        assert!(preferred_nodes_from_topology(None).is_empty());
    }

    #[test]
    fn reads_preferred_in_order() {
        let req = TopologyRequirement {
            requisite: vec![topo("node-a"), topo("node-b")],
            preferred: vec![topo("node-b"), topo("node-a")],
        };
        assert_eq!(
            preferred_nodes_from_topology(Some(&req)),
            vec!["node-b".to_string(), "node-a".to_string()]
        );
    }

    #[test]
    fn ignores_foreign_segment_keys() {
        let mut segments = HashMap::new();
        segments.insert("topology.kubernetes.io/zone".to_string(), "us-west-1a".to_string());
        let req = TopologyRequirement {
            requisite: vec![],
            preferred: vec![Topology { segments }],
        };
        assert!(preferred_nodes_from_topology(Some(&req)).is_empty());
    }
}
