//! `flint-lite-operator` — the fleet control plane for flint-lite hubs.
//!
//! One `FlintShare` per volume; this process renders and converges the
//! four objects each one needs, and reports what it observes. Design of
//! record: docs/plans/flint-lite-operator-plan.md.
//!
//! Wiring notes worth knowing before reading the code:
//!
//! - **`owns()` for three children, `watches()` for the fourth.** The
//!   PVC deliberately carries no ownerReference (owner GC would ignore
//!   `reclaim: Retain` and collect it), so its events come back via a
//!   watch and a name mapping instead.
//! - **A Secret watch.** Nothing in a CR changes when credentials
//!   rotate, so without this the operator would never know — and the
//!   hub would find out the hard way, by failing a heartbeat and
//!   fencing itself. Cost, stated rather than hidden: this caches every
//!   Secret in the watched scope in the operator's memory. A
//!   metadata-only watch would fix that, but `watches_stream` (the only
//!   way to feed `metadata_watcher` into a Controller) is behind
//!   kube's `unstable-runtime-stream-control` feature, and an unstable
//!   flag is a worse trade than the memory. Narrow `watchNamespace` if
//!   the cluster is large and the fleet is not.
//! - **A watch on FlintShare itself.** Conflict arbitration is a
//!   statement about OTHER objects: when a share appears or is deleted,
//!   everyone whose subtree it overlaps has to re-decide. Without it a
//!   promoted survivor would wait out the steady-state requeue.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::{watcher, Controller};
use kube::{Api, Client, ResourceExt};
use spdk_csi_driver::lite_operator::{bootstrap, conflict, crd::FlintShare, reconcile, render};
use spdk_csi_driver::orchestrator_lease::{self, KubeLeaseOps, LeaseConfig};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "flint-lite-operator", version)]
struct Args {
    /// Namespace to watch. Unset = every namespace (the fleet posture;
    /// conflict arbitration needs to see across namespaces to be
    /// meaningful).
    #[arg(long, env = "FLINT_OPERATOR_NAMESPACE")]
    namespace: Option<String>,

