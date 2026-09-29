//! Per-network export access — exports(5)'s `host(options)` half, as the
//! `access:` list of an export carries it:
//!
//! ```yaml
//! exports:
//!   - path: /data/exports
//!     options: [rw, sync, no_subtree_check]
//!     access:
//!       - network: 10.0.0.0/8
//!         permissions: rw
//!       - network: 0.0.0.0/0
//!         permissions: ro
//! ```
//!
//! Until 2026-09-29 the list was parsed and read nowhere: every client had
//! read-write access to every export, whatever the list said (the F70
//! class — configured, unenforced). Now:
//!
//! - a client is matched to the MOST SPECIFIC network that contains its
//!   address (longest prefix; a tie goes to the first listed), the way
//!   knfsd prefers a host entry over a network entry over a wildcard;
//! - `permissions: ro` makes that client's connection read-only: every
//!   mutating operation answers `NFS4ERR_ROFS`, exactly as an `ro` export
//!   does for everyone (`CompoundDispatcher::with_read_only`, F70);
//! - a client that matches NO entry of a non-empty list has no access to
//!   the export: PUTROOTFH, PUTPUBFH and PUTFH answer `NFS4ERR_ACCESS`, so
//!   its mount fails with EACCES and no filehandle-bearing operation can
//!   run. Session operations are still served — the session belongs to
//!   the server, the export is what is refused, as with knfsd;
//! - an EMPTY list restricts nothing: the behaviour before 2026-09-29, and
//!   what `#[serde(default)]` gives a config without an `access:` key.
//!
//! A `/0` network (`0.0.0.0/0` or `::/0`) constrains no address bit and so
//! matches EVERY peer, of either family. Every shipped config renders
//! `0.0.0.0/0`, and the IPv6 pods of a dual-stack cluster must not be
//! locked out by that spelling. Any longer prefix matches only its own
//! family, with an IPv4-mapped IPv6 peer (`::ffff:a.b.c.d`, what a
//! dual-stack listener reports for an IPv4 client) unmapped first.
//!
//! The decision is made ONCE PER CONNECTION, from the address the listener
//! accepted (`server_v4::handle_tcp_connection`), and rides in
//! `CompoundContext::peer`. In-process callers — the hub's File API, unit
//! tests — carry `PeerPolicy::default()`, "allowed, read-write"; the
//! export-wide `ro` still applies on top of it.

use std::net::IpAddr;

/// `a.b.c.d/len`, `x::y/len`, or a bare address (a host entry: the
/// full-length prefix). None for anything else, including a prefix longer
/// than the family allows.
pub(crate) fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (a, l) = s.split_once('/').unwrap_or((s, ""));
    let ip: IpAddr = a.trim().parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let len = if l.is_empty() { max } else { l.trim().parse().ok()? };
    (len <= max).then_some((ip, len))
}

/// Is `ip` inside `(net, len)`? Families must agree, after unmapping an
/// IPv4-mapped IPv6 address; a `/0` of either family matches all of that
/// family (the either-family reading of `/0` is `AccessRule::contains`).
pub(crate) fn in_cidr(ip: IpAddr, (net, len): (IpAddr, u8)) -> bool {
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

/// One `access:` entry: a network and what its clients may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRule {
    /// The network as written (`10.0.0.0/8`), for logs and errors.
    pub network: String,
    net: (IpAddr, u8),
    /// `permissions: ro`.
    pub read_only: bool,
}

impl AccessRule {
    /// None when `network` is not a CIDR or a bare address.
    pub fn new(network: &str, read_only: bool) -> Option<Self> {
        let net = parse_cidr(network)?;
        Some(Self { network: network.trim().to_string(), net, read_only })
    }

    pub fn prefix_len(&self) -> u8 {
        self.net.1
    }

    /// `/0` is "anyone", whichever family the peer is; every other prefix
    /// matches its own family only.
    pub fn contains(&self, peer: IpAddr) -> bool {
        self.net.1 == 0 || in_cidr(peer, self.net)
    }

    fn describe(&self) -> String {
        format!("{} {}", self.network, if self.read_only { "ro" } else { "rw" })
    }
}

/// An export's `access:` list, ready to decide a peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportAccess {
    rules: Vec<AccessRule>,
}

