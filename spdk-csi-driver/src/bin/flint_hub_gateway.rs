//! `flint-hub-gateway` — the externally-reachable door in front of the
//! fleet's hub file APIs.
//!
//! Design and rationale: `spdk_csi_driver::lite_gateway` and
//! `docs/flint-hub-gateway.md`. This file is wiring.
//!
//! It ships in the `flint-lite-operator` image rather than one of its
//! own. Same crate, same build, and one fewer image to publish, sign
//! and keep in step at release; the chart chooses which binary a pod
//! runs. The two processes share nothing at runtime — different
//! ServiceAccount, different RBAC, different pods.
//!
//! ## Refuses to start rather than start wrong
//!
//! Four of them, and each closes a hole that would otherwise be
//! discovered in production:
//!
//! - **No inbound token ⇒ exit.** The hub can decline to mount its
//!   routes when it has no token (404, not 401); a gateway has no such
//!   posture — it would simply be an open proxy to every project in the
//!   cluster.
//! - **No outbound credential ⇒ exit.** Neither a root key nor a shared
//!   token means every upstream call would be unauthenticated, and the
//!   hubs would answer 401 in a loop that looks like a bug in the hubs.
//! - **A root key shorter than 32 bytes ⇒ exit.** The whole fleet's
//!   credentials derive from it.
//! - **The inbound and outbound credentials being the same value ⇒
//!   exit.** That would hand every caller of the gateway a token the
//!   hubs accept directly, which is the one thing this component exists
//!   to prevent.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use futures::StreamExt;
use kube::runtime::{reflector, watcher, WatchStreamExt};
use kube::{Api, Client};
use spdk_csi_driver::lite_gateway::derive::{self, Binding, Minter};
use spdk_csi_driver::lite_gateway::{proxy, Config, Gateway};
use spdk_csi_driver::lite_operator::crd::FlintShare;
use spdk_csi_driver::pnfs::mds::fileapi::token::{self, TokenSource};
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "flint-hub-gateway", version)]
struct Args {
    #[arg(long, env = "FLINT_GATEWAY_LISTEN", default_value = "0.0.0.0:8090")]
    listen: SocketAddr,

    /// Namespace to resolve shares in. Unset = every namespace, and
    /// then a project id claimed by two shares is REFUSED rather than
    /// tie-broken. Set it if the fleet lives in one namespace.
    #[arg(long, env = "FLINT_GATEWAY_NAMESPACE")]
    namespace: Option<String>,

    /// Prepended to a project id to derive its FlintShare name, for
    /// shares that predate the `flint.io/project-id` label.
    #[arg(long, env = "FLINT_GATEWAY_SHARE_PREFIX", default_value = "fs-")]
    share_prefix: String,

    /// File holding the token CALLERS must present. Projected from a
    /// Secret as a whole directory (never `subPath`, which freezes at
    /// pod start), so it is re-read every 10s and a rotation needs no
    /// restart.
    #[arg(long, env = "FLINT_GATEWAY_TOKEN_FILE")]
    token_file: Option<String>,

    /// Fallback for the inbound token. Boot-time only — a process's
    /// environment cannot change under it.
    #[arg(long, env = "FLINT_GATEWAY_TOKEN")]
    token: Option<String>,

    /// File holding the HMAC root key the per-share tokens derive from.
    /// The single most sensitive thing this process holds.
    #[arg(long, env = "FLINT_GATEWAY_ROOT_KEY_FILE")]
    root_key_file: Option<String>,

    /// One token accepted by every hub, instead of deriving per share.
    /// Simpler, and it gives up single-project revocation.
    #[arg(long, env = "FLINT_GATEWAY_HUB_TOKEN")]
    hub_token: Option<String>,

    /// Seconds a request waits for a parked share before answering 503.
    /// The wake is armed either way and persists, so a timeout costs a
    /// retry rather than the wake. 0 = never wait.
    #[arg(long, env = "FLINT_GATEWAY_WAKE_WAIT_SECS", default_value_t = 25)]
    wake_wait_secs: u64,

