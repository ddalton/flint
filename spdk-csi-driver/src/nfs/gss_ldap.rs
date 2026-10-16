//! Kerberos principal → unix identity from an LDAP directory
//! (`FLINT_NFS_GSS_LDAP_URL`), RFC 2307 attributes, cached: what idmapd
//! and SSSD give knfsd, without either.
//!
//! The static map ([`super::gss_idmap`], `FLINT_NFS_GSS_IDMAP`) names a
//! few principals; a directory names everyone. The order is fixed: a
//! static rule wins (the nodes' machine principals are static rules),
//! else the directory, else the anonymous identity. Only USER principals
//! of the listed realms (`name@REALM`, one component) are looked up;
//! `host/`, `nfs/` and every other service principal never are.
//!
//! A principal is found by `uid=<name>` under the user base(s) (or, with
//! `FLINT_NFS_GSS_LDAP_PRINCIPAL_ATTR`, by `<attr>=<name@REALM>`, e.g.
//! `krbPrincipalName`), restricted by the user filter (default
//! `(objectClass=posixAccount)`). The entry must name the principal
//! EXACTLY: LDAP matches `uid` case-insensitively, Kerberos does not, and
//! `Alice@R` is not `alice@R`. Its `uidNumber` and `gidNumber` are the
//! uid and primary gid; its groups are the `posixGroup`s under the group
//! base(s) that list it by `memberUid` (and, optionally, by `member`, its
//! DN: RFC 2307bis), by their `gidNumber`.
//!
//! Failing closed. The directory never makes root: an entry with
//! `uidNumber` or `gidNumber` 0 is refused, and group 0 dropped, unless
//! `FLINT_NFS_GSS_LDAP_ALLOW_ROOT` (root_squash still applies after). Two
//! entries for one principal are refused, not guessed between. When the
//! directory cannot answer (unreachable, TLS, bind, timeout) the answer is
//! [`Lookup::Unavailable`], never the anonymous identity and never "no
//! uid": a cached answer is served while its TTL runs, and after that the
//! caller is refused until the directory is back. A malformed
//! configuration makes every lookup unavailable.
//!
//! Transport: `ldaps://`, or `ldap://` upgraded with StartTLS; plain
//! `ldap://` only with `FLINT_NFS_GSS_LDAP_INSECURE` (test rigs). rustls,
//! the server verified against `FLINT_NFS_GSS_LDAP_CA_FILE` (else the
//! system roots). Each lookup is one connection (bind, the user search,
//! the group search, unbind), bounded by `FLINT_NFS_GSS_LDAP_TIMEOUT`
//! from the moment it queues, and at most
//! `FLINT_NFS_GSS_LDAP_MAX_CONCURRENT` run at once, so a slow directory
//! holds up the callers it must answer and nothing else.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use super::gss_idmap::Identity;

/// `ldaps://host[:port]` or `ldap://host[:port]` (StartTLS).
pub const URL_ENV: &str = "FLINT_NFS_GSS_LDAP_URL";
/// `true`: `ldap://` without StartTLS. Test rigs only.
pub const INSECURE_ENV: &str = "FLINT_NFS_GSS_LDAP_INSECURE";
/// The realms whose user principals are looked up, comma separated.
pub const REALMS_ENV: &str = "FLINT_NFS_GSS_LDAP_REALMS";
/// Where users are, `;` separated (DNs contain commas).
pub const USER_BASE_ENV: &str = "FLINT_NFS_GSS_LDAP_USER_BASE";
/// Where groups are, `;` separated (default: the user bases).
pub const GROUP_BASE_ENV: &str = "FLINT_NFS_GSS_LDAP_GROUP_BASE";
/// Which entries are users (default `(objectClass=posixAccount)`).
pub const USER_FILTER_ENV: &str = "FLINT_NFS_GSS_LDAP_USER_FILTER";
/// The attribute holding the whole principal (e.g. `krbPrincipalName`);
/// unset: `uid` is the principal's name.
pub const PRINCIPAL_ATTR_ENV: &str = "FLINT_NFS_GSS_LDAP_PRINCIPAL_ATTR";
/// How groups list members: `memberUid` (RFC 2307, the default),
/// `member` (RFC 2307bis, the user's DN), both comma separated, or `none`.
pub const GROUP_MEMBERSHIP_ENV: &str = "FLINT_NFS_GSS_LDAP_GROUP_MEMBERSHIP";
/// The DN to bind as (unset: an anonymous bind).
pub const BIND_DN_ENV: &str = "FLINT_NFS_GSS_LDAP_BIND_DN";
/// Its password: a Secret's key in the server pod, never a literal.
pub const BIND_PASSWORD_ENV: &str = "FLINT_NFS_GSS_LDAP_BIND_PASSWORD";
/// PEM CA bundle the directory's certificate must chain to.
pub const CA_FILE_ENV: &str = "FLINT_NFS_GSS_LDAP_CA_FILE";
/// Seconds a found principal is served from the cache (default 300).
pub const TTL_ENV: &str = "FLINT_NFS_GSS_LDAP_TTL";
/// Seconds a principal the directory does not have is (default 60).
pub const NEGATIVE_TTL_ENV: &str = "FLINT_NFS_GSS_LDAP_NEGATIVE_TTL";
/// Seconds a lookup may take, queueing included (default 5).
pub const TIMEOUT_ENV: &str = "FLINT_NFS_GSS_LDAP_TIMEOUT";
/// Lookups in flight at once (default 4).
pub const MAX_CONCURRENT_ENV: &str = "FLINT_NFS_GSS_LDAP_MAX_CONCURRENT";
/// `true`: the directory may make uid 0 or gid 0.
pub const ALLOW_ROOT_ENV: &str = "FLINT_NFS_GSS_LDAP_ALLOW_ROOT";

