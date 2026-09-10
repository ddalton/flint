//! Joining a host and a port when the host might be IPv6.
//!
//! `format!("{host}:{port}")` is correct until the host is an IPv6
//! literal, at which point it produces `2001:db8::1:2049` — which is
//! not merely ugly, it is ambiguous: a parser taking the last colon as
//! the separator reads the host as `2001:db8:` and a parser taking the
//! first reads the port as `db8::1:2049`. RFC 3986 §3.2.2 settles it
//! with brackets, and RFC 2732 required them for exactly this reason.
//!
//! Three call sites in this crate needed the same three lines and only
//! one of them had them, so they live here once:
//!
//! * the NFS listener's bind address, where `::` must become `[::]:2049`
//!   before `TcpListener::bind` sees it;
//! * `lite_operator::reconcile::address_of`, which publishes
//!   `status.address` for consumers in other clusters to mount — the CRD
//!   validates a user-supplied `advertiseAddress` for brackets
//!   (`crd.rs`) but nothing validated the DERIVED path, so a
//!   LoadBalancer handing back an IPv6 ingress address published a
//!   malformed one;
//! * `lite_operator::hubstatus::poll_raw`, which had the check inline.
//!
//! # What counts as "needs brackets"
//!
//! A host needs brackets iff it contains a colon AND is not already
//! bracketed. That is deliberately a syntactic test rather than
//! `parse::<Ipv6Addr>()`: a scoped literal (`fe80::1%eth0`) is not a
//! bare `Ipv6Addr` but still needs the brackets, and a DNS name can
//! never contain a colon, so the test cannot misfire on one.

/// Join a host and a port, bracketing the host if it is an IPv6
/// literal. Already-bracketed hosts pass through unchanged, so this is
/// idempotent and safe to apply to input that may already be correct.
pub fn join(host: &str, port: impl std::fmt::Display) -> String {
    if needs_brackets(host) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Is this host an unbracketed IPv6 literal?
pub fn needs_brackets(host: &str) -> bool {
    host.contains(':') && !host.starts_with('[')
}

/// Does this string already carry a `:port` suffix?
///
/// Bracket-aware, and that is the whole point: the last colon of a BARE
/// IPv6 literal belongs to the address, so `"fd00::1".rsplit_once(':')`
/// hands back `("fd00:", "1")` — and `1` parses as a port. A naive
/// check therefore concludes the address is already complete and the
/// real port is silently dropped, producing an endpoint that names a
/// host and no service.
pub fn has_port(s: &str) -> bool {
    let tail = match s.rfind(']') {
        // Bracketed: only what follows the bracket can be a port.
        Some(b) => &s[b + 1..],
        // Unbracketed with more than one colon is a bare IPv6 literal;
        // every colon is part of the address.
        None if s.matches(':').count() > 1 => return false,
        None => s,
    };
    tail.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok())
}

/// Does this address (host or `host:port`) name the IPv6 wildcard —
/// `::`, `[::]`, or either with a port? Used to report whether a hub is
/// listening dual-stack, which on Linux (`net.ipv6.bindv6only=0`, the
/// default) means one socket serving both families.
pub fn is_ipv6_wildcard(addr: &str) -> bool {
    let h = addr.trim();
    let h = h.strip_prefix('[').map_or(h, |r| r.split(']').next().unwrap_or(r));
    h == "::"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_and_names_are_untouched() {
        assert_eq!(join("10.0.4.7", 2049), "10.0.4.7:2049");
        assert_eq!(join("0.0.0.0", 2049), "0.0.0.0:2049");
        assert_eq!(join("hub.flint.svc.cluster.local", 2049), "hub.flint.svc.cluster.local:2049");
        assert_eq!(join("a.elb.amazonaws.com", 2049), "a.elb.amazonaws.com:2049");
    }

    #[test]
    fn bare_ipv6_gets_brackets() {
        assert_eq!(join("2001:db8::1", 2049), "[2001:db8::1]:2049");
        assert_eq!(join("::", 2049), "[::]:2049");
        assert_eq!(join("::1", 8080), "[::1]:8080");
        assert_eq!(join("fd00:dead:beef::2", 2049), "[fd00:dead:beef::2]:2049");
    }

    /// The whole point: what came out before this module existed did
    /// not round-trip through a socket-address parser, and what comes
    /// out now does. This is the positive control — it fails if `join`
    /// stops bracketing.
    #[test]
    fn the_joined_form_parses_back_and_the_naive_form_does_not() {
        use std::net::SocketAddr;
        let naive = format!("{}:{}", "2001:db8::1", 2049);
        assert!(
            naive.parse::<SocketAddr>().is_err(),
            "the unbracketed form must be REJECTED, else this module is pointless: {naive}"
        );
        let joined = join("2001:db8::1", 2049);
        let sa: SocketAddr = joined.parse().expect("bracketed form must parse");
        assert_eq!(sa.port(), 2049);
        assert!(sa.is_ipv6());
    }

    /// Idempotent: a user-supplied `advertiseAddress` already carries
    /// brackets (the CRD's CEL rule demands them), and re-joining its
    /// host must not double them.
    #[test]
    fn already_bracketed_is_left_alone() {
        assert_eq!(join("[2001:db8::1]", 2049), "[2001:db8::1]:2049");
        assert!(!needs_brackets("[::1]"));
    }

    /// A scoped link-local is not a bare `Ipv6Addr`, so a
    /// parse-based test would have missed it.
    #[test]
    fn scoped_link_local_still_gets_brackets() {
        assert!("fe80::1%eth0".parse::<std::net::Ipv6Addr>().is_err());
        assert_eq!(join("fe80::1%eth0", 2049), "[fe80::1%eth0]:2049");
    }

    /// The bug this replaced: `rsplit_once(':')` on a bare IPv6
    /// literal finds a "port" that parses, so the caller kept the
    /// address as-is and the real port never got appended.
    #[test]
    fn has_port_is_not_fooled_by_an_ipv6_literal() {
        // The naive check, shown failing on the same input.
        let naive = |s: &str| s.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok());
        assert!(naive("fd00::1"), "the naive check calls this complete...");
        assert!(!has_port("fd00::1"), "...and it is not: there is no port here");

        assert!(!has_port("::1"));
        assert!(!has_port("2001:db8::1"));
        assert!(!has_port("[fd00::1]"));
        assert!(!has_port("10.0.0.1"));
        assert!(!has_port("host.example"));

        assert!(has_port("[fd00::1]:2049"));
        assert!(has_port("10.0.0.1:2049"));
        assert!(has_port("host.example:2049"));
    }

    #[test]
    fn wildcard_detection() {
        assert!(is_ipv6_wildcard("::"));
        assert!(is_ipv6_wildcard("[::]"));
        assert!(is_ipv6_wildcard("[::]:2049"));
        assert!(!is_ipv6_wildcard("0.0.0.0"));
        assert!(!is_ipv6_wildcard("::1"));
        assert!(!is_ipv6_wildcard("2001:db8::1"));
    }
}
