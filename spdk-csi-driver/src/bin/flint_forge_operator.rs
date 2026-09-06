//! `flint-forge-operator` — the `FlintRepo` controller.
//!
//! Ships in the operator image, the way `flint-lean-operator` does: same
//! crate, same build, the chart picks the binary. Duties per reconcile
//! are the ones `forge_operator::reconcile` documents — arbitrate the
//! bucket subtree, apply three children, poll the server's own
//! `/status`, move the one idle rung, write the status a door reads.
//!
//! It renders no PVC, verifies no flush and deletes no data. A
//! repository's durable state is the bucket and nothing else, which is
//! what makes suspending it a scale to zero and waking it a restore.
//!
//! Environment:
//!   FLINT_FORGE_OP_NAMESPACE   restrict the watch (unset = all)
//!   FLINT_FORGE_SYNCER_IMAGE   the image carrying flint-forge-syncer
//!   FLINT_FORGE_GIT_IMAGE      the image carrying gitcgi + http-backend
//!   FLINT_FORGE_LOG_LEVEL      RUST_LOG for the server pods
//!   FLINT_FORGE_DOOR_NAMESPACE where flint-hub-gateway runs; set it to
//!                              render the NetworkPolicy that makes
//!                              `X-Remote-User` trustworthy
//!   FLINT_FORGE_DOOR_POD_LABEL the door's pod label (k=v)

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::{watcher, Controller};
use kube::{Client, ResourceExt};
use tracing::{info, warn};

use spdk_csi_driver::forge_operator::crd::FlintRepo;
use spdk_csi_driver::forge_operator::reconcile::{full_pass, Defaults};
use spdk_csi_driver::forge_operator::render::{DoorSelector, PodPeer, RenderDefaults};

struct Ctx {
    client: Client,
    defaults: Defaults,
    /// Every `FlintRepo` the controller has seen. Arbitration needs the
    /// whole set, and reading it from the controller's own cache rather
    /// than listing per pass is what keeps a 3,000-repository fleet's
    /// reconcile rate a local computation.
    repos: kube::runtime::reflector::Store<FlintRepo>,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("reconcile: {0}")]
    Reconcile(#[from] spdk_csi_driver::forge_operator::reconcile::Error),
}

async fn reconcile(repo: Arc<FlintRepo>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let all: Vec<FlintRepo> = ctx.repos.state().iter().map(|r| (**r).clone()).collect();
    let outcome = full_pass(&ctx.client, &repo, &all, &ctx.defaults, chrono::Utc::now()).await?;
    info!(
        repo = %format!("{}/{}", repo.namespace().unwrap_or_default(), repo.name_any()),
        phase = ?outcome.phase,
        "reconciled"
    );
    Ok(Action::requeue(Duration::from_secs(outcome.requeue_secs)))
}