/// The settings the controller passes on to every NFS server as they are:
/// `NFS_GSS_LDAP_<S>` in the controller is `FLINT_NFS_GSS_LDAP_<S>` in the
/// server. The bind password and the CA bundle are not among them: they
/// come from a Secret and a mounted file.
pub const PASSED_ON: &[&str] = &[
    "URL",
    "INSECURE",
    "REALMS",
    "USER_BASE",
    "GROUP_BASE",
    "USER_FILTER",
    "PRINCIPAL_ATTR",
    "GROUP_MEMBERSHIP",
    "BIND_DN",
    "TTL",
    "NEGATIVE_TTL",
    "TIMEOUT",
    "MAX_CONCURRENT",
    "ALLOW_ROOT",
];

/// A search returning more entries than this is an error, not an answer.
const USER_SIZE_LIMIT: i32 = 16;
const GROUP_SIZE_LIMIT: i32 = 1024;
/// Principals cached at most; past it expired entries go, then nothing new
/// is cached (lookups still work, uncached).
const MAX_CACHED: usize = 100_000;

/// A password that never reaches a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// How groups name their members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Membership {
    /// `memberUid=<the user's uid>` (RFC 2307 posixGroup).
    pub member_uid: bool,
    /// `member=<the user's DN>` (RFC 2307bis).
    pub member_dn: bool,
}

/// The directory settings, from [`URL_ENV`] and the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LdapConfig {
    pub url: String,
    /// `ldap://` upgraded with StartTLS (`ldaps://` is TLS from the start).
    pub starttls: bool,
    /// `ldap://` with no TLS at all.
    pub insecure: bool,
    pub realms: Vec<String>,
    pub user_bases: Vec<String>,
    pub group_bases: Vec<String>,
    pub user_filter: String,
    pub principal_attr: Option<String>,
    pub membership: Membership,
    pub bind: Option<(String, Secret)>,
    pub ca_file: Option<String>,
    pub ttl: Duration,
    pub negative_ttl: Duration,
    pub timeout: Duration,
    pub max_concurrent: usize,
    pub allow_root: bool,
}

/// `true`/`false` and the usual spellings.
fn flag(v: Option<String>, what: &str) -> Result<bool, String> {
    match v.as_deref().map(|v| v.trim().to_ascii_lowercase()) {
        None => Ok(false),
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(false),
        Some(v) => Err(format!("{what}={v:?}: not true or false")),
    }
}

fn seconds(v: Option<String>, default: u64, what: &str) -> Result<Duration, String> {
    match v {
        None => Ok(Duration::from_secs(default)),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| format!("{what}={v:?}: {e}")),
    }
}

/// An attribute name: letters, digits, `-` (RFC 4512 `descr`).
fn attr_name(v: &str) -> bool {
    v.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl LdapConfig {
    /// From `get` (the env, or a test's map): `Ok(None)` when [`URL_ENV`]
    /// is unset, an error for anything malformed.
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let get = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let Some(url) = get(URL_ENV).map(|u| u.trim().to_string()) else {
            return Ok(None);
        };
        let insecure = flag(get(INSECURE_ENV), INSECURE_ENV)?;
        let host = |rest: &str| !rest.split(['/', '?']).next().unwrap_or("").is_empty();
        let starttls = if let Some(rest) = url.strip_prefix("ldaps://") {
            if !host(rest) {
                return Err(format!("{URL_ENV}={url:?}: no host"));
            }
            false
        } else if let Some(rest) = url.strip_prefix("ldap://") {
            if !host(rest) {
                return Err(format!("{URL_ENV}={url:?}: no host"));
            }
            !insecure
        } else {
            return Err(format!("{URL_ENV}={url:?}: ldaps:// or ldap:// (StartTLS)"));
        };
        let list = |k: &str, sep: char| -> Vec<String> {
            get(k)
                .map(|v| {
                    v.split(sep)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let realms = list(REALMS_ENV, ',');
        if realms.is_empty() {
            return Err(format!("{REALMS_ENV}: no realm (whose principals to look up)"));
        }
        if let Some(r) = realms.iter().find(|r| r.contains('*') || r.contains('@')) {
            return Err(format!("{REALMS_ENV}: {r:?}: a realm is spelt out"));
        }
        let user_bases = list(USER_BASE_ENV, ';');
        if user_bases.is_empty() {
            return Err(format!("{USER_BASE_ENV}: no base DN"));
        }
        let group_bases = match list(GROUP_BASE_ENV, ';') {
            g if g.is_empty() => user_bases.clone(),
            g => g,
        };
        let user_filter = get(USER_FILTER_ENV)
            .map(|f| f.trim().to_string())
            .unwrap_or_else(|| "(objectClass=posixAccount)".into());
        if !(user_filter.starts_with('(') && user_filter.ends_with(')')) {
            return Err(format!("{USER_FILTER_ENV}={user_filter:?}: a filter is parenthesised"));
        }
        let principal_attr = get(PRINCIPAL_ATTR_ENV).map(|a| a.trim().to_string());
        if let Some(a) = principal_attr.as_deref().filter(|a| !attr_name(a)) {
            return Err(format!("{PRINCIPAL_ATTR_ENV}={a:?}: not an attribute name"));
        }
        let mut membership = Membership::default();
        match get(GROUP_MEMBERSHIP_ENV) {
            None => membership.member_uid = true,
            Some(m) => {
                for kind in m.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                    match kind {
                        "memberUid" => membership.member_uid = true,
                        "member" => membership.member_dn = true,
                        "none" => {}
                        other => {
                            return Err(format!(
                                "{GROUP_MEMBERSHIP_ENV}: {other:?}: memberUid, member or none"
                            ))
                        }
                    }
                }
            }
        }
        let bind = match (get(BIND_DN_ENV), get(BIND_PASSWORD_ENV)) {
            (None, None) => None,
            (Some(dn), Some(pw)) => Some((dn.trim().to_string(), Secret(pw))),
            (Some(_), None) => return Err(format!("{BIND_DN_ENV} without {BIND_PASSWORD_ENV}")),
            (None, Some(_)) => return Err(format!("{BIND_PASSWORD_ENV} without {BIND_DN_ENV}")),
        };
        let max_concurrent = match get(MAX_CONCURRENT_ENV) {
            None => 4,
            Some(v) => v
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{MAX_CONCURRENT_ENV}={v:?}: a positive number"))?,
        };
        let timeout = seconds(get(TIMEOUT_ENV), 5, TIMEOUT_ENV)?;
        if timeout.is_zero() {
            return Err(format!("{TIMEOUT_ENV}: 0 would refuse every lookup"));
        }
        Ok(Some(LdapConfig {
            url,
            starttls,
            insecure,
            realms,
            user_bases,
            group_bases,
            user_filter,
            principal_attr,
            membership,
            bind,
            ca_file: get(CA_FILE_ENV).map(|f| f.trim().to_string()),
            ttl: seconds(get(TTL_ENV), 300, TTL_ENV)?,
            negative_ttl: seconds(get(NEGATIVE_TTL_ENV), 60, NEGATIVE_TTL_ENV)?,
            timeout,
            max_concurrent,
            allow_root: flag(get(ALLOW_ROOT_ENV), ALLOW_ROOT_ENV)?,
        }))
    }

    /// The name to look up, when `principal` is a user of one of the
    /// realms: `alice` for `alice@CORP`, nothing for `host/w1@CORP`.
    fn user_name<'a>(&self, principal: &'a str) -> Option<&'a str> {
        let (name, realm) = principal.rsplit_once('@')?;
        (!name.is_empty() && !name.contains('/') && self.realms.iter().any(|r| r == realm))
            .then_some(name)
    }

    /// The user search for `principal` (`name` its user name).
    fn user_search(&self, principal: &str, name: &str) -> String {
        match &self.principal_attr {
            Some(attr) => format!("(&{}({attr}={}))", self.user_filter, ldap_escape(principal)),
            None => format!("(&{}(uid={}))", self.user_filter, ldap_escape(name)),
        }
    }

    /// The group search for a user whose posix name is `uid` and entry
    /// `dn`; `None` when groups are not looked up.
    fn group_search(&self, uid: Option<&str>, dn: &str) -> Option<String> {
        let mut any = String::new();
        if let Some(uid) = uid.filter(|_| self.membership.member_uid) {
            any.push_str(&format!("(memberUid={})", ldap_escape(uid)));
        }
        if self.membership.member_dn && !dn.is_empty() {
            any.push_str(&format!("(member={})", ldap_escape(dn)));
        }
        (!any.is_empty()).then(|| format!("(&(objectClass=posixGroup)(|{any}))"))
    }
}

