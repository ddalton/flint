//! TLS on the pNFS control path (pNFS Kerberos plan, Phase 5).
//!
//! The MDS and the data servers talk gRPC: registration and heartbeats
//! (DS to MDS, port 50051), the MDS's commands to a DS (DsControl:
//! truncates, the F66 relay of a client's I/O), the CSI controller's
//! volume calls. All of it was plaintext, so whoever reached those ports
//! read and forged it: a rogue "data server" could register and attract
//! stripe writes, bypassing everything Kerberos guards on 2049.
//!
//! tonic 0.10's own TLS is built on rustls 0.21; flint is on 0.23, so the
//! TLS here is tokio-rustls 0.26 underneath tonic: [`incoming`] feeds a
//! tonic server TLS connections (`serve_with_incoming`), and [`channel`]
//! dials a tonic client through it (`connect_with_connector_lazy`).
//!
//! One CA (chart-generated, Secret `flint-pnfs-control-tls`) signs one
//! server certificate for the name [`SERVER_NAME`], which every control
//! server presents and every client checks; the MDS dials data servers by
//! IP, so names in the certificate could not be per-pod anyway. TLS gives
//! confidentiality, integrity and the server's identity; which CLIENT is
//! calling is a token's job (a pod-bound ServiceAccount token, reviewed).
//!
//! Files: `$FLINT_PNFS_CONTROL_TLS_DIR/{ca.crt, tls.crt, tls.key}`.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

/// The name in the control server certificate, which clients check.
pub const SERVER_NAME: &str = "flint-pnfs-control";
/// The directory with `ca.crt`, `tls.crt` and `tls.key`.
pub const DIR_ENV: &str = "FLINT_PNFS_CONTROL_TLS_DIR";
/// How long a client may take over its handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Both sides' TLS configuration.
#[derive(Clone)]
pub struct ControlTls {
    pub server: Arc<rustls::ServerConfig>,
    pub client: Arc<rustls::ClientConfig>,
}

impl ControlTls {
    /// From PEM: the CA every side trusts, and the server certificate and
    /// key (for the name [`SERVER_NAME`]).
    pub fn from_pem(ca: &[u8], cert: &[u8], key: &[u8]) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("tls.crt: {e}"))?;
        if chain.is_empty() {
            return Err("tls.crt: no certificate".into());
        }
        let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| format!("tls.key: {e}"))?;
        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(ca) {
            roots.add(c.map_err(|e| format!("ca.crt: {e}"))?).map_err(|e| format!("ca.crt: {e}"))?;
        }
        if roots.is_empty() {
            return Err("ca.crt: no certificate".into());
        }
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| format!("tls: {e}"))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| format!("tls.crt/tls.key: {e}"))?;
        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| format!("tls: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        // gRPC is HTTP/2.
        client.alpn_protocols = vec![b"h2".to_vec()];
        let mut server = server;
        server.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Self { server: Arc::new(server), client: Arc::new(client) })
    }

    /// From a directory with `ca.crt`, `tls.crt`, `tls.key`.
    pub fn from_dir(dir: &std::path::Path) -> Result<Self, String> {
        let read = |n: &str| std::fs::read(dir.join(n)).map_err(|e| format!("{}: {e}", dir.join(n).display()));
        Self::from_pem(&read("ca.crt")?, &read("tls.crt")?, &read("tls.key")?)
    }

    /// From [`DIR_ENV`]; `Ok(None)` when it is unset (plaintext, as
    /// before). An unreadable or bad directory is an error, never
    /// plaintext.
    pub fn from_env() -> Result<Option<Self>, String> {
        match std::env::var(DIR_ENV).ok().filter(|d| !d.trim().is_empty()) {
            None => Ok(None),
            Some(d) => Self::from_dir(std::path::Path::new(d.trim())).map(Some).map_err(|e| format!("{DIR_ENV}: {e}")),
        }
    }
}

static GLOBAL: std::sync::OnceLock<Option<ControlTls>> = std::sync::OnceLock::new();

/// Load this process's control TLS from [`DIR_ENV`] (at startup, before
/// any control listener or client). An error refuses to start.
pub fn init_from_env() -> Result<(), String> {
    let tls = ControlTls::from_env()?;
    let _ = GLOBAL.set(tls);
    Ok(())
}

/// This process's control TLS, `None` for plaintext (or before
/// [`init_from_env`]).
pub fn global() -> Option<&'static ControlTls> {
    GLOBAL.get().and_then(Option::as_ref)
}

/// A TLS connection, for tonic: the stream, and on the server side the
/// peer's address.
pub struct TlsConn<S> {
    inner: S,
    remote: Option<SocketAddr>,
}

impl<S: AsyncRead + Unpin> AsyncRead for TlsConn<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TlsConn<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// The peer of a TLS control connection.
#[derive(Clone, Debug)]
pub struct ControlPeer {
    pub remote: Option<SocketAddr>,
}

impl tonic::transport::server::Connected for TlsConn<tokio_rustls::server::TlsStream<TcpStream>> {
    type ConnectInfo = ControlPeer;
    fn connect_info(&self) -> ControlPeer {
        ControlPeer { remote: self.remote }
    }
}

