//! Restart the hubs a flint-csi-node roll left on dead storage
//! (nfs-proxy design §7a, "flint-csi-node rolls").
//!
//! A restart of the node's spdk-tgt — a csi-node DaemonSet roll, an
//! upgrade, a crash — takes the staged block device of every flint PVC
//! mounted on that node with it, and the hub's ext4 goes EIO and stays
//! that way (`maintenance-drain-csi-node-roll.md`, "the local half"). A
//! container restart does not help: it keeps the dead mount. The fix that
//! works is a new pod after the old one is GONE, so the volume is unstaged
//! and staged again — scale to zero, wait, scale back
//! (`IdleState::Restarting`). A bare pod delete races the ReplicaSet and
//! the replacement can inherit the dead staging mount.
//!
//! At about 100 hubs per node a routine driver upgrade is otherwise a mass
//! outage that each owner has to notice and fix by hand. Here: every
//! `every`, list the csi-node pods and the hub pods, and mark each hub
//! whose node's spdk-tgt started AFTER the hub pod did. The reconciler
//! does the rest.
//!
//! Not covered: the tgt of the node that HOSTS the volume, when that is
//! not the hub's node (a remote lvol reached over NVMe-oF). Its restart
//! breaks the hub the same way; finding it needs the PV's placement.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Client, ResourceExt};
use tracing::{debug, info, warn};

use super::crd::{FlintShare, Lifecycle};
use super::idle::{self, IdleState};
use super::render;

/// Where the csi-node pods are, and which container is the spdk-tgt.
#[derive(Debug, Clone)]
pub struct Config {
    pub namespace: String,
    pub selector: String,
    pub container: String,
    /// Only hubs whose PVC is in one of these classes are restarted: a
    /// tgt restart does nothing to a volume flint does not serve.
    pub storage_classes: Vec<String>,
    pub every: Duration,
}

/// A running hub pod, as the planner needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct HubPod {
    pub namespace: String,
    pub share: String,
    pub node: String,
    pub started: DateTime<Utc>,
}

/// The hubs whose node's spdk-tgt started after them: their staged
/// device died with the old tgt. A hub on a node whose tgt is not running
/// right now is left alone until it is (a restart then would stage onto a
/// tgt that is not there). Level-triggered and convergent: the new pod
/// starts after the tgt did, so it is never marked again.
pub fn stale_hubs(tgt_started: &HashMap<String, DateTime<Utc>>, hubs: &[HubPod]) -> Vec<HubPod> {
    hubs.iter()
        .filter(|h| tgt_started.get(&h.node).is_some_and(|t| *t > h.started))
        .cloned()
        .collect()
}

fn utc(t: &k8s_openapi::jiff::Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp(t.as_second(), t.subsec_nanosecond().max(0) as u32).unwrap_or_default()
}

/// node → when its spdk-tgt container last started (running ones only).
fn tgt_starts(pods: &[Pod], container: &str) -> HashMap<String, DateTime<Utc>> {
    let mut m = HashMap::new();
    for p in pods {
        let Some(node) = p.spec.as_ref().and_then(|s| s.node_name.clone()) else { continue };
        let started = p
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.name == container))
            .and_then(|c| c.state.as_ref())
            .and_then(|st| st.running.as_ref())
            .and_then(|r| r.started_at.as_ref())
            .map(|t| utc(&t.0));
        if let Some(t) = started {
            m.insert(node, t);
        }
    }
    m
}

fn hub_pods(pods: &[Pod]) -> Vec<HubPod> {
    pods.iter()
        .filter_map(|p| {
            Some(HubPod {
                namespace: p.namespace()?,
                share: p.labels().get("chert.us/share")?.clone(),
                node: p.spec.as_ref()?.node_name.clone()?,
                started: utc(&p.status.as_ref()?.start_time.as_ref()?.0),
            })
        })
        .collect()
}

