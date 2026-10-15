//! Kerberos principal → unix identity (`FLINT_NFS_GSS_IDMAP`), as knfsd's
//! idmapping does it.
//!
//! An RPCSEC_GSS call carries no uid. Without a mapping such a call is not
//! permission-checked (`authz` evaluates a uid it does not have) and files
//! it creates are the server's. With one, the authenticated principal
//! becomes a uid, gid and groups that `authz` checks and that creates
//! stamp, which is the first identity here that is not a claim: AUTH_SYS
//! uids are whatever the client says.
//!
//! Rules, comma separated, first match wins:
//! `<principal pattern>=<uid>:<gid>[:<group>;<group>...]`, the pattern as
//! in [`super::gss_principals`] (`host/*@REALM`, `alice@REALM`). A
//! principal no rule names gets the anonymous identity
//! (`FLINT_NFS_GSS_ANON`, `<uid>:<gid>`, default `65534:65534`, nobody),
//! never "no uid": knfsd maps an unknown principal to nobody, not to an
//! unchecked caller. A malformed map maps everyone to the anonymous
//! identity. Unset, calls carry no uid, as before.
//!
//! Machine principals (`host/`, `nfs/` of the nodes) are what the kernel
//! client uses for root and for lease management; knfsd maps them to root
//! (then `root_squash` applies). The chart maps them to 0:0, which with
//! this server's no-squash rule is root, as an AUTH_SYS root is.

use super::gss_principals::Pattern;

pub const ENV: &str = "FLINT_NFS_GSS_IDMAP";
pub const ANON_ENV: &str = "FLINT_NFS_GSS_ANON";
/// nobody/nogroup.
pub const NOBODY: Identity = Identity {
    uid: 65534,
    gid: 65534,
    gids: Vec::new(),
};

/// A unix identity: uid, primary gid, supplementary groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

impl Identity {
    /// `uid:gid[:g1;g2...]`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut parts = s.trim().splitn(3, ':');
        let num = |v: Option<&str>, what: &str| -> Result<u32, String> {
            v.map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{s:?}: no {what}"))?
                .parse::<u32>()
                .map_err(|e| format!("{s:?}: {what}: {e}"))
        };
        let uid = num(parts.next(), "uid")?;
        let gid = num(parts.next(), "gid")?;
        let gids = match parts.next().map(str::trim).filter(|g| !g.is_empty()) {
            None => Vec::new(),
            Some(g) => g
                .split(';')
                .map(|g| {
                    g.trim()
                        .parse::<u32>()
                        .map_err(|e| format!("{s:?}: group {g:?}: {e}"))
                })
                .collect::<Result<_, _>>()?,
        };
        // RFC 5531 caps AUTH_SYS at 16 groups; authz expects no more.
        if gids.len() > 16 {
            return Err(format!("{s:?}: more than 16 groups"));
        }
        Ok(Identity { uid, gid, gids })
    }
}

/// The map: rules in order, and who everyone else is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GssIdmap {
    rules: Vec<(Pattern, Identity)>,
    anon: Identity,
}

impl GssIdmap {
    pub fn parse(spec: &str, anon: Identity) -> Result<Self, String> {
        let mut rules = Vec::new();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (pattern, ident) = entry
                .split_once('=')
                .ok_or_else(|| format!("{entry:?}: <principal>=<uid>:<gid>[:<groups>]"))?;
            rules.push((Pattern::parse(pattern.trim())?, Identity::parse(ident)?));
        }
        Ok(GssIdmap { rules, anon })
    }

    /// From [`ENV`] and [`ANON_ENV`]; `None` when the map is unset.
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var(ENV).ok().filter(|s| !s.trim().is_empty())?;
        let anon = match std::env::var(ANON_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            None => NOBODY,
            Some(a) => Identity::parse(&a).unwrap_or_else(|e| {
                tracing::error!(
                    "❌ {ANON_ENV}={a:?} is malformed ({e}): using nobody (65534:65534)"
                );
                NOBODY
            }),
        };
        Some(Self::parse(&spec, anon.clone()).unwrap_or_else(|e| {
            tracing::error!(
                "❌ {ENV}={spec:?} is malformed ({e}): EVERY Kerberos caller is anonymous"
            );
            GssIdmap {
                rules: Vec::new(),
                anon,
            }
        }))
    }

    /// Who `principal` (`name@REALM`) is.
    pub fn map(&self, principal: &str) -> &Identity {
        self.rules
            .iter()
            .find(|(p, _)| p.matches(principal))
            .map(|(_, i)| i)
            .unwrap_or(&self.anon)
    }

    /// The process-wide map, read once.
    pub fn global() -> Option<&'static GssIdmap> {
        static MAP: std::sync::OnceLock<Option<GssIdmap>> = std::sync::OnceLock::new();
        MAP.get_or_init(|| {
            let m = GssIdmap::from_env();
            if let Some(m) = &m {
                tracing::info!(
                    "🔐 Kerberos principals map to unix identities: {} rules, others are {}:{}",
                    m.rules.len(),
                    m.anon.uid,
                    m.anon.gid
                );
            }
            m
        })
        .as_ref()
    }
}

