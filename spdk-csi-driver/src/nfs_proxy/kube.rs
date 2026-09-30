//! The proxy in a cluster (nfs-proxy design §6): the routing table is
//! derived from a FlintShare watch and rebuilt from scratch on restart —
//! nothing about hubs is persisted — and a refused hub is woken by
//! stamping `chert.us/requested-at`, the hub-gateway's `/wake` contract.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Patch, PatchParams};
use kube::runtime::{reflector, watcher, WatchStreamExt};
use kube::{Api, Client, ResourceExt};
use tracing::{info, warn};

use super::server::Waker;
use super::table::{HubRow, Table};
use crate::lite_gateway::resolve::ShareView;
use crate::lite_operator::crd::{FlintShare, Phase};
use crate::lite_operator::idle::ANN_REQUESTED_AT;

/// The rows for a fleet, and why each share left out was left out.
///
/// A share is routable once its hub has reported a `serverId` (the FH
/// instance id, H1) and the operator has assigned its `stateidTag`
/// (H2), and while it has an address. Parked shares stay listed: the
/// proxy is what wakes them. A name, serverId or tag claimed twice keeps
/// the FIRST share (by namespace/name, so the choice is stable across
/// rebuilds) and drops the rest loudly — refusing the whole table would
/// take every workspace down for one bad row.
///
/// `last_address` carries each share's address across the moments the
/// operator withholds `status.address` — every ladder transition, a WAKE
/// included, because a direct consumer must not mount an address with
/// nothing behind it. Behind the proxy that withholding is exactly wrong:
/// a share without a row is an absent workspace, so a client holding its
/// handles got ESTALE and a lookup got ENOENT in the very window the proxy
/// should answer DELAY and wait (step5-kind.sh). The address of a hub's
/// headless Service does not change, so the last one is still right.
pub fn rows_of(shares: &[Arc<FlintShare>], last_address: &mut std::collections::HashMap<String, String>) -> (Vec<HubRow>, Vec<String>) {
    let live: std::collections::HashSet<String> =
        shares.iter().map(|s| format!("{}/{}", s.namespace().unwrap_or_default(), s.name_any())).collect();
    last_address.retain(|k, _| live.contains(k));
    let mut views: Vec<(ShareView, Option<u32>)> = shares
        .iter()
        .map(|s| (ShareView::of(s), s.status.as_ref().and_then(|st| st.stateid_tag)))
        .collect();
    views.sort_by(|a, b| (&a.0.namespace, &a.0.name).cmp(&(&b.0.namespace, &b.0.name)));
    let mut rows: Vec<HubRow> = Vec::new();
    let mut skipped = Vec::new();
    let mut names = std::collections::HashMap::new();
    let mut ids = std::collections::HashMap::new();
    let mut tags = std::collections::HashMap::new();
    for (v, tag) in views {
        let who = format!("{}/{}", v.namespace, v.name);
        if v.deleting {
            last_address.remove(&who);
            continue;
        }
        if let Some(a) = &v.address {
            last_address.insert(who.clone(), a.clone());
        }
        let Some(address) = last_address.get(&who).cloned() else {
            skipped.push(format!("{who}: no address yet"));
            continue;
        };
        let Some(server_id) = v.server_id.as_deref().and_then(|s| s.trim().parse::<u64>().ok()) else {
            skipped.push(format!("{who}: no serverId yet"));
            continue;
        };
        let Some(tag) = tag else {
            skipped.push(format!("{who}: no stateidTag yet (is the operator's --nfs-proxy on?)"));
            continue;
        };
        let name = v.volume_id.clone().unwrap_or_else(|| v.name.clone());
        if let Some(first) = names.get(&name) {
            skipped.push(format!("{who}: workspace name {name:?} already served by {first}"));
            continue;
        }
        if let Some(first) = ids.get(&server_id) {
            skipped.push(format!("{who}: serverId {server_id} already claimed by {first}"));
            continue;
        }
        if let Some(first) = tags.get(&tag) {
            skipped.push(format!("{who}: stateidTag {tag} already claimed by {first}"));
            continue;
        }
        names.insert(name.clone(), who.clone());
        ids.insert(server_id, who.clone());
        tags.insert(tag, who.clone());
        rows.push(HubRow {
            name,
            address,
            server_id,
            stateid_tag: tag,
            share: Some((v.namespace.clone(), v.name.clone())),
            // An admin's Suspended is not the proxy's to override: the
            // client waits (DELAY) as a hard mount of it would, and no
            // wake is stamped.
            wakeable: v.phase != Some(Phase::Suspended),
        });
    }
    (rows, skipped)
}

/// Keep `table` in step with the fleet. Rebuilt from the reflector's
/// store every `every`: O(fleet) and allocation-light, and a store read
/// can never be out of step with itself the way an event mapper can.
pub async fn watch(client: Client, namespace: Option<String>, table: Arc<Table>, every: Duration) -> anyhow::Result<()> {
    let api: Api<FlintShare> = match &namespace {
        Some(ns) => Api::namespaced(client, ns),
        None => Api::all(client),
    };
    // Fail fast on RBAC, as the gateway does, rather than serve an empty
    // root forever.
    api.list(&kube::api::ListParams::default().limit(1)).await?;
    let (store, writer) = reflector::store::<FlintShare>();
    tokio::spawn(async move {
        watcher(api, watcher::Config::default())
            .default_backoff()
            .reflect(writer)
            .applied_objects()
            .for_each(|_| async {})
            .await;
    });
    store.wait_until_ready().await?;
    let mut last_skipped: Vec<String> = Vec::new();
    let mut last_address = std::collections::HashMap::new();
    loop {
        let (rows, skipped) = rows_of(&store.state(), &mut last_address);
        if skipped != last_skipped {
            for s in &skipped {
                warn!("share not routable: {s}");
            }
            last_skipped = skipped;
        }
        if let Err(e) = table.replace(rows) {
            // rows_of already dropped every duplicate; this is a bug.
            warn!("routing table refused the fleet: {e}");
        }
        tokio::time::sleep(every).await;
    }
}

