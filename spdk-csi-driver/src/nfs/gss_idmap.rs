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
//!
//! With a directory ([`super::gss_ldap`], `FLINT_NFS_GSS_LDAP_URL`) the
//! order is: a rule here, else the directory, else the anonymous
//! identity ([`map_principal`]). A user the directory has may connect
//! though the allowlist does not name it, and a principal whose identity
//! the directory cannot tell now is refused, never anonymous ([`admit`]).

use super::gss_ldap::{LdapIdmap, Lookup};
use super::gss_principals::{Pattern, PrincipalAllowlist};

pub const ENV: &str = "FLINT_NFS_GSS_IDMAP";
pub const ANON_ENV: &str = "FLINT_NFS_GSS_ANON";
/// knfsd's `root_squash`, for every caller (AUTH_SYS and Kerberos).
pub const ROOT_SQUASH_ENV: &str = "FLINT_NFS_ROOT_SQUASH";
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
        let anon = anon_from_env();
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
        self.rule(principal).unwrap_or(&self.anon)
    }

    /// The identity of the first rule naming `principal`, if one does.
    pub fn rule(&self, principal: &str) -> Option<&Identity> {
        self.rules
            .iter()
            .find(|(p, _)| p.matches(principal))
            .map(|(_, i)| i)
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

/// [`ANON_ENV`], or nobody when it is unset or malformed.
pub fn anon_from_env() -> Identity {
    match std::env::var(ANON_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        None => NOBODY,
        Some(a) => Identity::parse(&a).unwrap_or_else(|e| {
            tracing::error!("❌ {ANON_ENV}={a:?} is malformed ({e}): using nobody (65534:65534)");
            NOBODY
        }),
    }
}

/// Who a Kerberos principal is, and on whose word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mapped {
    /// A [`ENV`] rule names it.
    Rule(Identity),
    /// The directory has it.
    Directory(Identity),
    /// Neither, or no map and no directory at all: the anonymous
    /// identity. A Kerberos call never runs without a uid: before, with no
    /// map, it carried none and the permission check evaluated nothing,
    /// so a proven principal read and wrote whatever it asked for.
    Anonymous(Identity),
    /// The directory cannot say now and has nothing cached: refuse.
    Unavailable,
}

/// Who `principal` is: a rule of `map`, else what `ldap` says, else the
/// anonymous identity (with neither configured, [`anon_from_env`]). Only user principals the directory covers are
/// looked up there; every other principal is a rule or anonymous.
pub async fn map_principal(
    principal: &str,
    map: Option<&GssIdmap>,
    ldap: Option<&LdapIdmap>,
) -> Mapped {
    if let Some(i) = map.and_then(|m| m.rule(principal)) {
        return Mapped::Rule(i.clone());
    }
    if let Some(l) = ldap.filter(|l| l.covers(principal)) {
        match l.resolve(principal).await {
            Lookup::Found(i) => return Mapped::Directory(i),
            Lookup::Unavailable => return Mapped::Unavailable,
            Lookup::NotFound => {}
        }
    }
    match (map, ldap) {
        (Some(m), _) => Mapped::Anonymous(m.anon.clone()),
        (None, Some(l)) => Mapped::Anonymous(l.anon().clone()),
        (None, None) => Mapped::Anonymous(anon_from_env()),
    }
}

/// [`map_principal`] with the process-wide map and directory.
pub async fn map_global(principal: &str) -> Mapped {
    map_principal(principal, GssIdmap::global(), LdapIdmap::global()).await
}

/// Whether `principal`, which is `mapped`, may establish a context: the
/// allowlist names it (or there is none), or the directory has it. An
/// identity the directory cannot tell now refuses it even when listed:
/// its calls would only be refused one by one.
pub fn admit(
    principal: &str,
    allowed: Option<&PrincipalAllowlist>,
    mapped: &Mapped,
) -> Result<(), String> {
    match mapped {
        Mapped::Unavailable => Err(format!(
            "{principal}: the directory cannot tell who it is now ({})",
            super::gss_ldap::URL_ENV
        )),
        Mapped::Directory(_) => Ok(()),
        _ if allowed.map_or(true, |a| a.permits(principal)) => Ok(()),
        _ => Err(format!(
            "{principal} is not in {}",
            super::gss_principals::ENV
        )),
    }
}

/// The (uid, gid) and groups a COMPOUND runs as: an AUTH_SYS caller's own
/// (`gss` is `None`), or a Kerberos caller's mapped identity. `None` when
/// it cannot be told now: the call is refused, never run as someone.
pub fn compound_identity(
    gss: Option<&Mapped>,
    unix: Option<(u32, u32)>,
    unix_gids: Vec<u32>,
) -> Option<(Option<(u32, u32)>, Vec<u32>)> {
    match gss {
        None => Some((unix, unix_gids)),
        Some(Mapped::Rule(i) | Mapped::Directory(i) | Mapped::Anonymous(i)) => {
            Some((Some((i.uid, i.gid)), i.gids.clone()))
        }
        Some(Mapped::Unavailable) => None,
    }
}

