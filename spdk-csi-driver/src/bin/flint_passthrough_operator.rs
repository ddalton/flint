//! `flint-passthrough-operator` — the FlintPassthroughMount webhook.
//!
//! It is a webhook and NOTHING ELSE. No controller, no watch, no
//! status, no S3 client, no reconcile loop: a passthrough mount owns no
//! state to converge, so the process's whole job is to be reachable
//! when the API server calls it. It ships in the operator image
//! alongside flint-lite/-hub/-lean (the chart picks the binary — the
//! flint-hub-gateway precedent), because a second image to publish,
//! scan and keep in step at release is the part of a new component
//! that actually costs something.
//!
//! Startup, in order: read-or-create the TLS Secret, apply the
//! MutatingWebhookConfiguration carrying that CA, then serve. The
//! registration is applied AFTER the cert exists and BEFORE the
//! listener is up only by a few milliseconds — with `failurePolicy:
//! Fail` that window makes labelled pods fail admission rather than
//! start unmounted, which is the right way round.
//!
//! Environment:
//!   FLINT_PT_WEBHOOK_SERVICE     our Service name (REQUIRED)
//!   FLINT_PT_WEBHOOK_NAMESPACE   Service namespace (default flint-system)
//!   FLINT_PT_WEBHOOK_LISTEN      default 0.0.0.0:9443
//!   FLINT_PT_WEBHOOK_PORT        Service port in the registration (default 9443)
//!   FLINT_PT_SIDECAR_IMAGE       injected mounter image (REQUIRED)
//!   FLINT_PT_SIDECAR_RESOURCES   ResourceRequirements as JSON (optional)

use tracing::info;

use spdk_csi_driver::passthrough::inject::InjectDefaults;
use spdk_csi_driver::passthrough::webhook;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    spdk_csi_driver::install_crypto_provider();
    let client = kube::Client::try_default().await?;

    // No default. A webhook that cannot name its own Service cannot
    // write a registration the API server can dial, and starting
    // anyway would leave every labelled pod failing admission against
    // a webhook nobody can reach — with the operator itself perfectly
    // healthy.
    let service = std::env::var("FLINT_PT_WEBHOOK_SERVICE").map_err(|_| {
        anyhow::anyhow!(
            "FLINT_PT_WEBHOOK_SERVICE is unset — the webhook needs its own Service name to \
             register a reachable endpoint"
        )
    })?;
    let ns =
        std::env::var("FLINT_PT_WEBHOOK_NAMESPACE").unwrap_or_else(|_| "flint-system".into());
    let listen: std::net::SocketAddr = std::env::var("FLINT_PT_WEBHOOK_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:9443".into())
        .parse()?;
    let port: u16 = std::env::var("FLINT_PT_WEBHOOK_PORT")
        .ok()
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(9443);
    // No default, for the same reason the Dockerfile pins mount-s3:
    // this string names the image a PRIVILEGED container in someone
    // else's pod will run. A fallback here would have been
    // `…:latest` — a moving privileged image chosen by nobody, reached
    // by forgetting to set an env var. The chart always sets it.
    let image = std::env::var("FLINT_PT_SIDECAR_IMAGE").map_err(|_| {
        anyhow::anyhow!(
            "FLINT_PT_SIDECAR_IMAGE is unset — it names the mounter image every injected \
             sidecar runs, privileged, and this refuses to pick one for you"
        )
    })?;
    let resources = match std::env::var("FLINT_PT_SIDECAR_RESOURCES") {
        Ok(j) if !j.trim().is_empty() && j.trim() != "{}" => Some(
            serde_json::from_str(&j)
                .map_err(|e| anyhow::anyhow!("FLINT_PT_SIDECAR_RESOURCES is not valid ResourceRequirements JSON: {e}"))?,
        ),
        _ => None,
    };

    let bundle = webhook::ensure_cert_secret(&client, &ns, &service).await?;
    webhook::ensure_webhook_config(&client, &ns, &service, port, &bundle.ca_pem).await?;
    info!("flint-passthrough webhook serving on {listen} as {service}.{ns}.svc");
    webhook::serve(client, InjectDefaults { image, resources }, listen, bundle).await;
    Ok(())
}
