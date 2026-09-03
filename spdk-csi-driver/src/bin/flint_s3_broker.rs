//! `flint-s3-broker` — the STS-shaped identity exchange for `s3.csi.chert.us`.
//! See `spdk_csi_driver::s3csi::broker`.
//!
//! Environment:
//!   FLINT_S3B_LISTEN                 0.0.0.0:8080
//!   FLINT_S3B_TLS_CERT / _TLS_KEY    serve https when both set
//!   FLINT_S3B_BACKEND                static | sts | rest
//!   FLINT_S3B_STATIC_ACCESS_KEY_ID / _STATIC_SECRET_ACCESS_KEY / _STATIC_SESSION_TOKEN
//!   FLINT_S3B_STS_URL / _STS_ROLE_ARN
//!   FLINT_S3B_REST_URL / _REST_HEADERS ("K=V;K2=V2")
//!   FLINT_S3B_AUDIENCE               s3.csi.chert.us
//!   FLINT_S3B_NODE_PRINCIPAL         system:serviceaccount:flint-system:flint-s3-csi-node
//!   FLINT_S3B_DEFAULT_LIFETIME_SECS  900
//!   FLINT_S3B_MAX_LIFETIME_SECS      3600
//!   FLINT_S3B_REQUIRE_REGISTRATION   true

use spdk_csi_driver::s3csi::broker::{Broker, BrokerConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    spdk_csi_driver::install_crypto_provider();
    let cfg = BrokerConfig::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = kube::Client::try_default().await?;
    tracing::info!(backend = ?cfg.backend, listen = %cfg.listen, node_principal = %cfg.node_principal, "flint-s3-broker starting");
    Broker::new(cfg, client).serve().await;
    Ok(())
}
