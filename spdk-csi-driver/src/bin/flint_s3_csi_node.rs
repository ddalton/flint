//! `flint-s3-csi-node` — the `s3.flint.io` CSI node plugin, one per node.
//!
//! Node-only: Identity + Node services over the kubelet plugin socket;
//! no controller service, no attach. See `spdk_csi_driver::s3csi`.
//!
//! Environment:
//!   FLINT_S3CSI_ENDPOINT             unix:///csi/csi.sock
//!   FLINT_S3CSI_NODE_NAME            REQUIRED (downward API spec.nodeName)
//!   FLINT_S3CSI_PASSTHROUGH_IMAGE    REQUIRED — the worker image every
//!                                    passthrough volume runs (chart-pinned;
//!                                    never from a CR)
//!   FLINT_S3CSI_LEAN_IMAGE           the lean worker image
//!   FLINT_S3CSI_WORKER_NAMESPACE     flint-workers
//!   FLINT_S3CSI_WORKER_RESOURCES     ResourceRequirements JSON
//!   FLINT_S3CSI_BROKER_URL           unset ⇒ static/ambient only
//!   FLINT_S3CSI_BROKER_CA            PEM path for an https broker
//!   FLINT_S3CSI_NODE_TOKEN_FILE      /var/run/secrets/flint-s3/token
//!   FLINT_S3CSI_CREDS_LIFETIME_SECS  900
//!   FLINT_S3CSI_REGION               us-east-1 (AWS_REGION handed to workers)
//!   FLINT_S3CSI_WORKER_PRIORITY_CLASS  unset
//!   FLINT_S3CSI_COMM_SIZE            16Mi (the memory-backed comm emptyDir)
//!   FLINT_S3CSI_SCRATCH_SIZE         1Gi  (the worker's /tmp)
//!   FLINT_S3CSI_KUBELET_ROOT         /var/lib/kubelet
//!   FLINT_S3CSI_PLUGIN_ROOT          <kubelet root>/plugins/s3.flint.io

use kube::api::Api;
use tracing::{info, warn};

use spdk_csi_driver::csi::identity_server::IdentityServer;
use spdk_csi_driver::csi::node_server::NodeServer;
use spdk_csi_driver::s3csi::node::{Config, S3Identity, S3Node};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    spdk_csi_driver::install_crypto_provider();

    let mut cfg = Config::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = kube::Client::try_default().await?;
    match Api::<k8s_openapi::api::core::v1::Node>::all(client.clone()).get(&cfg.node_name).await {
        Ok(n) => cfg.node_uid = n.metadata.uid,
        Err(e) => warn!("cannot read Node {} (workers will carry no ownerReference): {e}", cfg.node_name),
    }
    std::fs::create_dir_all(cfg.plugin_root.join("volumes"))?;
    let endpoint = std::env::var("FLINT_S3CSI_ENDPOINT").unwrap_or_else(|_| "unix:///csi/csi.sock".into());
    info!(
        node = %cfg.node_name, workers = %cfg.worker_namespace, image = %cfg.passthrough_image,
        broker = cfg.broker.as_ref().map(|b| b.base_url().to_string()).unwrap_or_else(|| "none".into()),
        plugin_root = %cfg.plugin_root.display(), "flint-s3-csi-node starting"
    );

    let node = S3Node::new(cfg, client);
    node.adopt_existing().await;

    let router = tonic::transport::Server::builder()
        .add_service(IdentityServer::new(S3Identity))
        .add_service(NodeServer::new(node));

    let Some(socket_path) = endpoint.strip_prefix("unix://") else {
        anyhow::bail!("FLINT_S3CSI_ENDPOINT must be unix://<path>, got {endpoint}");
    };
    if std::path::Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    let stream = tokio_stream::wrappers::UnixListenerStream::new(listener);
    info!("s3.flint.io listening on {socket_path}");
    router.serve_with_incoming(stream).await?;
    Ok(())
}