/// A failed pass is retried, never abandoned. The interval is short
/// because the failures this sees are apiserver blips and a server that
/// has not come up yet — both resolve on their own, and a long backoff
/// would leave a repository the door is holding a request for down for
/// no reason.
fn error_policy(_repo: Arc<FlintRepo>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    warn!("reconcile failed, retrying: {err}");
    Action::requeue(Duration::from_secs(10))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    spdk_csi_driver::install_crypto_provider();

    let client = Client::try_default().await?;

    // The CRD is applied from the compiled-in copy at startup, the same
    // posture the other two operators take: the chart's `crds/` copy is
    // install-time bootstrap, and the binary is the source of truth.
    {
        use kube::api::PostParams;
        let crds: Api<
            k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
        > = Api::all(client.clone());
        let crd = spdk_csi_driver::forge_operator::crd::crd();
        match crds.create(&PostParams::default(), &crd).await {
            Ok(_) => info!("FlintRepo CRD created"),
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                crds.patch(
                    "flintrepos.chert.us",
                    &PatchParams::apply("flint-forge-operator").force(),
                    &Patch::Apply(&crd),
                )
                .await?;
                info!("FlintRepo CRD updated");
            }
            Err(e) => return Err(e.into()),
        }
    }

    let repos: Api<FlintRepo> = match std::env::var("FLINT_FORGE_OP_NAMESPACE") {
        Ok(ns) => Api::namespaced(client.clone(), &ns),
        Err(_) => Api::all(client.clone()),
    };

    let mut render = RenderDefaults::default();
    if let Ok(i) = std::env::var("FLINT_FORGE_SYNCER_IMAGE") {
        render.syncer_image = i;
    }
    if let Ok(i) = std::env::var("FLINT_FORGE_GIT_IMAGE") {
        render.git_image = i;
    }
    if let Ok(l) = std::env::var("FLINT_FORGE_LOG_LEVEL") {
        render.log_level = l;
    }
    if let Some(why) = spdk_csi_driver::forge_operator::render::server_images_disagree(&render) {
        warn!("{why}");
    }
    // Where the door runs. Set it and every repository gets a
    // NetworkPolicy admitting only the gateway to its git port; leave
    // it unset and reaching the port IS the authorization, which the
    // design says out loud rather than leaving to be discovered.
    if let Ok(ns) = std::env::var("FLINT_FORGE_DOOR_NAMESPACE") {
        let label = std::env::var("FLINT_FORGE_DOOR_POD_LABEL")
            .unwrap_or_else(|_| "app.kubernetes.io/name=flint-hub-gateway".into());
        let (k, v) = label.split_once('=').unwrap_or(("app.kubernetes.io/name", label.as_str()));
        render.door = Some(DoorSelector {
            namespace: ns,
            pod_labels: std::collections::BTreeMap::from([(k.to_string(), v.to_string())]),
        });

        // AND THIS OPERATOR, or the policy it just wrote denies its own
        // `/status` poll: a NetworkPolicy is default-deny for every port
        // it does not name, and the git port is not the status port.
        // Without this the repository never leaves `Starting`, the idle
        // ladder never gets an input, and nothing anywhere logs an
        // error — the pod is Ready and the operator is simply blind.
        match (
            std::env::var("FLINT_FORGE_OPERATOR_NAMESPACE"),
            std::env::var("FLINT_FORGE_OPERATOR_POD_LABEL"),
        ) {
            (Ok(op_ns), Ok(op_label)) if !op_ns.is_empty() && !op_label.is_empty() => {
                let (ok, ov) = op_label
                    .split_once('=')
                    .unwrap_or(("app.kubernetes.io/name", op_label.as_str()));
                render.operator = Some(PodPeer {
                    namespace: op_ns,
                    pod_labels: std::collections::BTreeMap::from([
                        (ok.to_string(), ov.to_string()),
                    ]),
                });
            }
            _ => warn!(
                "a NetworkPolicy will be rendered but FLINT_FORGE_OPERATOR_NAMESPACE / \
                 FLINT_FORGE_OPERATOR_POD_LABEL are not both set, so this operator's own \
                 /status poll will be DENIED by it and every repository will sit in Starting"
            ),
        }
        info!("rendering a NetworkPolicy admitting the door and this operator");
    } else {
        warn!(
            "FLINT_FORGE_DOOR_NAMESPACE is unset: no NetworkPolicy is rendered, so anything that \
             can reach a repository's git port can set X-Remote-User and be believed"
        );
    }

    let controller = Controller::new(repos, watcher::Config::default())
        // The children are owned, so a hand edit or a stray delete is
        // reconciled back rather than discovered on the next timer.
        .owns(
            Api::<Deployment>::all(client.clone()),
            watcher::Config::default(),
        )
        .owns(Api::<Service>::all(client.clone()), watcher::Config::default())
        .owns(Api::<ConfigMap>::all(client.clone()), watcher::Config::default());

    let ctx = Arc::new(Ctx {
        client,
        defaults: Defaults { render, ..Defaults::default() },
        repos: controller.store(),
    });

    info!("flint-forge-operator: watching FlintRepo");
    controller
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("controller: {e:?}");
            }
        })
        .await;
    Ok(())
}