/// knfsd's `root_squash`: uid 0 becomes `anon` (gid and groups too), and
/// gid 0 is replaced, and dropped from the groups, for everyone else. A
/// root on any client is then an ordinary, unprivileged caller.
pub fn squash_root(id: (Option<(u32, u32)>, Vec<u32>), anon: &Identity) -> (Option<(u32, u32)>, Vec<u32>) {
    match id {
        (Some((0, _)), _) => (Some((anon.uid, anon.gid)), anon.gids.clone()),
        (Some((uid, gid)), gids) => (
            Some((uid, if gid == 0 { anon.gid } else { gid })),
            gids.into_iter().filter(|g| *g != 0).collect(),
        ),
        none => none,
    }
}

/// Who root becomes when [`ROOT_SQUASH_ENV`] is on (the anonymous
/// identity, [`ANON_ENV`]); `None` when it is off. Read once.
pub fn root_squash() -> Option<&'static Identity> {
    static SQUASH: std::sync::OnceLock<Option<Identity>> = std::sync::OnceLock::new();
    SQUASH
        .get_or_init(|| {
            let on = std::env::var(ROOT_SQUASH_ENV)
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false);
            if !on {
                return None;
            }
            let anon = std::env::var(ANON_ENV)
                .ok()
                .and_then(|a| Identity::parse(&a).ok())
                .unwrap_or(NOBODY);
            tracing::info!("🔐 root_squash: uid 0 is {}:{}", anon.uid, anon.gid);
            Some(anon)
        })
        .as_ref()
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

    #[tokio::test]
    async fn kerberos_callers_get_mapped_identities_and_sys_callers_keep_theirs() {
        let m = map();
        // AUTH_SYS: its own claim, map or not.
        assert_eq!(
            compound_identity(None, Some((7, 8)), vec![9]),
            Some((Some((7, 8)), vec![9]))
        );
        // Kerberos with a map: the principal's identity, never the
        // (absent) unix credential.
        let alice = map_principal("alice@FLINT.TEST", Some(&m), None).await;
        assert_eq!(
            compound_identity(Some(&alice), None, vec![]),
            Some((Some((1000, 1000)), vec![100, 200]))
        );
        let eve = map_principal("eve@ELSEWHERE", Some(&m), None).await;
        assert_eq!(eve, Mapped::Anonymous(NOBODY));
        assert_eq!(
            compound_identity(Some(&eve), None, vec![]),
            Some((Some((65534, 65534)), vec![]))
        );
        // Kerberos without a map or a directory: anonymous, never no uid
        // (no uid meant the permission check evaluated nothing).
        let unmapped = map_principal("alice@FLINT.TEST", None, None).await;
        assert_eq!(unmapped, Mapped::Anonymous(anon_from_env()));
        assert_eq!(
            compound_identity(Some(&unmapped), None, vec![]),
            Some((Some((anon_from_env().uid, anon_from_env().gid)), anon_from_env().gids))
        );
        // An identity that cannot be told is a refusal, never a uid.
        assert_eq!(compound_identity(Some(&Mapped::Unavailable), None, vec![]), None);
    }

    mod with_a_directory {
        use super::*;
        use crate::nfs::gss_ldap::fake::{entry, FakeDirectory};
        use crate::nfs::gss_ldap::{LdapConfig, LdapIdmap};
        use std::sync::Arc;

        const PEOPLE: &str = "ou=People,dc=flint,dc=test";

        fn user(name: &str, uid: &str) -> (String, Vec<crate::nfs::gss_ldap::Entry>) {
            (
                format!("(&(objectClass=posixAccount)(uid={name}))"),
                vec![entry(
                    &format!("uid={name},{PEOPLE}"),
                    &[("uid", &[name]), ("uidNumber", &[uid]), ("gidNumber", &[uid])],
                )],
            )
        }

        /// alice is 1000 in the static map and 3000 in the directory;
        /// carol is 3001 in the directory only.
        fn setup() -> (GssIdmap, Arc<FakeDirectory>, LdapIdmap) {
            let d = Arc::new(FakeDirectory::default());
            for (name, uid) in [("alice", "3000"), ("carol", "3001")] {
                let (f, e) = user(name, uid);
                d.answer(PEOPLE, &f, e);
            }
            let cfg = LdapConfig::parse(|k: &str| {
                let v = match k {
                    "FLINT_NFS_GSS_LDAP_URL" => Some("ldaps://ldap.flint.test"),
                    "FLINT_NFS_GSS_LDAP_REALMS" => Some("FLINT.TEST"),
                    "FLINT_NFS_GSS_LDAP_USER_BASE" => Some(PEOPLE),
                    "FLINT_NFS_GSS_LDAP_GROUP_MEMBERSHIP" => Some("none"),
                    _ => None,
                };
                v.map(str::to_string)
            })
            .unwrap()
            .unwrap();
            let ldap = LdapIdmap::new(cfg, Arc::new(d.clone()), NOBODY);
            (map(), d, ldap)
        }

        fn id(uid: u32) -> Identity {
            Identity { uid, gid: uid, gids: vec![] }
        }

        #[tokio::test]
        async fn a_static_rule_wins_then_the_directory_then_anonymous() {
            let (m, d, ldap) = setup();
            let at = |p: &'static str| {
                let (m, ldap) = (&m, &ldap);
                async move { map_principal(p, Some(m), Some(ldap)).await }
            };
            assert_eq!(
                at("alice@FLINT.TEST").await,
                Mapped::Rule(Identity { uid: 1000, gid: 1000, gids: vec![100, 200] }),
                "the static map overrides the directory"
            );
            assert_eq!(
                at("host/w1.flint.test@FLINT.TEST").await,
                Mapped::Rule(id(0)),
                "machine principals keep machineIdentity"
            );
            assert_eq!(d.connects(), 0, "neither went to the directory");
            // The static catch-all *@FLINT.TEST would name carol: without
            // it, the directory does.
            let no_catch_all = GssIdmap::parse("host/*@FLINT.TEST=0:0", NOBODY).unwrap();
            assert_eq!(
                map_principal("carol@FLINT.TEST", Some(&no_catch_all), Some(&ldap)).await,
                Mapped::Directory(id(3001))
            );
            assert_eq!(
                map_principal("dave@FLINT.TEST", Some(&no_catch_all), Some(&ldap)).await,
                Mapped::Anonymous(NOBODY),
                "not in the directory: anonymous, as without one"
            );
            assert_eq!(
                map_principal("svc/x@FLINT.TEST", Some(&no_catch_all), Some(&ldap)).await,
                Mapped::Anonymous(NOBODY),
                "a service principal is never looked up"
            );
            // A directory and no static map: its anonymous identity.
            assert_eq!(
                map_principal("dave@FLINT.TEST", None, Some(&ldap)).await,
                Mapped::Anonymous(NOBODY)
            );
        }

        #[tokio::test]
        async fn a_directory_that_cannot_answer_refuses_its_users_only() {
            let (_, d, ldap) = setup();
            let m = GssIdmap::parse("host/*@FLINT.TEST=0:0", NOBODY).unwrap();
            d.set_down(true);
            let carol = map_principal("carol@FLINT.TEST", Some(&m), Some(&ldap)).await;
            assert_eq!(carol, Mapped::Unavailable, "never anonymous, never unchecked");
            assert_eq!(compound_identity(Some(&carol), None, vec![]), None);
            assert_eq!(
                map_principal("host/w1@FLINT.TEST", Some(&m), Some(&ldap)).await,
                Mapped::Rule(id(0)),
                "the nodes keep working"
            );
        }

        #[tokio::test]
        async fn the_directory_admits_its_users_and_an_outage_refuses() {
            let (_, d, ldap) = setup();
            let m = GssIdmap::parse("host/*@FLINT.TEST=0:0", NOBODY).unwrap();
            let allowed = PrincipalAllowlist::parse("host/*@FLINT.TEST,nfs/*@FLINT.TEST").unwrap();
            let admit_now = |p: &'static str| {
                let (m, ldap, allowed) = (&m, &ldap, &allowed);
                async move { admit(p, Some(allowed), &map_principal(p, Some(m), Some(ldap)).await) }
            };
            assert_eq!(admit_now("carol@FLINT.TEST").await, Ok(()), "found: allowed");
            assert_eq!(
                admit_now("dave@FLINT.TEST").await,
                Err("dave@FLINT.TEST is not in FLINT_NFS_GSS_PRINCIPALS".into()),
                "not found: the static list decides"
            );
            assert_eq!(admit_now("host/w1@FLINT.TEST").await, Ok(()));
            d.set_down(true);
            assert_eq!(admit_now("carol@FLINT.TEST").await, Ok(()), "cached");
            assert!(
                admit_now("erin@FLINT.TEST").await.unwrap_err().contains("cannot tell"),
                "unknown during an outage: refused"
            );
            // Listed statically but undecidable: refused too, or its
            // calls would bounce one by one.
            let everyone = PrincipalAllowlist::parse("*@FLINT.TEST").unwrap();
            let erin = map_principal("erin@FLINT.TEST", Some(&m), Some(&ldap)).await;
            assert!(admit("erin@FLINT.TEST", Some(&everyone), &erin).is_err());
            assert!(admit("erin@FLINT.TEST", None, &erin).is_err());
        }
    }

    #[test]
    fn root_squash_makes_root_anonymous_and_strips_gid_0() {
        let anon = NOBODY;
        assert_eq!(squash_root((Some((0, 0)), vec![0, 10]), &anon), (Some((65534, 65534)), vec![]));
        assert_eq!(squash_root((Some((0, 5)), vec![]), &anon), (Some((65534, 65534)), vec![]));
        assert_eq!(squash_root((Some((7, 0)), vec![0, 10]), &anon), (Some((7, 65534)), vec![10]));
        assert_eq!(squash_root((Some((7, 8)), vec![9]), &anon), (Some((7, 8)), vec![9]), "others untouched");
        assert_eq!(squash_root((None, vec![]), &anon), (None, vec![]), "no uid stays no uid");
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
