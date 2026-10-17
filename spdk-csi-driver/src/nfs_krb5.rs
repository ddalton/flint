//! Kerberos (`sec=krb5`, `krb5i`, `krb5p`) for RWX NFS volumes on
//! Kubernetes, with keytabs from krb5-operator (chart: `nfs.kerberos`).
//!
//! The server side: each volume's NFS server has a principal of its own,
//! `nfs/<volume>.<server name>` ([`volume_server_name`]), which the
//! controller asks krb5-operator for (a `KerberosPrincipal` per volume,
//! deleted with the volume). Its pod mounts that keytab Secret alone and
//! gets `KRB5_KTNAME` (a bare path: the acceptor's `Keytab::load` does not
//! take a `FILE:` prefix), plus the security env. So one server's key
//! opens no other volume: a compromised server pod can neither read the
//! tickets clients present to another volume's server nor pass for it.
//! Volumes whose servers were made before (no per-volume name on the pod)
//! keep the shared `nfs/<server name>` until they are recreated.
//!
//! The client side is the node's kernel and its rpc.gssd, and rpc.gssd
//! names the server from the mount address (a Service ClusterIP): it
//! REVERSE-resolves the IP, then forward-resolves the name, through the
//! node's own resolver, and asks for `nfs/<that name>`. A node does not
//! resolve cluster DNS, so before a Kerberos mount the node plugin writes
//! `<ClusterIP> <volume's server name>` into the host's `/etc/hosts` (the
//! name rides in the publish context, [`SERVER_NAME_CONTEXT`]). Measured against a real rpc.gssd (nfs-utils 2.6.4): by IP alone it
//! fails ("unable to resolve <ip> to hostname"); by an unresolvable name
//! it fails (`get_full_hostname`); with the hosts entry a `sec=krb5i`
//! mount works. Our lines carry a marker, the rest of the file is left as
//! it is, and lines for IPs no Kerberos mount uses any more are dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Seek, Write};
use std::path::Path;

use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, EnvVar, EnvVarSource, KeyToPath, SecretKeySelector, SecretVolumeSource,
    Volume, VolumeMount,
};

/// Marks the `/etc/hosts` lines this module owns.
pub const HOSTS_MARKER: &str = "# flint-nfs-krb5";
/// Node plugin env: the name every Kerberos NFS server goes by.
pub const SERVER_NAME_ENV: &str = "FLINT_NFS_KRB5_SERVER_NAME";
/// Node plugin env: the host's hosts file, as mounted (default
/// [`DEFAULT_HOSTS_FILE`]).
pub const HOSTS_FILE_ENV: &str = "FLINT_NFS_KRB5_HOSTS_FILE";
pub const DEFAULT_HOSTS_FILE: &str = "/host/etc/hosts";
/// Where an NFS server pod has its keytab Secret.
pub const KEYTAB_DIR: &str = "/etc/flint/krb5";
/// Publish context: the name the node maps the server's IP to (the
/// volume's own; absent for a server with the shared principal).
pub const SERVER_NAME_CONTEXT: &str = "nfs.chert.us/krb5-server-name";
/// On the NFS server pod: the name its keytab is for.
pub const SERVER_NAME_ANNOTATION: &str = "chert.us/krb5-server-name";

/// The name a volume's NFS server goes by: `<volume>.<base>`, `base` being
/// the shared server name (under `.svc.<cluster domain>`, which the nodes'
/// krb5.conf maps to the realm). A volume id that is not a DNS label (63
/// characters of `[a-z0-9-]`) is replaced by a digest of it.
pub fn volume_server_name(base: &str, volume_id: &str) -> String {
    let id = volume_id.to_ascii_lowercase();
    let label_ok = !id.is_empty()
        && id.len() <= 63
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-');
    let label = if label_ok {
        id
    } else {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(volume_id.as_bytes());
        format!("v-{}", h[..8].iter().map(|b| format!("{b:02x}")).collect::<String>())
    };
    format!("{label}.{base}")
}

/// Volume context: the volume's NFS server has a principal of its own. Set
/// at CreateVolume, so a volume made before keeps the shared principal its
/// nodes were told about, whichever controller recreates its server.
pub const VOLUME_KEY_CONTEXT: &str = "nfs.chert.us/krb5-volume-key";

/// CreateVolume: mark a new volume for a principal of its own, when the
/// controller makes them ([`ServerSecurity::per_volume`]).
pub fn mark_new_volume(volume_context: &mut std::collections::HashMap<String, String>) {
    if ServerSecurity::from_env().per_volume() {
        volume_context.insert(VOLUME_KEY_CONTEXT.to_string(), "true".to_string());
    }
}

/// The volume's `KerberosPrincipal` and its keytab Secret.
pub fn volume_principal_name(volume_id: &str) -> String {
    format!("flint-nfs-{volume_id}")
}
pub fn volume_keytab_secret(volume_id: &str) -> String {
    format!("flint-nfs-krb5-{volume_id}")
}

/// Where an NFS server pod has the LDAP directory's CA bundle, as `ca.crt`.
pub const LDAP_CA_DIR: &str = "/etc/flint/ldap-ca";

/// Whether NFS mount options ask for Kerberos (`sec=krb5`, `krb5i`,
/// `krb5p`, or a list naming one).
pub fn is_krb5_sec(mount_opts: &str) -> bool {
    mount_opts
        .split(',')
        .filter_map(|o| o.trim().strip_prefix("sec="))
        .flat_map(|v| v.split(':'))
        .any(|f| matches!(f, "krb5" | "krb5i" | "krb5p"))
}

/// The server address of an NFS mount source (`10.96.0.5:/`,
/// `[fd00::5]:/x`).
fn source_ip(source: &str) -> Option<&str> {
    if let Some(rest) = source.strip_prefix('[') {
        return rest.split_once(']').map(|(ip, _)| ip);
    }
    source
        .split_once(":/")
        .map(|(ip, _)| ip)
        .filter(|ip| !ip.is_empty())
}

/// The server IPs of the Kerberos NFS mounts in `mounts` (`/proc/mounts`).
pub fn krb5_mount_ips(mounts: &str) -> BTreeSet<String> {
    mounts
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (source, fstype, opts) = (f.first()?, f.get(2)?, f.get(3)?);
            (fstype.starts_with("nfs") && is_krb5_sec(opts))
                .then(|| source_ip(source).map(str::to_string))?
        })
        .collect()
}

/// Our lines in `hosts`: IP to name.
pub fn our_entries(hosts: &str) -> BTreeMap<String, String> {
    hosts
        .lines()
        .filter_map(|l| l.trim_end().strip_suffix(HOSTS_MARKER))
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.to_string(), f.next()?.to_string()))
        })
        .collect()
}

