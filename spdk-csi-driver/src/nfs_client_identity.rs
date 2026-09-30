//! flint-nfs-client-identity: puts a client cluster's certificate where
//! the node's `tlshd` reads it (nfs-proxy design §6a "cert-manager").
//!
//! An `nfs:` volume is mounted by kubelet, so the NFS connection leaves
//! from the NODE's network namespace: no mesh sees it, and the only way
//! the node can prove which cluster it belongs to is RPC-with-TLS
//! (`xprtsec=mtls`). The kernel hands that handshake to `tlshd`, a host
//! daemon that reads a certificate and key from host FILES. This agent,
//! one per node, keeps those files equal to a cert-manager Secret:
//!
//! - it installs only material that validates (the key matches the
//!   certificate, the certificate is inside its validity window and names
//!   a URI SAN — the identity the proxy's `clients:` rules key on);
//!   anything else leaves the host's files as they were;
//! - it writes each file atomically (a temp file in the same directory,
//!   fsync, rename), the key 0600;
//! - optionally it points `tlshd.conf` at the files and restarts `tlshd`
//!   when — and only when — that edit changed something. A renewal does
//!   not need a restart: tlshd re-reads the files on every handshake
//!   (§6a check 5, measured);
//! - it checks the node can do this at all (kernel >= 6.5, `tlshd`
//!   running) and reports readiness, so "this node cannot mount" is
//!   visible before a mount fails with an opaque error.
//!
//! One node, one identity: Linux trunks a second mount of the same server
//! onto the first client's connection (2b, measured), so a node never
//! presents two certificates. The identity is the cluster's.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// The files the agent installs, by the name tlshd is pointed at.
pub const CERT_FILE: &str = "client.crt";
pub const KEY_FILE: &str = "client.key";
pub const CA_FILE: &str = "ca.crt";

/// RFC 9289 needs the kernel's TLS handshake upcall (net/handshake),
/// merged in 6.5; `xprtsec=` came with it.
pub const MIN_KERNEL: (u32, u32) = (6, 5);

/// What a Secret's certificate says, once it validated.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub uris: Vec<String>,
    pub not_after: SystemTime,
}

/// Validate a client certificate, its key and the CA bundle tlshd will
/// verify the SERVER with. `now` is a parameter so the window is testable.
pub fn validate(cert: &[u8], key: &[u8], ca: &[u8], now: SystemTime) -> Result<Checked, String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificate: {e}"))?;
    let leaf = chain.first().ok_or("certificate: no certificate in the file")?.clone();
    let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| format!("key: {e}"))?;
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    rustls::sign::CertifiedKey::from_der(chain, key, &provider)
        .map_err(|e| format!("the key does not belong to the certificate: {e}"))?;
    let n_ca = CertificateDer::pem_slice_iter(ca)
        .map(|c| c.map_err(|e| format!("ca: {e}")))
        .collect::<Result<Vec<_>, _>>()?
        .len();
    if n_ca == 0 {
        return Err("ca: no certificate in the file (tlshd could not verify the proxy)".into());
    }
    let (_, x) = x509_parser::parse_x509_certificate(&leaf).map_err(|e| format!("certificate: {e}"))?;
    let secs = |t: &x509_parser::time::ASN1Time| SystemTime::UNIX_EPOCH + Duration::from_secs(t.timestamp().max(0) as u64);
    let (nb, na) = (secs(&x.validity().not_before), secs(&x.validity().not_after));
    if now < nb {
        return Err("certificate: not valid yet".into());
    }
    if now >= na {
        return Err("certificate: expired".into());
    }
    let uris = crate::nfs_proxy::tls::uri_sans(&leaf);
    if uris.is_empty() {
        return Err("certificate: no URI SAN — the proxy's `clients:` rules could never match it".into());
    }
    Ok(Checked { uris, not_after: na })
}

