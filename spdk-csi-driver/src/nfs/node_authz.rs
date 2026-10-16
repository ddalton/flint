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
//!
//! **On a pNFS MDS** one server holds many volumes, so the node set alone
//! would let any node attached to one volume into all of them. There the
//! MDS keeps [`VolumeNodes`] as well (fed by [`watch_pnfs`]): per volume,
//! the nodes it is attached to. The node set above is their union, and
//! the dispatcher refuses a filehandle inside a volume whose nodes do not
//! include the session's client (see [`VolumeNodes`]).

use std::collections::{HashMap, HashSet};
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

/// Per volume, the hostnames of the nodes it is attached to (pNFS MDS).
/// A volume is a top-level name under the MDS export: the PV's
/// `pnfs.chert.us/volume-file`, a directory (dir mode) or a file (legacy
/// file mode). The export root itself is no volume and every attached
/// node may use it; every name under it is a volume, and only that
/// volume's nodes may reach it (a name that is no volume reaches none).
#[derive(Debug, Default)]
pub struct VolumeNodes {
    volumes: RwLock<Option<HashMap<String, HashSet<String>>>>,
}

impl VolumeNodes {
    /// Replace the map (from fresh PersistentVolume and VolumeAttachment
    /// lists).
    pub fn set(&self, volumes: HashMap<String, HashSet<String>>) {
        *self.volumes.write().unwrap() = Some(volumes);
    }

    /// May the client made by `client_principal` reach `volume`? Only a
    /// machine principal of a node `volume` is attached to: a client is
    /// a node's (EXCHANGE_ID is refused to anything else), and the users
    /// riding its session reach what the node does.
    pub fn may_reach(&self, volume: &str, client_principal: &[u8]) -> bool {
        let Caller::Machine(host) = caller(client_principal) else {
            return false;
        };
        self.volumes
            .read()
            .unwrap()
            .as_ref()
            .and_then(|v| v.get(volume))
            .is_some_and(|hosts| hosts.contains(&host))
    }
}

/// The volume a path relative to the MDS export is in: its first
/// component, or `None` for the export root.
pub fn volume_of(relative: &std::path::Path) -> Option<String> {
    match relative.components().next()? {
        std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
        _ => None,
    }
}

/// The per-volume map, when this process is an MDS with [`ENV`] on
/// ([`enable_volumes`] was called); `None` otherwise.
pub fn volumes() -> Option<&'static VolumeNodes> {
    VOLUMES.get()
}

static VOLUMES: std::sync::OnceLock<VolumeNodes> = std::sync::OnceLock::new();

/// Turn the per-volume gate on (the MDS, before it serves). Empty until
/// [`watch_pnfs`]'s first lists: every volume refused.
pub fn enable_volumes() -> &'static VolumeNodes {
    VOLUMES.get_or_init(VolumeNodes::default)
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

/// One VolumeAttachment, as [`pnfs_volume_hosts`] needs it.
pub struct Attachment<'a> {
    pub pv: &'a str,
    pub node: &'a str,
    pub attached: bool,
}

/// Fold the lists into the per-volume map: each pNFS PV (`pv` → its
/// volume name) gets the principal hostnames of the nodes it is attached
/// to. `hostname` maps a Node name to its `Hostname` address when the
/// Node has one. Every pNFS volume has an entry, empty when attached
/// nowhere.
pub fn pnfs_volume_hosts(
    pvs: &HashMap<String, String>,
    attachments: &[Attachment<'_>],
    hostname: impl Fn(&str) -> Option<String>,
    domain: Option<&str>,
) -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> =
        pvs.values().map(|v| (v.clone(), HashSet::new())).collect();
    for a in attachments.iter().filter(|a| a.attached) {
        if let Some(volume) = pvs.get(a.pv) {
            let host = hostname(a.node).unwrap_or_else(|| a.node.to_string());
            out.entry(volume.clone()).or_default().insert(principal_host(&host, domain));
        }
    }
    out
}

