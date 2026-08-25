//! `flint-lean-gateway` — the lean control plane's door (plan §2.2,
//! Phase 3). Ships in the flint-lite-operator image like
//! flint-hub-gateway does: same crate, same build, the chart picks the
//! binary. Shares NO runtime state with the hub gateway and resolves
//! no FlintShare CRs — lean workspaces are bucket cells, and the
//! tenancy map is explicit configuration.
//!
//! Refuses to start rather than start wrong (the hub gateway's
//! posture):
//! - no inbound bearer ⇒ exit (an unauthenticated gateway is an open
//!   writer to every configured workspace);
//! - a bearer shorter than 16 bytes ⇒ exit;
//! - an empty workspace map ⇒ exit (nothing to serve is a config
//!   error, not a healthy idle).
//!
//! Environment:
//!   FLINT_LEAN_GW_LISTEN       default 0.0.0.0:8091
//!   FLINT_LEAN_GW_BUCKET       (required)
//!   FLINT_LEAN_GW_ENDPOINT     S3 endpoint override (MinIO/proxy)
//!   FLINT_LEAN_GW_TOKEN        (required, ≥16 bytes) inbound bearer
//!   FLINT_LEAN_GW_WORKSPACES   (required) comma list of id=prefix
//!   FLINT_LEAN_GW_MAX_PUT_MB   HITL whole-object cap (default 64)

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use flint_lean::gateway::{routes, GatewayCore};
use flint_store::s3::S3Store;
use flint_store::ObjectStore;

fn env_req(name: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!("flint-lean-gateway: {name} is required — refusing to start");
            std::process::exit(2);
        }
    }
}

#[tokio::main]
async fn main() {
    let listen: SocketAddr = std::env::var("FLINT_LEAN_GW_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8091".into())
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("flint-lean-gateway: bad listen address: {e}");
            std::process::exit(2);
        });
    let bucket = env_req("FLINT_LEAN_GW_BUCKET");
    let token = env_req("FLINT_LEAN_GW_TOKEN");
    if token.len() < 16 {
        eprintln!("flint-lean-gateway: FLINT_LEAN_GW_TOKEN shorter than 16 bytes — refusing to start");
        std::process::exit(2);
    }
    let mut workspaces = BTreeMap::new();
    for pair in env_req("FLINT_LEAN_GW_WORKSPACES").split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((id, prefix)) if !id.is_empty() && !prefix.is_empty() => {
                workspaces.insert(id.to_string(), prefix.trim_end_matches('/').to_string());
            }
            _ => {
                eprintln!("flint-lean-gateway: bad workspace mapping {pair:?} (want id=prefix)");
                std::process::exit(2);
            }
        }
    }
    if workspaces.is_empty() {
        eprintln!("flint-lean-gateway: FLINT_LEAN_GW_WORKSPACES is empty — refusing to start");
        std::process::exit(2);
    }
    let max_put_bytes = std::env::var("FLINT_LEAN_GW_MAX_PUT_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(64)
        * 1024
        * 1024;

    let endpoint = std::env::var("FLINT_LEAN_GW_ENDPOINT").ok();
    let store = match S3Store::connect(bucket, endpoint).await {
        Ok(s) => Arc::new(s) as Arc<dyn ObjectStore>,
        Err(e) => {
            eprintln!("flint-lean-gateway: store connect: {e}");
            std::process::exit(1);
        }
    };

    let n = workspaces.len();
    let core = Arc::new(GatewayCore { store, workspaces, token, max_put_bytes });
    eprintln!("flint-lean-gateway: serving {n} workspaces on {listen}");
    warp::serve(routes(core)).run(listen).await;
}