    /// Proxy no mutating operation. A browse UI needs none, and this is
    /// the difference between a compromise that reads every project and
    /// one that rewrites them.
    #[arg(long, env = "FLINT_GATEWAY_READ_ONLY", action = clap::ArgAction::Set, default_value_t = false)]
    read_only: bool,

    /// Largest upload accepted, in bytes. Matches the hub's own default.
    #[arg(long, env = "FLINT_GATEWAY_MAX_UPLOAD_BYTES", default_value_t = 5 * 1024 * 1024 * 1024)]
    max_upload_bytes: u64,

    /// How long to wait for a hub's response HEADERS. The body streams
    /// untimed after that, so this does not cap a large download.
    #[arg(long, env = "FLINT_GATEWAY_UPSTREAM_TIMEOUT_SECS", default_value_t = 30)]
    upstream_timeout_secs: u64,

    /// Print the token for one share's identity and exit, instead of
    /// serving.
    ///
    /// This exists so the derivation has ONE implementation. Whoever
    /// provisions a share has to write the same value into that share's
    /// Secret, and a second implementation of an HMAC that must agree
    /// byte-for-byte is a fleet-wide outage waiting for a typo. Takes
    /// `<endpoint>,<bucket>,<keyPrefix>,<version>`; endpoint is empty
    /// for real S3.
    #[arg(long, value_name = "ENDPOINT,BUCKET,PREFIX,VERSION")]
    derive_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let minter = build_minter(&args)?;

    if let Some(spec) = args.derive_token.as_deref() {
        println!("{}", derive_one(&minter, spec)?);
        return Ok(());
    }

    // Before any TLS: two rustls providers are in this crate's tree, so
    // the process default has to be chosen explicitly or the kube
    // client construction below panics outright.
    spdk_csi_driver::install_crypto_provider();

    let inbound = build_inbound(&args)?;
    reject_shared_credential(&inbound, &minter)?;
    let _refresher = token::spawn_refresher(inbound.clone());

    let client = Client::try_default().await?;
    let shares: Api<FlintShare> = match &args.namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    };
    // Fail fast and legibly if the CRD is not served, rather than
    // watch-erroring forever behind a Ready probe that passes.
    shares.list(&kube::api::ListParams::default().limit(1)).await?;

    let (store, writer) = reflector::store::<FlintShare>();
    let ready = Arc::new(AtomicBool::new(false));

    let gw = Arc::new(Gateway {
        client: client.clone(),
        store: store.clone(),
        cfg: Config {
            namespace: args.namespace.clone(),
            share_name_prefix: args.share_prefix.clone(),
            wake_wait: Duration::from_secs(args.wake_wait_secs),
            read_only: args.read_only,
            max_upload_bytes: args.max_upload_bytes,
            upstream_timeout: Duration::from_secs(args.upstream_timeout_secs),
        },
        minter,
        inbound,
        http: proxy::upstream_client(Duration::from_secs(5))?,
        ready: ready.clone(),
    });

    // The reflector is the ONLY thing that reads shares. Every request
    // is answered from this cache, so a burst of UI traffic costs the
    // API server nothing — and the gateway keeps answering through an
    // API server blip on whatever it last saw, which for a read is the
    // right failure direction.
    tokio::spawn(async move {
        let stream = watcher(shares, watcher::Config::default())
            .default_backoff()
            .reflect(writer)
            .applied_objects();
        futures::pin_mut!(stream);
        while let Some(ev) = stream.next().await {
            if let Err(e) = ev {
                warn!("share watch: {e}");
            }
        }
        error!("share watch ended — the cache is now frozen");
    });

    {
        let store = store.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            store.wait_until_ready().await.ok();
            ready.store(true, Ordering::Relaxed);
            info!("share cache listed — serving");
        });
    }

    info!(
        listen = %args.listen,
        namespace = %args.namespace.clone().unwrap_or_else(|| "<all>".into()),
        read_only = args.read_only,
        "flint-hub-gateway starting"
    );
    warp::serve(proxy::routes(gw)).run(args.listen).await;
    Ok(())
}

