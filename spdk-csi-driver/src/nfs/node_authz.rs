//! Only the nodes a volume is attached to may use its NFS server
//! (`FLINT_NFS_ATTACHED_NODES_ONLY`, chart `nfs.kerberos.attachedNodesOnly`).
//!
//! An RWX volume's server is reachable from every pod network peer, and a
//! node's Kerberos machine key (`host/<node>`, `nfs/<node>`) opens every
//! volume that admits machine principals. Kubernetes knows better: a
//! volume is attached to the nodes in its VolumeAttachments. This module
//! holds that set (fed by [`watch`]) and decides, per operation:
//!
//! - **EXCHANGE_ID and CREATE_SESSION** (making a client or a session):
//!   only a machine principal (`host/`, `nfs/`, `root/` + a hostname) of
//!   an attached node. The Linux client does state management with its
//!   machine credential; a user principal, or an AUTH_SYS caller (no node
//!   identity), cannot make clients here.
//! - **SEQUENCE and BIND_CONN_TO_SESSION** (using a session): the
//!   session's client must still belong to an attached node, so a detach
//!   revokes the node's clients at their next call (NFS4ERR_BADSESSION;
//!   its next EXCHANGE_ID is refused); and a machine principal riding the
//!   session must itself be an attached node's. A user principal may use
//!   an attached node's session: that is how users' calls arrive.
//!
//! Until the first attachment list arrives the set is empty and every
//! node is refused: fail closed. Off (the default), everything is allowed.

use std::collections::HashSet;
use std::sync::RwLock;

pub const ENV: &str = "FLINT_NFS_ATTACHED_NODES_ONLY";
/// Appended to node hostnames without a dot, as KerberosNodeKeytab does.
pub const DOMAIN_ENV: &str = "FLINT_NFS_NODE_DOMAIN";
/// The PersistentVolume whose VolumeAttachments count.
pub const PV_ENV: &str = "FLINT_NFS_PV_NAME";

/// Machine principals' services: a client host's own key.
const MACHINE_SERVICES: [&str; 3] = ["host", "nfs", "root"];

/// The attached nodes' hostnames (as their machine principals spell them).
#[derive(Debug, Default)]
pub struct AttachedNodes {
    hosts: RwLock<Option<HashSet<String>>>,
}

/// What a caller is, from the RPC-level principal (`gss:<name>@<REALM>`
/// for Kerberos, anything else for AUTH_SYS/NONE).
#[derive(Debug, PartialEq, Eq)]
pub enum Caller<'a> {
    /// A node's own key: its host, lower case.
    Machine(String),
    /// A Kerberos user (or service) principal.
    User(&'a str),
    /// No Kerberos: no node identity.
    Unauthenticated,
}

pub fn caller(principal: &[u8]) -> Caller<'_> {
    let Some(name) = std::str::from_utf8(principal).ok().and_then(|p| p.strip_prefix("gss:")) else {
        return Caller::Unauthenticated;
    };
    let local = name.rsplit_once('@').map(|(n, _)| n).unwrap_or(name);
    match local.split_once('/') {
        Some((svc, host)) if MACHINE_SERVICES.contains(&svc) && !host.is_empty() && !host.contains('/') => {
            Caller::Machine(host.to_ascii_lowercase())
        }
        _ => Caller::User(name),
    }
}

/// The hostname a node's machine principals carry: `domain` appended when
/// it has no dot, lower case (KerberosNodeKeytab's rule).
pub fn principal_host(hostname: &str, domain: Option<&str>) -> String {
    let h = hostname.to_ascii_lowercase();
    match domain.map(|d| d.trim_matches('.')).filter(|d| !d.is_empty()) {
        Some(d) if !h.contains('.') => format!("{h}.{}", d.to_ascii_lowercase()),
        _ => h,
    }
}

impl AttachedNodes {
    /// Replace the set (from a fresh VolumeAttachment list).
    pub fn set(&self, hosts: HashSet<String>) {
        *self.hosts.write().unwrap() = Some(hosts);
    }

    fn attached(&self, host: &str) -> bool {
        self.hosts.read().unwrap().as_ref().is_some_and(|h| h.contains(host))
    }

    /// EXCHANGE_ID, CREATE_SESSION: may this caller make a client or a
    /// session?
    pub fn may_create(&self, principal: &[u8]) -> bool {
        matches!(caller(principal), Caller::Machine(h) if self.attached(&h))
    }

    /// SEQUENCE, BIND_CONN_TO_SESSION: may this caller use a session of
    /// the client made by `client_principal`?
    pub fn may_use(&self, principal: &[u8], client_principal: &[u8]) -> bool {
        let client_ok = matches!(caller(client_principal), Caller::Machine(h) if self.attached(&h));
        client_ok
            && match caller(principal) {
                Caller::Machine(h) => self.attached(&h),
                Caller::User(_) => true,
                Caller::Unauthenticated => false,
            }
    }
}

/// The process-wide set when [`ENV`] is on; `None` (everything allowed)
/// when it is off.
pub fn global() -> Option<&'static AttachedNodes> {
    static NODES: std::sync::OnceLock<Option<AttachedNodes>> = std::sync::OnceLock::new();
    NODES
        .get_or_init(|| {
            let on = std::env::var(ENV)
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false);
            on.then(AttachedNodes::default)
        })
        .as_ref()
}