/// The (uid, gid) and groups a COMPOUND runs as: an AUTH_SYS caller's own,
/// or, for a Kerberos caller, its principal's under `map` (none without a
/// map).
pub fn compound_identity(
    gss_principal: Option<&str>,
    unix: Option<(u32, u32)>,
    unix_gids: Vec<u32>,
    map: Option<&GssIdmap>,
) -> (Option<(u32, u32)>, Vec<u32>) {
    match (gss_principal, map) {
        (Some(p), Some(m)) => {
            let i = m.map(p);
            (Some((i.uid, i.gid)), i.gids.clone())
        }
        (Some(_), None) => (None, Vec::new()),
        (None, _) => (unix, unix_gids),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> GssIdmap {
        GssIdmap::parse(
            "host/*@FLINT.TEST=0:0, nfs/*@FLINT.TEST=0:0, alice@FLINT.TEST=1000:1000:100;200, *@FLINT.TEST=5000:5000",
            NOBODY,
        )
        .unwrap()
    }

    #[test]
    fn principals_become_their_identities_first_match_wins() {
        let m = map();
        assert_eq!(
            m.map("host/w1.flint.test@FLINT.TEST"),
            &Identity {
                uid: 0,
                gid: 0,
                gids: vec![]
            }
        );
        assert_eq!(
            m.map("alice@FLINT.TEST"),
            &Identity {
                uid: 1000,
                gid: 1000,
                gids: vec![100, 200]
            }
        );
        assert_eq!(
            m.map("bob@FLINT.TEST").uid,
            5000,
            "the realm's catch-all, after alice"
        );
        assert_eq!(
            m.map("svc/x@FLINT.TEST"),
            &NOBODY,
            "two components: not *@FLINT.TEST"
        );
        assert_eq!(
            m.map("alice@OTHER.TEST"),
            &NOBODY,
            "another realm is nobody, never alice"
        );
    }

    #[test]
    fn identities_parse_strictly() {
        assert_eq!(
            Identity::parse("7:8").unwrap(),
            Identity {
                uid: 7,
                gid: 8,
                gids: vec![]
            }
        );
        assert_eq!(Identity::parse(" 7:8:9 ").unwrap().gids, vec![9]);
        for bad in [
            "",
            "7",
            "7:",
            ":8",
            "x:8",
            "7:8:a",
            "-1:0",
            "7:8:1;;2",
            "4294967296:0",
        ] {
            assert!(Identity::parse(bad).is_err(), "{bad:?}");
        }
        let many = (0..17).map(|g| g.to_string()).collect::<Vec<_>>().join(";");
        assert!(
            Identity::parse(&format!("1:1:{many}")).is_err(),
            "17 groups"
        );
        for bad in ["alice@R", "alice@R=", "alice=1:1", "alice@*=1:1", "=1:1"] {
            assert!(GssIdmap::parse(bad, NOBODY).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn kerberos_callers_get_mapped_identities_and_sys_callers_keep_theirs() {
        let m = map();
        // AUTH_SYS: its own claim, map or not.
        assert_eq!(
            compound_identity(None, Some((7, 8)), vec![9], Some(&m)),
            (Some((7, 8)), vec![9])
        );
        // Kerberos with a map: the principal's identity, never the
        // (absent) unix credential.
        assert_eq!(
            compound_identity(Some("alice@FLINT.TEST"), None, vec![], Some(&m)),
            (Some((1000, 1000)), vec![100, 200])
        );
        assert_eq!(
            compound_identity(Some("eve@ELSEWHERE"), None, vec![], Some(&m)),
            (Some((65534, 65534)), vec![])
        );
        // Kerberos without a map: no uid, as before.
        assert_eq!(
            compound_identity(Some("alice@FLINT.TEST"), None, vec![], None),
            (None, vec![])
        );
    }

    #[test]
    fn an_empty_map_makes_everyone_anonymous() {
        let m = GssIdmap::parse(
            "",
            Identity {
                uid: 99,
                gid: 99,
                gids: vec![],
            },
        )
        .unwrap();
        assert_eq!(m.map("host/w1@FLINT.TEST").uid, 99);
    }
}
