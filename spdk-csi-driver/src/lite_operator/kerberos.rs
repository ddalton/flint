//! Kerberos for a share (`spec.security.kerberos`): the hub's server
//! principal and keytab from krb5-operator, and the RWX servers' env
//! (`nfs_krb5::server_pod_parts`) on the hub pod.
//! docs/plans/flint-lite-kerberos-plan.md, L1.
//!
//! The hub binary (`flint-pnfs-mds`, standalone) already does Kerberos
//! when its env says so and stops at startup on a bad value; this module
//! only decides that env, and refuses here first what the hub would
//! refuse, so a bad spec is a condition on the share rather than a
//! crash-looping pod.

use crate::lite_operator::crd::{FlintShare, KerberosSpec};
use crate::lite_operator::render;
use crate::nfs_krb5::{LdapCa, ServerSecurity};
use kube::ResourceExt;

/// The floor when `minSec` is absent.
pub const DEFAULT_MIN_SEC: &str = "krb5i";
const DEFAULT_MACHINE: &str = "0:0";
const DEFAULT_ANON: &str = "65534:65534";

/// The KerberosPrincipal and its keytab Secret: `<share>-krb5`.
pub fn principal_name(share: &FlintShare) -> String {
    format!("{}-krb5", render::names(share).service)
}

/// The name clients mount the hub by: its Service's cluster DNS name.
pub fn server_name(share: &FlintShare, cluster_domain: &str) -> String {
    format!(
        "{}.{}.svc.{}",
        render::names(share).service,
        share.namespace().unwrap_or_default(),
        cluster_domain.trim_matches('.')
    )
}

/// Every name the hub's keytab holds a key for: the Service's, the
/// `advertiseAddress` host when it is a DNS name (an IP has no
/// principal), and `serverNames`; lower case, no duplicates.
pub fn server_names(share: &FlintShare, k: &KerberosSpec, cluster_domain: &str) -> Vec<String> {
    let mut names = vec![server_name(share, cluster_domain)];
    let advertised = share
        .spec
        .service
        .as_ref()
        .and_then(|s| s.advertise_address.as_deref())
        .and_then(advertised_host);
    for n in advertised.into_iter().chain(k.server_names.iter().flatten().cloned()) {
        let n = n.trim().trim_end_matches('.').to_ascii_lowercase();
        if !n.is_empty() && !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

/// The host of `host:port` when it is a DNS name, not an IP literal.
fn advertised_host(addr: &str) -> Option<String> {
    let host = addr.trim();
    if host.starts_with('[') {
        return None; // [IPv6]:port
    }
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    (host.parse::<std::net::IpAddr>().is_err() && !host.is_empty()).then(|| host.to_string())
}

/// Each name's principal in the realm.
pub fn principals(share: &FlintShare, k: &KerberosSpec, cluster_domain: &str) -> Vec<String> {
    server_names(share, k, cluster_domain)
        .iter()
        .map(|n| format!("nfs/{n}@{}", k.realm))
        .collect()
}

/// What the hub would refuse at startup, refused here instead, and what
/// the hub cannot do at all.
pub fn validate(share: &FlintShare, k: &KerberosSpec, nfs_proxy: bool) -> Result<(), Invalid> {
    if nfs_proxy {
        return Err(Invalid::BehindProxy);
    }
    let bad = |m: String| Err(Invalid::Spec(m));
    if k.realm.trim().is_empty() || k.realm.contains(['/', '@', ',']) || k.realm != k.realm.to_ascii_uppercase() {
        return bad(format!("realm {:?}: an upper-case realm name, e.g. CORP.EXAMPLE", k.realm));
    }
    if k.realm_ref.trim().is_empty() {
        return bad("realmRef: the KerberosRealm to make the principal in".into());
    }
    let sec = security(share, k);
    // The hub's own checks, on the values it would be given.
    if let Some(floor) = &sec.min_sec {
        crate::nfs::sec_policy::SecPolicy::validate_env_value(floor).map_err(|e| Invalid::Spec(format!("minSec: {e}")))?;
    }
    if let Some(p) = &sec.gss_principals {
        crate::nfs::gss_principals::PrincipalAllowlist::parse(p)
            .map_err(|e| Invalid::Spec(format!("allowedPrincipals: {e}")))?;
    }
    let anon = crate::nfs::gss_idmap::Identity::parse(sec.gss_anon.as_deref().unwrap_or(DEFAULT_ANON))
        .map_err(|e| Invalid::Spec(format!("anonIdentity: {e}")))?;
    if let Some(m) = &sec.gss_idmap {
        crate::nfs::gss_idmap::GssIdmap::parse(m, anon).map_err(|e| Invalid::Spec(format!("idmap/machineIdentity: {e}")))?;
    }
    Ok(())
}

/// Why a share's Kerberos is not served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    /// The fleet runs the NFS proxy, which does not end GSS.
    BehindProxy,
    /// A setting the hub would refuse.
    Spec(String),
}

impl Invalid {
    pub fn reason(&self) -> &'static str {
        match self {
            Invalid::BehindProxy => "KerberosBehindProxy",
            Invalid::Spec(_) => "KerberosInvalid",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Invalid::BehindProxy => "spec.security.kerberos is set, but this fleet runs the NFS proxy, which \
                                     refuses RPCSEC_GSS: a GSS context is bound to the server principal, and \
                                     the proxy does not end GSS. No hub is run rather than one serving sec=sys."
                .into(),
            Invalid::Spec(m) => format!("spec.security.kerberos: {m}"),
        }
    }
}