/// Keep [`global`]'s set equal to the hostnames of the nodes `pv` is
/// attached to (VolumeAttachments with `status.attached`), re-listing on
/// every VolumeAttachment or Node change. Runs until the process ends.
pub async fn watch(nodes: &'static AttachedNodes, pv: String, domain: Option<String>) {
    use futures::StreamExt;
    use k8s_openapi::api::core::v1::Node;
    use k8s_openapi::api::storage::v1::VolumeAttachment;
    use kube::api::{Api, ListParams};
    use kube::runtime::watcher;

    // The NFS server's first TLS client: without a process-level provider
    // `Client::try_default` panics ("Could not automatically determine the
    // process-level CryptoProvider"), the task dies, and every node is
    // refused forever. See `crate::install_crypto_provider`.
    crate::install_crypto_provider();
    let client = loop {
        match kube::Client::try_default().await {
            Ok(c) => break c,
            Err(e) => {
                tracing::error!("❌ {ENV}: no Kubernetes API ({e}); every node is refused until it answers");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };
    let vas: Api<VolumeAttachment> = Api::all(client.clone());
    let node_api: Api<Node> = Api::all(client.clone());
    let resync = |vas: Api<VolumeAttachment>, node_api: Api<Node>, pv: String, domain: Option<String>| async move {
        let list = vas.list(&ListParams::default()).await?;
        let mut hosts = HashSet::new();
        for va in list.items {
            let mine = va.spec.source.persistent_volume_name.as_deref() == Some(pv.as_str());
            let attached = va.status.as_ref().is_some_and(|s| s.attached);
            if !(mine && attached) {
                continue;
            }
            let node = node_api.get_opt(&va.spec.node_name).await?;
            let hostname = node
                .as_ref()
                .and_then(|n| n.status.as_ref())
                .and_then(|s| s.addresses.as_ref())
                .and_then(|a| a.iter().find(|a| a.type_ == "Hostname"))
                .map(|a| a.address.clone())
                .unwrap_or_else(|| va.spec.node_name.clone());
            hosts.insert(principal_host(&hostname, domain.as_deref()));
        }
        Ok::<_, kube::Error>(hosts)
    };
    let mut events = futures::stream::select(
        watcher(vas.clone(), watcher::Config::default()).map(|_| ()).boxed(),
        watcher(node_api.clone(), watcher::Config::default()).map(|_| ()).boxed(),
    );
    let mut last: Option<HashSet<String>> = None;
    loop {
        match resync(vas.clone(), node_api.clone(), pv.clone(), domain.clone()).await {
            Ok(hosts) if last.as_ref() != Some(&hosts) => {
                tracing::info!("🔐 {ENV}: volume {pv} serves nodes {:?}", hosts);
                nodes.set(hosts.clone());
                last = Some(hosts);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("{ENV}: listing VolumeAttachments: {e}"),
        }
        if events.next().await.is_none() {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(hosts: &[&str]) -> AttachedNodes {
        let n = AttachedNodes::default();
        n.set(hosts.iter().map(|h| h.to_string()).collect());
        n
    }

    #[test]
    fn callers_are_machines_users_or_unauthenticated() {
        assert_eq!(caller(b"gss:host/W1.corp@CORP"), Caller::Machine("w1.corp".into()));
        assert_eq!(caller(b"gss:nfs/w1@CORP"), Caller::Machine("w1".into()));
        assert_eq!(caller(b"gss:root/w1@CORP"), Caller::Machine("w1".into()));
        assert_eq!(caller(b"gss:bob@CORP"), Caller::User("bob@CORP"));
        assert_eq!(caller(b"gss:HTTP/w1@CORP"), Caller::User("HTTP/w1@CORP"), "not a machine service");
        assert_eq!(caller(b"gss:host/a/b@CORP"), Caller::User("host/a/b@CORP"));
        assert_eq!(caller(b"gss:host/@CORP"), Caller::User("host/@CORP"));
        assert_eq!(caller(b"\x00\x00\x00\x01sys-cred"), Caller::Unauthenticated);
        assert_eq!(caller(b""), Caller::Unauthenticated);
    }

    #[test]
    fn only_attached_nodes_make_clients_and_sessions() {
        let n = nodes(&["w1.corp"]);
        assert!(n.may_create(b"gss:host/w1.corp@CORP"));
        assert!(n.may_create(b"gss:nfs/W1.CORP@CORP"), "hostnames compare case-insensitively");
        assert!(!n.may_create(b"gss:host/w2.corp@CORP"), "another node's key");
        assert!(!n.may_create(b"gss:bob@CORP"), "a user cannot make a client");
        assert!(!n.may_create(b"sys"), "no node identity");
    }

    #[test]
    fn sessions_follow_the_attachment() {
        let n = nodes(&["w1"]);
        let w1 = b"gss:host/w1@CORP";
        assert!(n.may_use(w1, w1));
        assert!(n.may_use(b"gss:bob@CORP", w1), "users ride an attached node's session");
        assert!(!n.may_use(b"gss:host/w2@CORP", w1), "another node's key on w1's session");
        assert!(!n.may_use(b"sys", w1), "AUTH_SYS on a Kerberos node's session");
        assert!(!n.may_use(b"gss:bob@CORP", b"gss:host/w2@CORP"), "w2's client is not attached");
        // Detached: w1's own calls stop.
        n.set(HashSet::new());
        assert!(!n.may_use(w1, w1));
        assert!(!n.may_use(b"gss:bob@CORP", w1));
    }

    #[test]
    fn before_the_first_list_every_node_is_refused() {
        let n = AttachedNodes::default();
        assert!(!n.may_create(b"gss:host/w1@CORP"));
        assert!(!n.may_use(b"gss:host/w1@CORP", b"gss:host/w1@CORP"));
    }

    #[test]
    fn node_hostnames_take_the_domain_as_node_keytabs_do() {
        assert_eq!(principal_host("Worker-1", Some("corp.example.")), "worker-1.corp.example");
        assert_eq!(principal_host("w1.a.b", Some("corp.example")), "w1.a.b");
        assert_eq!(principal_host("kind-worker", None), "kind-worker");
    }
}