/// `hosts` with our lines replaced: `<ip> <name>` for each of `entries`,
/// after everything that is not ours.
pub fn hosts_with(hosts: &str, entries: &BTreeMap<String, String>) -> String {
    let mut out: String = hosts
        .lines()
        .filter(|l| !l.trim_end().ends_with(HOSTS_MARKER))
        .map(|l| format!("{l}\n"))
        .collect();
    for (ip, name) in entries {
        out.push_str(&format!("{ip} {name} {HOSTS_MARKER}\n"));
    }
    out
}

/// Make `ip` resolve to `name` (and back) on the host before a Kerberos
/// mount of it: `hosts_file` keeps our line for `ip` and, as they are, our
/// lines for the IPs in `live` (the Kerberos mounts the node has), and no
/// others. Written in
/// place, under an exclusive lock: the host's `/etc/hosts` is a single-file
/// bind mount here (and in kind nodes), which a rename cannot replace.
/// Whether it changed the file.
pub fn ensure_host_entry(
    hosts_file: &Path,
    ip: &str,
    name: &str,
    live: &BTreeSet<String>,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(hosts_file)?;
    // SAFETY: flock on a descriptor this function owns; released on close.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let current = std::fs::read_to_string(hosts_file)?;
    let mut entries: BTreeMap<String, String> = our_entries(&current)
        .into_iter()
        .filter(|(i, _)| live.contains(i))
        .collect();
    entries.insert(ip.to_string(), name.to_string());
    let next = hosts_with(&current, &entries);
    if next == current {
        return Ok(false);
    }
    f.set_len(0)?;
    f.rewind()?;
    f.write_all(next.as_bytes())?;
    f.sync_all()?;
    Ok(true)
}

/// The node plugin, before mounting `server_ip` with `mount_opts`: for a
/// Kerberos mount, the hosts entry rpc.gssd needs, naming the volume's
/// server `server_name` (from the publish context), or the shared name for
/// a server that has none. An error is the reason the mount cannot work.
pub fn prepare_node_mount(server_ip: &str, mount_opts: &str, server_name: Option<&str>) -> Result<(), String> {
    if !is_krb5_sec(mount_opts) {
        return Ok(());
    }
    let shared = std::env::var(SERVER_NAME_ENV)
        .ok()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            format!(
                "a sec=krb5* NFS mount needs Kerberos on this node: install the chart with \
             nfs.kerberos.enabled ({SERVER_NAME_ENV} is not set)"
            )
        })?;
    let name = server_name.filter(|n| !n.is_empty()).map(str::to_string).unwrap_or(shared);
    write_host_entry(server_ip, &name)
}

/// `<server_ip> <name>` into the host's hosts file (see [`ensure_host_entry`]).
fn write_host_entry(server_ip: &str, name: &str) -> Result<(), String> {
    let hosts = std::env::var(HOSTS_FILE_ENV).unwrap_or_else(|_| DEFAULT_HOSTS_FILE.to_string());
    let live = std::fs::read_to_string("/proc/mounts")
        .map(|m| krb5_mount_ips(&m))
        .unwrap_or_default();
    match ensure_host_entry(Path::new(&hosts), server_ip, name, &live) {
        Ok(true) => eprintln!("🔐 [NFS_KRB5] {hosts}: {server_ip} is {name} for rpc.gssd"),
        Ok(false) => {}
        Err(e) => return Err(format!("writing {server_ip} {name} to {hosts}: {e}")),
    }
    Ok(())
}

/// Controller env (chart `pnfs.kerberos`): the DNS suffix the MDS shard
/// Services live under, `<namespace>.svc.<cluster domain>`. Set only with
/// pNFS Kerberos on.
pub const PNFS_MDS_NAME_BASE_ENV: &str = "PNFS_KRB5_MDS_NAME_BASE";

/// The Kerberos name of the MDS shard serving `volume_id`:
/// `flint-pnfs-mds-<shard>.<base>`, the shard from the volume id's
/// `~m<shard>` suffix (none: shard 0, the volumes made before sharding).
/// It is the shard's per-shard Service (`pnfs-mds.yaml`), whose
/// `KerberosPrincipal` is `nfs/<this name>` (`pnfs-kerberos.yaml`). Shard
/// 0 is the same name whether a PV carries the per-shard Service's
/// ClusterIP or the legacy alias `flint-pnfs-mds`'s: the node maps the
/// PV's IP to it, and rpc.gssd asks for the principal by that name.
pub fn pnfs_mds_server_name(base: &str, volume_id: &str) -> String {
    let shard = crate::pnfs_csi::parse_shard_suffix(volume_id)
        .map(|(_, s)| s)
        .unwrap_or(0);
    format!("flint-pnfs-mds-{shard}.{base}")
}

/// [`pnfs_mds_server_name`] from [`PNFS_MDS_NAME_BASE_ENV`]; `None` with
/// pNFS Kerberos off.
pub fn pnfs_mds_server_name_from_env(volume_id: &str) -> Option<String> {
    std::env::var(PNFS_MDS_NAME_BASE_ENV)
        .ok()
        .filter(|b| !b.trim().is_empty())
        .map(|b| pnfs_mds_server_name(b.trim(), volume_id))
}

/// The node plugin, before a pNFS mount of `mds_ip`: for a Kerberos mount,
/// the hosts entry rpc.gssd needs, naming the MDS shard by `mds_name` (the
/// publish context's [`SERVER_NAME_CONTEXT`], from ControllerPublish).
/// Stricter than [`prepare_node_mount`]: there is no shared name to fall
/// back to (the RWX servers' `nfs/<serverName>` is not the MDS's key), so
/// a missing name is an error, never a guess. Nothing for `sec=sys`.
pub fn prepare_pnfs_node_mount(mds_ip: &str, mount_opts: &str, mds_name: Option<&str>) -> Result<(), String> {
    let Some(name) = pnfs_krb5_mds_name(mount_opts, mds_name)? else {
        return Ok(());
    };
    write_host_entry(mds_ip, name)
}

/// The name [`prepare_pnfs_node_mount`] maps the MDS to: `None` for a
/// mount that is not Kerberos, an error for one with no name.
fn pnfs_krb5_mds_name<'a>(mount_opts: &str, mds_name: Option<&'a str>) -> Result<Option<&'a str>, String> {
    if !is_krb5_sec(mount_opts) {
        return Ok(None);
    }
    match mds_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => Ok(Some(n)),
        None => Err(format!(
            "a sec=krb5* pNFS mount needs the MDS shard's Kerberos name ({SERVER_NAME_CONTEXT} \
             in the publish context): install the chart with pnfs.kerberos.enabled, then \
             re-attach the volume (a VolumeAttachment made before has no name)"
        )),
    }
}