/// The hub's security: the RWX servers' settings for `k`
/// (flint-csi-driver-chart `krb5ServerEnv`). Not attached-nodes-only
/// (a standalone hub refuses it at startup: its clients are not
/// VolumeAttachments) and no layouts setting (it grants none).
pub fn security(share: &FlintShare, k: &KerberosSpec) -> ServerSecurity {
    let realm = &k.realm;
    let rules = k.idmap.clone().unwrap_or_default();
    let principals = match k.allowed_principals.as_ref().filter(|p| !p.is_empty()) {
        Some(p) => p.join(","),
        None => [format!("host/*@{realm}"), format!("nfs/*@{realm}")]
            .into_iter()
            .chain(rules.iter().map(|r| r.principal.clone()))
            .collect::<Vec<_>>()
            .join(","),
    };
    let machine = k.machine_identity.as_deref().unwrap_or(DEFAULT_MACHINE);
    let idmap = rules
        .iter()
        .map(|r| {
            let mut id = format!("{}={}:{}", r.principal, r.uid, r.gid);
            if let Some(g) = r.groups.as_ref().filter(|g| !g.is_empty()) {
                id.push(':');
                id.push_str(&g.iter().map(u32::to_string).collect::<Vec<_>>().join(";"));
            }
            id
        })
        .chain([format!("host/*@{realm}={machine}"), format!("nfs/*@{realm}={machine}")])
        .collect::<Vec<_>>()
        .join(",");
    let mut sec = ServerSecurity {
        keytab_secret: Some(principal_name(share)),
        min_sec: Some(k.min_sec.clone().unwrap_or_else(|| DEFAULT_MIN_SEC.into())),
        gss_principals: Some(principals),
        gss_idmap: Some(idmap),
        gss_anon: Some(k.anon_identity.clone().unwrap_or_else(|| DEFAULT_ANON.into())),
        enforce_permissions: share.spec.enforce_permissions().then(|| "enforce".into()),
        root_squash: k.root_squash.unwrap_or(false).then(|| "true".into()),
        require_mach_cred: k.require_mach_cred.unwrap_or(true).then(|| "true".into()),
        ..Default::default()
    };
    if let Some(l) = &k.ldap {
        let mut set = |s: &str, v: Option<String>| {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                sec.gss_ldap.push((s.to_string(), v));
            }
        };
        set("URL", Some(l.url.clone()));
        set("INSECURE", l.insecure.unwrap_or(false).then(|| "true".into()));
        set(
            "REALMS",
            Some(l.realms.clone().filter(|r| !r.is_empty()).unwrap_or_else(|| vec![realm.clone()]).join(",")),
        );
        set("USER_BASE", Some(l.user_base.clone()));
        set("GROUP_BASE", l.group_base.clone());
        set("USER_FILTER", l.user_filter.clone());
        set("PRINCIPAL_ATTR", l.principal_attribute.clone());
        set("GROUP_MEMBERSHIP", l.group_membership.clone());
        set("BIND_DN", l.bind_dn.clone());
        sec.gss_ldap_password = l
            .bind_password_secret
            .as_ref()
            .map(|r| (r.name.clone(), r.key.clone().unwrap_or_else(|| "password".into())));
        let key = |r: &crate::lite_operator::crd::SecretKeyRef| r.key.clone().unwrap_or_else(|| "ca.crt".into());
        sec.gss_ldap_ca = match (&l.ca_config_map, &l.ca_secret) {
            (Some(c), _) => Some(LdapCa::ConfigMap { name: c.name.clone(), key: key(c) }),
            (None, Some(s)) => Some(LdapCa::Secret { name: s.name.clone(), key: key(s) }),
            (None, None) => None,
        };
    }
    sec
}

