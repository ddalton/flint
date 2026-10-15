//! Which Kerberos principals may establish an RPCSEC_GSS context
//! (`FLINT_NFS_GSS_PRINCIPALS`).
//!
//! A valid ticket proves who the caller is, not that it may use the
//! export: the server maps no principal to a uid, and a call without a uid
//! is not permission-checked, so without a list every principal of the
//! realm that can get a ticket for the server's `nfs/` key has the whole
//! export. knfsd answers this with idmapping (unmapped principals become
//! nobody) and per-export client lists; this is the first half of that:
//! only the principals listed may connect. With chart `nfs.kerberos` the
//! list is the nodes' machine principals, `host/*@REALM,nfs/*@REALM`.
//!
//! Patterns, comma separated: `<component>[/<component>...]@<REALM>`. A
//! component is literal text with `*` matching any run of characters
//! within it (`host/*`, `nfs/*.corp.example`); the number of components
//! must match, and the realm is always spelt out: a wildcard realm would
//! let a cross-realm trust in. Principals are compared case-sensitively,
//! as Kerberos compares them.

use std::fmt;

/// The env var the NFS server reads.
pub const ENV: &str = "FLINT_NFS_GSS_PRINCIPALS";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    components: Vec<String>,
    realm: String,
}

/// The principals that may establish a context. Empty refuses everyone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrincipalAllowlist {
    patterns: Vec<Pattern>,
}

impl fmt::Display for PrincipalAllowlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: Vec<String> = self
            .patterns
            .iter()
            .map(|p| format!("{}@{}", p.components.join("/"), p.realm))
            .collect();
        write!(f, "{}", s.join(","))
    }
}

/// `*` matches any run (possibly empty) of characters of `text`.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

impl PrincipalAllowlist {
    /// Parse a comma-separated list; any malformed entry is an error.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut patterns = Vec::new();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (name, realm) = entry
                .rsplit_once('@')
                .ok_or_else(|| format!("{entry:?}: no @REALM (the realm is always spelt out)"))?;
            if realm.is_empty() || realm.contains('*') {
                return Err(format!(
                    "{entry:?}: the realm must be spelt out, without '*'"
                ));
            }
            let components: Vec<String> = name.split('/').map(str::to_string).collect();
            if components.iter().any(|c| c.is_empty()) {
                return Err(format!("{entry:?}: an empty name component"));
            }
            patterns.push(Pattern {
                components,
                realm: realm.to_string(),
            });
        }
        if patterns.is_empty() {
            return Err("no principals".into());
        }
        Ok(Self { patterns })
    }

    /// From [`ENV`]: `None` when unset (every authenticated principal may
    /// connect). A malformed list refuses everyone: failing open would
    /// grant what the operator meant to restrict.
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var(ENV).ok().filter(|s| !s.trim().is_empty())?;
        Some(Self::parse(&spec).unwrap_or_else(|e| {
            tracing::error!(
                "❌ {ENV}={spec:?} is malformed ({e}): every RPCSEC_GSS principal will be REFUSED"
            );
            Self::default()
        }))
    }

    /// Whether `principal` (`name@REALM`, as the acceptor reports it) may
    /// connect.
    pub fn permits(&self, principal: &str) -> bool {
        let Some((name, realm)) = principal.rsplit_once('@') else {
            return false;
        };
        let components: Vec<&str> = name.split('/').collect();
        self.patterns.iter().any(|p| {
            p.realm == realm
                && p.components.len() == components.len()
                && p.components
                    .iter()
                    .zip(&components)
                    .all(|(pc, c)| !c.is_empty() && glob(pc, c))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_principals_of_the_realm_and_nothing_else() {
        let a = PrincipalAllowlist::parse("host/*@FLINT.TEST, nfs/*@FLINT.TEST").unwrap();
        for ok in [
            "host/w1.flint.test@FLINT.TEST",
            "nfs/kind-worker@FLINT.TEST",
        ] {
            assert!(a.permits(ok), "{ok}");
        }
        for no in [
            "alice@FLINT.TEST",         // a user
            "host/w1@OTHER.TEST",       // another realm (cross-realm trust)
            "HOST/w1@FLINT.TEST",       // case-sensitive, as Kerberos is
            "host/w1/extra@FLINT.TEST", // more components
            "host@FLINT.TEST",          // fewer
            "host/@FLINT.TEST",         // an empty host
            "krbtgt/FLINT.TEST@FLINT.TEST",
            "host/w1", // no realm
            "host/w1@flint.test",
        ] {
            assert!(!a.permits(no), "{no}");
        }
        assert_eq!(a.to_string(), "host/*@FLINT.TEST,nfs/*@FLINT.TEST");
    }

    #[test]
    fn globs_stay_within_a_component() {
        let a =
            PrincipalAllowlist::parse("nfs/*.corp.example@CORP,svc-backup@CORP,root/node-*-a@CORP")
                .unwrap();
        assert!(a.permits("nfs/w1.corp.example@CORP"));
        assert!(!a.permits("nfs/w1.corp.example.evil@CORP"));
        assert!(!a.permits("nfs/corp.example@CORP"), "the dot is literal");
        assert!(a.permits("svc-backup@CORP"));
        assert!(!a.permits("svc-backup2@CORP"));
        assert!(a.permits("root/node-1-a@CORP") && a.permits("root/node--a@CORP"));
        assert!(!a.permits("root/node-1-b@CORP"));
    }

    #[test]
    fn a_list_must_name_its_realms() {
        for bad in [
            "",
            " , ",
            "host/*",
            "host/*@",
            "host/*@*",
            "host/*@FLINT.*",
            "host//x@R",
            "/x@R",
            "x/@R",
        ] {
            assert!(PrincipalAllowlist::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_empty_list_refuses_everyone() {
        assert!(!PrincipalAllowlist::default().permits("host/w1@FLINT.TEST"));
    }

    #[test]
    fn glob_basics() {
        assert!(glob("*", "") && glob("*", "abc") && glob("a*c", "abc") && glob("a*c", "ac"));
        assert!(glob("*b*", "abc") && !glob("a*d", "abc") && glob("a**", "a"));
    }
}