/// Node plugin env (chart `pnfs.kerberos`, Phase 3): the pNFS data
/// servers, as `<service>=<name>` pairs separated by commas: each Service
/// a data server is reached at (its own and its multipath ones), and the
/// name its principal is for, `nfs/<name>`
/// (`flint-pnfs-ds-<i>.flint-pnfs-ds-headless.<ns>.svc.<domain>`, which
/// krb5-operator's `hostPrincipals` mint). Unset with pNFS Kerberos off.
pub const PNFS_DS_ENV: &str = "FLINT_PNFS_KRB5_DS";

/// Marks the data servers' `/etc/hosts` lines. Not a suffix of
/// [`HOSTS_MARKER`]'s lines, nor theirs of it: the RWX/MDS writer drops its
/// lines for IPs no Kerberos mount uses, and no mount ever names a data
/// server's IP, so these must not be its lines.
pub const DS_HOSTS_MARKER: &str = "# flint-pnfs-ds-krb5";

/// [`PNFS_DS_ENV`]'s pairs, in order; malformed items are skipped.
pub fn parse_pnfs_ds(value: &str) -> Vec<(String, String)> {
    value
        .split(',')
        .filter_map(|item| {
            let (svc, name) = item.trim().split_once('=')?;
            let (svc, name) = (svc.trim(), name.trim());
            (!svc.is_empty() && !name.is_empty()).then(|| (svc.to_string(), name.to_string()))
        })
        .collect()
}

/// Marks the lines `flint-nfs-client-identity --krb5-hosts` owns: each
/// Kerberos FlintShare hub's ClusterIP and server name. Not a suffix of
/// the other markers, nor they of it.
pub const LITE_HOSTS_MARKER: &str = "# flint-lite-krb5";

/// `hosts` with the lines marked `marker` replaced by `entries` (IP to
/// name), after everything else.
pub fn marked_hosts_with(hosts: &str, marker: &str, entries: &BTreeMap<String, String>) -> String {
    let mut out: String = hosts
        .lines()
        .filter(|l| !l.trim_end().ends_with(marker))
        .map(|l| format!("{l}\n"))
        .collect();
    for (ip, name) in entries {
        out.push_str(&format!("{ip} {name} {marker}\n"));
    }
    out
}

/// Make the lines marked `marker` of `hosts_file` exactly `entries`, in
/// place under an exclusive lock (as [`ensure_host_entry`]). Whether it
/// changed the file.
pub fn ensure_marked_hosts(hosts_file: &Path, marker: &str, entries: &BTreeMap<String, String>) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(hosts_file)?;
    // SAFETY: flock on a descriptor this function owns; released on close.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let current = std::fs::read_to_string(hosts_file)?;
    let next = marked_hosts_with(&current, marker, entries);
    if next == current {
        return Ok(false);
    }
    f.set_len(0)?;
    f.rewind()?;
    f.write_all(next.as_bytes())?;
    f.sync_all()?;
    Ok(true)
}

/// `hosts` with the data servers' lines replaced by `entries`.
pub fn ds_hosts_with(hosts: &str, entries: &BTreeMap<String, String>) -> String {
    marked_hosts_with(hosts, DS_HOSTS_MARKER, entries)
}

/// [`ensure_marked_hosts`] for the data servers' lines.
pub fn ensure_ds_hosts(hosts_file: &Path, entries: &BTreeMap<String, String>) -> std::io::Result<bool> {
    ensure_marked_hosts(hosts_file, DS_HOSTS_MARKER, entries)
}

