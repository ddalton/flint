//! flint-nfs-proxy: many lite hubs behind one NFSv4.1 port
//! (docs/plans/flint-lite-nfs-proxy-design.md).
//!
//!   flint-nfs-proxy --config /etc/flint-nfs-proxy/config.yaml
//!
//! The config names the listen address, the state directory (the
//! persisted client table), the hubs (name, address, serverId,
//! stateidTag) and the identities allowed to see them. In a cluster the
//! hub rows come from FlintShare status instead (step 4).

use clap::Parser;
use spdk_csi_driver::nfs_proxy::kube as kube_source;
use spdk_csi_driver::nfs_proxy::kube::KubeWaker;
use spdk_csi_driver::nfs_proxy::server::{Proxy, ProxyConfig};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before any kube client exists (the 1.26/1.27 startup panic; the
    // lib's crypto_provider_tests guard every binary for it).
    spdk_csi_driver::install_crypto_provider();
    let mut filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // tlshd presents the server's IP address as SNI, which RFC 6066
    // forbids and rustls warns about on EVERY handshake. Harmless (the
    // proxy serves one certificate); silenced unless asked for.
    if !std::env::var("RUST_LOG").unwrap_or_default().contains("rustls") {
        filter = filter.add_directive("rustls::msgs::handshake=error".parse()?);
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let args = Args::parse();
    let raw = std::fs::read_to_string(&args.config)?;
    let cfg: ProxyConfig = serde_yaml::from_str(&raw)?;
    // Before any state exists: every lease the proxy hands out uses it.
    // The same as the hubs' (ProxyConfig::lease_secs says why).
    spdk_csi_driver::nfs::v4::state::lease::set_lease_time(std::time::Duration::from_secs(cfg.lease_secs));
    let proxy = match &cfg.kube {
        None => Proxy::new(&cfg).await.map_err(anyhow::Error::msg)?,
        Some(k) => {
            let client = kube::Client::try_default().await?;
            let p = Proxy::new_with(&cfg, std::sync::Arc::new(KubeWaker { client: client.clone() }))
                .await
                .map_err(anyhow::Error::msg)?;
            let (table, ns) = (p.table().clone(), k.namespace.clone());
            tokio::spawn(async move {
                if let Err(e) = kube_source::watch(client, ns, table, std::time::Duration::from_secs(2)).await {
                    // Without the watch the root is empty forever; die so
                    // the Deployment restarts us and the failure is visible.
                    tracing::error!("FlintShare watch failed: {e}");
                    std::process::exit(1);
                }
            });
            p
        }
    };
    proxy.serve(&cfg.listen).await?;
    Ok(())
}