/// The share's `KerberosPrincipal` (krb5-operator): every server name's
/// `nfs/` principal, keys in Secret [`principal_name`]. The caller sets
/// the owner reference to the FlintShare, which survives hibernation.
pub fn principal_object(share: &FlintShare, k: &KerberosSpec, cluster_domain: &str) -> serde_json::Value {
    let mut spec = serde_json::json!({
        "realmRef": k.realm_ref,
        "principals": principals(share, k, cluster_domain),
        "secretName": principal_name(share),
        "deletionPolicy": k.deletion_policy.as_deref().unwrap_or("Delete"),
    });
    if let Some(ns) = k.realm_namespace.as_deref().filter(|n| !n.is_empty()) {
        spec["realmNamespace"] = ns.into();
    }
    serde_json::json!({
        "apiVersion": "krb5.ddalton.dev/v1alpha1",
        "kind": "KerberosPrincipal",
        "metadata": {
            "name": principal_name(share),
            "namespace": share.namespace().unwrap_or_default(),
            "labels": {
                "app.kubernetes.io/managed-by": "flint-lite-operator",
                "chert.us/flintshare": share.name_any(),
            },
        },
        "spec": spec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lite_operator::crd::{FlintShareSpec, KerberosIdmapRule, SecuritySpec};

    fn share(k: KerberosSpec, advertise: Option<&str>) -> FlintShare {
        let mut spec: FlintShareSpec = serde_json::from_value(serde_json::json!({
            "persistence": {"size": "1Gi"},
        }))
        .unwrap();
        spec.security = Some(SecuritySpec { enforce_permissions: None, kerberos: Some(k) });
        if let Some(a) = advertise {
            spec.service = Some(serde_json::from_value(serde_json::json!({"advertiseAddress": a})).unwrap());
        }
        let mut s = FlintShare::new("ws1", spec);
        s.metadata.namespace = Some("team-a".into());
        s
    }

    fn k() -> KerberosSpec {
        serde_json::from_value(serde_json::json!({"realm": "CORP.EXAMPLE", "realmRef": "corp"})).unwrap()
    }

    #[test]
    fn the_defaults_are_the_rwx_servers() {
        let s = share(k(), None);
        let sec = security(&s, s.spec.kerberos().unwrap());
        assert_eq!(sec.keytab_secret.as_deref(), Some("ws1-krb5"));
        assert_eq!(sec.min_sec.as_deref(), Some("krb5i"));
        assert_eq!(sec.gss_principals.as_deref(), Some("host/*@CORP.EXAMPLE,nfs/*@CORP.EXAMPLE"));
        assert_eq!(sec.gss_idmap.as_deref(), Some("host/*@CORP.EXAMPLE=0:0,nfs/*@CORP.EXAMPLE=0:0"));
        assert_eq!(sec.gss_anon.as_deref(), Some("65534:65534"));
        assert_eq!(sec.enforce_permissions.as_deref(), Some("enforce"), "Kerberos enforces by default");
        assert_eq!(sec.require_mach_cred.as_deref(), Some("true"));
        assert_eq!(sec.root_squash, None);
        assert_eq!(sec.attached_nodes_only, None, "a standalone hub refuses it at startup");
        assert!(validate(&s, s.spec.kerberos().unwrap(), false).is_ok());
    }

    #[test]
    fn enforcement_can_still_be_turned_off_explicitly() {
        let mut s = share(k(), None);
        s.spec.security.as_mut().unwrap().enforce_permissions = Some(false);
        assert_eq!(security(&s, s.spec.kerberos().unwrap()).enforce_permissions, None);
    }

    #[test]
    fn idmap_rules_are_allowed_and_come_before_the_machines() {
        let mut kk = k();
        kk.idmap = Some(vec![KerberosIdmapRule {
            principal: "alice@CORP.EXAMPLE".into(),
            uid: 1000,
            gid: 1000,
            groups: Some(vec![100, 200]),
        }]);
        let s = share(kk, None);
        let sec = security(&s, s.spec.kerberos().unwrap());
        assert_eq!(
            sec.gss_principals.as_deref(),
            Some("host/*@CORP.EXAMPLE,nfs/*@CORP.EXAMPLE,alice@CORP.EXAMPLE")
        );
        assert_eq!(
            sec.gss_idmap.as_deref(),
            Some("alice@CORP.EXAMPLE=1000:1000:100;200,host/*@CORP.EXAMPLE=0:0,nfs/*@CORP.EXAMPLE=0:0")
        );
        assert!(validate(&s, s.spec.kerberos().unwrap(), false).is_ok(), "the hub's own parsers accept it");
    }

    #[test]
    fn the_principal_carries_every_name_the_hub_is_mounted_by() {
        let mut kk = k();
        kk.server_names = Some(vec!["WS1.Corp.Example.".into(), "ws1.team-a.svc.cluster.local".into()]);
        let s = share(kk, Some("hub.example.internal:2049"));
        let obj = principal_object(&s, s.spec.kerberos().unwrap(), "cluster.local");
        assert_eq!(obj["metadata"]["name"], "ws1-krb5");
        assert_eq!(obj["metadata"]["namespace"], "team-a");
        assert_eq!(obj["spec"]["secretName"], "ws1-krb5");
        assert_eq!(obj["spec"]["realmRef"], "corp");
        assert_eq!(
            obj["spec"]["principals"],
            serde_json::json!([
                "nfs/ws1.team-a.svc.cluster.local@CORP.EXAMPLE",
                "nfs/hub.example.internal@CORP.EXAMPLE",
                "nfs/ws1.corp.example@CORP.EXAMPLE",
            ])
        );
        // An IP has no principal.
        for ip in ["10.0.4.7:2049", "[2001:db8::1]:2049"] {
            let s = share(k(), Some(ip));
            assert_eq!(server_names(&s, s.spec.kerberos().unwrap(), "cluster.local").len(), 1, "{ip}");
        }
    }

    #[test]
    fn what_the_hub_would_refuse_is_refused_first() {
        let s = share(k(), None);
        assert_eq!(validate(&s, s.spec.kerberos().unwrap(), true), Err(Invalid::BehindProxy));
        for (field, bad) in [
            ("realm", serde_json::json!({"realm": "corp.example", "realmRef": "corp"})),
            ("minSec", serde_json::json!({"realm": "C.E", "realmRef": "corp", "minSec": "krb5p"})),
            ("allowedPrincipals", serde_json::json!({"realm": "C.E", "realmRef": "corp", "allowedPrincipals": ["no-realm"]})),
            ("anonIdentity", serde_json::json!({"realm": "C.E", "realmRef": "corp", "anonIdentity": "nobody"})),
        ] {
            let kk: KerberosSpec = serde_json::from_value(bad).unwrap();
            let s = share(kk, None);
            let e = validate(&s, s.spec.kerberos().unwrap(), false).expect_err(field);
            assert_eq!(e.reason(), "KerberosInvalid", "{field}");
        }
    }

    #[test]
    fn ldap_becomes_the_servers_directory_env() {
        let mut kk = k();
        kk.ldap = Some(
            serde_json::from_value(serde_json::json!({
                "url": "ldaps://ldap.corp.example",
                "userBase": "ou=People,dc=corp,dc=example",
                "bindDN": "cn=nfs,dc=corp,dc=example",
                "bindPasswordSecret": {"name": "ldap-bind"},
                "caConfigMap": {"name": "corp-ca"},
            }))
            .unwrap(),
        );
        let s = share(kk, None);
        let sec = security(&s, s.spec.kerberos().unwrap());
        let get = |k: &str| sec.gss_ldap.iter().find(|(s, _)| s == k).map(|(_, v)| v.as_str());
        assert_eq!(get("URL"), Some("ldaps://ldap.corp.example"));
        assert_eq!(get("REALMS"), Some("CORP.EXAMPLE"));
        assert_eq!(get("BIND_DN"), Some("cn=nfs,dc=corp,dc=example"));
        assert_eq!(get("INSECURE"), None);
        assert_eq!(sec.gss_ldap_password, Some(("ldap-bind".into(), "password".into())));
        assert_eq!(sec.gss_ldap_ca, Some(LdapCa::ConfigMap { name: "corp-ca".into(), key: "ca.crt".into() }));
    }
}