/// Write `data` to `dir/name` atomically with `mode`. Ok(true) when the
/// file changed; an identical file is left alone (no rename, no mtime).
pub fn install_file(dir: &Path, name: &str, data: &[u8], mode: u32) -> std::io::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dst = dir.join(name);
    if std::fs::read(&dst).ok().as_deref() == Some(data) {
        let cur = std::fs::metadata(&dst)?.permissions().mode() & 0o777;
        if cur == mode {
            return Ok(false);
        }
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(mode))?;
        return Ok(true);
    }
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &dst)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(true)
}

/// `tlshd.conf` with `[authenticate.client]` pointing at `dir`'s files.
/// None when it already does. Everything else in the file is kept, line
/// for line: it is the node's, and `[authenticate.server]` may serve a
/// knfsd on the same host.
pub fn tlshd_conf_pointing_at(conf: &str, dir: &Path) -> Option<String> {
    let want = [
        ("x509.truststore", dir.join(CA_FILE)),
        ("x509.certificate", dir.join(CERT_FILE)),
        ("x509.private_key", dir.join(KEY_FILE)),
    ];
    let key_of = |line: &str| -> Option<String> {
        let t = line.trim();
        if t.starts_with('#') || t.starts_with(';') {
            return None;
        }
        t.split_once('=').map(|(k, _)| k.trim().to_string())
    };
    let mut out: Vec<String> = Vec::new();
    let mut in_client = false;
    let mut seen_client = false;
    let mut done = [false; 3];
    let flush = |out: &mut Vec<String>, done: &mut [bool; 3]| {
        for (i, (k, v)) in want.iter().enumerate() {
            if !done[i] {
                out.push(format!("{k}= {}", v.display()));
                done[i] = true;
            }
        }
    };
    for line in conf.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_client {
                flush(&mut out, &mut done);
            }
            in_client = t == "[authenticate.client]";
            seen_client |= in_client;
            out.push(line.to_string());
            continue;
        }
        if in_client {
            if let Some(k) = key_of(line) {
                if let Some(i) = want.iter().position(|(w, _)| *w == k) {
                    if !done[i] {
                        out.push(format!("{k}= {}", want[i].1.display()));
                        done[i] = true;
                    }
                    continue; // a duplicate of a managed key is dropped
                }
            }
        }
        out.push(line.to_string());
    }
    if in_client {
        flush(&mut out, &mut done);
    }
    if !seen_client {
        if !out.iter().any(|l| l.trim() == "[authenticate]") {
            out.push("[authenticate]".into());
        }
        out.push("[authenticate.client]".into());
        flush(&mut out, &mut done);
    }
    let mut new = out.join("\n");
    new.push('\n');
    (new != conf).then_some(new)
}

/// `uname -r` → (major, minor) >= MIN_KERNEL.
pub fn kernel_ok(release: &str) -> bool {
    let mut it = release.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty());
    match (it.next().and_then(|s| s.parse::<u32>().ok()), it.next().and_then(|s| s.parse::<u32>().ok())) {
        (Some(a), Some(b)) => (a, b) >= MIN_KERNEL,
        _ => false,
    }
}

/// Is a process named `tlshd` running? Needs the host's /proc (hostPID).
pub fn tlshd_running(proc_root: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(proc_root) else { return false };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "tlshd"))
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The mounted Secret (a whole-volume mount: kubelet swaps it on a
    /// renewal).
    pub secret_dir: PathBuf,
    pub cert_key: String,
    pub key_key: String,
    pub ca_key: String,
    /// Where the files land, as this process sees it (a hostPath mount).
    pub install_dir: PathBuf,
    /// The same directory as the HOST sees it: what tlshd.conf names.
    pub host_dir: PathBuf,
    /// Manage tlshd.conf (the host's, as this process sees it) and
    /// restart tlshd with `restart` when it changed. None = the node
    /// image points tlshd at `host_dir` itself.
    pub tlshd_conf: Option<PathBuf>,
    pub restart: Vec<String>,
    pub proc_root: PathBuf,
    pub kernel_release: PathBuf,
}

