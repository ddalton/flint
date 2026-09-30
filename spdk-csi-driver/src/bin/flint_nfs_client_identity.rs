//! flint-nfs-client-identity: one per client node (a DaemonSet in each
//! cluster that mounts flint through flint-nfs-proxy with xprtsec=mtls).
//! Keeps the node's tlshd certificate equal to a cert-manager Secret and
//! reports whether the node can mount (nfs_client_identity.rs says how).
//!
//!   flint-nfs-client-identity --secret-dir /var/run/flint/client-tls \
//!       --install-dir /host/etc/flint/nfs-tls --host-dir /etc/flint/nfs-tls \
//!       [--tlshd-conf /proc/1/root/etc/tlshd.conf] [--node-label]
//!   flint-nfs-client-identity --check     (the readiness probe)

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use clap::Parser;
use spdk_csi_driver::nfs_client_identity::{reconcile, AgentConfig};

/// The node label a workload can require (nodeAffinity) so it is not
/// scheduled where its mount would fail.
const READY_LABEL: &str = "chert.us/nfs-tls-ready";

#[derive(Parser)]
struct Args {
    /// Exit 0 iff the last pass found the node ready (readiness probe).
    #[arg(long)]
    check: bool,
    /// One pass, then exit 0 iff ready (drills, debugging on a node).
    #[arg(long)]
    once: bool,
    #[arg(long, default_value = "/run/flint-nfs-client-identity/status")]
    status_file: PathBuf,
    #[arg(long, default_value = "/var/run/flint/client-tls")]
    secret_dir: PathBuf,
    #[arg(long, default_value = "tls.crt")]
    cert_key: String,
    #[arg(long, default_value = "tls.key")]
    key_key: String,
    #[arg(long, default_value = "ca.crt")]
    ca_key: String,
    #[arg(long, default_value = "/host/etc/flint/nfs-tls")]
    install_dir: PathBuf,
    #[arg(long, default_value = "/etc/flint/nfs-tls")]
    host_dir: PathBuf,
    /// Manage the host's tlshd.conf (as this process sees it).
    #[arg(long)]
    tlshd_conf: Option<PathBuf>,
    #[arg(long, default_value = "nsenter -t 1 -m -u -i -n -p -- systemctl restart tlshd")]
    restart_cmd: String,
    #[arg(long, default_value = "/proc")]
    proc_root: PathBuf,
    #[arg(long, default_value = "/proc/sys/kernel/osrelease")]
    kernel_release: PathBuf,
    #[arg(long, default_value_t = 30)]
    interval_secs: u64,
    /// Label this node (NODE_NAME) chert.us/nfs-tls-ready=true|false.
    #[arg(long)]
    node_label: bool,
}

async fn label(client: &kube::Client, node: &str, ready: bool) -> Result<(), kube::Error> {
    use k8s_openapi::api::core::v1::Node;
    use kube::api::{Api, Patch, PatchParams};
    let p = serde_json::json!({ "metadata": { "labels": { READY_LABEL: ready.to_string() } } });
    Api::<Node>::all(client.clone()).patch(node, &PatchParams::default(), &Patch::Merge(&p)).await.map(|_| ())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    spdk_csi_driver::install_crypto_provider();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    if args.check {
        let s = std::fs::read_to_string(&args.status_file).unwrap_or_default();
        print!("{s}");
        std::process::exit(if s.starts_with("ready\n") { 0 } else { 1 });
    }
    let cfg = AgentConfig {
        secret_dir: args.secret_dir,
        cert_key: args.cert_key,
        key_key: args.key_key,
        ca_key: args.ca_key,
        install_dir: args.install_dir,
        host_dir: args.host_dir,
        tlshd_conf: args.tlshd_conf,
        restart: args.restart_cmd.split_whitespace().map(String::from).collect(),
        proc_root: args.proc_root,
        kernel_release: args.kernel_release,
    };
    let node = std::env::var("NODE_NAME").ok();
    let client = match (args.node_label, &node) {
        (true, Some(_)) => Some(kube::Client::try_default().await?),
        (true, None) => anyhow::bail!("--node-label needs NODE_NAME (the downward API's spec.nodeName)"),
        _ => None,
    };
    if let Some(dir) = args.status_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut last: Option<(bool, Vec<String>)> = None;
    let mut labelled: Option<bool> = None;
    loop {
        let s = reconcile(&cfg, SystemTime::now());
        std::fs::write(&args.status_file, s.render())?;
        if args.once {
            print!("{}", s.render());
            std::process::exit(if s.ready { 0 } else { 1 });
        }
        let now = (s.ready, s.reasons.clone());
        if last.as_ref() != Some(&now) {
            if s.ready {
                tracing::info!("node ready: identity {:?}", s.identity);
            } else {
                tracing::warn!("node NOT ready: {}", s.reasons.join("; "));
            }
            last = Some(now);
        }
        if let (Some(c), Some(n)) = (&client, &node) {
            if labelled != Some(s.ready) {
                match label(c, n, s.ready).await {
                    Ok(()) => labelled = Some(s.ready),
                    Err(e) => tracing::warn!("label {n} {READY_LABEL}={}: {e}", s.ready),
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(args.interval_secs.max(1))).await;
    }
}