/// The pNFS servers' [`watch`]: keep `volumes` (the MDS's; a data server
/// has none) equal to, per pNFS volume (a PV with
/// `pnfs.chert.us/volume-file`), the nodes it is attached to, and `nodes`
/// to their union, re-listing on every PersistentVolume,
/// VolumeAttachment or Node change. Volumes of every MDS shard are kept;
/// a shard holds only its own, so the others' names are never looked up
/// here. Runs until the process ends.
pub async fn watch_pnfs(nodes: &'static AttachedNodes, volumes: Option<&'static VolumeNodes>, domain: Option<String>) {
    use futures::StreamExt;
    use k8s_openapi::api::core::v1::{Node, PersistentVolume};
    use k8s_openapi::api::storage::v1::VolumeAttachment;
    use kube::api::{Api, ListParams};
    use kube::runtime::watcher;
    use crate::pnfs_csi::ctx_keys;

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
    let pv_api: Api<PersistentVolume> = Api::all(client.clone());
    let vas: Api<VolumeAttachment> = Api::all(client.clone());
    let node_api: Api<Node> = Api::all(client.clone());
    let resync = |domain: Option<String>| {
        let (pv_api, vas, node_api) = (pv_api.clone(), vas.clone(), node_api.clone());
        async move {
            let pvs: HashMap<String, String> = pv_api
                .list(&ListParams::default())
                .await?
                .items
                .into_iter()
                .filter_map(|pv| {
                    let name = pv.metadata.name?;
                    let attrs = pv.spec?.csi?.volume_attributes?;
                    attrs.contains_key(ctx_keys::MDS_IP).then_some(())?;
                    let volume = attrs.get(ctx_keys::VOLUME_FILE).filter(|v| !v.is_empty())?.clone();
                    Some((name, volume))
                })
                .collect();
            let list = vas.list(&ListParams::default()).await?.items;
            let attachments: Vec<Attachment<'_>> = list
                .iter()
                .filter_map(|va| {
                    Some(Attachment {
                        pv: va.spec.source.persistent_volume_name.as_deref()?,
                        node: &va.spec.node_name,
                        attached: va.status.as_ref().is_some_and(|s| s.attached),
                    })
                })
                .collect();
            let mut hostnames = HashMap::new();
            for a in attachments.iter().filter(|a| a.attached && pvs.contains_key(a.pv)) {
                if hostnames.contains_key(a.node) {
                    continue;
                }
                let hostname = node_api
                    .get_opt(a.node)
                    .await?
                    .and_then(|n| n.status)
                    .and_then(|s| s.addresses)
                    .and_then(|a| a.into_iter().find(|a| a.type_ == "Hostname"))
                    .map(|a| a.address);
                hostnames.insert(a.node.to_string(), hostname);
            }
            Ok::<_, kube::Error>(pnfs_volume_hosts(
                &pvs,
                &attachments,
                |n| hostnames.get(n).cloned().flatten(),
                domain.as_deref(),
            ))
        }
    };
    let mut events = futures::stream::select_all([
        watcher(pv_api.clone(), watcher::Config::default()).map(|_| ()).boxed(),
        watcher(vas.clone(), watcher::Config::default()).map(|_| ()).boxed(),
        watcher(node_api.clone(), watcher::Config::default()).map(|_| ()).boxed(),
    ]);
    let mut last: Option<HashMap<String, HashSet<String>>> = None;
    loop {
        match resync(domain.clone()).await {
            Ok(map) if last.as_ref() != Some(&map) => {
                let mut attached: Vec<_> = map.iter().filter(|(_, h)| !h.is_empty()).collect();
                attached.sort_by(|a, b| a.0.cmp(b.0));
                tracing::info!(
                    "🔐 {ENV}: {} pNFS volume(s), attached: {:?}",
                    map.len(),
                    attached
                );
                // Union first, then the map: a node that just gained a
                // volume may make its session before the map shows it,
                // and is refused the volume until it does; never the
                // reverse (a volume admitting a node whose sessions are
                // still refused is harmless either way).
                nodes.set(map.values().flatten().cloned().collect());
                if let Some(volumes) = volumes {
                    volumes.set(map.clone());
                }
                last = Some(map);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("{ENV}: listing PersistentVolumes or VolumeAttachments: {e}"),
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

    #[test]
    fn a_volume_admits_only_its_own_nodes() {
        let v = VolumeNodes::default();
        assert!(!v.may_reach("pvc-a", b"gss:host/w1@CORP"), "before the first list every volume is refused");
        v.set(HashMap::from([
            ("pvc-a".to_string(), HashSet::from(["w1".to_string()])),
            ("pvc-b".to_string(), HashSet::from(["w2".to_string()])),
        ]));
        assert!(v.may_reach("pvc-a", b"gss:host/w1@CORP"));
        assert!(v.may_reach("pvc-a", b"gss:nfs/W1@CORP"));
        assert!(!v.may_reach("pvc-b", b"gss:host/w1@CORP"), "w1 is attached to pvc-a only");
        assert!(!v.may_reach("pvc-c", b"gss:host/w1@CORP"), "no such volume");
        assert!(!v.may_reach("pvc-a", b"gss:bob@CORP"), "a user's client is no node's");
        assert!(!v.may_reach("pvc-a", b"sys"));
    }

    #[test]
    fn a_path_is_in_the_volume_of_its_first_component() {
        use std::path::Path;
        assert_eq!(volume_of(Path::new("")), None);
        assert_eq!(volume_of(Path::new("pvc-a")), Some("pvc-a".into()));
        assert_eq!(volume_of(Path::new("pvc-a/d/f")), Some("pvc-a".into()));
    }

    #[test]
    fn volumes_fold_from_pvs_and_attachments() {
        let pvs = HashMap::from([
            ("pv-1".to_string(), "pvc-a".to_string()),
            ("pv-2".to_string(), "pvc-b".to_string()),
        ]);
        let atts = [
            Attachment { pv: "pv-1", node: "n1", attached: true },
            Attachment { pv: "pv-1", node: "n2", attached: false },
            Attachment { pv: "pv-9", node: "n3", attached: true },
        ];
        let map = pnfs_volume_hosts(&pvs, &atts, |n| (n == "n1").then(|| "Worker-1".to_string()), Some("corp"));
        assert_eq!(map["pvc-a"], HashSet::from(["worker-1.corp".to_string()]));
        assert!(map["pvc-b"].is_empty(), "attached nowhere");
        assert_eq!(map.len(), 2, "a VolumeAttachment of another driver's PV adds nothing");
    }
}