/// The node plugin, before a pNFS mount: for a Kerberos mount with the
/// data servers named ([`PNFS_DS_ENV`]), map every data server address to
/// its principal's name in the host's hosts file. The Linux client names a
/// data server by the reverse lookup of the address GETDEVICEINFO gave it,
/// so without these lines rpc.gssd asks for no principal it can get (and
/// the client then sent the data server its I/O as AUTH_SYS, plan Phase 0
/// E1). Each Service name is resolved here, through the cluster's DNS
/// (the node plugin runs ClusterFirstWithHostNet), to the ClusterIP the
/// MDS hands out. A name that does not resolve is left out, loudly; none
/// resolving is an error. Nothing for `sec=sys` or with the env unset.
pub fn prepare_pnfs_ds_hosts(mount_opts: &str) -> Result<(), String> {
    if !is_krb5_sec(mount_opts) {
        return Ok(());
    }
    let Some(pairs) = std::env::var(PNFS_DS_ENV).ok().map(|v| parse_pnfs_ds(&v)).filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    let mut entries = BTreeMap::new();
    for (svc, name) in &pairs {
        use std::net::ToSocketAddrs;
        match (svc.as_str(), 2049).to_socket_addrs() {
            Ok(addrs) => {
                for a in addrs {
                    entries.insert(a.ip().to_string(), name.clone());
                }
            }
            Err(e) => eprintln!("⚠️ [NFS_KRB5] data server {svc} does not resolve ({e}): no hosts line for it"),
        }
    }
    if entries.is_empty() {
        return Err(format!(
            "a sec=krb5* pNFS mount needs the data servers' addresses, and none of {} resolved",
            pairs.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let hosts = std::env::var(HOSTS_FILE_ENV).unwrap_or_else(|_| DEFAULT_HOSTS_FILE.to_string());
    match ensure_ds_hosts(Path::new(&hosts), &entries) {
        Ok(true) => eprintln!("🔐 [NFS_KRB5] {hosts}: {} data server address(es) named for rpc.gssd", entries.len()),
        Ok(false) => {}
        Err(e) => return Err(format!("writing the data servers' lines to {hosts}: {e}")),
    }
    Ok(())
}

/// The NFS servers' security settings, as the controller passes them on
/// (chart `nfs.kerberos`, `nfs.enforcePermissions`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerSecurity {
    /// The Secret with `nfs/<server name>`'s keytab.
    pub keytab_secret: Option<String>,
    /// `FLINT_NFS_MIN_SEC`.
    pub min_sec: Option<String>,
    /// `FLINT_NFS_GSS_PRINCIPALS`: who may connect with Kerberos.
    pub gss_principals: Option<String>,
    /// `FLINT_NFS_GSS_IDMAP`: principal → uid:gid[:groups].
    pub gss_idmap: Option<String>,
    /// `FLINT_NFS_GSS_ANON`: who an unmapped principal is.
    pub gss_anon: Option<String>,
    /// `FLINT_NFS_ENFORCE_PERMISSIONS`: off, warn or enforce.
    pub enforce_permissions: Option<String>,
    /// `FLINT_NFS_ROOT_SQUASH`: uid 0 is the anonymous identity.
    pub root_squash: Option<String>,
    /// `FLINT_NFS_ATTACHED_NODES_ONLY`: only the nodes the volume is
    /// attached to (`nfs::node_authz`).
    pub attached_nodes_only: Option<String>,
    /// `FLINT_NFS_NODE_DOMAIN`: appended to node hostnames without a dot.
    pub node_domain: Option<String>,
    /// `FLINT_NFS_REQUIRE_MACH_CRED`: every EXCHANGE_ID must ask for
    /// SP4_MACH_CRED (`CompoundDispatcher::with_require_mach_cred`).
    pub require_mach_cred: Option<String>,
    /// The NFS server pods' service account: [`Self::attached_nodes_only`]
    /// reads VolumeAttachments and Nodes.
    pub service_account: Option<String>,
    /// Per-volume server principals: the realm (`EXAMPLE.COM`), its
    /// `KerberosRealm` and that object's namespace, the shared server name
    /// the volumes' names extend, and what happens to a volume's principal
    /// when it is deleted (`Delete`, `Retain`).
    pub realm: Option<String>,
    pub realm_ref: Option<String>,
    pub realm_namespace: Option<String>,
    pub server_name: Option<String>,
    pub deletion_policy: Option<String>,
    /// The directory lookup (`nfs::gss_ldap`): each set
    /// `NFS_GSS_LDAP_<S>` of [`crate::nfs::gss_ldap::PASSED_ON`], as
    /// (`<S>`, value).
    pub gss_ldap: Vec<(String, String)>,
    /// The Secret (name, key) with the bind password: the server reads it
    /// as an env var from the Secret, so it is never in the pod spec.
    pub gss_ldap_password: Option<(String, String)>,
    /// The directory's CA bundle: a ConfigMap or a Secret, and its key.
    pub gss_ldap_ca: Option<LdapCa>,
}

/// Where the LDAP CA bundle comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LdapCa {
    ConfigMap { name: String, key: String },
    Secret { name: String, key: String },
}

impl ServerSecurity {
    /// From the controller's env (`NFS_KRB5_KEYTAB_SECRET`, `NFS_MIN_SEC`,
    /// `NFS_GSS_PRINCIPALS`, `NFS_GSS_IDMAP`, `NFS_GSS_ANON`,
    /// `NFS_ENFORCE_PERMISSIONS`, `NFS_ROOT_SQUASH`, `NFS_ATTACHED_NODES_ONLY`,
    /// `NFS_NODE_DOMAIN`, `NFS_REQUIRE_MACH_CRED`,
    /// `NFS_SERVER_SERVICE_ACCOUNT`, `NFS_GSS_LDAP_*`).
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        let gss_ldap = crate::nfs::gss_ldap::PASSED_ON
            .iter()
            .filter_map(|s| var(&format!("NFS_GSS_LDAP_{s}")).map(|v| (s.to_string(), v)))
            .collect();
        let ca_key = var("NFS_GSS_LDAP_CA_KEY").unwrap_or_else(|| "ca.crt".into());
        let gss_ldap_ca = match (var("NFS_GSS_LDAP_CA_CONFIGMAP"), var("NFS_GSS_LDAP_CA_SECRET")) {
            (Some(name), _) => Some(LdapCa::ConfigMap { name, key: ca_key }),
            (None, Some(name)) => Some(LdapCa::Secret { name, key: ca_key }),
            (None, None) => None,
        };
        ServerSecurity {
            gss_ldap,
            gss_ldap_password: var("NFS_GSS_LDAP_BIND_PASSWORD_SECRET").map(|name| {
                (
                    name,
                    var("NFS_GSS_LDAP_BIND_PASSWORD_KEY").unwrap_or_else(|| "password".into()),
                )
            }),
            gss_ldap_ca,
            keytab_secret: var("NFS_KRB5_KEYTAB_SECRET"),
            min_sec: var("NFS_MIN_SEC"),
            gss_principals: var("NFS_GSS_PRINCIPALS"),
            gss_idmap: var("NFS_GSS_IDMAP"),
            gss_anon: var("NFS_GSS_ANON"),
            enforce_permissions: var("NFS_ENFORCE_PERMISSIONS"),
            root_squash: var("NFS_ROOT_SQUASH"),
            attached_nodes_only: var("NFS_ATTACHED_NODES_ONLY"),
            node_domain: var("NFS_NODE_DOMAIN"),
            require_mach_cred: var("NFS_REQUIRE_MACH_CRED"),
            service_account: var("NFS_SERVER_SERVICE_ACCOUNT"),
            realm: var("NFS_KRB5_REALM"),
            realm_ref: var("NFS_KRB5_REALM_REF"),
            realm_namespace: var("NFS_KRB5_REALM_NAMESPACE"),
            server_name: var("NFS_KRB5_SERVER_NAME"),
            deletion_policy: var("NFS_KRB5_DELETION_POLICY"),
        }
    }

    /// Each volume gets its own server principal: the realm and the shared
    /// server name are known.
    pub fn per_volume(&self) -> bool {
        self.realm.is_some() && self.realm_ref.is_some() && self.server_name.is_some()
    }

    /// The volume's `KerberosPrincipal` (krb5-operator), when
    /// [`Self::per_volume`]: `nfs/<volume server name>` in a Secret of its
    /// own.
    pub fn volume_principal(&self, namespace: &str, volume_id: &str) -> Option<serde_json::Value> {
        if !self.per_volume() {
            return None;
        }
        let name = volume_server_name(self.server_name.as_deref()?, volume_id);
        let mut spec = serde_json::json!({
            "realmRef": self.realm_ref,
            "principals": [format!("nfs/{name}@{}", self.realm.as_deref()?)],
            "secretName": volume_keytab_secret(volume_id),
            "deletionPolicy": self.deletion_policy.as_deref().unwrap_or("Delete"),
        });
        if let Some(ns) = &self.realm_namespace {
            spec["realmNamespace"] = ns.clone().into();
        }
        Some(serde_json::json!({
            "apiVersion": "krb5.ddalton.dev/v1alpha1",
            "kind": "KerberosPrincipal",
            "metadata": {
                "name": volume_principal_name(volume_id),
                "namespace": namespace,
                "labels": {
                    "app.kubernetes.io/managed-by": "flint-csi",
                    "chert.us/volume-id": volume_id,
                },
            },
            "spec": spec,
        }))
    }

    /// The server pod's security for `volume_id`: its own keytab Secret
    /// when [`Self::per_volume`] and the volume was made for one
    /// ([`VOLUME_KEY_CONTEXT`]), with the name its key is for.
    pub fn for_volume(
        &self,
        volume_id: &str,
        volume_context: &std::collections::HashMap<String, String>,
    ) -> (ServerSecurity, Option<String>) {
        let mut sec = self.clone();
        let name = (self.per_volume() && volume_context.get(VOLUME_KEY_CONTEXT).is_some_and(|v| v == "true"))
            .then(|| volume_server_name(self.server_name.as_deref().unwrap_or_default(), volume_id));
        if name.is_some() {
            sec.keytab_secret = Some(volume_keytab_secret(volume_id));
        }
        (sec, name)
    }
}