/// TLS connections accepted on `listener`, for tonic's
/// `serve_with_incoming`. Each handshake runs in a task of its own, with a
/// timeout, so a slow or failing client delays nobody, and a failed
/// handshake is dropped (logged), never handed to the server: an error
/// item would stop it.
pub fn incoming(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
) -> tokio_stream::wrappers::ReceiverStream<Result<TlsConn<tokio_rustls::server::TlsStream<TcpStream>>, std::io::Error>> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    tokio::spawn(async move {
        loop {
            let (tcp, remote) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("control TLS: accept: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let (acceptor, conn_tx) = (acceptor.clone(), tx.clone());
            tokio::spawn(async move {
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                    Ok(Ok(tls)) => {
                        let _ = conn_tx.send(Ok(TlsConn { inner: tls, remote: Some(remote) })).await;
                    }
                    Ok(Err(e)) => tracing::warn!("control TLS: handshake from {remote}: {e}"),
                    Err(_) => tracing::warn!("control TLS: handshake from {remote} timed out"),
                }
            });
            if tx.is_closed() {
                return;
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// A tonic channel to `endpoint` (`host:port`, or a URI), over TLS with
/// `config`, checking the server name [`SERVER_NAME`]. Lazy, as the
/// plaintext channels it replaces: it connects on first use.
pub fn channel(endpoint: &str, config: Arc<rustls::ClientConfig>) -> Result<tonic::transport::Channel, String> {
    let hostport = endpoint
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let uri = format!("http://{hostport}");
    let ep = tonic::transport::Endpoint::from_shared(uri.clone()).map_err(|e| format!("{uri}: {e}"))?;
    let connector = tower::service_fn(move |uri: tonic::transport::Uri| {
        let config = config.clone();
        async move {
            let host = uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_string();
            let port = uri.port_u16().unwrap_or(443);
            let tcp = TcpStream::connect((host.as_str(), port)).await?;
            tcp.set_nodelay(true)?;
            let name = ServerName::try_from(SERVER_NAME).expect("a valid DNS name");
            let tls = tokio_rustls::TlsConnector::from(config).connect(name, tcp).await?;
            Ok::<_, std::io::Error>(TlsConn { inner: tls, remote: None })
        }
    });
    Ok(ep.connect_with_connector_lazy(connector))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs_proxy::tls::testpki;
    use crate::pnfs::grpc::{DsControlClient, DsControlServer, TruncateStripeFileRequest};

    fn tls_for(ca: &testpki::Ca) -> ControlTls {
        let leaf = ca.server(SERVER_NAME);
        ControlTls::from_pem(ca.pem.as_bytes(), leaf.cert_pem.as_bytes(), leaf.key_pem.as_bytes()).unwrap()
    }

    /// A DsControl server over TLS on a loopback port.
    async fn serve(tls: &ControlTls, dir: &std::path::Path) -> SocketAddr {
        crate::install_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = crate::pnfs::ds::server::DsControlService::new("ds-tls".into(), dir.to_path_buf());
        let incoming = incoming(listener, tls.server.clone());
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(DsControlServer::new(svc))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        addr
    }

    fn truncate() -> TruncateStripeFileRequest {
        TruncateStripeFileRequest { device_id: "ds-tls".into(), rel_path: "absent.stripe0".into(), new_length: 0 }
    }

    #[tokio::test]
    async fn the_control_path_speaks_grpc_over_tls_to_a_trusting_client_only() {
        let dir = tempfile::tempdir().unwrap();
        let ca = testpki::ca("control-ca");
        let tls = tls_for(&ca);
        let addr = serve(&tls, dir.path()).await;

        // A client trusting the CA: the call goes through.
        let ch = channel(&addr.to_string(), tls.client.clone()).unwrap();
        let r = DsControlClient::new(ch).truncate_stripe_file(truncate()).await;
        assert!(r.is_ok(), "{r:?}");

        // A client trusting another CA: no handshake, no call.
        let other = tls_for(&testpki::ca("other-ca"));
        let ch = channel(&addr.to_string(), other.client.clone()).unwrap();
        let r = DsControlClient::new(ch).truncate_stripe_file(truncate()).await;
        assert!(r.is_err(), "a server it does not trust");

        // A plaintext client: refused (the server speaks only TLS).
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect_lazy();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            DsControlClient::new(ch).truncate_stripe_file(truncate()),
        )
        .await;
        assert!(matches!(r, Ok(Err(_)) | Err(_)), "plaintext gets nowhere: {r:?}");

        // And the server still serves the trusting client after both.
        let ch = channel(&addr.to_string(), tls.client.clone()).unwrap();
        assert!(DsControlClient::new(ch).truncate_stripe_file(truncate()).await.is_ok());
    }

    #[test]
    fn bad_material_is_an_error_never_plaintext() {
        let ca = testpki::ca("ca");
        let leaf = ca.server(SERVER_NAME);
        assert!(ControlTls::from_pem(b"", leaf.cert_pem.as_bytes(), leaf.key_pem.as_bytes()).is_err());
        assert!(ControlTls::from_pem(ca.pem.as_bytes(), b"", leaf.key_pem.as_bytes()).is_err());
        assert!(ControlTls::from_pem(ca.pem.as_bytes(), leaf.cert_pem.as_bytes(), b"").is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(ControlTls::from_dir(dir.path()).is_err(), "missing files");
    }
}