impl ExportAccess {
    /// No rules: nobody is refused, nobody is made read-only.
    pub fn allow_all() -> Self {
        Self::default()
    }

    pub fn new(rules: Vec<AccessRule>) -> Self {
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[AccessRule] {
        &self.rules
    }

    /// What `peer` may do: the most specific matching rule decides; no
    /// matching rule of a non-empty list is a denial; an empty list allows.
    pub fn decide(&self, peer: IpAddr) -> PeerPolicy {
        if self.rules.is_empty() {
            return PeerPolicy::ALLOW;
        }
        let mut best: Option<&AccessRule> = None;
        for r in &self.rules {
            // Strictly longer wins, so among equals the FIRST listed stays.
            if r.contains(peer) && best.map_or(true, |b| r.prefix_len() > b.prefix_len()) {
                best = Some(r);
            }
        }
        match best {
            None => PeerPolicy::DENIED,
            Some(r) if r.read_only => PeerPolicy::READ_ONLY,
            Some(_) => PeerPolicy::ALLOW,
        }
    }

    /// One line for the boot log: `10.0.0.0/8 rw, 0.0.0.0/0 ro`.
    pub fn describe(&self) -> String {
        if self.rules.is_empty() {
            return "no access list — every client read-write".to_string();
        }
        self.rules.iter().map(AccessRule::describe).collect::<Vec<_>>().join(", ")
    }
}

/// What one connection may do to the export. `Copy`, so it rides in every
/// COMPOUND's context for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerPolicy {
    /// Outside every `access:` network: the filehandle-introducing ops
    /// answer NFS4ERR_ACCESS, so nothing can reach a file.
    pub denied: bool,
    /// `permissions: ro`: every mutating op answers NFS4ERR_ROFS. Also set
    /// for a denied peer, as a second fence behind the first.
    pub read_only: bool,
}

impl PeerPolicy {
    /// The default: allowed, read-write. What every in-process caller has.
    pub const ALLOW: Self = Self { denied: false, read_only: false };
    pub const READ_ONLY: Self = Self { denied: false, read_only: true };
    pub const DENIED: Self = Self { denied: true, read_only: true };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn rules(v: &[(&str, bool)]) -> ExportAccess {
        ExportAccess::new(v.iter().map(|(n, ro)| AccessRule::new(n, *ro).unwrap()).collect())
    }

    #[test]
    fn cidr_parse_and_match_edges() {
        assert!(in_cidr(ip("10.0.0.255"), parse_cidr("10.0.0.0/24").unwrap()));
        assert!(!in_cidr(ip("10.0.1.0"), parse_cidr("10.0.0.0/24").unwrap()));
        assert!(in_cidr(ip("1.2.3.4"), parse_cidr("0.0.0.0/0").unwrap()));
        assert!(in_cidr(ip("::1"), parse_cidr("::1/128").unwrap()));
        // a bare address is a host entry
        assert_eq!(parse_cidr("192.168.1.100"), Some((ip("192.168.1.100"), 32)));
        assert!(parse_cidr("10.0.0.0/33").is_none());
        assert!(parse_cidr("::/129").is_none());
        assert!(parse_cidr("nonsense").is_none());
        assert!(parse_cidr("10.0.0.0/x").is_none());
        // an IPv4 client on a dual-stack listener is its IPv4 self
        assert!(in_cidr(ip("::ffff:10.1.2.3"), parse_cidr("10.0.0.0/8").unwrap()));
        // families do not cross for a real prefix
        assert!(!in_cidr(ip("fd00::1"), parse_cidr("10.0.0.0/8").unwrap()));
        assert!(!in_cidr(ip("10.1.2.3"), parse_cidr("fd00::/8").unwrap()));
    }

    /// The empty list is the pre-2026-09-29 server: nobody refused, nobody
    /// read-only. A config without `access:` deserializes to it.
    #[test]
    fn an_empty_list_allows_everyone_read_write() {
        let a = ExportAccess::allow_all();
        assert!(a.is_empty());
        for p in ["10.1.2.3", "192.168.1.1", "fd00::1", "::ffff:10.1.2.3"] {
            assert_eq!(a.decide(ip(p)), PeerPolicy::ALLOW, "{p}");
        }
    }