/// RFC 4515 assertion-value escaping: `\ * ( )` and NUL.
fn ldap_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\5c"),
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\0' => out.push_str("\\00"),
            c => out.push(c),
        }
    }
    out
}

/// One directory entry; attribute names lowercased (LDAP compares them
/// without case).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub dn: String,
    pub attrs: HashMap<String, Vec<String>>,
}

impl Entry {
    fn get(&self, attr: &str) -> &[String] {
        self.attrs
            .get(&attr.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// A single-valued numeric attribute (`uidNumber`, `gidNumber`).
    fn number(&self, attr: &str) -> Result<u32, String> {
        match self.get(attr) {
            [] => Err(format!("{}: no {attr}", self.dn)),
            [v] => v
                .trim()
                .parse::<u32>()
                .map_err(|e| format!("{}: {attr} {v:?}: {e}", self.dn)),
            many => Err(format!("{}: {} values of {attr}", self.dn, many.len())),
        }
    }
}

/// A connection to the directory, bound.
#[async_trait::async_trait]
pub trait Searcher: Send {
    /// The entries under `base` (subtree) matching `filter`. An error is
    /// the directory not answering, never "no entries".
    async fn search(
        &mut self,
        base: &str,
        filter: &str,
        attrs: &[&str],
        size_limit: i32,
    ) -> Result<Vec<Entry>, String>;
    /// Unbind; best effort.
    async fn close(&mut self) {}
}

/// Where lookups go: the LDAP server, or a test's fake.
#[async_trait::async_trait]
pub trait Directory: Send + Sync {
    async fn connect(&self) -> Result<Box<dyn Searcher>, String>;
}

/// What a lookup found.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Found(Identity),
    /// No such user.
    NotFound,
    /// An entry the server will not use (uid 0, two entries, no
    /// uidNumber...): as good as none, and logged.
    Rejected(String),
}

/// Look `principal` (user `name`) up, on one connection.
async fn lookup(
    dir: &dyn Directory,
    cfg: &LdapConfig,
    principal: &str,
    name: &str,
) -> Result<Outcome, String> {
    let mut s = dir.connect().await?;
    let r = lookup_on(s.as_mut(), cfg, principal, name).await;
    s.close().await;
    r
}

async fn lookup_on(
    s: &mut dyn Searcher,
    cfg: &LdapConfig,
    principal: &str,
    name: &str,
) -> Result<Outcome, String> {
    let filter = cfg.user_search(principal, name);
    let mut attrs = vec!["uid", "uidNumber", "gidNumber"];
    if let Some(a) = &cfg.principal_attr {
        attrs.push(a.as_str());
    }
    let mut users = Vec::new();
    for base in &cfg.user_bases {
        users.extend(s.search(base, &filter, &attrs, USER_SIZE_LIMIT).await?);
    }
    let user = match users.len() {
        0 => return Ok(Outcome::NotFound),
        1 => users.pop().expect("one"),
        n => return Ok(Outcome::Rejected(format!("{n} entries match {filter}"))),
    };
    let (uid, gid, posix_name) = match user_identity(cfg, principal, name, &user) {
        Ok(u) => u,
        Err(why) => return Ok(Outcome::Rejected(why)),
    };
    let mut groups = Vec::new();
    if let Some(filter) = cfg.group_search(posix_name.as_deref(), &user.dn) {
        for base in &cfg.group_bases {
            groups.extend(s.search(base, &filter, &["gidNumber"], GROUP_SIZE_LIMIT).await?);
        }
    }
    Ok(Outcome::Found(Identity {
        uid,
        gid,
        gids: group_ids(&groups, gid, cfg.allow_root),
    }))
}

/// uid, primary gid and posix name (for `memberUid`) of `user`, the entry
/// found for `principal`: an error is why it is refused.
fn user_identity(
    cfg: &LdapConfig,
    principal: &str,
    name: &str,
    user: &Entry,
) -> Result<(u32, u32, Option<String>), String> {
    // The search matched without case; Kerberos names do not.
    let posix_name = match &cfg.principal_attr {
        Some(attr) => {
            if !user.get(attr).iter().any(|v| v == principal) {
                return Err(format!("{}: {attr} is not exactly {principal}", user.dn));
            }
            user.get("uid").first().cloned()
        }
        None => {
            if !user.get("uid").iter().any(|v| v == name) {
                return Err(format!("{}: uid is not exactly {name}", user.dn));
            }
            Some(name.to_string())
        }
    };
    let uid = user.number("uidNumber")?;
    let gid = user.number("gidNumber")?;
    if !cfg.allow_root && (uid == 0 || gid == 0) {
        return Err(format!(
            "{}: uidNumber {uid}, gidNumber {gid}: the directory does not make root \
             ({ALLOW_ROOT_ENV})",
            user.dn
        ));
    }
    Ok((uid, gid, posix_name))
}

/// The supplementary groups: each group's `gidNumber`, without the primary
/// gid, and without 0 unless `allow_root`. A group with a malformed
/// `gidNumber` is left out (and logged).
fn group_ids(groups: &[Entry], primary: u32, allow_root: bool) -> Vec<u32> {
    let mut gids = BTreeSet::new();
    for g in groups {
        match g.number("gidNumber") {
            Ok(0) if !allow_root => {
                tracing::warn!("⚠️  LDAP group {} is gid 0: left out ({ALLOW_ROOT_ENV})", g.dn)
            }
            Ok(gid) if gid != primary => {
                gids.insert(gid);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("⚠️  LDAP group left out: {e}"),
        }
    }
    gids.into_iter().collect()
}

/// What the directory says about a principal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(Identity),
    /// Not in the directory, or an entry the server refuses.
    NotFound,
    /// The directory cannot say now and nothing valid is cached: refuse
    /// the caller. Never the anonymous identity.
    Unavailable,
}

struct Cached {
    outcome: Outcome,
    at: Instant,
}

impl Cached {
    /// Within its TTL: `ttl` for a user found, `negative_ttl` otherwise.
    fn valid(&self, cfg: &LdapConfig) -> bool {
        self.at.elapsed()
            < match self.outcome {
                Outcome::Found(_) => cfg.ttl,
                _ => cfg.negative_ttl,
            }
    }
}

type Cache = Arc<Mutex<HashMap<String, Cached>>>;

struct Live {
    cfg: Arc<LdapConfig>,
    dir: Arc<dyn Directory>,
    cache: Cache,
    permits: Arc<tokio::sync::Semaphore>,
}

/// The directory lookup, cached; or, when its settings are malformed, a
/// lookup that is always [`Lookup::Unavailable`].
pub struct LdapIdmap {
    live: Result<Live, String>,
    /// Who a principal the directory does not have is (`FLINT_NFS_GSS_ANON`).
    anon: Identity,
}

impl LdapIdmap {
    pub fn new(cfg: LdapConfig, dir: Arc<dyn Directory>, anon: Identity) -> Self {
        let permits = Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent));
        LdapIdmap {
            live: Ok(Live {
                cfg: Arc::new(cfg),
                dir,
                cache: Arc::new(Mutex::new(HashMap::new())),
                permits,
            }),
            anon,
        }
    }

    /// Settings that do not parse: every principal the static map does
    /// not name is refused (failing open would grant what the directory
    /// was meant to decide).
    pub fn broken(reason: String, anon: Identity) -> Self {
        LdapIdmap {
            live: Err(reason),
            anon,
        }
    }

    pub fn anon(&self) -> &Identity {
        &self.anon
    }

    /// Whether the directory decides who `principal` is: a user of one of
    /// the realms. Everything, when the settings are broken.
    pub fn covers(&self, principal: &str) -> bool {
        match &self.live {
            Ok(l) => l.cfg.user_name(principal).is_some(),
            Err(_) => true,
        }
    }

    /// The cached answer for `principal`, if still valid.
    fn cached(live: &Live, principal: &str) -> Option<Lookup> {
        let cache = live.cache.lock().unwrap_or_else(|p| p.into_inner());
        let c = cache.get(principal)?;
        c.valid(&live.cfg).then(|| match &c.outcome {
            Outcome::Found(i) => Lookup::Found(i.clone()),
            _ => Lookup::NotFound,
        })
    }

    fn remember(cache: &Cache, cfg: &LdapConfig, principal: &str, outcome: Outcome) {
        let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.len() >= MAX_CACHED && !cache.contains_key(principal) {
            cache.retain(|_, c| c.valid(cfg));
            if cache.len() >= MAX_CACHED {
                return;
            }
        }
        cache.insert(
            principal.to_string(),
            Cached {
                outcome,
                at: Instant::now(),
            },
        );
    }

    /// Who the directory says `principal` is. Call only for principals it
    /// [covers](Self::covers).
    pub async fn resolve(&self, principal: &str) -> Lookup {
        let live = match &self.live {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("❌ {principal}: the LDAP settings are malformed ({e}): refused");
                return Lookup::Unavailable;
            }
        };
        let Some(name) = live.cfg.user_name(principal) else {
            return Lookup::NotFound;
        };
        if let Some(hit) = Self::cached(live, principal) {
            return hit;
        }
        // One deadline for the queue and the lookup.
        let deadline = Instant::now() + live.cfg.timeout;
        let permit =
            match tokio::time::timeout_at(deadline, live.permits.clone().acquire_owned()).await {
                Ok(Ok(p)) => p,
                _ => {
                    tracing::warn!(
                        "❌ {principal}: {} LDAP lookups already in flight for {:?}: refused",
                        live.cfg.max_concurrent,
                        live.cfg.timeout
                    );
                    return Lookup::Unavailable;
                }
            };
        // Another caller may have looked it up while this one queued.
        if let Some(hit) = Self::cached(live, principal) {
            return hit;
        }
        // Its own task, holding the permit and bounded by the deadline
        // even if this caller goes away; it caches the answer before it
        // lets the permit go, so a caller queued behind it finds it. A
        // panic in the client is an error here, not a dead NFS connection.
        let (dir, cfg, cache) = (live.dir.clone(), live.cfg.clone(), live.cache.clone());
        let (p, n) = (principal.to_string(), name.to_string());
        let task = tokio::spawn(async move {
            let r = tokio::time::timeout_at(deadline, lookup(dir.as_ref(), &cfg, &p, &n)).await;
            if let Ok(Ok(o)) = &r {
                Self::remember(&cache, &cfg, &p, o.clone());
            }
            drop(permit);
            r
        });
        let outcome = match task.await {
            Ok(Ok(Ok(o))) => o,
            Ok(Ok(Err(e))) => {
                tracing::warn!("❌ {principal}: LDAP lookup failed ({e}): refused");
                return Lookup::Unavailable;
            }
            Ok(Err(_)) => {
                tracing::warn!(
                    "❌ {principal}: LDAP lookup took over {:?}: refused",
                    live.cfg.timeout
                );
                return Lookup::Unavailable;
            }
            Err(e) => {
                tracing::error!("❌ {principal}: LDAP lookup panicked ({e}): refused");
                return Lookup::Unavailable;
            }
        };
        let answer = match &outcome {
            Outcome::Found(i) => {
                tracing::info!(
                    "🔐 {principal} is {}:{} (+{} groups), from the directory",
                    i.uid,
                    i.gid,
                    i.gids.len()
                );
                Lookup::Found(i.clone())
            }
            Outcome::NotFound => {
                tracing::info!("🔐 {principal} is not in the directory");
                Lookup::NotFound
            }
            Outcome::Rejected(why) => {
                tracing::warn!("❌ {principal}: the directory's entry is refused: {why}");
                Lookup::NotFound
            }
        };
        answer
    }

    /// From the env; `None` when [`URL_ENV`] is unset.
    pub fn from_env(anon: Identity) -> Option<Self> {
        let cfg = match LdapConfig::parse(|k| std::env::var(k).ok()) {
            Ok(None) => return None,
            Ok(Some(c)) => c,
            Err(e) => {
                tracing::error!(
                    "❌ LDAP identity lookup is malformed ({e}): EVERY Kerberos user \
                     principal the static map does not name will be REFUSED"
                );
                return Some(Self::broken(e, anon));
            }
        };
        let dir = match Ldap3Directory::new(&cfg) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(
                    "❌ LDAP identity lookup cannot start ({e}): EVERY Kerberos user \
                     principal the static map does not name will be REFUSED"
                );
                return Some(Self::broken(e, anon));
            }
        };
        if cfg.insecure {
            tracing::error!(
                "🚨 {INSECURE_ENV}: LDAP lookups and the bind password cross the network \
                 IN THE CLEAR. Test rigs only."
            );
        }
        tracing::info!(
            "🔐 Kerberos users of {} are looked up in {} ({}), cached {:?} (absent: {:?})",
            cfg.realms.join(","),
            cfg.url,
            if cfg.starttls { "StartTLS" } else if cfg.insecure { "NO TLS" } else { "TLS" },
            cfg.ttl,
            cfg.negative_ttl
        );
        Some(Self::new(cfg, Arc::new(dir), anon))
    }

    /// The process-wide lookup, set up once.
    pub fn global() -> Option<&'static LdapIdmap> {
        static LDAP: std::sync::OnceLock<Option<LdapIdmap>> = std::sync::OnceLock::new();
        LDAP.get_or_init(|| Self::from_env(super::gss_idmap::anon_from_env()))
            .as_ref()
    }
}