/// The outcome of one pass: ready or not, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub ready: bool,
    pub reasons: Vec<String>,
    pub identity: Vec<String>,
}

impl Status {
    pub fn render(&self) -> String {
        let mut s = format!("{}\n", if self.ready { "ready" } else { "not-ready" });
        for u in &self.identity {
            s.push_str(&format!("identity {u}\n"));
        }
        for r in &self.reasons {
            s.push_str(&format!("reason {r}\n"));
        }
        s
    }
}

/// One reconcile pass. Never partially installs: all three files
/// validate first, then all three are written.
pub fn reconcile(cfg: &AgentConfig, now: SystemTime) -> Status {
    let mut reasons = Vec::new();
    let mut identity = Vec::new();
    let read = |k: &str| std::fs::read(cfg.secret_dir.join(k)).map_err(|e| format!("secret {k}: {e}"));
    match (read(&cfg.cert_key), read(&cfg.key_key), read(&cfg.ca_key)) {
        (Ok(c), Ok(k), Ok(a)) => match validate(&c, &k, &a, now) {
            Ok(ch) => {
                identity = ch.uris.clone();
                if let Ok(left) = ch.not_after.duration_since(now) {
                    if left < Duration::from_secs(7 * 86400) {
                        tracing::warn!("client certificate expires in {} h: is cert-manager renewing it?", left.as_secs() / 3600);
                    }
                }
                let wrote = std::fs::create_dir_all(&cfg.install_dir).and_then(|_| {
                    let a = install_file(&cfg.install_dir, CA_FILE, &a, 0o644)?;
                    let c = install_file(&cfg.install_dir, CERT_FILE, &c, 0o644)?;
                    let k = install_file(&cfg.install_dir, KEY_FILE, &k, 0o600)?;
                    Ok(a || c || k)
                });
                match wrote {
                    Ok(true) => tracing::info!("installed the client certificate for {:?} in {}", ch.uris, cfg.host_dir.display()),
                    Ok(false) => {}
                    Err(e) => reasons.push(format!("install into {}: {e}", cfg.install_dir.display())),
                }
            }
            Err(e) => reasons.push(format!("the Secret does not validate, host files left as they were: {e}")),
        },
        (c, k, a) => {
            for e in [c.err(), k.err(), a.err()].into_iter().flatten() {
                reasons.push(e);
            }
        }
    }
    if let Some(conf) = &cfg.tlshd_conf {
        match std::fs::read_to_string(conf) {
            Ok(text) => {
                if let Some(new) = tlshd_conf_pointing_at(&text, &cfg.host_dir) {
                    let dir = conf.parent().unwrap_or(Path::new("/"));
                    let name = conf.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    match install_file(dir, &name, new.as_bytes(), 0o644) {
                        Ok(_) => {
                            tracing::info!("pointed {} at {}; restarting tlshd", conf.display(), cfg.host_dir.display());
                            match std::process::Command::new(&cfg.restart[0]).args(&cfg.restart[1..]).status() {
                                Ok(s) if s.success() => {
                                    // `systemctl restart` returns once the
                                    // child is FORKED; until it execs, its
                                    // comm is "(tlshd)" and the check below
                                    // would call the node not ready for a
                                    // whole interval (measured on the box).
                                    for _ in 0..50 {
                                        if tlshd_running(&cfg.proc_root) {
                                            break;
                                        }
                                        std::thread::sleep(Duration::from_millis(100));
                                    }
                                }
                                Ok(s) => reasons.push(format!("tlshd restart exited {s}")),
                                Err(e) => reasons.push(format!("tlshd restart: {e}")),
                            }
                        }
                        Err(e) => reasons.push(format!("write {}: {e}", conf.display())),
                    }
                }
            }
            Err(e) => reasons.push(format!("{}: {e} (is ktls-utils installed on the node?)", conf.display())),
        }
    }
    match std::fs::read_to_string(&cfg.kernel_release) {
        Ok(r) if kernel_ok(r.trim()) => {}
        Ok(r) => reasons.push(format!("kernel {} is older than {}.{}: no xprtsec=mtls", r.trim(), MIN_KERNEL.0, MIN_KERNEL.1)),
        Err(e) => reasons.push(format!("kernel release: {e}")),
    }
    if !tlshd_running(&cfg.proc_root) {
        reasons.push("tlshd is not running on this node (ktls-utils; systemctl enable --now tlshd)".into());
    }
    Status { ready: reasons.is_empty(), reasons, identity }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs_proxy::tls::testpki;

    fn now() -> SystemTime {
        SystemTime::now()
    }

    #[test]
    fn a_matching_pair_with_a_uri_validates_and_names_the_identity() {
        let ca = testpki::ca("ca");
        let l = ca.client(Some("spiffe://clusters/a"));
        let c = validate(l.cert_pem.as_bytes(), l.key_pem.as_bytes(), ca.pem.as_bytes(), now()).unwrap();
        assert_eq!(c.uris, vec!["spiffe://clusters/a"]);
    }

    #[test]
    fn what_tlshd_could_not_use_is_refused() {
        let ca = testpki::ca("ca");
        let a = ca.client(Some("spiffe://clusters/a"));
        let b = ca.client(Some("spiffe://clusters/b"));
        let e = validate(a.cert_pem.as_bytes(), b.key_pem.as_bytes(), ca.pem.as_bytes(), now()).unwrap_err();
        assert!(e.contains("does not belong"), "{e}");
        let bare = ca.client(None);
        let e = validate(bare.cert_pem.as_bytes(), bare.key_pem.as_bytes(), ca.pem.as_bytes(), now()).unwrap_err();
        assert!(e.contains("no URI SAN"), "{e}");
        let e = validate(a.cert_pem.as_bytes(), a.key_pem.as_bytes(), b"", now()).unwrap_err();
        assert!(e.contains("ca:"), "{e}");
        // rcgen's default validity ends in 4096; a clock past it expires it.
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(5000 * 365 * 86400);
        let e = validate(a.cert_pem.as_bytes(), a.key_pem.as_bytes(), ca.pem.as_bytes(), later).unwrap_err();
        assert!(e.contains("expired"), "{e}");
        let before = SystemTime::UNIX_EPOCH;
        let e = validate(a.cert_pem.as_bytes(), a.key_pem.as_bytes(), ca.pem.as_bytes(), before).unwrap_err();
        assert!(e.contains("not valid yet"), "{e}");
    }

    #[test]
    fn install_is_atomic_idempotent_and_keeps_the_key_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        assert!(install_file(d.path(), "k", b"one", 0o600).unwrap());
        assert!(!install_file(d.path(), "k", b"one", 0o600).unwrap(), "identical: untouched");
        let m = std::fs::metadata(d.path().join("k")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o600);
        assert!(install_file(d.path(), "k", b"two", 0o600).unwrap());
        assert_eq!(std::fs::read(d.path().join("k")).unwrap(), b"two");
        assert!(!d.path().join(".k.tmp").exists(), "no temp file left behind");
        // Same bytes, wrong mode: fixed.
        std::fs::set_permissions(d.path().join("k"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(install_file(d.path(), "k", b"two", 0o600).unwrap());
        assert_eq!(std::fs::metadata(d.path().join("k")).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// The shipped ktls-utils 1.0 file (Ubuntu 25.10) with its commented
    /// defaults: the client keys are set in place, the server section and
    /// every other line are kept, and a second pass changes nothing.
    #[test]
    fn tlshd_conf_is_pointed_at_the_files_and_nothing_else_moves() {
        let shipped = "[debug]\nloglevel=0\ntls=0\nnl=0\n\n[authenticate]\n#keyrings= <keyring>;<keyring>\n\n[authenticate.client]\n#x509.truststore= <pathname>\n#x509.certificate= <pathname>\n#x509.private_key= <pathname>\n\n[authenticate.server]\n#x509.truststore= <pathname>\nx509.certificate= /etc/knfsd/server.crt\n";
        let dir = Path::new("/etc/flint/nfs-tls");
        let new = tlshd_conf_pointing_at(shipped, dir).unwrap();
        assert!(new.contains("[authenticate.client]\n#x509.truststore= <pathname>\n#x509.certificate= <pathname>\n#x509.private_key= <pathname>\n\nx509.truststore= /etc/flint/nfs-tls/ca.crt\nx509.certificate= /etc/flint/nfs-tls/client.crt\nx509.private_key= /etc/flint/nfs-tls/client.key\n[authenticate.server]"), "{new}");
        assert!(new.contains("[authenticate.server]\n#x509.truststore= <pathname>\nx509.certificate= /etc/knfsd/server.crt\n"), "the server section is the node's");
        assert_eq!(tlshd_conf_pointing_at(&new, dir), None, "idempotent");
        // A conf pointing elsewhere is corrected in place, once each.
        let other = "[authenticate.client]\nx509.certificate= /old/c\nx509.certificate= /old/dup\n";
        let fixed = tlshd_conf_pointing_at(other, dir).unwrap();
        assert_eq!(fixed.matches("x509.certificate").count(), 1);
        assert!(fixed.starts_with("[authenticate.client]\nx509.certificate= /etc/flint/nfs-tls/client.crt\n"));
        // No client section at all: one is appended.
        let none = tlshd_conf_pointing_at("[debug]\nloglevel=0\n", dir).unwrap();
        assert!(none.ends_with("[authenticate]\n[authenticate.client]\nx509.truststore= /etc/flint/nfs-tls/ca.crt\nx509.certificate= /etc/flint/nfs-tls/client.crt\nx509.private_key= /etc/flint/nfs-tls/client.key\n"), "{none}");
    }

    #[test]
    fn the_kernel_floor_is_six_five() {
        assert!(kernel_ok("6.12.0-061200-generic"));
        assert!(kernel_ok("6.5.0"));
        assert!(kernel_ok("7.0.1-arch1"));
        assert!(!kernel_ok("6.4.16-200.fc38.x86_64"));
        assert!(!kernel_ok("5.15.0-1051-aws"));
        assert!(!kernel_ok("garbage"));
    }

    fn agent(dir: &Path) -> AgentConfig {
        std::fs::create_dir_all(dir.join("secret")).unwrap();
        std::fs::create_dir_all(dir.join("proc/42")).unwrap();
        std::fs::write(dir.join("proc/42/comm"), "tlshd\n").unwrap();
        std::fs::write(dir.join("osrelease"), "6.12.0\n").unwrap();
        AgentConfig {
            secret_dir: dir.join("secret"),
            cert_key: "tls.crt".into(),
            key_key: "tls.key".into(),
            ca_key: "ca.crt".into(),
            install_dir: dir.join("host/etc/flint/nfs-tls"),
            host_dir: PathBuf::from("/etc/flint/nfs-tls"),
            tlshd_conf: None,
            restart: vec!["true".into()],
            proc_root: dir.join("proc"),
            kernel_release: dir.join("osrelease"),
        }
    }

    fn put(dir: &Path, l: &testpki::Leaf, ca: &testpki::Ca) {
        std::fs::write(dir.join("secret/tls.crt"), &l.cert_pem).unwrap();
        std::fs::write(dir.join("secret/tls.key"), &l.key_pem).unwrap();
        std::fs::write(dir.join("secret/ca.crt"), &ca.pem).unwrap();
    }

    /// The loop end to end over a fake host: a valid Secret is installed
    /// and the node is ready; a Secret whose key does not match (a
    /// half-rotated one) leaves the host's files as they were and the
    /// node not ready; a renewal replaces them.
    #[test]
    fn reconcile_installs_only_what_validates() {
        let d = tempfile::tempdir().unwrap();
        let cfg = agent(d.path());
        let ca = testpki::ca("ca");
        let a = ca.client(Some("spiffe://clusters/a"));
        put(d.path(), &a, &ca);
        let s = reconcile(&cfg, now());
        assert!(s.ready, "{s:?}");
        assert_eq!(s.identity, vec!["spiffe://clusters/a"]);
        let installed = |f: &str| std::fs::read_to_string(cfg.install_dir.join(f)).unwrap();
        assert_eq!(installed(CERT_FILE), a.cert_pem);

        let b = ca.client(Some("spiffe://clusters/b"));
        std::fs::write(d.path().join("secret/tls.key"), &b.key_pem).unwrap();
        let s = reconcile(&cfg, now());
        assert!(!s.ready && s.reasons[0].contains("does not belong"), "{s:?}");
        assert_eq!(installed(KEY_FILE), a.key_pem, "the host keeps the last good pair");

        put(d.path(), &b, &ca);
        let s = reconcile(&cfg, now());
        assert!(s.ready, "{s:?}");
        assert_eq!((installed(CERT_FILE), installed(KEY_FILE)), (b.cert_pem.clone(), b.key_pem.clone()));
    }

    #[test]
    fn a_node_that_cannot_do_mtls_says_why() {
        let d = tempfile::tempdir().unwrap();
        let cfg = agent(d.path());
        let ca = testpki::ca("ca");
        put(d.path(), &ca.client(Some("spiffe://clusters/a")), &ca);
        std::fs::remove_file(d.path().join("proc/42/comm")).unwrap();
        std::fs::write(d.path().join("osrelease"), "5.15.0-1051-aws\n").unwrap();
        let s = reconcile(&cfg, now());
        assert!(!s.ready);
        assert!(s.reasons.iter().any(|r| r.contains("tlshd is not running")), "{s:?}");
        assert!(s.reasons.iter().any(|r| r.contains("older than 6.5")), "{s:?}");
        assert!(s.render().starts_with("not-ready\nidentity spiffe://clusters/a\n"));
    }

    /// Configure mode: the conf is edited and tlshd restarted ONCE; the
    /// next pass finds nothing to do and does not restart it again.
    #[test]
    fn tlshd_is_restarted_only_when_its_conf_changed() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = agent(d.path());
        let conf = d.path().join("tlshd.conf");
        std::fs::write(&conf, "[authenticate.client]\n").unwrap();
        let log = d.path().join("restarts");
        cfg.tlshd_conf = Some(conf.clone());
        cfg.restart = vec!["sh".into(), "-c".into(), format!("echo x >> {}", log.display())];
        let ca = testpki::ca("ca");
        put(d.path(), &ca.client(Some("spiffe://clusters/a")), &ca);
        assert!(reconcile(&cfg, now()).ready);
        assert!(reconcile(&cfg, now()).ready);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "x\n", "one restart for one edit");
        assert!(std::fs::read_to_string(&conf).unwrap().contains("x509.private_key= /etc/flint/nfs-tls/client.key"));
    }

    /// The client chart runs this binary out of the flint-lite-operator
    /// image, so it must pull the image the operator chart ships: the same
    /// repository at the same appVersion. release.sh then checks the binary
    /// is in that image as published.
    #[test]
    fn the_client_chart_pulls_the_operator_charts_image() {
        const NC_CHART: &str = include_str!("../../flint-nfs-client-chart/Chart.yaml");
        const OP_CHART: &str = include_str!("../../flint-lite-operator-chart/Chart.yaml");
        const NC_VALUES: &str = include_str!("../../flint-nfs-client-chart/values.yaml");
        const OP_VALUES: &str = include_str!("../../flint-lite-operator-chart/values.yaml");
        let field = |y: &str, key: &str| {
            y.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.trim().trim_matches('"').to_string())
                .unwrap_or_else(|| panic!("{key} missing"))
        };
        assert_eq!(field(NC_CHART, "appVersion:"), field(OP_CHART, "appVersion:"), "the two charts' appVersions");
        assert_eq!(field(NC_VALUES, "  repository:"), field(OP_VALUES, "  repository:"), "the two charts' image repositories");
    }
}
