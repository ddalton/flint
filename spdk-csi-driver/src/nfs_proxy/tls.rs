//! RPC-with-TLS (RFC 9289) at the proxy (nfs-proxy design §6a).
//!
//! The kernel client (`xprtsec=mtls`, with `tlshd` on the node) opens a
//! plain TCP connection and sends ONE RPC first: a NULL call whose
//! credential flavor is `AUTH_TLS`. The server answers it with an
//! `AUTH_NONE` verifier whose body is the eight bytes `STARTTLS`, and
//! both sides then run a TLS handshake on the same socket; every later
//! RPC record travels inside TLS. The first bytes on the wire are an RPC
//! call, not a ClientHello, so nothing between client and proxy can
//! route or terminate this: both ingress paths carry it as opaque TCP.
//!
//! The proxy requires a client certificate that chains to the configured
//! CA. The certificate's URI SANs are the connection's identity, and the
//! identity rules (`clients:`) key on them — whichever path the
//! connection took, because the proof is end to end. TLS 1.3 only
//! (RFC 9289 §5.1), ALPN `sunrpc` (§5.2).
//!
//! The three files are cert-manager's (a `Certificate` Secret and a
//! trust-manager `Bundle`): they are re-read on a timer and a new
//! configuration takes over for NEW connections. An existing connection
//! keeps what it handshook with. A file that fails to parse keeps the
//! previous configuration (and says so): a half-written rotation must
//! not take the listener down.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::ServerConfig;
use serde::Deserialize;
use tracing::{info, warn};

use crate::nfs::rpc::{Auth, AuthFlavor, CallMessage, ReplyBuilder};

/// RFC 9289 §4.1: the verifier body of the reply to the `AUTH_TLS` probe.
pub const STARTTLS: &[u8] = b"STARTTLS";

/// RFC 9289 §5.2.
pub const ALPN_SUNRPC: &[u8] = b"sunrpc";

fn default_reload_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// The proxy's certificate chain (PEM), leaf first.
    pub cert: PathBuf,
    /// Its private key (PEM: PKCS#8, PKCS#1 or SEC1).
    pub key: PathBuf,
    /// The CA bundle client certificates must chain to (PEM).
    pub client_ca: PathBuf,
    /// How often the three files are re-read.
    #[serde(default = "default_reload_secs")]
    pub reload_secs: u64,
}

/// The live TLS configuration, replaced whole on a successful reload.
pub struct Tls {
    cfg: TlsConfig,
    current: RwLock<(Arc<ServerConfig>, u64)>,
}

fn read(p: &PathBuf) -> Result<Vec<u8>, String> {
    std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn fingerprint(parts: &[&[u8]]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

/// A server configuration from PEM bytes: TLS 1.3, a REQUIRED client
/// certificate chaining to `ca`, ALPN `sunrpc`.
pub fn server_config(cert: &[u8], key: &[u8], ca: &[u8]) -> Result<ServerConfig, String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cert: {e}"))?;
    if chain.is_empty() {
        return Err("cert: no certificate in the file".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| format!("key: {e}"))?;
    let mut roots = rustls::RootCertStore::empty();
    let mut n = 0;
    for c in CertificateDer::pem_slice_iter(ca) {
        let c = c.map_err(|e| format!("clientCa: {e}"))?;
        roots.add(c).map_err(|e| format!("clientCa: {e}"))?;
        n += 1;
    }
    if n == 0 {
        return Err("clientCa: no certificate in the file".into());
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    // `builder_with_provider`: the process may hold both providers (see
    // `install_crypto_provider`); naming one here needs neither default.
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| format!("clientCa: {e}"))?;
    let mut sc = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("tls: {e}"))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|e| format!("cert/key: {e}"))?;
    sc.alpn_protocols = vec![ALPN_SUNRPC.to_vec()];
    Ok(sc)
}

impl Tls {
    pub fn load(cfg: &TlsConfig) -> Result<Arc<Self>, String> {
        let (sc, fp) = Self::read_config(cfg)?;
        Ok(Arc::new(Tls { cfg: cfg.clone(), current: RwLock::new((Arc::new(sc), fp)) }))
    }

    fn read_config(cfg: &TlsConfig) -> Result<(ServerConfig, u64), String> {
        let (c, k, a) = (read(&cfg.cert)?, read(&cfg.key)?, read(&cfg.client_ca)?);
        let fp = fingerprint(&[&c, &k, &a]);
        Ok((server_config(&c, &k, &a)?, fp))
    }

    /// The configuration a NEW connection handshakes with.
    pub fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        tokio_rustls::TlsAcceptor::from(self.current.read().unwrap().0.clone())
    }

    /// Re-read the files. Ok(true) when a new configuration took over;
    /// Err keeps the old one.
    pub fn reload(&self) -> Result<bool, String> {
        let (c, k, a) = (read(&self.cfg.cert)?, read(&self.cfg.key)?, read(&self.cfg.client_ca)?);
        let fp = fingerprint(&[&c, &k, &a]);
        if fp == self.current.read().unwrap().1 {
            return Ok(false);
        }
        let sc = server_config(&c, &k, &a)?;
        *self.current.write().unwrap() = (Arc::new(sc), fp);
        Ok(true)
    }

    pub async fn reload_loop(self: Arc<Self>) {
        let every = Duration::from_secs(self.cfg.reload_secs.max(1));
        loop {
            tokio::time::sleep(every).await;
            match self.reload() {
                Ok(true) => info!("tls: new certificate/CA files loaded; new connections use them"),
                Ok(false) => {}
                Err(e) => warn!("tls: reload failed, keeping the previous configuration: {e}"),
            }
        }
    }
}

