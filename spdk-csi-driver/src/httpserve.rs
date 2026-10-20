//! Serving warp filters on hyper 1 (warp 0.4).
//!
//! warp 0.4 dropped three things this crate used from 0.3, and this
//! module is where they live now:
//!
//! - **A bind that reports its port.** `bind_ephemeral` /
//!   `try_bind_ephemeral` are gone; warp 0.4 binds inside `run` and
//!   panics on a taken port. [`listener`] binds first — fallibly, and
//!   with the bound address in hand — and the caller hands the listener
//!   to `warp::serve(f).incoming(l).run()`.
//! - **TLS.** `serve(f).tls()` is gone. [`tls_acceptor`] +
//!   [`serve_tls`] serve a `warp::service(f)` over tokio-rustls on the
//!   aws-lc-rs provider the rest of the crate names (see
//!   `install_crypto_provider`), ALPN `h2` then `http/1.1` — what 0.3
//!   offered.
//!
//! The helpers take a listener or an already-built service rather than
//! a filter: warp's `serve` bounds name a crate-private trait, so a
//! function generic over filters cannot state them. Each call site
//! builds `warp::service(routes)` where the concrete type satisfies
//! them.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// How long a client may take to finish the TLS handshake before its
/// connection is dropped. A stalled handshake otherwise holds a task
/// and a socket for as long as the peer cares to wait.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A reply that streams `stream` as its body, unbuffered.
///
/// warp 0.4 does not export its body type, and `warp::reply::stream`
/// wants a `Sync` stream, which reqwest's `bytes_stream` (and most boxed
/// streams) are not. `SyncStream` makes a `Send` stream `Sync` the sound
/// way: it only ever hands out `&mut` access.
pub fn stream_response<S, B, E>(stream: S) -> warp::reply::Response
where
    S: futures::Stream<Item = Result<B, E>> + Send + 'static,
    B: Into<bytes::Bytes> + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
{
    warp::Reply::into_response(warp::reply::stream(sync_wrapper::SyncStream::new(stream)))
}

/// Bind `addr` now and return the address actually bound (port 0 picks
/// one) with a tokio listener for `warp::serve(f).incoming(..)`.
///
/// Must be called inside a tokio runtime.
pub fn listener(addr: impl Into<SocketAddr>) -> std::io::Result<(SocketAddr, tokio::net::TcpListener)> {
    let std_listener = std::net::TcpListener::bind(addr.into())?;
    std_listener.set_nonblocking(true)?;
    let bound = std_listener.local_addr()?;
    Ok((bound, tokio::net::TcpListener::from_std(std_listener)?))
}

/// A TLS acceptor from a PEM certificate chain and a PEM private key.
pub fn tls_acceptor(cert_path: &str, key_path: &str) -> Result<tokio_rustls::TlsAcceptor, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| format!("read {cert_path}: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("parse {cert_path}: {e}"))?;
    if certs.is_empty() {
        return Err(format!("{cert_path} holds no certificate"));
    }
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| format!("read {key_path}: {e}"))?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("tls versions: {e}"))?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| format!("{cert_path} / {key_path}: {e}"))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

/// Serve `svc` (normally `warp::service(routes)`) over TLS on
/// `listener`, forever. A failed or stalled handshake drops that
/// connection only.
pub async fn serve_tls<S, B>(listener: tokio::net::TcpListener, acceptor: tokio_rustls::TlsAcceptor, svc: S)
where
    S: tower::Service<http::Request<hyper::body::Incoming>, Response = http::Response<B>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: http_body::Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                // EMFILE and friends: back off rather than spin.
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let svc = svc.clone();
        tokio::spawn(async move {
            let tls = match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => {
                    tracing::debug!("tls handshake from {peer} failed: {e}");
                    return;
                }
                Err(_) => {
                    tracing::debug!("tls handshake from {peer} timed out");
                    return;
                }
            };
            let svc = hyper_util::service::TowerToHyperService::new(svc);
            if let Err(e) = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                .await
            {
                tracing::debug!("connection from {peer}: {e}");
            }
        });
    }
}