fn principals_api(client: &kube::Client, namespace: &str) -> kube::Api<kube::api::DynamicObject> {
    use kube::api::{ApiResource, GroupVersionKind};
    let gvk = GroupVersionKind::gvk("krb5.ddalton.dev", "v1alpha1", "KerberosPrincipal");
    kube::Api::namespaced_with(client.clone(), namespace, &ApiResource::from_gvk(&gvk))
}

/// Ask krb5-operator for the volume's server principal (idempotent: an
/// existing one keeps its keys, so a recreated server pod serves the
/// tickets clients already hold). Its pod waits for the Secret.
pub async fn ensure_volume_principal(
    client: &kube::Client,
    namespace: &str,
    sec: &ServerSecurity,
    volume_id: &str,
) -> Result<(), String> {
    use kube::api::{Patch, PatchParams};
    let Some(obj) = sec.volume_principal(namespace, volume_id) else {
        return Ok(());
    };
    principals_api(client, namespace)
        .patch(
            &volume_principal_name(volume_id),
            &PatchParams::apply("flint-csi").force(),
            &Patch::Apply(&obj),
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("KerberosPrincipal {namespace}/{}: {e}", volume_principal_name(volume_id)))
}

/// Delete the volume's server principal (krb5-operator then deletes it
/// from the KDC, unless `Retain`). Absent is done.
pub async fn delete_volume_principal(client: &kube::Client, namespace: &str, volume_id: &str) -> Result<(), String> {
    match principals_api(client, namespace)
        .delete(&volume_principal_name(volume_id), &Default::default())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(format!("deleting KerberosPrincipal {namespace}/{}: {e}", volume_principal_name(volume_id))),
    }
}

