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
use spdk_csi_driver::nfs_proxy::server::{Proxy, ProxyConfig};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let raw = std::fs::read_to_string(&args.config)?;
    let cfg: ProxyConfig = serde_yaml::from_str(&raw)?;
    // Before any state exists: every lease the proxy hands out uses it.
    // The same as the hubs' (ProxyConfig::lease_secs says why).
    spdk_csi_driver::nfs::v4::state::lease::set_lease_time(std::time::Duration::from_secs(cfg.lease_secs));
    let proxy = Proxy::new(&cfg).await.map_err(anyhow::Error::msg)?;
    proxy.serve(&cfg.listen).await?;
    Ok(())
}