/// The real directory: ldap3 over rustls.
pub struct Ldap3Directory {
    url: String,
    starttls: bool,
    tls: Option<Arc<rustls::ClientConfig>>,
    bind: Option<(String, Secret)>,
    timeout: Duration,
}

impl Ldap3Directory {
    pub fn new(cfg: &LdapConfig) -> Result<Self, String> {
        // ldap3 builds its default TLS config from the process provider.
        crate::install_crypto_provider();
        let tls = match &cfg.ca_file {
            None => None,
            Some(f) => Some(Arc::new(tls_config(f)?)),
        };
        Ok(Ldap3Directory {
            url: cfg.url.clone(),
            starttls: cfg.starttls,
            tls,
            bind: cfg.bind.clone(),
            timeout: cfg.timeout,
        })
    }
}

/// A client config trusting the PEM certificates in `ca_file`.
fn tls_config(ca_file: &str) -> Result<rustls::ClientConfig, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let pem = std::fs::read(ca_file).map_err(|e| format!("{CA_FILE_ENV}={ca_file}: {e}"))?;
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(&pem) {
        let c = c.map_err(|e| format!("{CA_FILE_ENV}={ca_file}: {e}"))?;
        roots
            .add(c)
            .map_err(|e| format!("{CA_FILE_ENV}={ca_file}: {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!("{CA_FILE_ENV}={ca_file}: no certificate in the file"));
    }
    // Named, not the process default: see `nfs_proxy::tls`.
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("tls: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

#[async_trait::async_trait]
impl Directory for Ldap3Directory {
    async fn connect(&self) -> Result<Box<dyn Searcher>, String> {
        let mut settings = ldap3::LdapConnSettings::new()
            .set_conn_timeout(self.timeout)
            .set_starttls(self.starttls);
        if let Some(tls) = &self.tls {
            settings = settings.set_config(tls.clone());
        }
        let (conn, mut ldap) = ldap3::LdapConnAsync::with_settings(settings, &self.url)
            .await
            .map_err(|e| format!("{}: {e}", self.url))?;
        tokio::spawn(async move {
            if let Err(e) = conn.drive().await {
                tracing::debug!("LDAP connection: {e}");
            }
        });
        if let Some((dn, pw)) = &self.bind {
            ldap.with_timeout(self.timeout);
            ldap.simple_bind(dn, &pw.0)
                .await
                .and_then(|r| r.success())
                .map_err(|e| format!("bind as {dn}: {e}"))?;
        }
        Ok(Box::new(Ldap3Searcher {
            ldap,
            timeout: self.timeout,
        }))
    }
}

struct Ldap3Searcher {
    ldap: ldap3::Ldap,
    timeout: Duration,
}

#[async_trait::async_trait]
impl Searcher for Ldap3Searcher {
    async fn search(
        &mut self,
        base: &str,
        filter: &str,
        attrs: &[&str],
        size_limit: i32,
    ) -> Result<Vec<Entry>, String> {
        self.ldap
            .with_timeout(self.timeout)
            .with_search_options(ldap3::SearchOptions::new().sizelimit(size_limit));
        let (entries, _) = self
            .ldap
            .search(base, ldap3::Scope::Subtree, filter, attrs.to_vec())
            .await
            .and_then(|r| r.success())
            .map_err(|e| format!("search {base} {filter}: {e}"))?;
        Ok(entries
            .into_iter()
            .map(|e| {
                let e = ldap3::SearchEntry::construct(e);
                Entry {
                    dn: e.dn,
                    attrs: e
                        .attrs
                        .into_iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), v))
                        .collect(),
                }
            })
            .collect())
    }

    async fn close(&mut self) {
        let _ = self.ldap.unbind().await;
    }
}

