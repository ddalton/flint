//! The routing table (nfs-proxy design §6 "Where each identifier
//! lives"): workspace name → hub address, `serverId` (the FH instance
//! id, H1) and `stateidTag` (H2), plus each hub's root filehandle once a
//! backend has learned it. In a cluster the rows come from FlintShare
//! status (step 4); here they come from the proxy's config, which is
//! what the box rig and the tests use.
//!
//! [`View`] is ONE connection's view: every answer filtered by the
//! allowlist of the identity that connection proved (§7: the source
//! address in step 2, the client certificate in step 2b). A workspace
//! outside the allowlist is indistinguishable from an absent one.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use serde::Deserialize;

use super::pseudo::RootListing;
use super::route::Namespace;
use crate::nfs::v4::protocol::Nfs4FileHandle;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HubRow {
    pub name: String,
    pub address: String,
    pub server_id: u64,
    pub stateid_tag: u32,
}

/// Who a connection is, and what it may see.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityRule {
    pub name: String,
    /// CIDRs (`10.0.0.0/8`, `127.0.0.1/32`, `::1/128`).
    #[serde(default)]
    pub sources: Vec<String>,
    /// Workspace names; a trailing `*` matches a prefix.
    pub workspaces: Vec<String>,
}

fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (a, l) = s.split_once('/').unwrap_or((s, ""));
    let ip: IpAddr = a.trim().parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let len = if l.is_empty() { max } else { l.trim().parse().ok()? };
    (len <= max).then_some((ip, len))
}

fn in_cidr(ip: IpAddr, (net, len): (IpAddr, u8)) -> bool {
    // An IPv4 client seen on a dual-stack socket arrives as ::ffff:a.b.c.d.
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let m = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            u32::from(a) & m == u32::from(n) & m
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let m = if len == 0 { 0 } else { u128::MAX << (128 - len) };
            u128::from(a) & m == u128::from(n) & m
        }
        _ => false,
    }
}

fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[derive(Default)]
struct Rows {
    by_name: HashMap<String, HubRow>,
    by_id: HashMap<u64, String>,
    by_tag: HashMap<u32, u64>,
    generation: u64,
}

pub struct Table {
    rows: RwLock<Rows>,
    roots: RwLock<HashMap<u64, Nfs4FileHandle>>,
    rules: Vec<(IdentityRule, Vec<(IpAddr, u8)>)>,
}

impl Table {
    pub fn new(hubs: Vec<HubRow>, rules: Vec<IdentityRule>) -> Result<Arc<Self>, String> {
        let mut parsed = Vec::new();
        for r in rules {
            let cidrs = r
                .sources
                .iter()
                .map(|s| parse_cidr(s).ok_or_else(|| format!("identity {}: bad source {s:?}", r.name)))
                .collect::<Result<Vec<_>, _>>()?;
            parsed.push((r, cidrs));
        }
        let t = Arc::new(Table { rows: RwLock::new(Rows::default()), roots: RwLock::new(HashMap::new()), rules: parsed });
        t.replace(hubs)?;
        Ok(t)
    }

    /// Replace every row (a watch resync, or a config reload). A server
    /// id or tag claimed twice is refused whole: routing by either would
    /// be ambiguous, and a wrong hub for FREE_STATEID frees real state.
    pub fn replace(&self, hubs: Vec<HubRow>) -> Result<(), String> {
        let mut r = Rows::default();
        for h in hubs {
            if r.by_id.insert(h.server_id, h.name.clone()).is_some() {
                return Err(format!("serverId {} claimed twice", h.server_id));
            }
            if r.by_tag.insert(h.stateid_tag, h.server_id).is_some() {
                return Err(format!("stateidTag {} claimed twice", h.stateid_tag));
            }
            if r.by_name.insert(h.name.clone(), h.clone()).is_some() {
                return Err(format!("workspace {} listed twice", h.name));
            }
        }
        let mut rows = self.rows.write().unwrap();
        let changed = {
            let mut a: Vec<_> = rows.by_name.values().cloned().collect();
            let mut b: Vec<_> = r.by_name.values().cloned().collect();
            a.sort_by(|x, y| x.name.cmp(&y.name));
            b.sort_by(|x, y| x.name.cmp(&y.name));
            a != b
        };
        r.generation = if changed {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // strictly increasing, even for two changes in one second
            now.max(rows.generation + 1)
        } else {
            rows.generation
        };
        *rows = r;
        Ok(())
    }

    pub fn hub(&self, server_id: u64) -> Option<HubRow> {
        let rows = self.rows.read().unwrap();
        let name = rows.by_id.get(&server_id)?;
        rows.by_name.get(name).cloned()
    }

    pub fn set_root(&self, server_id: u64, fh: Nfs4FileHandle) {
        self.roots.write().unwrap().insert(server_id, fh);
    }

    /// The view of a connection from `peer`: the union of every rule
    /// whose sources contain it. No rule, no workspaces.
    pub fn view(self: &Arc<Self>, peer: IpAddr) -> View {
        let patterns: Vec<String> = self
            .rules
            .iter()
            .filter(|(_, cidrs)| cidrs.iter().any(|c| in_cidr(peer, *c)))
            .flat_map(|(r, _)| r.workspaces.iter().cloned())
            .collect();
        View { table: self.clone(), patterns }
    }
}

pub struct View {
    table: Arc<Table>,
    patterns: Vec<String>,
}