/// Stamps `chert.us/requested-at` on the share (a merge patch on one
/// annotation, exactly as the hub-gateway does, so neither becomes an
/// SSA owner of it).
pub struct KubeWaker {
    pub client: Client,
}

impl Waker for KubeWaker {
    fn wake(&self, hub: &HubRow) {
        let Some((ns, name)) = hub.share.clone() else { return };
        if !hub.wakeable {
            info!("{ns}/{name} is Suspended by an admin; not waking it");
            return;
        }
        let api: Api<FlintShare> = Api::namespaced(self.client.clone(), &ns);
        tokio::spawn(async move {
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let patch = serde_json::json!({ "metadata": { "annotations": { ANN_REQUESTED_AT: now } } });
            match api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await {
                Ok(_) => info!("{ns}/{name}: wake requested"),
                Err(e) => warn!("{ns}/{name}: wake request failed: {e}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lite_operator::crd::FlintShareStatus;

    fn share(ns: &str, name: &str, server_id: Option<&str>, tag: Option<u32>, phase: Phase) -> Arc<FlintShare> {
        let mut s: FlintShare = serde_json::from_value(serde_json::json!({
            "apiVersion": "chert.us/v1alpha1",
            "kind": "FlintShare",
            "metadata": { "name": name, "namespace": ns },
            "spec": { "persistence": { "size": "1Gi" } }
        }))
        .unwrap();
        s.status = Some(FlintShareStatus {
            phase: Some(phase),
            address: Some(format!("{name}.{ns}.svc.cluster.local:2049")),
            server_id: server_id.map(String::from),
            stateid_tag: tag,
            ..Default::default()
        });
        Arc::new(s)
    }

    #[test]
    fn a_share_is_routable_once_it_has_a_server_id_and_a_tag() {
        let fleet = vec![
            share("t", "ready", Some("11"), Some(1), Phase::Ready),
            share("t", "parked", Some("12"), Some(2), Phase::Hibernated),
            share("t", "no-id", None, Some(3), Phase::Pending),
            share("t", "no-tag", Some("14"), None, Phase::Ready),
        ];
        let (rows, skipped) = rows_of(&fleet, &mut Default::default());
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["parked", "ready"], "parked shares stay listed: the proxy wakes them");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert_eq!(rows[0].share, Some(("t".into(), "parked".into())));
    }

    #[test]
    fn a_collision_drops_the_later_share_not_the_table() {
        let fleet = vec![
            share("b", "x", Some("21"), Some(9), Phase::Ready),
            share("a", "x", Some("22"), Some(8), Phase::Ready), // same workspace name
            share("a", "y", Some("21"), Some(7), Phase::Ready), // same serverId as b/x
            share("a", "z", Some("23"), Some(9), Phase::Ready), // same tag as b/x
            share("a", "ok", Some("24"), Some(6), Phase::Ready),
        ];
        let (rows, skipped) = rows_of(&fleet, &mut Default::default());
        let mut got: Vec<_> = rows.iter().map(|r| format!("{}={}", r.name, r.server_id)).collect();
        got.sort();
        // a/x is first by namespace, so it keeps "x"; b/x then collides on
        // the name, and a/y (serverId 21) was first to claim 21 — so b/x
        // is dropped, and a/z's tag 9 is free once b/x is gone.
        assert_eq!(got, vec!["ok=24", "x=22", "y=21", "z=23"]);
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        let t = Table::new(rows, vec![]);
        assert!(t.is_ok(), "the table accepts what rows_of produced");
    }

    /// The operator withholds `status.address` during every ladder
    /// transition, a wake included. The row must stay (at the last address)
    /// or the workspace vanishes mid-wake: ESTALE for held handles, ENOENT
    /// for a lookup (step5-kind.sh, run 1). A share never seen with an
    /// address is still left out, and a deleted one is forgotten.
    #[test]
    fn a_share_keeps_its_row_while_the_operator_withholds_its_address() {
        let mut last = std::collections::HashMap::new();
        let up = share("t", "w", Some("41"), Some(4), Phase::Ready);
        assert_eq!(rows_of(&[up.clone()], &mut last).0.len(), 1);
        let mut waking = (*up).clone();
        waking.status.as_mut().unwrap().address = None;
        waking.status.as_mut().unwrap().phase = Some(Phase::Starting);
        let (rows, skipped) = rows_of(&[Arc::new(waking.clone())], &mut last);
        assert_eq!(rows.len(), 1, "mid-wake the workspace must stay routable: {skipped:?}");
        assert_eq!(rows[0].address, "w.t.svc.cluster.local:2049");
        let (rows, _) = rows_of(&[Arc::new(waking)], &mut Default::default());
        assert!(rows.is_empty(), "never seen with an address: nothing to dial");
        assert!(rows_of(&[], &mut last).0.is_empty());
        assert!(last.is_empty(), "a share that is gone is forgotten");
    }

    #[test]
    fn an_admin_suspended_share_is_listed_but_not_woken() {
        let (rows, _) = rows_of(&[share("t", "s", Some("31"), Some(5), Phase::Suspended)], &mut Default::default());
        assert!(!rows[0].wakeable);
    }
}