/// A directory in memory, for tests: answers by (base, filter), counts
/// connections, and can be down or hang.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    pub struct FakeDirectory {
        pub answers: Mutex<HashMap<(String, String), Vec<Entry>>>,
        pub searches: Mutex<Vec<String>>,
        pub connects: AtomicUsize,
        pub down: AtomicBool,
        pub hang: AtomicBool,
    }

    impl FakeDirectory {
        pub fn answer(&self, base: &str, filter: &str, entries: Vec<Entry>) {
            self.answers
                .lock()
                .unwrap()
                .insert((base.into(), filter.into()), entries);
        }
        pub fn connects(&self) -> usize {
            self.connects.load(Ordering::SeqCst)
        }
        pub fn set_down(&self, down: bool) {
            self.down.store(down, Ordering::SeqCst)
        }
    }

    /// An entry: `dn`, then (attribute, values) pairs.
    pub fn entry(dn: &str, attrs: &[(&str, &[&str])]) -> Entry {
        Entry {
            dn: dn.into(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.iter().map(|s| s.to_string()).collect()))
                .collect(),
        }
    }

    struct FakeSearcher(Arc<FakeDirectory>);

    #[async_trait::async_trait]
    impl Directory for Arc<FakeDirectory> {
        async fn connect(&self) -> Result<Box<dyn Searcher>, String> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            if self.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.down.load(Ordering::SeqCst) {
                return Err("connection refused".into());
            }
            Ok(Box::new(FakeSearcher(self.clone())))
        }
    }

    #[async_trait::async_trait]
    impl Searcher for FakeSearcher {
        async fn search(
            &mut self,
            base: &str,
            filter: &str,
            _attrs: &[&str],
            _size_limit: i32,
        ) -> Result<Vec<Entry>, String> {
            self.0.searches.lock().unwrap().push(format!("{base} {filter}"));
            Ok(self
                .0
                .answers
                .lock()
                .unwrap()
                .get(&(base.to_string(), filter.to_string()))
                .cloned()
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{entry, FakeDirectory};
    use super::*;
    use crate::nfs::gss_idmap::NOBODY;

    const PEOPLE: &str = "ou=People,dc=corp,dc=example";
    const GROUPS: &str = "ou=Groups,dc=corp,dc=example";

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    fn config(extra: &[(&str, &str)]) -> LdapConfig {
        let mut pairs = vec![
            (URL_ENV, "ldaps://ldap.corp.example"),
            (REALMS_ENV, "CORP.EXAMPLE"),
            (USER_BASE_ENV, PEOPLE),
            (GROUP_BASE_ENV, GROUPS),
        ];
        pairs.extend_from_slice(extra);
        LdapConfig::parse(env(&pairs)).unwrap().unwrap()
    }

    fn alice() -> Entry {
        entry(
            "uid=alice,ou=People,dc=corp,dc=example",
            &[("uid", &["alice"]), ("uidNumber", &["1000"]), ("gidNumber", &["1000"])],
        )
    }

    const ALICE_USER: &str = "(&(objectClass=posixAccount)(uid=alice))";
    const ALICE_GROUPS: &str = "(&(objectClass=posixGroup)(|(memberUid=alice)))";

    fn directory() -> Arc<FakeDirectory> {
        let d = Arc::new(FakeDirectory::default());
        d.answer(PEOPLE, ALICE_USER, vec![alice()]);
        d.answer(
            GROUPS,
            ALICE_GROUPS,
            vec![
                entry("cn=dev,ou=Groups", &[("gidNumber", &["200"])]),
                entry("cn=staff,ou=Groups", &[("gidNumber", &["100"])]),
                entry("cn=alice,ou=Groups", &[("gidNumber", &["1000"])]), // her primary
                entry("cn=root,ou=Groups", &[("gidNumber", &["0"])]),
                entry("cn=broken,ou=Groups", &[("gidNumber", &["x"])]),
                entry("cn=dev2,ou=Groups", &[("gidNumber", &["200"])]),
            ],
        );
        d
    }

    fn idmap(cfg: LdapConfig, d: &Arc<FakeDirectory>) -> LdapIdmap {
        LdapIdmap::new(cfg, Arc::new(d.clone()), NOBODY)
    }

    #[test]
    fn settings_parse_and_tls_is_required() {
        assert_eq!(LdapConfig::parse(env(&[])).unwrap(), None, "off when unset");
        let c = config(&[]);
        assert!(!c.starttls && !c.insecure, "ldaps is TLS from the start");
        assert_eq!(c.user_filter, "(objectClass=posixAccount)");
        assert_eq!(c.membership, Membership { member_uid: true, member_dn: false });
        assert_eq!(
            (c.ttl, c.negative_ttl, c.timeout, c.max_concurrent, c.allow_root),
            (Duration::from_secs(300), Duration::from_secs(60), Duration::from_secs(5), 4, false)
        );
        assert!(config(&[(URL_ENV, "ldap://ldap:389")]).starttls, "ldap:// is StartTLS");
        let plain = config(&[(URL_ENV, "ldap://ldap:389"), (INSECURE_ENV, "true")]);
        assert!(!plain.starttls && plain.insecure, "plain only when asked");
        // Groups default to the user bases; several bases split on ';'.
        let c = LdapConfig::parse(env(&[
            (URL_ENV, "ldaps://l"),
            (REALMS_ENV, "A.EXAMPLE, B.EXAMPLE"),
            (USER_BASE_ENV, "ou=a,dc=x; ou=b,dc=x"),
            (GROUP_MEMBERSHIP_ENV, "memberUid,member"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.realms, vec!["A.EXAMPLE", "B.EXAMPLE"]);
        assert_eq!(c.user_bases, vec!["ou=a,dc=x", "ou=b,dc=x"]);
        assert_eq!(c.group_bases, c.user_bases);
        assert_eq!(c.membership, Membership { member_uid: true, member_dn: true });
        let pw = config(&[(BIND_DN_ENV, "cn=nfs,dc=x"), (BIND_PASSWORD_ENV, "hunter2")]);
        assert!(!format!("{pw:?}").contains("hunter2"), "the password is never printed");
        for bad in [
            &[(URL_ENV, "ldapi:///run/slapd")][..],
            &[(URL_ENV, "ldaps://")],
            &[(URL_ENV, "http://ldap")],
            &[(REALMS_ENV, "")],
            &[(REALMS_ENV, "*")],
            &[(USER_BASE_ENV, " ; ")],
            &[(USER_FILTER_ENV, "objectClass=posixAccount")],
            &[(PRINCIPAL_ATTR_ENV, "krb(PrincipalName")],
            &[(GROUP_MEMBERSHIP_ENV, "uniqueMember")],
            &[(BIND_DN_ENV, "cn=nfs,dc=x")],
            &[(BIND_PASSWORD_ENV, "pw")],
            &[(TTL_ENV, "-1")],
            &[(TIMEOUT_ENV, "0")],
            &[(MAX_CONCURRENT_ENV, "0")],
            &[(INSECURE_ENV, "maybe")],
        ] {
            let mut pairs = vec![
                (URL_ENV, "ldaps://ldap"),
                (REALMS_ENV, "CORP.EXAMPLE"),
                (USER_BASE_ENV, PEOPLE),
            ];
            for &(k, v) in bad {
                pairs.retain(|(pk, _)| *pk != k);
                pairs.push((k, v));
            }
            assert!(LdapConfig::parse(env(&pairs)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_user_principals_of_the_realms_are_looked_up() {
        let m = idmap(config(&[]), &directory());
        assert!(m.covers("alice@CORP.EXAMPLE"));
        for no in [
            "host/w1.corp.example@CORP.EXAMPLE",
            "nfs/w1@CORP.EXAMPLE",
            "alice@OTHER.EXAMPLE",
            "alice@corp.example",
            "@CORP.EXAMPLE",
            "alice",
        ] {
            assert!(!m.covers(no), "{no}");
        }
        let broken = LdapIdmap::broken("bad".into(), NOBODY);
        assert!(broken.covers("host/w1@ANY"), "broken settings decide everyone: refused");
    }

    #[tokio::test]
    async fn rfc2307_user_and_groups() {
        let d = directory();
        let m = idmap(config(&[]), &d);
        assert_eq!(
            m.resolve("alice@CORP.EXAMPLE").await,
            Lookup::Found(Identity { uid: 1000, gid: 1000, gids: vec![100, 200] }),
            "groups deduplicated and sorted; the primary, gid 0 and a malformed group left out"
        );
        assert_eq!(
            *d.searches.lock().unwrap(),
            vec![format!("{PEOPLE} {ALICE_USER}"), format!("{GROUPS} {ALICE_GROUPS}")]
        );
        assert_eq!(m.resolve("bob@CORP.EXAMPLE").await, Lookup::NotFound);
    }

    #[tokio::test]
    async fn by_principal_attribute_and_member_dn() {
        let d = Arc::new(FakeDirectory::default());
        let cfg = config(&[
            (PRINCIPAL_ATTR_ENV, "krbPrincipalName"),
            (GROUP_MEMBERSHIP_ENV, "memberUid,member"),
        ]);
        d.answer(
            PEOPLE,
            "(&(objectClass=posixAccount)(krbPrincipalName=alice@CORP.EXAMPLE))",
            vec![entry(
                "uid=asmith,ou=People,dc=corp,dc=example",
                &[
                    ("uid", &["asmith"]),
                    ("krbPrincipalName", &["alice@CORP.EXAMPLE"]),
                    ("uidNumber", &["1000"]),
                    ("gidNumber", &["1000"]),
                ],
            )],
        );
        d.answer(
            GROUPS,
            "(&(objectClass=posixGroup)(|(memberUid=asmith)\
             (member=uid=asmith,ou=People,dc=corp,dc=example)))",
            vec![entry("cn=dev", &[("gidNumber", &["200"])])],
        );
        let m = idmap(cfg, &d);
        assert_eq!(
            m.resolve("alice@CORP.EXAMPLE").await,
            Lookup::Found(Identity { uid: 1000, gid: 1000, gids: vec![200] })
        );
    }

    #[tokio::test]
    async fn the_entry_must_name_the_principal_exactly() {
        let d = Arc::new(FakeDirectory::default());
        // LDAP's uid match ignores case; Kerberos does not.
        d.answer(PEOPLE, "(&(objectClass=posixAccount)(uid=ALICE))", vec![alice()]);
        let m = idmap(config(&[]), &d);
        assert_eq!(m.resolve("ALICE@CORP.EXAMPLE").await, Lookup::NotFound);
        // A name cannot widen the filter.
        let m = idmap(config(&[]), &d);
        let _ = m.resolve("a*)(uid=*@CORP.EXAMPLE").await;
        assert!(d
            .searches
            .lock()
            .unwrap()
            .contains(&format!("{PEOPLE} (&(objectClass=posixAccount)(uid=a\\2a\\29\\28uid=\\2a))")));
    }

    #[tokio::test]
    async fn the_directory_never_makes_root() {
        let d = Arc::new(FakeDirectory::default());
        let root = |dn: &str, uid: &str, gid: &str| {
            entry(dn, &[("uid", &["r"]), ("uidNumber", &[uid]), ("gidNumber", &[gid])])
        };
        d.answer(PEOPLE, "(&(objectClass=posixAccount)(uid=r))", vec![root("uid=r", "0", "5")]);
        assert_eq!(idmap(config(&[]), &d).resolve("r@CORP.EXAMPLE").await, Lookup::NotFound);
        d.answer(PEOPLE, "(&(objectClass=posixAccount)(uid=r))", vec![root("uid=r", "5", "0")]);
        assert_eq!(idmap(config(&[]), &d).resolve("r@CORP.EXAMPLE").await, Lookup::NotFound);
        let allowed = idmap(config(&[(ALLOW_ROOT_ENV, "true")]), &d);
        assert_eq!(
            allowed.resolve("r@CORP.EXAMPLE").await,
            Lookup::Found(Identity { uid: 5, gid: 0, gids: vec![] }),
            "only when explicitly allowed"
        );
    }

    #[tokio::test]
    async fn malformed_and_ambiguous_entries_are_refused() {
        let cfg = config(&[(USER_BASE_ENV, "ou=a;ou=b")]);
        let user = |f: &str| format!("(&(objectClass=posixAccount)(uid={f}))");
        let d = Arc::new(FakeDirectory::default());
        // One user, in both bases: which is she?
        d.answer("ou=a", &user("twice"), vec![entry("uid=twice,ou=a", &[("uid", &["twice"]), ("uidNumber", &["1"]), ("gidNumber", &["1"])])]);
        d.answer("ou=b", &user("twice"), vec![entry("uid=twice,ou=b", &[("uid", &["twice"]), ("uidNumber", &["2"]), ("gidNumber", &["2"])])]);
        d.answer("ou=a", &user("nonum"), vec![entry("uid=nonum", &[("uid", &["nonum"]), ("gidNumber", &["1"])])]);
        d.answer("ou=a", &user("multi"), vec![entry("uid=multi", &[("uid", &["multi"]), ("uidNumber", &["1", "2"]), ("gidNumber", &["1"])])]);
        d.answer("ou=a", &user("neg"), vec![entry("uid=neg", &[("uid", &["neg"]), ("uidNumber", &["-1"]), ("gidNumber", &["1"])])]);
        let m = idmap(cfg, &d);
        for p in ["twice", "nonum", "multi", "neg"] {
            assert_eq!(m.resolve(&format!("{p}@CORP.EXAMPLE")).await, Lookup::NotFound, "{p}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cached_answers_serve_within_their_ttl_and_then_fail_closed() {
        let d = directory();
        let m = idmap(config(&[]), &d);
        let alice = Lookup::Found(Identity { uid: 1000, gid: 1000, gids: vec![100, 200] });
        assert_eq!(m.resolve("alice@CORP.EXAMPLE").await, alice);
        assert_eq!(m.resolve("bob@CORP.EXAMPLE").await, Lookup::NotFound);
        assert_eq!(d.connects(), 2);
        // The directory goes away: the cached answers still hold.
        d.set_down(true);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(m.resolve("alice@CORP.EXAMPLE").await, alice);
        assert_eq!(m.resolve("bob@CORP.EXAMPLE").await, Lookup::NotFound);
        assert_eq!(d.connects(), 2, "served from the cache");
        // An unknown principal is refused, never anonymous.
        assert_eq!(m.resolve("carol@CORP.EXAMPLE").await, Lookup::Unavailable);
        // Past the negative TTL bob is unknown again; past the TTL, alice.
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(m.resolve("bob@CORP.EXAMPLE").await, Lookup::Unavailable);
        assert_eq!(m.resolve("alice@CORP.EXAMPLE").await, alice, "her TTL runs");
        tokio::time::advance(Duration::from_secs(240)).await;
        assert_eq!(
            m.resolve("alice@CORP.EXAMPLE").await,
            Lookup::Unavailable,
            "an expired answer is not served, however good it was"
        );
        // Back: looked up afresh.
        d.set_down(false);
        assert_eq!(m.resolve("alice@CORP.EXAMPLE").await, alice);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_directory_times_out_and_frees_its_slot() {
        let d = directory();
        d.hang.store(true, std::sync::atomic::Ordering::SeqCst);
        let m = idmap(config(&[(MAX_CONCURRENT_ENV, "1"), (TIMEOUT_ENV, "2")]), &d);
        let t0 = Instant::now();
        assert_eq!(m.resolve("alice@CORP.EXAMPLE").await, Lookup::Unavailable);
        let waited = t0.elapsed();
        assert!(waited >= Duration::from_secs(2) && waited < Duration::from_secs(3), "bounded by the timeout: {waited:?}");
        // The aborted lookup gave its permit back: the next one runs.
        d.hang.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(m.resolve("alice@CORP.EXAMPLE").await, Lookup::Found(_)));
        let Ok(live) = &m.live else { unreachable!() };
        assert_eq!(live.permits.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_lookups_share_one_answer() {
        let d = directory();
        let m = Arc::new(idmap(config(&[(MAX_CONCURRENT_ENV, "1")]), &d));
        let calls: Vec<_> = (0..8)
            .map(|_| {
                let m = m.clone();
                tokio::spawn(async move { m.resolve("alice@CORP.EXAMPLE").await })
            })
            .collect();
        for c in calls {
            assert!(matches!(c.await.unwrap(), Lookup::Found(_)));
        }
        assert_eq!(d.connects(), 1, "the rest found it cached once they got a slot");
    }

    #[tokio::test]
    async fn broken_settings_refuse() {
        let m = LdapIdmap::broken("bad".into(), NOBODY);
        assert_eq!(m.resolve("alice@CORP.EXAMPLE").await, Lookup::Unavailable);
    }

    #[test]
    fn escaping() {
        assert_eq!(ldap_escape("a*b(c)d\\e\0"), "a\\2ab\\28c\\29d\\5ce\\00");
        assert_eq!(ldap_escape("alice"), "alice");
    }
}