    /// The example config's shape: the cluster network read-write, one
    /// host read-only; the host entry is the more specific and wins for
    /// that address alone.
    #[test]
    fn the_most_specific_network_decides() {
        let a = rules(&[("10.0.0.0/8", false), ("10.0.0.7/32", true)]);
        assert_eq!(a.decide(ip("10.0.0.7")), PeerPolicy::READ_ONLY, "the host entry wins");
        assert_eq!(a.decide(ip("10.0.0.8")), PeerPolicy::ALLOW, "its neighbour is the network's");
        // order does not matter for specificity
        let b = rules(&[("10.0.0.7/32", true), ("10.0.0.0/8", false)]);
        assert_eq!(b.decide(ip("10.0.0.7")), PeerPolicy::READ_ONLY);
        assert_eq!(b.decide(ip("10.0.0.8")), PeerPolicy::ALLOW);
    }

    /// Two entries for the same network: the first listed is the answer,
    /// like exports(5), not the last and not a coin toss.
    #[test]
    fn among_equally_specific_entries_the_first_listed_wins() {
        let a = rules(&[("10.0.0.0/8", true), ("10.0.0.0/8", false)]);
        assert_eq!(a.decide(ip("10.1.1.1")), PeerPolicy::READ_ONLY);
        let b = rules(&[("10.0.0.0/8", false), ("10.0.0.0/8", true)]);
        assert_eq!(b.decide(ip("10.1.1.1")), PeerPolicy::ALLOW);
    }

    /// A peer outside every listed network has no access; the same peer
    /// against a list that names a catch-all is served by the catch-all.
    #[test]
    fn a_peer_outside_every_network_is_denied_unless_a_catch_all_exists() {
        let only_ten = rules(&[("10.0.0.0/8", false)]);
        assert_eq!(only_ten.decide(ip("192.168.1.1")), PeerPolicy::DENIED);
        assert_eq!(only_ten.decide(ip("fd00::1")), PeerPolicy::DENIED);
        assert_eq!(only_ten.decide(ip("10.1.2.3")), PeerPolicy::ALLOW);
        assert_eq!(only_ten.decide(ip("::ffff:10.1.2.3")), PeerPolicy::ALLOW, "v4-mapped is v4");

        let with_catch_all = rules(&[("10.0.0.0/8", false), ("0.0.0.0/0", true)]);
        assert_eq!(with_catch_all.decide(ip("192.168.1.1")), PeerPolicy::READ_ONLY);
        assert_eq!(with_catch_all.decide(ip("10.1.2.3")), PeerPolicy::ALLOW, "/8 beats /0");
    }

    /// `0.0.0.0/0` — what every shipped config renders — means anyone,
    /// IPv6 peers included; so does `::/0`. A real prefix stays in its
    /// family.
    #[test]
    fn a_zero_prefix_matches_either_family() {
        let v4_any = rules(&[("0.0.0.0/0", false)]);
        assert_eq!(v4_any.decide(ip("fd00::1")), PeerPolicy::ALLOW);
        assert_eq!(v4_any.decide(ip("10.1.2.3")), PeerPolicy::ALLOW);
        let v6_any = rules(&[("::/0", true)]);
        assert_eq!(v6_any.decide(ip("10.1.2.3")), PeerPolicy::READ_ONLY);
        assert_eq!(v6_any.decide(ip("fd00::1")), PeerPolicy::READ_ONLY);
        let v6_site = rules(&[("fd00::/8", false)]);
        assert_eq!(v6_site.decide(ip("10.1.2.3")), PeerPolicy::DENIED);
        assert_eq!(v6_site.decide(ip("fd00::1")), PeerPolicy::ALLOW);
    }

    #[test]
    fn describe_names_every_rule_in_order() {
        assert_eq!(
            rules(&[("10.0.0.0/8", false), ("0.0.0.0/0", true)]).describe(),
            "10.0.0.0/8 rw, 0.0.0.0/0 ro"
        );
        assert!(ExportAccess::allow_all().describe().contains("every client read-write"));
        assert!(AccessRule::new("10.0.0.0/33", false).is_none());
    }
}