/// One pass. Returns the shares it marked.
pub async fn pass(client: &Client, fleet: &kube::runtime::reflector::Store<FlintShare>, cfg: &Config) -> kube::Result<Vec<String>> {
    let csi = Api::<Pod>::namespaced(client.clone(), &cfg.namespace)
        .list(&ListParams::default().labels(&cfg.selector))
        .await?;
    let tgts = tgt_starts(&csi.items, &cfg.container);
    if tgts.is_empty() {
        debug!("csi-node roll watch: no running {} container under {} in {}", cfg.container, cfg.selector, cfg.namespace);
        return Ok(vec![]);
    }
    let hubs = Api::<Pod>::all(client.clone())
        .list(&ListParams::default().labels("app.kubernetes.io/managed-by=flint-lite-operator,chert.us/role=lite"))
        .await?;
    let mut marked = Vec::new();
    for h in stale_hubs(&tgts, &hub_pods(&hubs.items)) {
        let Some(share) = fleet
            .state()
            .into_iter()
            .find(|s| s.namespace().as_deref() == Some(h.namespace.as_str()) && s.name_any() == h.share)
        else {
            continue;
        };
        // Only a share the ladder has UP. Anything else is down, or on
        // its way somewhere a restart would interrupt (a hibernate
        // verification, a reprovision).
        if share.spec.lifecycle.clone().unwrap_or_default() != Lifecycle::Active || idle::state_of(&share) != IdleState::Active {
            continue;
        }
        let claim = render::names(&share).claim;
        let pvc = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &h.namespace).get_opt(&claim).await?;
        let class = pvc.and_then(|c| c.spec.and_then(|s| s.storage_class_name)).unwrap_or_default();
        if !cfg.storage_classes.iter().any(|c| *c == class) {
            continue;
        }
        // Guarded by the resourceVersion the decision was made on: the
        // store can lag, and an unguarded write over a transition the
        // ladder just made (a suspend, a hibernate) would undo it — the
        // restart ends in Active. A conflict means "decide again next pass".
        let patch = serde_json::json!({ "metadata": {
            "resourceVersion": share.resource_version(),
            "annotations": {
                (idle::ANN_IDLE_STATE): IdleState::Restarting.as_str(),
                (idle::ANN_IDLE_SINCE): Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
        }});
        match Api::<FlintShare>::namespaced(client.clone(), &h.namespace)
            .patch(&h.share, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Err(kube::Error::Api(e)) if e.code == 409 => {
                debug!("{}/{}: changed since this pass read it; deciding again next pass", h.namespace, h.share);
                continue;
            }
            r => {
                r?;
            }
        }
        info!(
            share = %format!("{}/{}", h.namespace, h.share), node = %h.node,
            "the node's {} restarted after this hub started ({} > {}): its staged volume is dead; restarting the hub",
            cfg.container, tgts[&h.node].to_rfc3339(), h.started.to_rfc3339(),
        );
        marked.push(format!("{}/{}", h.namespace, h.share));
    }
    Ok(marked)
}

pub async fn run(client: Client, fleet: kube::runtime::reflector::Store<FlintShare>, cfg: Config) {
    info!(
        "restarting hubs after an spdk-tgt restart: {} in {} ({}), classes {:?}, every {:?}",
        cfg.selector, cfg.namespace, cfg.container, cfg.storage_classes, cfg.every
    );
    loop {
        tokio::time::sleep(cfg.every).await;
        // Only the lease holder acts, as for the reconciler.
        if !crate::orchestrator_lease::is_leader() {
            continue;
        }
        if let Err(e) = pass(&client, &fleet, &cfg).await {
            warn!("csi-node roll watch: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn hub(share: &str, node: &str, started: &str) -> HubPod {
        HubPod { namespace: "ns".into(), share: share.into(), node: node.into(), started: at(started) }
    }

    /// A tgt restarted after a hub on its node started: that hub's device
    /// died with it. Hubs on other nodes, hubs started after the tgt (the
    /// replacement), and hubs on a node whose tgt is not running now are
    /// all left alone.
    #[test]
    fn only_hubs_older_than_their_nodes_tgt_are_stale() {
        let tgts = HashMap::from([
            ("n1".to_string(), at("2026-09-30T12:00:00Z")),
            ("n2".to_string(), at("2026-09-30T08:00:00Z")),
        ]);
        let hubs = vec![
            hub("old-on-n1", "n1", "2026-09-30T10:00:00Z"),
            hub("new-on-n1", "n1", "2026-09-30T12:00:05Z"),
            hub("on-n2", "n2", "2026-09-30T10:00:00Z"),
            hub("on-n3", "n3", "2026-09-30T10:00:00Z"),
        ];
        let stale: Vec<String> = stale_hubs(&tgts, &hubs).into_iter().map(|h| h.share).collect();
        assert_eq!(stale, vec!["old-on-n1"]);
    }
}