impl View {
    fn allowed_name(&self, name: &str) -> bool {
        self.patterns.iter().any(|p| name_matches(p, name))
    }

    /// The hub row for an allowed server id (to dial it).
    pub fn hub(&self, server_id: u64) -> Option<HubRow> {
        self.table.hub(server_id).filter(|h| self.allowed_name(&h.name))
    }

    pub fn table(&self) -> &Arc<Table> {
        &self.table
    }

    pub fn is_hub_root_known(&self, server_id: u64) -> bool {
        self.table.roots.read().unwrap().contains_key(&server_id)
    }
}

impl Namespace for View {
    fn hub_by_name(&self, name: &str) -> Option<u64> {
        if !self.allowed_name(name) {
            return None;
        }
        self.table.rows.read().unwrap().by_name.get(name).map(|h| h.server_id)
    }
    fn hub_allowed(&self, server_id: u64) -> bool {
        self.hub(server_id).is_some()
    }
    fn hub_by_tag(&self, tag: u32) -> Option<u64> {
        let id = *self.table.rows.read().unwrap().by_tag.get(&tag)?;
        self.hub_allowed(id).then_some(id)
    }
    fn is_hub_root(&self, server_id: u64, fh: &Nfs4FileHandle) -> bool {
        self.table.roots.read().unwrap().get(&server_id).is_some_and(|r| r.data == fh.data)
    }
}

impl RootListing for View {
    fn workspaces(&self) -> Vec<String> {
        self.table.rows.read().unwrap().by_name.keys().filter(|n| self.allowed_name(n)).cloned().collect()
    }
    fn generation(&self) -> u64 {
        self.table.rows.read().unwrap().generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, id: u64, tag: u32) -> HubRow {
        HubRow { name: name.into(), address: format!("10.1.0.{id}:2049"), server_id: id, stateid_tag: tag }
    }

    fn table() -> Arc<Table> {
        Table::new(
            vec![row("team-a-1", 1, 11), row("team-a-2", 2, 12), row("team-b-1", 3, 13)],
            vec![
                IdentityRule { name: "a".into(), sources: vec!["10.0.0.0/24".into()], workspaces: vec!["team-a-*".into()] },
                IdentityRule { name: "b".into(), sources: vec!["10.0.1.7".into()], workspaces: vec!["team-b-1".into()] },
            ],
        )
        .unwrap()
    }

    #[test]
    fn a_connection_sees_only_its_identitys_workspaces_on_every_path() {
        let t = table();
        let a = t.view("10.0.0.9".parse().unwrap());
        let mut ws = a.workspaces();
        ws.sort();
        assert_eq!(ws, vec!["team-a-1", "team-a-2"]);
        assert_eq!(a.hub_by_name("team-a-2"), Some(2));
        assert_eq!(a.hub_by_name("team-b-1"), None);
        assert!(a.hub_allowed(1) && !a.hub_allowed(3));
        assert_eq!((a.hub_by_tag(11), a.hub_by_tag(13)), (Some(1), None));

        let b = t.view("10.0.1.7".parse().unwrap());
        assert_eq!(b.workspaces(), vec!["team-b-1"]);
        assert!(b.hub_allowed(3) && !b.hub_allowed(1));
    }

    #[test]
    fn an_unknown_source_sees_an_empty_root() {
        let v = table().view("192.168.1.1".parse().unwrap());
        assert!(v.workspaces().is_empty());
        assert_eq!(v.hub_by_name("team-a-1"), None);
        assert!(!v.hub_allowed(1));
    }

    #[test]
    fn an_ipv4_client_on_a_dual_stack_socket_still_matches() {
        let v = table().view("::ffff:10.0.0.9".parse().unwrap());
        assert_eq!(v.hub_by_name("team-a-1"), Some(1));
    }

    #[test]
    fn a_duplicate_server_id_or_tag_is_refused_whole() {
        let t = table();
        assert!(t.replace(vec![row("x", 1, 1), row("y", 1, 2)]).is_err());
        assert!(t.replace(vec![row("x", 1, 1), row("y", 2, 1)]).is_err());
        // the old rows are still served
        assert_eq!(t.view("10.0.0.9".parse().unwrap()).hub_by_name("team-a-1"), Some(1));
    }

    #[test]
    fn the_generation_moves_only_when_the_listing_does() {
        let t = table();
        let g0 = t.view("10.0.0.9".parse().unwrap()).generation();
        t.replace(vec![row("team-a-1", 1, 11), row("team-a-2", 2, 12), row("team-b-1", 3, 13)]).unwrap();
        assert_eq!(t.view("10.0.0.9".parse().unwrap()).generation(), g0);
        t.replace(vec![row("team-a-1", 1, 11)]).unwrap();
        assert!(t.view("10.0.0.9".parse().unwrap()).generation() > g0);
    }

    #[test]
    fn cidr_edges() {
        assert!(in_cidr("10.0.0.255".parse().unwrap(), parse_cidr("10.0.0.0/24").unwrap()));
        assert!(!in_cidr("10.0.1.0".parse().unwrap(), parse_cidr("10.0.0.0/24").unwrap()));
        assert!(in_cidr("1.2.3.4".parse().unwrap(), parse_cidr("0.0.0.0/0").unwrap()));
        assert!(in_cidr("::1".parse().unwrap(), parse_cidr("::1/128").unwrap()));
        assert!(parse_cidr("10.0.0.0/33").is_none());
    }
}
