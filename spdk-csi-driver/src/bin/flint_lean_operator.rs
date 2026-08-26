//! `flint-lean-operator` — the FlintLeanWorkspace controller.
//!
//! Ships in the operator image (same crate, same build; the chart picks
//! the binary — the flint-hub-gateway precedent), published under two
//! names from one manifest digest: flint-lite-operator and the alias
//! flint-lean-operator, which is what the lean chart names so a lean
//! install never pulls something called "lite". Runs a SEPARATE
//! controller: no FlintShare coupling, no hub lifecycle, no
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
use kube::{Client, Resource, ResourceExt};
use tokio::sync::Mutex;
use tracing::{info, warn};

use spdk_csi_driver::lean_operator::crd::{FlintLeanWorkspace, FlintLeanWorkspaceStatus};
use spdk_csi_driver::lean_operator::reconcile::full_pass;
use spdk_csi_driver::tier::store::s3::S3Store;
use spdk_csi_driver::tier::store::ObjectStore;

/// How often the expensive posture half runs (claim, sweep, probe,
/// lifecycle). Answers questions that change on the timescale of a
/// proxy upgrade or an admin edit.
const POSTURE_EVERY_SECS: u64 = 1800;

/// How often the cheap observation half runs: ONE epoch read that
/// carries the sidecar's echo (plus one orphans GET only when the lease
/// is dead). At 3,000 workspaces that is ~25 GET/s — a third of the
/// sidecars' own idle read rate (§7) — and it is what makes
/// `CITED-SEQ`, `LAG` and `STAGED` mean anything.
const OBSERVE_EVERY_SECS: u64 = 120;

struct Ctx {
    client: Client,
    identity: String,
    recorder: kube::runtime::events::Recorder,
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

    let generation = ws.metadata.generation;
    // Two cadences (see `full_pass`): the posture — claim, MPU sweep,
    // conformance probe, lifecycle read and provisioning — on the slow
    // one; the observation of what the sidecar is actually doing on
    // every pass. A `LAG` printer column refreshed every thirty
    // minutes is not a lag column.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last_posture = ws.status.as_ref().and_then(|s| s.last_verified_unix).unwrap_or(0);
    // A SPEC EDIT re-assesses the bucket immediately. Waiting out the
    // slow cadence to re-check a knob the user just changed would
    // report the old verdict for up to half an hour, which is exactly
    // when someone is watching.
    let edited = ws.metadata.generation.is_some()
        && ws.metadata.generation
            != ws
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .and_then(|c| c.iter().find(|c| c.r#type == "BoundaryModeAccepted"))
                .and_then(|c| c.observed_generation);
    let posture = edited || now.saturating_sub(last_posture) >= POSTURE_EVERY_SECS;
    let report = full_pass(&store, &ws.spec, &ctx.identity, generation, posture)
        .await
        .map_err(|e| Error::Store(e.to_string()))?;
    // An observation pass asserts nothing about the claim, so it must
    // not overwrite what the last posture pass concluded.
    let prev = ws.status.as_ref();
    let phase = if report.phase.is_empty() {
        prev.and_then(|s| s.phase.clone()).unwrap_or_default()
    } else {
        report.phase.clone()
    };
    let message = if report.phase.is_empty() {
        prev.and_then(|s| s.message.clone()).unwrap_or_default()
    } else {
        report.message.clone()
    };

    // Conditions carry lastTransitionTime, and it must mean "when this
    // changed" — so the new set is merged onto the OBSERVED one rather
    // than replacing it.
    let mut conditions = ws.status.as_ref().and_then(|s| s.conditions.clone()).unwrap_or_default();
    for c in report.conditions.iter().cloned() {
        spdk_csi_driver::lean_operator::boundary::set_condition(&mut conditions, c);
    }

    // D9's DR signature deserves an EVENT as well as a condition: the
    // condition is a state an operator has to look at, the event is one
    // that reaches them.
    if let Some(n) = report.stranded_candidates.filter(|n| *n > 0) {
        event(
            &ctx,
            &ws,
            kube::runtime::events::EventType::Warning,
            "UncitedWorkStranded",
            &format!(
                "{n} durable object(s) are staged and uncited with no live sidecar — invisible                  to every manifest-resolving reader. Run `flint-sync recover-staged` on this                  workspace to re-cite them as one flagged boundary"
            ),
        )
        .await;
    }

    let refused = phase == "Refused";
    let status = FlintLeanWorkspaceStatus {
        phase: Some(phase.clone()),
        message: Some(message.clone()),
        standing_project_id: report
            .standing_project_id
            .clone()
            .or_else(|| prev.and_then(|s| s.standing_project_id.clone())),
        // Stamps the POSTURE, which is what the cadence above is
        // measured from — an observation pass that bumped it would
        // starve the posture forever.
        last_verified_unix: if posture { Some(now) } else { prev.and_then(|s| s.last_verified_unix) },
        conditions: Some(conditions),
        observed_boundary_mode: report.observed_boundary_mode.clone(),
        observed_sidecar_version: report.observed_sidecar_version.clone(),
        cited_seq: report.cited_seq,
        visibility_lag_secs: report.visibility_lag_secs,
        staged_uncited: report.staged_uncited,
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
    if posture {
        info!("workspace {ns}/{name}: {phase} — {message}");
    }
    Ok(Action::requeue(Duration::from_secs(OBSERVE_EVERY_SECS)))
}

/// Events are best-effort by design: losing one must never fail a
/// reconcile that otherwise converged.
async fn event(
    ctx: &Ctx,
    ws: &FlintLeanWorkspace,
    ty: kube::runtime::events::EventType,
    reason: &str,
    note: &str,
) {
    use kube::runtime::events::Event as KEvent;
    if let Err(e) = ctx
        .recorder
        .publish(
            &KEvent {
                type_: ty,
                reason: reason.to_string(),
                note: Some(note.to_string()),
                action: "Reconcile".to_string(),
                secondary: None,
            },
            &ws.object_ref(&()),
        )
        .await
    {
        warn!("could not publish event {reason}: {e}");
    }
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
    let recorder = kube::runtime::events::Recorder::new(
        client.clone(),
        kube::runtime::events::Reporter {
            controller: "flint-lean-operator".into(),
            instance: std::env::var("POD_NAME").ok(),
        },
    );
    let ctx = Arc::new(Ctx { client, identity, recorder, stores: Mutex::new(BTreeMap::new()) });

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
