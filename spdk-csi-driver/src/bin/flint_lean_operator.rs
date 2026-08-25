//! `flint-lean-operator` — the FlintLeanWorkspace controller.
//!
//! Ships in the flint-lite-operator image (same crate, same build; the
//! chart picks the binary — the flint-hub-gateway precedent) but runs
//! a SEPARATE controller: no FlintShare coupling, no hub lifecycle, no
//! per-workspace Deployments/PVCs at all. Duties per reconcile: claim
//! stamping with both adopt arms, bucket posture (operator principal),
//! the stale-MPU sweep, status. The mutating webhook (native-sidecar
//! injection) serves over TLS with operator-generated certs persisted
//! in a Secret — no cert-manager dependency; see
//! `lean_operator::webhook`.
//!
//! Environment:
//!   FLINT_LEAN_OP_NAMESPACE        restrict the watch (unset = all)
//!   FLINT_LEAN_OP_IDENTITY         audit tag stamped into created claims
//!                                  (default: the pod hostname)
//!   FLINT_LEAN_WEBHOOK_SERVICE     our Service name; unset = webhook off
//!   FLINT_LEAN_WEBHOOK_NAMESPACE   Service namespace (default flint-system)
//!   FLINT_LEAN_WEBHOOK_LISTEN      default 0.0.0.0:9443
//!   FLINT_LEAN_SIDECAR_IMAGE       injected sidecar image default

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::{watcher, Controller};
use kube::{Client, ResourceExt};
use tokio::sync::Mutex;
use tracing::{info, warn};

use spdk_csi_driver::lean_operator::crd::{FlintLeanWorkspace, FlintLeanWorkspaceStatus};
use spdk_csi_driver::lean_operator::reconcile::verify_workspace;
use spdk_csi_driver::tier::store::s3::S3Store;
use spdk_csi_driver::tier::store::ObjectStore;

struct Ctx {
    client: Client,
    identity: String,
    /// One store client per (bucket, endpoint) — the OPERATOR principal
    /// (ambient credentials), never a workspace's sidecar secret.
    stores: Mutex<BTreeMap<(String, Option<String>), Arc<dyn ObjectStore>>>,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("store: {0}")]
    Store(String),
}

async fn store_for(ctx: &Ctx, bucket: &str, endpoint: Option<&str>) -> Result<Arc<dyn ObjectStore>, Error> {
    let key = (bucket.to_string(), endpoint.map(|s| s.to_string()));
    let mut stores = ctx.stores.lock().await;
    if let Some(s) = stores.get(&key) {
        return Ok(s.clone());
    }
    let s = S3Store::connect(bucket.to_string(), endpoint.map(|s| s.to_string()))
        .await
        .map_err(|e| Error::Store(e.to_string()))?;
    let s: Arc<dyn ObjectStore> = Arc::new(s);
    stores.insert(key, s.clone());
    Ok(s)
}

async fn reconcile(ws: Arc<FlintLeanWorkspace>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = ws.namespace().unwrap_or_default();
    let name = ws.name_any();
    let store = store_for(&ctx, &ws.spec.bucket, ws.spec.endpoint.as_deref()).await?;

    let (phase, message, standing) = verify_workspace(
        &store,
        &ws.spec.key_prefix,
        &ws.spec.project_id,
        &ctx.identity,
    )
    .await
    .map_err(|e| Error::Store(e.to_string()))?;

    let refused = phase == "Refused";
    let status = FlintLeanWorkspaceStatus {
        phase: Some(phase.clone()),
        message: Some(message.clone()),
        standing_project_id: standing,
        last_verified_unix: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ),
    };
    let api: Api<FlintLeanWorkspace> = Api::namespaced(ctx.client.clone(), &ns);
    api.patch_status(
        &name,
        &PatchParams::apply("flint-lean-operator"),
        &Patch::Merge(serde_json::json!({ "status": status })),
    )
    .await?;
    if refused {
        warn!("workspace {ns}/{name}: {message}");
        // A refusal only clears when the standing claim is removed
        // explicitly; re-check on a slow cadence.
        return Ok(Action::requeue(Duration::from_secs(600)));
    }
    info!("workspace {ns}/{name}: {phase} — {message}");
    Ok(Action::requeue(Duration::from_secs(1800)))
}

fn error_policy(ws: Arc<FlintLeanWorkspace>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    warn!("reconcile {} failed: {err}", ws.name_any());
    Action::requeue(Duration::from_secs(60))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    spdk_csi_driver::install_crypto_provider();
    let client = Client::try_default().await?;

    let identity = std::env::var("FLINT_LEAN_OP_IDENTITY")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "flint-lean-operator".into());

    // The CRD is applied from the compiled-in copy at startup (the
    // lite operator's posture: the chart's crds/ copy is bootstrap,
    // the binary is the source of truth).
    {
        use kube::api::PostParams;
        let crds: Api<
            k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
        > = Api::all(client.clone());
        let crd = spdk_csi_driver::lean_operator::crd::crd();
        match crds.create(&PostParams::default(), &crd).await {
            Ok(_) => info!("FlintLeanWorkspace CRD created"),
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                let name = "flintleanworkspaces.flint.io";
                crds.patch(
                    name,
                    &PatchParams::apply("flint-lean-operator").force(),
                    &Patch::Apply(&crd),
                )
                .await?;
                info!("FlintLeanWorkspace CRD updated");
            }
            Err(e) => return Err(e.into()),
        }
    }

    // The mutating webhook: enabled when the chart names our Service.
    // Cert material lives in a Secret (replicas share it); the
    // registration (caBundle included) is server-side-applied here.
    if let Ok(service) = std::env::var("FLINT_LEAN_WEBHOOK_SERVICE") {
        let ns = std::env::var("FLINT_LEAN_WEBHOOK_NAMESPACE")
            .unwrap_or_else(|_| "flint-system".into());
        let listen: std::net::SocketAddr = std::env::var("FLINT_LEAN_WEBHOOK_LISTEN")
            .unwrap_or_else(|_| "0.0.0.0:9443".into())
            .parse()?;
        let image = std::env::var("FLINT_LEAN_SIDECAR_IMAGE")
            .unwrap_or_else(|_| "flint-sync:latest".into());
        let bundle =
            spdk_csi_driver::lean_operator::webhook::ensure_cert_secret(&client, &ns, &service)
                .await?;
        spdk_csi_driver::lean_operator::webhook::ensure_webhook_config(
            &client, &ns, &service, &bundle.ca_pem,
        )
        .await?;
        let wh_client = client.clone();
        tokio::spawn(spdk_csi_driver::lean_operator::webhook::serve(
            wh_client,
            spdk_csi_driver::lean_operator::inject::InjectDefaults { image },
            listen,
            bundle,
        ));
        info!("lean webhook serving on {listen} as {service}.{ns}.svc");
    } else {
        warn!("FLINT_LEAN_WEBHOOK_SERVICE unset — running WITHOUT the injection webhook");
    }

    let workspaces: Api<FlintLeanWorkspace> = match std::env::var("FLINT_LEAN_OP_NAMESPACE") {
        Ok(ns) => Api::namespaced(client.clone(), &ns),
        Err(_) => Api::all(client.clone()),
    };
    let ctx = Arc::new(Ctx { client, identity, stores: Mutex::new(BTreeMap::new()) });

    info!("flint-lean-operator: watching FlintLeanWorkspace");
    Controller::new(workspaces, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("controller: {e:?}");
            }
        })
        .await;
    Ok(())
}