/// What an NFS server pod gets: the keytab Secret's volume and mount with
/// `KRB5_KTNAME`, and the server's env for the rest of `sec`.
pub fn server_pod_parts(sec: &ServerSecurity) -> (Vec<EnvVar>, Vec<Volume>, Vec<VolumeMount>) {
    let mut env = Vec::new();
    let mut volumes = Vec::new();
    let mut mounts = Vec::new();
    let set = |env: &mut Vec<EnvVar>, name: &str, v: &Option<String>| {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
            env.push(EnvVar {
                name: name.into(),
                value: Some(v.into()),
                ..Default::default()
            });
        }
    };
    if let Some(secret) = sec.keytab_secret.as_deref().filter(|s| !s.is_empty()) {
        env.push(EnvVar {
            name: "KRB5_KTNAME".into(),
            value: Some(format!("{KEYTAB_DIR}/krb5.keytab")),
            ..Default::default()
        });
        volumes.push(Volume {
            name: "krb5-keytab".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.into()),
                // The server runs as 65532; the Secret's files are root's.
                default_mode: Some(0o444),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(VolumeMount {
            name: "krb5-keytab".into(),
            mount_path: KEYTAB_DIR.into(),
            read_only: Some(true),
            ..Default::default()
        });
    }
    set(
        &mut env,
        crate::nfs::gss_principals::ENV,
        &sec.gss_principals,
    );
    set(&mut env, crate::nfs::gss_idmap::ENV, &sec.gss_idmap);
    set(&mut env, crate::nfs::gss_idmap::ANON_ENV, &sec.gss_anon);
    set(&mut env, "FLINT_NFS_MIN_SEC", &sec.min_sec);
    set(
        &mut env,
        "FLINT_NFS_ENFORCE_PERMISSIONS",
        &sec.enforce_permissions,
    );
    set(&mut env, crate::nfs::gss_idmap::ROOT_SQUASH_ENV, &sec.root_squash);
    set(&mut env, crate::nfs::node_authz::ENV, &sec.attached_nodes_only);
    set(&mut env, crate::nfs::node_authz::DOMAIN_ENV, &sec.node_domain);
    set(
        &mut env,
        crate::nfs::v4::operations::session::REQUIRE_MACH_CRED_ENV,
        &sec.require_mach_cred,
    );
    // The directory lookup: its settings as they are, the bind password
    // from its Secret, the CA bundle as a file.
    for (s, v) in &sec.gss_ldap {
        set(&mut env, &format!("FLINT_NFS_GSS_LDAP_{s}"), &Some(v.clone()));
    }
    if let Some((name, key)) = &sec.gss_ldap_password {
        env.push(EnvVar {
            name: crate::nfs::gss_ldap::BIND_PASSWORD_ENV.into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: name.clone(),
                    key: key.clone(),
                    optional: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(ca) = &sec.gss_ldap_ca {
        let item = |key: &str| {
            Some(vec![KeyToPath {
                key: key.into(),
                path: "ca.crt".into(),
                ..Default::default()
            }])
        };
        let mut volume = Volume {
            name: "ldap-ca".into(),
            ..Default::default()
        };
        match ca {
            LdapCa::ConfigMap { name, key } => {
                volume.config_map = Some(ConfigMapVolumeSource {
                    name: name.clone(),
                    items: item(key),
                    default_mode: Some(0o444),
                    ..Default::default()
                })
            }
            LdapCa::Secret { name, key } => {
                volume.secret = Some(SecretVolumeSource {
                    secret_name: Some(name.clone()),
                    items: item(key),
                    default_mode: Some(0o444),
                    ..Default::default()
                })
            }
        }
        volumes.push(volume);
        mounts.push(VolumeMount {
            name: "ldap-ca".into(),
            mount_path: LDAP_CA_DIR.into(),
            read_only: Some(true),
            ..Default::default()
        });
        env.push(EnvVar {
            name: crate::nfs::gss_ldap::CA_FILE_ENV.into(),
            value: Some(format!("{LDAP_CA_DIR}/ca.crt")),
            ..Default::default()
        });
    }
    (env, volumes, mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kerberos_flavors_are_recognized() {
        for yes in [
            "vers=4.2,sec=krb5",
            "sec=krb5i,noresvport",
            "vers=4.2,noresvport,sec=krb5p",
            "sec=sys:krb5i",
        ] {
            assert!(is_krb5_sec(yes), "{yes}");
        }
        for no in [
            "vers=4.2,noresvport,sec=sys",
            "",
            "sec=none",
            "krb5i",
            "secure=krb5",
        ] {
            assert!(!is_krb5_sec(no), "{no}");
        }
    }

    #[test]
    fn the_kerberos_mounts_are_found_in_proc_mounts() {
        let mounts = "\
10.96.0.5:/ /var/lib/kubelet/pods/a/volumes/x nfs4 rw,relatime,vers=4.2,sec=krb5i,addr=10.96.0.5 0 0
10.96.0.6:/ /var/lib/kubelet/pods/b/volumes/y nfs4 rw,relatime,vers=4.2,sec=sys,addr=10.96.0.6 0 0
[fd00::7]:/ /mnt/z nfs4 rw,vers=4.2,sec=krb5p 0 0
/dev/sda1 / ext4 rw 0 0
10.96.0.5:/ /var/lib/kubelet/pods/c/volumes/x nfs4 rw,vers=4.2,sec=krb5i 0 0
";
        let ips: Vec<String> = krb5_mount_ips(mounts).into_iter().collect();
        assert_eq!(ips, vec!["10.96.0.5", "fd00::7"]);
    }

    #[test]
    fn only_our_lines_change() {
        let name = "flint-nfs.flint-system.svc.cluster.local";
        let base = "127.0.0.1 localhost\n10.0.0.4 node-a\n";
        let e = |v: &[&str]| v.iter().map(|ip| (ip.to_string(), name.to_string())).collect::<BTreeMap<_, _>>();
        let one = hosts_with(base, &e(&["10.96.0.5"]));
        assert_eq!(one, format!("{base}10.96.0.5 {name} {HOSTS_MARKER}\n"));
        assert_eq!(our_entries(&one), e(&["10.96.0.5"]), "read back");
        // Another volume: both; one unmounted: its line goes.
        let two = hosts_with(&one, &e(&["10.96.0.5", "10.96.0.9"]));
        assert!(two.starts_with(base) && two.matches(HOSTS_MARKER).count() == 2);
        assert_eq!(hosts_with(&two, &e(&["10.96.0.9"])), format!("{base}10.96.0.9 {name} {HOSTS_MARKER}\n"));
        // None: the file as it was.
        assert_eq!(hosts_with(&two, &BTreeMap::new()), base);
        // A file without a final newline keeps its lines.
        assert_eq!(hosts_with("127.0.0.1 localhost", &BTreeMap::new()), "127.0.0.1 localhost\n");
    }

    #[test]
    fn each_volume_server_has_its_own_name() {
        let base = "flint-nfs.flint-system.svc.cluster.local";
        let a = volume_server_name(base, "pvc-0a1b2c3d-1111-2222-3333-444455556666");
        assert_eq!(a, format!("pvc-0a1b2c3d-1111-2222-3333-444455556666.{base}"));
        assert_ne!(a, volume_server_name(base, "pvc-ffffffff-1111-2222-3333-444455556666"));
        assert_eq!(volume_server_name(base, "PVC-X"), format!("pvc-x.{base}"), "lower case");
        // Not a DNS label: a digest, stable and still a label.
        for bad in ["a_b", "-x", &"x".repeat(64), "a.b"] {
            let n = volume_server_name(base, bad);
            let label = n.strip_suffix(&format!(".{base}")).unwrap();
            assert!(label.starts_with("v-") && label.len() == 18, "{bad}: {n}");
            assert_eq!(n, volume_server_name(base, bad));
        }
    }

    #[test]
    fn a_new_volume_asks_for_its_own_principal_and_an_old_one_does_not() {
        let sec = ServerSecurity {
            keytab_secret: Some("flint-nfs-krb5-keytab".into()),
            realm: Some("CORP.TEST".into()),
            realm_ref: Some("corp".into()),
            realm_namespace: Some("auth".into()),
            server_name: Some("flint-nfs.fs.svc.cluster.local".into()),
            ..Default::default()
        };
        assert!(sec.per_volume());
        let p = sec.volume_principal("fs", "pvc-1").unwrap();
        assert_eq!(p["kind"], "KerberosPrincipal");
        assert_eq!(p["metadata"]["name"], "flint-nfs-pvc-1");
        assert_eq!(p["metadata"]["namespace"], "fs");
        assert_eq!(p["spec"]["principals"][0], "nfs/pvc-1.flint-nfs.fs.svc.cluster.local@CORP.TEST");
        assert_eq!(p["spec"]["secretName"], "flint-nfs-krb5-pvc-1");
        assert_eq!((&p["spec"]["realmRef"], &p["spec"]["realmNamespace"]), (&"corp".into(), &"auth".into()));
        assert_eq!(p["spec"]["deletionPolicy"], "Delete");

        let new: std::collections::HashMap<String, String> =
            [(VOLUME_KEY_CONTEXT.to_string(), "true".to_string())].into();
        let (s, name) = sec.for_volume("pvc-1", &new);
        assert_eq!(name.as_deref(), Some("pvc-1.flint-nfs.fs.svc.cluster.local"));
        assert_eq!(s.keytab_secret.as_deref(), Some("flint-nfs-krb5-pvc-1"), "its own keytab, not the shared one");
        // Made before: the shared key its nodes know, whoever recreates it.
        let (s, name) = sec.for_volume("pvc-1", &Default::default());
        assert_eq!((name, s.keytab_secret.as_deref()), (None, Some("flint-nfs-krb5-keytab")));
        // Without the realm, nothing per volume.
        let shared_only = ServerSecurity { realm: None, ..sec.clone() };
        assert!(!shared_only.per_volume() && shared_only.volume_principal("fs", "pvc-1").is_none());
        assert_eq!(shared_only.for_volume("pvc-1", &new).1, None);
    }

    #[test]
    fn the_host_file_is_rewritten_in_place_only_on_change() {
        use std::os::unix::fs::MetadataExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("hosts");
        std::fs::write(&p, "127.0.0.1 localhost\n").unwrap();
        let ino = std::fs::metadata(&p).unwrap().ino();
        let name = "flint-nfs.ns.svc.cluster.local";
        // An older line for a mount still live keeps its own name.
        std::fs::write(&p, format!("127.0.0.1 localhost\n10.96.0.4 shared.name {HOSTS_MARKER}\n")).unwrap();
        let old = ["10.96.0.4".to_string()].into_iter().collect();
        assert!(ensure_host_entry(&p, "10.96.0.5", "pvc-5.name", &old).unwrap());
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("10.96.0.4 shared.name") && s.contains("10.96.0.5 pvc-5.name"), "{s}");
        std::fs::write(&p, "127.0.0.1 localhost\n").unwrap();
        assert!(ensure_host_entry(&p, "10.96.0.5", name, &BTreeSet::new()).unwrap());
        assert!(
            !ensure_host_entry(&p, "10.96.0.5", name, &BTreeSet::new()).unwrap(),
            "already there"
        );
        let live = ["10.96.0.5".to_string()].into_iter().collect();
        assert!(ensure_host_entry(&p, "10.96.0.6", name, &live).unwrap());
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.starts_with("127.0.0.1 localhost\n"));
        assert!(
            s.contains(&format!("10.96.0.5 {name}")) && s.contains(&format!("10.96.0.6 {name}"))
        );
        // A volume no longer mounted with Kerberos loses its line.
        assert!(ensure_host_entry(&p, "10.96.0.6", name, &BTreeSet::new()).unwrap());
        assert!(!std::fs::read_to_string(&p).unwrap().contains("10.96.0.5"));
        assert_eq!(
            std::fs::metadata(&p).unwrap().ino(),
            ino,
            "the same file: a bind mount cannot be renamed over"
        );
    }

    #[test]
    fn a_sys_mount_needs_nothing_and_a_kerberos_one_needs_the_chart() {
        assert!(prepare_node_mount("10.96.0.5", "vers=4.2,sec=sys", None).is_ok());
        // This test alone touches SERVER_NAME_ENV.
        std::env::remove_var(SERVER_NAME_ENV);
        let e = prepare_node_mount("10.96.0.5", "vers=4.2,sec=krb5i", Some("pvc-1.x")).unwrap_err();
        assert!(e.contains("nfs.kerberos.enabled"), "{e}");
    }

    #[test]
    fn the_server_pod_gets_the_keytab_and_its_security_env() {
        let sec = ServerSecurity {
            keytab_secret: Some("flint-nfs-krb5-keytab".into()),
            min_sec: Some("krb5i".into()),
            gss_principals: Some("host/*@FLINT.TEST".into()),
            gss_idmap: Some("host/*@FLINT.TEST=0:0".into()),
            gss_anon: Some("65534:65534".into()),
            enforce_permissions: Some("enforce".into()),
            root_squash: Some("true".into()),
            attached_nodes_only: Some("true".into()),
            node_domain: Some("corp.example".into()),
            require_mach_cred: Some("true".into()),
            service_account: Some("flint-nfs-server".into()),
            ..Default::default()
        };
        let (env, vols, mounts) = server_pod_parts(&sec);
        let get = |n: &str| {
            env.iter()
                .find(|e| e.name == n)
                .and_then(|e| e.value.clone())
        };
        assert_eq!(
            get("KRB5_KTNAME").as_deref(),
            Some("/etc/flint/krb5/krb5.keytab"),
            "a bare path, no FILE:"
        );
        assert_eq!(get("FLINT_NFS_MIN_SEC").as_deref(), Some("krb5i"));
        assert_eq!(
            get("FLINT_NFS_GSS_PRINCIPALS").as_deref(),
            Some("host/*@FLINT.TEST")
        );
        assert_eq!(
            get("FLINT_NFS_GSS_IDMAP").as_deref(),
            Some("host/*@FLINT.TEST=0:0")
        );
        assert_eq!(get("FLINT_NFS_GSS_ANON").as_deref(), Some("65534:65534"));
        assert_eq!(
            get("FLINT_NFS_ENFORCE_PERMISSIONS").as_deref(),
            Some("enforce")
        );
        assert_eq!(get("FLINT_NFS_ROOT_SQUASH").as_deref(), Some("true"));
        assert_eq!(get("FLINT_NFS_ATTACHED_NODES_ONLY").as_deref(), Some("true"));
        assert_eq!(get("FLINT_NFS_NODE_DOMAIN").as_deref(), Some("corp.example"));
        assert_eq!(get("FLINT_NFS_REQUIRE_MACH_CRED").as_deref(), Some("true"));
        let s = vols[0].secret.as_ref().unwrap();
        assert_eq!(s.secret_name.as_deref(), Some("flint-nfs-krb5-keytab"));
        assert_eq!(
            s.default_mode,
            Some(0o444),
            "readable by the non-root server"
        );
        assert_eq!(mounts[0].mount_path, KEYTAB_DIR);
        let (env, vols, mounts) = server_pod_parts(&ServerSecurity::default());
        assert!(
            env.is_empty() && vols.is_empty() && mounts.is_empty(),
            "nothing without settings"
        );
        let floor_only = ServerSecurity {
            keytab_secret: Some(String::new()),
            enforce_permissions: Some("warn".into()),
            ..Default::default()
        };
        let (env, vols, _) = server_pod_parts(&floor_only);
        assert_eq!(
            (env.len(), vols.len()),
            (1, 0),
            "permissions apply to sys volumes too"
        );
    }

    #[test]
    fn the_server_pod_gets_the_directory_with_its_password_from_the_secret() {
        let sec = ServerSecurity {
            gss_ldap: vec![
                ("URL".into(), "ldaps://ldap.corp.example".into()),
                ("REALMS".into(), "CORP.EXAMPLE".into()),
                ("USER_BASE".into(), "ou=People,dc=corp,dc=example".into()),
            ],
            gss_ldap_password: Some(("flint-ldap-bind".into(), "password".into())),
            gss_ldap_ca: Some(LdapCa::ConfigMap {
                name: "corp-ca".into(),
                key: "root.pem".into(),
            }),
            ..Default::default()
        };
        let (env, vols, mounts) = server_pod_parts(&sec);
        let get = |n: &str| env.iter().find(|e| e.name == n);
        assert_eq!(
            get("FLINT_NFS_GSS_LDAP_URL").and_then(|e| e.value.as_deref()),
            Some("ldaps://ldap.corp.example")
        );
        assert_eq!(
            get("FLINT_NFS_GSS_LDAP_USER_BASE").and_then(|e| e.value.as_deref()),
            Some("ou=People,dc=corp,dc=example")
        );
        let pw = get("FLINT_NFS_GSS_LDAP_BIND_PASSWORD").unwrap();
        assert_eq!(pw.value, None, "never a literal in the pod spec");
        let r = pw.value_from.as_ref().unwrap().secret_key_ref.as_ref().unwrap();
        assert_eq!((r.name.as_str(), r.key.as_str()), ("flint-ldap-bind", "password"));
        assert_eq!(
            get("FLINT_NFS_GSS_LDAP_CA_FILE").and_then(|e| e.value.as_deref()),
            Some("/etc/flint/ldap-ca/ca.crt")
        );
        let cm = vols[0].config_map.as_ref().unwrap();
        assert_eq!(cm.name, "corp-ca");
        let item = &cm.items.as_ref().unwrap()[0];
        assert_eq!((item.key.as_str(), item.path.as_str()), ("root.pem", "ca.crt"));
        assert_eq!(mounts[0].mount_path, LDAP_CA_DIR);
        // From a Secret instead.
        let sec = ServerSecurity {
            gss_ldap_ca: Some(LdapCa::Secret {
                name: "corp-ca".into(),
                key: "ca.crt".into(),
            }),
            ..Default::default()
        };
        let (_, vols, _) = server_pod_parts(&sec);
        assert_eq!(
            vols[0].secret.as_ref().unwrap().secret_name.as_deref(),
            Some("corp-ca")
        );
    }

    #[test]
    fn the_pnfs_mds_name_is_its_shard_service() {
        let base = "flint-system.svc.cluster.local";
        assert_eq!(
            pnfs_mds_server_name(base, "pvc-0a1b~m3"),
            "flint-pnfs-mds-3.flint-system.svc.cluster.local"
        );
        // No suffix: made before sharding, served by shard 0 (through the
        // legacy alias Service, whose IP the node maps to this name too).
        assert_eq!(
            pnfs_mds_server_name(base, "pvc-0a1b"),
            "flint-pnfs-mds-0.flint-system.svc.cluster.local"
        );
        assert_eq!(
            pnfs_mds_server_name(base, "pvc-0a1b~m0"),
            "flint-pnfs-mds-0.flint-system.svc.cluster.local"
        );
        // A suffix that is not one (no digits) is shard 0, as the
        // controller's own routing reads it.
        assert_eq!(
            pnfs_mds_server_name(base, "pvc~mx"),
            "flint-pnfs-mds-0.flint-system.svc.cluster.local"
        );
    }

    #[test]
    fn a_kerberos_pnfs_mount_needs_the_mds_name() {
        // sec=sys: nothing to do, name or not, and no file is touched.
        assert_eq!(pnfs_krb5_mds_name("minorversion=2,sec=sys", None), Ok(None));
        assert_eq!(pnfs_krb5_mds_name("minorversion=2,sec=sys", Some("x")), Ok(None));
        assert!(prepare_pnfs_node_mount("10.96.0.9", "minorversion=2,sec=sys,port=2049", None).is_ok());
        // sec=krb5*: the publish context's name, never a fallback.
        for opts in ["minorversion=2,sec=krb5i", "sec=krb5", "sec=krb5p,port=2049"] {
            let e = pnfs_krb5_mds_name(opts, None).unwrap_err();
            assert!(e.contains(SERVER_NAME_CONTEXT) && e.contains("pnfs.kerberos"), "{e}");
            assert!(pnfs_krb5_mds_name(opts, Some("  ")).is_err(), "blank is no name");
            assert_eq!(
                pnfs_krb5_mds_name(opts, Some("flint-pnfs-mds-1.ns.svc.cluster.local")),
                Ok(Some("flint-pnfs-mds-1.ns.svc.cluster.local"))
            );
        }
        // The RWX shared name (FLINT_NFS_KRB5_SERVER_NAME) is never read:
        // that key is the RWX servers', not the MDS's.
        assert!(prepare_pnfs_node_mount("10.96.0.9", "sec=krb5i", None).is_err());
    }

    /// The MDS pods get the server security env from the chart
    /// (`flint-csi-driver-chart.krb5ServerEnv`), the RWX servers from
    /// [`server_pod_parts`]. The names both must set are
    /// `tests/system/krb5-server-env-names.txt`, which
    /// `tests/system/chart-kerberos-defaults.sh` checks the chart against.
    /// The two node-gate names are left out: attached-nodes-only is not
    /// on the MDS yet (it has no attachment watcher).
    #[test]
    fn server_env_names_match_the_chart_helper() {
        let sec = ServerSecurity {
            keytab_secret: Some("k".into()),
            min_sec: Some("krb5i".into()),
            gss_principals: Some("host/*@R".into()),
            gss_idmap: Some("host/*@R=0:0".into()),
            gss_anon: Some("65534:65534".into()),
            enforce_permissions: Some("enforce".into()),
            root_squash: Some("true".into()),
            attached_nodes_only: Some("true".into()),
            node_domain: Some("corp".into()),
            require_mach_cred: Some("true".into()),
            gss_ldap: crate::nfs::gss_ldap::PASSED_ON
                .iter()
                .map(|s| (s.to_string(), "v".to_string()))
                .collect(),
            gss_ldap_password: Some(("pw".into(), "password".into())),
            gss_ldap_ca: Some(LdapCa::ConfigMap { name: "ca".into(), key: "ca.crt".into() }),
            ..Default::default()
        };
        let (env, _, _) = server_pod_parts(&sec);
        let mut got: Vec<String> = env.into_iter().map(|e| e.name).collect();
        got.sort();
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/system/krb5-server-env-names.txt");
        let mut want: Vec<String> = std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();
        want.sort();
        assert_eq!(got, want, "server_pod_parts and {file} disagree: change both, and the chart helper");
    }

    #[test]
    fn data_server_names_parse_as_service_name_pairs() {
        assert_eq!(
            parse_pnfs_ds("a.ns.svc.x=ds-0.h.ns.svc.x, a-mp1.ns.svc.x=ds-0.h.ns.svc.x,bad,=n,s=,"),
            vec![
                ("a.ns.svc.x".to_string(), "ds-0.h.ns.svc.x".to_string()),
                ("a-mp1.ns.svc.x".to_string(), "ds-0.h.ns.svc.x".to_string()),
            ]
        );
    }

    #[test]
    fn the_data_server_lines_and_the_server_lines_leave_each_other_alone() {
        let base = "127.0.0.1 localhost\n10.0.0.9 mds.ns.svc.x # flint-nfs-krb5\n10.0.0.1 old.ds # flint-pnfs-ds-krb5\n";
        let ds = BTreeMap::from([
            ("10.0.0.2".to_string(), "ds-0.h".to_string()),
            ("10.0.0.3".to_string(), "ds-0.h".to_string()),
        ]);
        let after = ds_hosts_with(base, &ds);
        assert_eq!(
            after,
            "127.0.0.1 localhost\n10.0.0.9 mds.ns.svc.x # flint-nfs-krb5\n10.0.0.2 ds-0.h # flint-pnfs-ds-krb5\n10.0.0.3 ds-0.h # flint-pnfs-ds-krb5\n"
        );
        // The server writer neither reads the data server lines as its
        // own nor drops them.
        assert_eq!(our_entries(&after), BTreeMap::from([("10.0.0.9".to_string(), "mds.ns.svc.x".to_string())]));
        let rewritten = hosts_with(&after, &BTreeMap::new());
        assert!(rewritten.contains("10.0.0.2 ds-0.h # flint-pnfs-ds-krb5"));
        assert!(!rewritten.contains("flint-nfs-krb5"));
    }

    #[test]
    fn data_server_lines_are_rewritten_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        std::fs::write(&hosts, "127.0.0.1 localhost\n").unwrap();
        let ds = BTreeMap::from([("10.0.0.2".to_string(), "ds-0.h".to_string())]);
        assert!(ensure_ds_hosts(&hosts, &ds).unwrap());
        assert!(!ensure_ds_hosts(&hosts, &ds).unwrap(), "unchanged the second time");
        assert_eq!(
            std::fs::read_to_string(&hosts).unwrap(),
            "127.0.0.1 localhost\n10.0.0.2 ds-0.h # flint-pnfs-ds-krb5\n"
        );
    }
}