/// Is this call the RFC 9289 probe: NULL with an `AUTH_TLS` credential?
pub fn is_probe(call: &CallMessage) -> bool {
    call.procedure == 0 && call.cred.flavor == AuthFlavor::Tls
}

/// The reply to the probe: accepted, verifier `AUTH_NONE` with body
/// `STARTTLS`, SUCCESS, no results.
pub fn starttls_reply(xid: u32) -> Bytes {
    ReplyBuilder::success_with_verf(xid, &Auth { flavor: AuthFlavor::Null, body: Bytes::from_static(STARTTLS) }).finish()
}

/// The URI SANs of a DER certificate: the identity a client proved.
pub fn uri_sans(cert: &[u8]) -> Vec<String> {
    use x509_parser::extensions::GeneralName;
    let Ok((_, c)) = x509_parser::parse_x509_certificate(cert) else {
        return Vec::new();
    };
    let Ok(Some(san)) = c.subject_alternative_name() else {
        return Vec::new();
    };
    san.value
        .general_names
        .iter()
        .filter_map(|g| match g {
            GeneralName::URI(u) => Some(u.to_string()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod testpki {
    //! A throwaway CA, a server certificate and client certificates.
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, SanType};

    pub struct Ca {
        pub pem: String,
        cert: rcgen::Certificate,
        key: KeyPair,
    }

    pub struct Leaf {
        pub cert_pem: String,
        pub key_pem: String,
    }

    pub fn ca(name: &str) -> Ca {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        let key = KeyPair::generate().unwrap();
        let cert = p.self_signed(&key).unwrap();
        Ca { pem: cert.pem(), cert, key }
    }

    impl Ca {
        pub fn server(&self, dns: &str) -> Leaf {
            let p = CertificateParams::new(vec![dns.to_string()]).unwrap();
            self.sign(p)
        }
        /// A client certificate; `uri` = its URI SAN, if any.
        pub fn client(&self, uri: Option<&str>) -> Leaf {
            let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
            p.distinguished_name.push(rcgen::DnType::CommonName, "client");
            if let Some(u) = uri {
                p.subject_alt_names = vec![SanType::URI(u.try_into().unwrap())];
            }
            self.sign(p)
        }
        fn sign(&self, p: CertificateParams) -> Leaf {
            let key = KeyPair::generate().unwrap();
            let cert = p.signed_by(&key, &self.cert, &self.key).unwrap();
            Leaf { cert_pem: cert.pem(), key_pem: key.serialize_pem() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uri_san_is_read_from_a_client_certificate() {
        let ca = testpki::ca("ca");
        let leaf = ca.client(Some("spiffe://clusters/a"));
        let der = CertificateDer::from_pem_slice(leaf.cert_pem.as_bytes()).unwrap();
        assert_eq!(uri_sans(&der), vec!["spiffe://clusters/a".to_string()]);
        // Control: a certificate without one has no identity.
        let bare = ca.client(None);
        let der = CertificateDer::from_pem_slice(bare.cert_pem.as_bytes()).unwrap();
        assert!(uri_sans(&der).is_empty());
    }

    #[test]
    fn the_starttls_reply_carries_the_token_in_an_auth_none_verifier() {
        let r = starttls_reply(0x1234);
        // xid, REPLY, MSG_ACCEPTED, verf { AUTH_NONE, opaque "STARTTLS" }, SUCCESS
        let mut want = Vec::new();
        for w in [0x1234u32, 1, 0, 0, 8] {
            want.extend_from_slice(&w.to_be_bytes());
        }
        want.extend_from_slice(b"STARTTLS");
        want.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(r.as_ref(), want.as_slice());
    }

    #[test]
    fn a_bad_file_on_reload_keeps_the_previous_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let ca = testpki::ca("ca");
        let srv = ca.server("proxy");
        let w = |n: &str, s: &str| std::fs::write(dir.path().join(n), s).unwrap();
        w("tls.crt", &srv.cert_pem);
        w("tls.key", &srv.key_pem);
        w("ca.crt", &ca.pem);
        let cfg = TlsConfig {
            cert: dir.path().join("tls.crt"),
            key: dir.path().join("tls.key"),
            client_ca: dir.path().join("ca.crt"),
            reload_secs: 1,
        };
        let t = Tls::load(&cfg).unwrap();
        assert_eq!(t.reload(), Ok(false), "unchanged files: nothing to do");
        w("ca.crt", "not a pem");
        assert!(t.reload().is_err(), "a half-written file is refused");
        let srv2 = ca.server("proxy");
        w("ca.crt", &ca.pem);
        w("tls.crt", &srv2.cert_pem);
        w("tls.key", &srv2.key_pem);
        assert_eq!(t.reload(), Ok(true), "a rotation takes over");
    }
}