/// How this process produces the credential it presents to a hub.
fn build_minter(args: &Args) -> anyhow::Result<Minter> {
    match (&args.root_key_file, &args.hub_token) {
        (Some(_), Some(_)) => anyhow::bail!(
            "--root-key-file and --hub-token are alternatives; passing both leaves it \
             ambiguous which credential the fleet's hubs were provisioned with"
        ),
        (Some(path), None) => {
            let raw = std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("root key file {path}: {e}"))?;
            // Trim only trailing whitespace a file editor or
            // `--from-literal` adds. Interior bytes are key material.
            let key: Vec<u8> = {
                let mut k = raw;
                while matches!(k.last(), Some(b'\n' | b'\r' | b' ' | b'\t')) {
                    k.pop();
                }
                k
            };
            if key.len() < 32 {
                anyhow::bail!(
                    "root key is {} bytes; at least 32 are required — every hub credential \
                     in the fleet derives from it",
                    key.len()
                );
            }
            Ok(Minter::Derived(key))
        }
        (None, Some(t)) if !t.trim().is_empty() => Ok(Minter::Shared(t.trim().to_string())),
        _ => anyhow::bail!(
            "no upstream credential: pass --root-key-file (derive one token per share) or \
             --hub-token (one token for every hub). Without one every call to a hub would \
             be unauthenticated."
        ),
    }
}

/// The credential callers must present to the gateway.
fn build_inbound(args: &Args) -> anyhow::Result<Arc<TokenSource>> {
    if let Some(path) = args.token_file.as_deref().filter(|p| !p.is_empty()) {
        let t = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("gateway token file {path}: {e}"))?;
        if t.trim().is_empty() {
            anyhow::bail!("gateway token file {path} is empty");
        }
        return Ok(TokenSource::new(t.trim(), Some(path.into())));
    }
    if let Some(t) = args.token.as_deref().filter(|t| !t.trim().is_empty()) {
        warn!(
            "inbound token came from the environment — it cannot be rotated without a \
             restart. Prefer --token-file with a projected Secret."
        );
        return Ok(TokenSource::fixed(t.trim()));
    }
    anyhow::bail!(
        "no inbound token: pass --token-file or $FLINT_GATEWAY_TOKEN. There is no \
         unauthenticated mode — this process can reach every project's files."
    )
}

/// The inbound and outbound credentials must not be the same value.
///
/// If they were, every caller of the gateway would be holding a token
/// the hubs accept directly — so the `/status` route the gateway
/// refuses to proxy would be reachable by going around it, and so would
/// every other project's hub. Cheap to check, and it is exactly the
/// configuration a hurried install produces by pasting one secret into
/// two places.
fn reject_shared_credential(inbound: &Arc<TokenSource>, minter: &Minter) -> anyhow::Result<()> {
    if let Minter::Shared(hub) = minter {
        if *inbound.current() == **hub {
            anyhow::bail!(
                "the inbound gateway token and --hub-token are the same value. Every caller \
                 would then hold a credential the hubs accept directly, bypassing this \
                 gateway entirely. Use two different secrets."
            );
        }
    }
    Ok(())
}

/// `--derive-token <endpoint>,<bucket>,<prefix>,<version>`.
fn derive_one(minter: &Minter, spec: &str) -> anyhow::Result<String> {
    let parts: Vec<&str> = spec.split(',').collect();
    let [endpoint, bucket, prefix, version] = parts.as_slice() else {
        anyhow::bail!(
            "--derive-token wants exactly <endpoint>,<bucket>,<keyPrefix>,<version> \
             (endpoint empty for real S3); got {} field(s)",
            parts.len()
        );
    };
    let version: u64 = version
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("version must be a positive integer, got {version:?}"))?;
    if version < 1 {
        anyhow::bail!("version starts at 1");
    }
    let b = Binding { endpoint, bucket, key_prefix: prefix, version };
    match minter {
        Minter::Derived(root) => Ok(derive::derive(root, &b)),
        Minter::Shared(t) => Ok(t.clone()),
    }
}