    /// Hub image for shares that do not pin one. A fleet-wide upgrade
    /// is a change to this value plus one operator rollout.
    #[arg(
        long,
        env = "FLINT_HUB_IMAGE",
        default_value = "dilipdalton/flint-pnfs:1.33.0"
    )]
    hub_image: String,

    #[arg(long, env = "FLINT_HUB_IMAGE_PULL_POLICY", default_value = "IfNotPresent")]
    hub_image_pull_policy: String,

    /// Default startupProbe budget in 10s periods for tiered hubs.
    #[arg(long, env = "FLINT_HUB_STARTUP_FAILURE_THRESHOLD", default_value_t = 60)]
    startup_failure_threshold: i32,

    /// Namespace holding the leader-election Lease (this operator's own
    /// namespace).
    #[arg(long, env = "POD_NAMESPACE", default_value = "flint-system")]
    lease_namespace: String,

    /// `disabled` turns leader election off (dev, and emergencies).
    #[arg(long, env = "FLINT_OPERATOR_ELECTION", default_value = "enabled")]
    election: String,

    /// Skip the startup CRD apply. For clusters where the operator is
    /// deliberately not allowed to manage CRDs — read
    /// `lite_operator::bootstrap` first: the cost is silent pruning of
    /// any field newer than the installed schema.
    // `action = Set` (not the default flag behaviour) so the chart can
    // pass `--manage-crd=false` and an env var can carry a value.
    #[arg(
        long,
        env = "FLINT_OPERATOR_MANAGE_CRD",
        action = clap::ArgAction::Set,
        default_value_t = true
    )]
    manage_crd: bool,
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
    // Before ANY TLS: two rustls providers are in this crate's tree, so
    // the process default has to be chosen explicitly or the client
    // construction below panics outright.
    spdk_csi_driver::install_crypto_provider();
    let client = Client::try_default().await?;

    if args.manage_crd {
        match bootstrap::ensure_crd(&client).await? {
            bootstrap::Outcome::Ready => {}
            bootstrap::Outcome::Degraded(why) => warn!("operator is DEGRADED: {why}"),
        }
    }

    // Leader election. Two operators applying the same render are
    // convergent rather than harmful, but arbitration decisions taken
    // from two differently-stale caches are not, so only the lease
    // holder acts. Reuses the same elector the CSI orchestrators use
    // (clock-skew independent: a holder is failed only after ITS record
    // has been unchanged for a full lease duration by our own clock).
    let mut lease_cfg = LeaseConfig::from_setting(Some(&args.election));
    lease_cfg.lease_name = "flint-lite-operator".to_string();
    if lease_cfg.enabled {
        let id = format!(
            "{}-{}",
            std::env::var("POD_NAME").unwrap_or_else(|_| "flint-lite-operator".into()),
            std::process::id()
        );
        let ops = Arc::new(KubeLeaseOps::new(
            client.clone(),
            &args.lease_namespace,
            &lease_cfg,
        ));
        let cfg = lease_cfg.clone();
        tokio::spawn(orchestrator_lease::run_election(ops, id, cfg));
    } else {
        warn!("leader election DISABLED — run exactly one operator replica");
    }

    let shares: Api<FlintShare> = match &args.namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    };
    // Fail fast and legibly if the CRD is not served, instead of
    // watch-erroring forever.
    shares.list(&kube::api::ListParams::default().limit(1)).await?;

    // BOUND THE FAN-OUT. `Config::default()` is `concurrency: 0`,
    // documented as UNBOUNDED, and this binary never overrode it — so a
    // cold start or a leader handover at the design target (3000 CRs)
    // admits 3000 simultaneous reconciles, each of which snapshots the
    // whole fleet. That is ~2-3 GB of transient allocation against a
    // 256Mi limit: a deterministic OOMKill, and the CrashLoop that
    // follows re-enters the same herd.
    //
    // The debounce coalesces the burst of watch events a single change
    // fans out into (a share edit touches its Deployment, Service,
    // ConfigMap and the share itself), at the cost of up to 250 ms of
    // added wake latency — measured, not assumed, by the wake canary.
    let controller = Controller::new(shares, watcher::Config::default()).with_config(
        kube::runtime::controller::Config::default()
            .concurrency(32)
            .debounce(Duration::from_millis(250)),
    );
    let store = controller.store();

    let ctx = Arc::new(reconcile::Ctx {
        client: client.clone(),
        defaults: render::RenderDefaults {
            image: args.hub_image.clone(),
            image_pull_policy: args.hub_image_pull_policy.clone(),
            startup_failure_threshold: args.startup_failure_threshold,
            ..Default::default()
        },
        recorder: Recorder::new(
            client.clone(),
            Reporter {
                controller: "flint-lite-operator".into(),
                instance: std::env::var("POD_NAME").ok(),
            },
        ),
        fleet: store.clone(),
        admit_cache: std::sync::Mutex::new(None),
    });

    info!(
        image = %args.hub_image,
        namespace = %args.namespace.clone().unwrap_or_else(|| "<all>".into()),
        "flint-lite-operator starting"
    );

    let secret_store = store.clone();
    let claim_store = store.clone();
    let share_store = store.clone();

    controller
        .owns(Api::<Deployment>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Service>::all(client.clone()), watcher::Config::default())
        .owns(Api::<ConfigMap>::all(client.clone()), watcher::Config::default())
        // Credentials rotate without any CR changing.
        //
        // LABEL-SELECTED, and that is the whole point: unselected, this
        // watch held every Secret in the cluster in the operator's
        // memory — service-account tokens, other tenants' credentials,
        // all of it — to notice changes in the few a FlintShare names.
        // A Secret without the label is not a correctness problem: the
        // checksum comes from a direct `get` during reconcile, so the
        // rotation still rolls the hub on the next periodic pass. The
        // share reports `CredentialsWatched: false` when that is the
        // case.
        .watches(
            Api::<Secret>::all(client.clone()),
            watcher::Config::default().labels(reconcile::LABEL_CREDENTIALS),
            move |s: Secret| {
                let ns = s.namespace().unwrap_or_default();
                let name = s.name_any();
                let all: Vec<_> = secret_store.state();
                reconcile::shares_referencing_secret(&all, &ns, &name)
                    .into_iter()
                    .map(|(ns, n)| ObjectRef::new(&n).within(&ns))
                    .collect::<Vec<_>>()
            },
        )
        // The claim has no ownerReference to travel by (on purpose).
        .watches(
            Api::<PersistentVolumeClaim>::all(client.clone()),
            watcher::Config::default(),
            move |p: PersistentVolumeClaim| {
                let ns = p.namespace().unwrap_or_default();
                let name = p.name_any();
                let all: Vec<_> = claim_store.state();
                reconcile::shares_using_claim(&all, &ns, &name)
                    .into_iter()
                    .map(|(ns, n)| ObjectRef::new(&n).within(&ns))
                    .collect::<Vec<_>>()
            },
        )
        // A share appearing or disappearing changes who owns a subtree.
        .watches(
            Api::<FlintShare>::all(client.clone()),
            watcher::Config::default(),
            move |s: FlintShare| {
                let me = conflict::Candidate::of(&s);
                let all: Vec<_> = share_store
                    .state()
                    .iter()
                    .map(|s| conflict::Candidate::of(s))
                    .collect();
                conflict::overlap_set(&all, &me)
                    .into_iter()
                    .map(|c| ObjectRef::new(&c.name).within(&c.namespace))
                    .collect::<Vec<_>>()
            },
        )
        .run(gated_reconcile, reconcile::error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, action)) => tracing::debug!("reconciled {}: {action:?}", obj.name),
                Err(e) => warn!("controller error: {e}"),
            }
        })
        .await;

    Ok(())
}

/// Reconcile only while this process holds the lease. A standing-by
/// operator keeps its caches warm and its watches open — it just does
/// not write.
async fn gated_reconcile(
    share: Arc<FlintShare>,
    ctx: Arc<reconcile::Ctx>,
) -> Result<kube::runtime::controller::Action, reconcile::Error> {
    if !orchestrator_lease::is_leader() {
        return Ok(kube::runtime::controller::Action::requeue(
            Duration::from_secs(10),
        ));
    }
    reconcile::reconcile(share, ctx).await
}
