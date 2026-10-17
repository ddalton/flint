//! Client nodes of Kerberos FlintShare hubs (docs/plans/flint-lite-kerberos-plan.md,
//! L3): keep the host's hosts file mapping each hub's ClusterIP to its
//! server name (`status.kerberos`), so a PV can name the hub by the name
//! its principal carries. The kubelet mounts with the host's resolver,
//! which does not resolve `*.svc.<clusterDomain>`, and `rpc.gssd` names
//! the server by the reverse lookup of the address it connects to; a
//! wake can give the hub a new ClusterIP, so the line follows the status.
//!
//! Run by `flint-nfs-client-identity --krb5-hosts` (flint-nfs-client-chart
//! `kerberos.enabled`). One watch on FlintShares per node, not a poll.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::lite_operator::crd::FlintShare;
use crate::nfs_krb5::{ensure_marked_hosts, LITE_HOSTS_MARKER};

/// IP to server name for every Kerberos share with a ClusterIP. Two names
/// on one IP cannot both be a hub's; the first (by namespace/name) wins.
pub fn desired<'a>(shares: impl IntoIterator<Item = &'a FlintShare>) -> BTreeMap<String, String> {
    let mut found: Vec<(String, String, String)> = shares
        .into_iter()
        .filter_map(|s| {
            let k = s.status.as_ref()?.kerberos.as_ref()?;
            let ip = k.cluster_ip.as_deref()?.trim();
            let name = k.server_name.trim();
            if ip.is_empty() || name.is_empty() || ip.parse::<std::net::IpAddr>().is_err() || name.contains(char::is_whitespace) {
                return None;
            }
            let key = format!("{}/{}", s.metadata.namespace.as_deref().unwrap_or(""), s.metadata.name.as_deref().unwrap_or(""));
            Some((key, ip.to_string(), name.to_string()))
        })
        .collect();
    found.sort();
    let mut out = BTreeMap::new();
    for (_, ip, name) in found {
        out.entry(ip).or_insert(name);
    }
    out
}

/// Watch FlintShares and keep `hosts_file`'s marked lines equal to
/// [`desired`]. Writes `ready` to `status_file` once the first full list
/// is in and applied (the readiness probe); never writes from an empty,
/// not-yet-listed store, which would drop every line on a restart.
pub async fn run(client: kube::Client, hosts_file: PathBuf, status_file: PathBuf, every: Duration) -> anyhow::Result<()> {
    use futures::StreamExt;
    use kube::runtime::{reflector, watcher, WatchStreamExt};
    let api = kube::Api::<FlintShare>::all(client);
    let (reader, writer) = reflector::store();
    let stream = reflector(writer, watcher(api, watcher::Config::default()).default_backoff()).applied_objects();
    tokio::spawn(async move {
        futures::pin_mut!(stream);
        while let Some(ev) = stream.next().await {
            if let Err(e) = ev {
                tracing::warn!("FlintShare watch: {e}");
            }
        }
    });
    reader.wait_until_ready().await?;
    if let Some(dir) = status_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut last: Option<BTreeMap<String, String>> = None;
    loop {
        let shares = reader.state();
        let want = desired(shares.iter().map(|s| s.as_ref()));
        if last.as_ref() != Some(&want) {
            match ensure_marked_hosts(&hosts_file, LITE_HOSTS_MARKER, &want) {
                Ok(changed) => {
                    if changed {
                        tracing::info!("{}: {} Kerberos hub line(s) {:?}", hosts_file.display(), want.len(), want);
                    }
                    std::fs::write(&status_file, "ready\n")?;
                    last = Some(want);
                }
                Err(e) => {
                    tracing::warn!("{}: {e}", hosts_file.display());
                    std::fs::write(&status_file, format!("not ready\n{e}\n"))?;
                }
            }
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lite_operator::crd::{FlintShareStatus, KerberosStatus};

    fn share(ns: &str, name: &str, k: Option<(&str, Option<&str>)>) -> FlintShare {
        let spec = serde_json::from_value(serde_json::json!({"persistence": {"size": "1Gi"}})).unwrap();
        let mut s = FlintShare::new(name, spec);
        s.metadata.namespace = Some(ns.into());
        s.status = Some(FlintShareStatus {
            kerberos: k.map(|(n, ip)| KerberosStatus {
                server_name: n.into(),
                principals: vec![],
                cluster_ip: ip.map(String::from),
            }),
            ..Default::default()
        });
        s
    }

    #[test]
    fn only_kerberos_hubs_with_an_address_get_a_line() {
        let shares = [
            share("a", "ws1", Some(("ws1.a.svc.cluster.local", Some("10.96.0.10")))),
            share("a", "sys", None),
            share("a", "parked", Some(("parked.a.svc.cluster.local", None))),
            share("a", "headless", Some(("h.a.svc.cluster.local", Some("None")))),
            share("b", "ws2", Some(("ws2.b.svc.cluster.local", Some("fd00::7")))),
        ];
        let got = desired(shares.iter());
        assert_eq!(
            got,
            BTreeMap::from([
                ("10.96.0.10".to_string(), "ws1.a.svc.cluster.local".to_string()),
                ("fd00::7".to_string(), "ws2.b.svc.cluster.local".to_string()),
            ])
        );
    }

    #[test]
    fn one_ip_gets_one_name_whatever_the_order() {
        let a = share("a", "x", Some(("x.a.svc.cluster.local", Some("10.96.0.10"))));
        let b = share("b", "y", Some(("y.b.svc.cluster.local", Some("10.96.0.10"))));
        assert_eq!(desired([&a, &b]), desired([&b, &a]));
        assert_eq!(desired([&b, &a])["10.96.0.10"], "x.a.svc.cluster.local");
    }

    #[test]
    fn the_hosts_file_keeps_everything_else() {
        let hosts = "127.0.0.1 localhost\n10.0.0.9 other # flint-nfs-krb5\n10.96.0.1 old.svc # flint-lite-krb5\n";
        let want = desired([&share("a", "ws1", Some(("ws1.a.svc.cluster.local", Some("10.96.0.10"))))]);
        let next = crate::nfs_krb5::marked_hosts_with(hosts, LITE_HOSTS_MARKER, &want);
        assert_eq!(
            next,
            "127.0.0.1 localhost\n10.0.0.9 other # flint-nfs-krb5\n10.96.0.10 ws1.a.svc.cluster.local # flint-lite-krb5\n"
        );
    }
}
