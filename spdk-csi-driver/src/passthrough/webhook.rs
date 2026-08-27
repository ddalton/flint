//! The mutating-admission wrapper around `inject`: registration, the
//! AdmissionReview handler, and the TLS server.
//!
//! `failurePolicy: Fail` + `objectSelector: flint.io/passthrough-mount
//! Exists`. An opted-in pod either gets its mount or does not schedule.
//! The alternative — Ignore — starts the pod against an empty
//! directory, which no probe in the pod can distinguish from a bucket
//! that happens to have no objects under the prefix. Un-labelled pods
//! are never looked at.
//!
//! Cert material comes from `crate::webhook_certs`, shared with the
//! lean webhook.

use std::sync::Arc;

use base64::Engine;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use serde_json::{json, Value};
use tracing::info;

use super::inject::{inject_mount, InjectDefaults, INJECT_LABEL};
use super::spec::MountSpec;

pub const CERT_SECRET: &str = "flint-passthrough-webhook-cert";
pub const WEBHOOK_CONFIG: &str = "flint-passthrough-inject";
pub const MUTATE_PATH: &str = "mutate";
pub const CRD_GROUP: &str = "flint.io";
pub const CRD_VERSION: &str = "v1alpha1";
pub const CRD_KIND: &str = "FlintPassthroughMount";
const CA_CN: &str = "flint-passthrough-webhook-ca";

pub use crate::webhook_certs::CertBundle;

/// Handle one AdmissionReview. `lookup` resolves (namespace, name) to
/// the CR as raw JSON; injected so the core is unit-testable without a
/// cluster, and raw so that a spec the CRD schema would have rejected
/// is DENIED here with a parse error rather than defaulted into
/// something plausible.
pub async fn mutate_review<F, Fut>(review: Value, defaults: &InjectDefaults, lookup: F) -> Value
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Value>, String>>,
{
    let uid = review
        .pointer("/request/uid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let respond = |body: Value| {
        json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "response": body,
        })
    };
    let deny = |uid: &str, msg: String| {
        respond(json!({ "uid": uid, "allowed": false, "status": { "message": msg } }))
    };
    let allow_unchanged = respond(json!({ "uid": uid, "allowed": true }));

    let namespace = review
        .pointer("/request/namespace")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let pod: Pod = match review.pointer("/request/object") {
        Some(obj) => match serde_json::from_value(obj.clone()) {
            Ok(p) => p,
            Err(e) => return deny(&uid, format!("object is not a Pod: {e}")),
        },
        None => return deny(&uid, "no object in the review".into()),
    };
    let Some(cr_name) =
        pod.metadata.labels.as_ref().and_then(|l| l.get(INJECT_LABEL)).cloned()
    else {
        return allow_unchanged;
    };

    let cr = match lookup(namespace.clone(), cr_name.clone()).await {
        Ok(Some(cr)) => cr,
        Ok(None) => {
            return deny(
                &uid,
                format!(
                    "pod opts into passthrough mount {namespace}/{cr_name}, but no such \
                     {CRD_KIND} exists — refusing to start the pod with an empty directory \
                     where the bucket should be"
                ),
            );
        }
        Err(e) => return deny(&uid, format!("{CRD_KIND} lookup failed: {e}")),
    };
    let spec: MountSpec = match cr.get("spec").cloned() {
        Some(s) => match serde_json::from_value(s) {
            Ok(s) => s,
            Err(e) => {
                return deny(&uid, format!("{CRD_KIND} {namespace}/{cr_name} spec is invalid: {e}"))
            }
        },
        None => return deny(&uid, format!("{CRD_KIND} {namespace}/{cr_name} has no spec")),
    };

    let mutated = match inject_mount(&pod, &cr_name, &spec, defaults) {
        Ok(m) => m,
        Err(e) => return deny(&uid, format!("injection failed: {e}")),
    };
    if mutated == pod {
        return allow_unchanged;
    }
    let patch = json!([{ "op": "replace", "path": "/spec", "value": mutated.spec }]);
    let patch_b64 =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&patch).unwrap());
    respond(json!({
        "uid": uid,
        "allowed": true,
        "patchType": "JSONPatch",
        "patch": patch_b64,
    }))
}

pub async fn ensure_cert_secret(
    client: &Client,
    namespace: &str,
    service: &str,
) -> anyhow::Result<CertBundle> {
    crate::webhook_certs::ensure_cert_secret(client, namespace, service, CERT_SECRET, CA_CN).await
}

/// Server-side apply of the MutatingWebhookConfiguration.
pub async fn ensure_webhook_config(
    client: &Client,
    namespace: &str,
    service: &str,
    port: u16,
    ca_pem: &str,
) -> anyhow::Result<()> {
    use k8s_openapi::api::admissionregistration::v1::MutatingWebhookConfiguration;
    let api: Api<MutatingWebhookConfiguration> = Api::all(client.clone());
    let ca_b64 = base64::engine::general_purpose::STANDARD.encode(ca_pem);
    let cfg: MutatingWebhookConfiguration = serde_json::from_value(json!({
        "apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "MutatingWebhookConfiguration",
        "metadata": { "name": WEBHOOK_CONFIG },
        "webhooks": [{
            "name": "inject.passthrough.flint.io",
            "admissionReviewVersions": ["v1"],
            "sideEffects": "None",
            "failurePolicy": "Fail",
            "objectSelector": {
                "matchExpressions": [{ "key": INJECT_LABEL, "operator": "Exists" }]
            },
            "rules": [{
                "apiGroups": [""],
                "apiVersions": ["v1"],
                "operations": ["CREATE"],
                "resources": ["pods"],
                "scope": "Namespaced"
            }],
            "clientConfig": {
                "service": {
                    "name": service,
                    "namespace": namespace,
                    "path": format!("/{MUTATE_PATH}"),
                    "port": port,
                },
                "caBundle": ca_b64,
            }
        }]
    }))?;
    api.patch(
        WEBHOOK_CONFIG,
        &PatchParams::apply("flint-passthrough-operator").force(),
        &Patch::Apply(&cfg),
    )
    .await?;
    info!("mutating webhook {WEBHOOK_CONFIG} applied");
    Ok(())
}

/// Fetch a `FlintPassthroughMount` as raw JSON.
pub async fn get_mount(client: &Client, ns: &str, name: &str) -> Result<Option<Value>, String> {
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind};
    let gvk = GroupVersionKind::gvk(CRD_GROUP, CRD_VERSION, CRD_KIND);
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
    match api.get_opt(name).await {
        Ok(Some(o)) => serde_json::to_value(o).map(Some).map_err(|e| e.to_string()),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Serve /mutate over TLS. Blocks; run in its own task.
pub async fn serve(
    client: Client,
    defaults: InjectDefaults,
    listen: std::net::SocketAddr,
    bundle: CertBundle,
) {
    use warp::Filter;
    let client = Arc::new(client);
    let defaults = Arc::new(defaults);
    let mutate = warp::post()
        .and(warp::path(MUTATE_PATH))
        .and(warp::body::json())
        .then(move |review: Value| {
            let client = client.clone();
            let defaults = defaults.clone();
            async move {
                let resp = mutate_review(review, &defaults, |ns, name| async move {
                    get_mount(&client, &ns, &name).await
                })
                .await;
                warp::reply::json(&resp)
            }
        });
    let healthz = warp::get().and(warp::path("healthz")).map(|| "ok");
    warp::serve(mutate.or(healthz))
        .tls()
        .cert(bundle.cert_pem.as_bytes())
        .key(bundle.key_pem.as_bytes())
        .run(listen)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_for(pod: Value) -> Value {
        json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": { "uid": "review-1", "namespace": "agents", "object": pod },
        })
    }
    fn labeled_pod() -> Value {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": "agent-1", "labels": { INJECT_LABEL: "proj1" } },
            "spec": { "containers": [{ "name": "agent", "image": "agent:1" }] }
        })
    }
    fn cr(spec: Value) -> Value {
        json!({
            "apiVersion": "flint.io/v1alpha1", "kind": CRD_KIND,
            "metadata": { "name": "proj1", "namespace": "agents" },
            "spec": spec,
        })
    }
    fn good_cr() -> Value {
        cr(json!({ "bucket": "agentws", "keyPrefix": "tenants/proj1",
                   "credentialsSecretRef": "proj1-creds" }))
    }
    const D: fn() -> InjectDefaults = || InjectDefaults {
        image: "flint-passthrough:test".into(),
        resources: None,
    };

    #[tokio::test]
    async fn labeled_pod_gets_a_jsonpatch_with_the_mounter() {
        let resp =
            mutate_review(review_for(labeled_pod()), &D(), |_, _| async { Ok(Some(good_cr())) })
                .await;
        let r = resp.pointer("/response").unwrap();
        assert_eq!(r["uid"], "review-1");
        assert_eq!(r["allowed"], true);
        assert_eq!(r["patchType"], "JSONPatch");
        let patch: Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(r["patch"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(patch[0]["path"], "/spec");
        let init = &patch[0]["value"]["initContainers"][0];
        assert_eq!(init["name"], super::super::inject::SIDECAR_NAME);
        assert_eq!(init["restartPolicy"], "Always");
        assert_eq!(init["securityContext"]["privileged"], true);
    }

    /// failurePolicy Fail end-to-end: no CR means no pod. Starting it
    /// would hand the workload an empty directory that looks exactly
    /// like an empty bucket.
    #[tokio::test]
    async fn missing_cr_denies_the_pod() {
        let resp = mutate_review(review_for(labeled_pod()), &D(), |_, _| async { Ok(None) }).await;
        let r = resp.pointer("/response").unwrap();
        assert_eq!(r["allowed"], false);
        assert!(r["status"]["message"].as_str().unwrap().contains("no such FlintPassthroughMount"));
    }

    /// A spec an older CRD schema let through must be denied HERE, with
    /// the reason, rather than defaulted into a plausible mount.
    #[tokio::test]
    async fn an_unparseable_spec_denies_the_pod() {
        let resp = mutate_review(review_for(labeled_pod()), &D(), |_, _| async {
            Ok(Some(cr(json!({ "bucket": "b", "readOnly": "yes-please" }))))
        })
        .await;
        let r = resp.pointer("/response").unwrap();
        assert_eq!(r["allowed"], false);
        assert!(r["status"]["message"].as_str().unwrap().contains("spec is invalid"));
    }

    #[tokio::test]
    async fn an_unknown_field_denies_rather_than_being_ignored() {
        let resp = mutate_review(review_for(labeled_pod()), &D(), |_, _| async {
            Ok(Some(cr(json!({ "bucket": "b", "readonly": true }))))
        })
        .await;
        assert_eq!(resp.pointer("/response/allowed").unwrap(), false);
    }

    #[tokio::test]
    async fn a_lookup_failure_denies_rather_than_admitting_unmounted() {
        let resp = mutate_review(review_for(labeled_pod()), &D(), |_, _| async {
            Err("connection refused".into())
        })
        .await;
        assert_eq!(resp.pointer("/response/allowed").unwrap(), false);
    }

    #[tokio::test]
    async fn unlabeled_pod_is_untouched() {
        let pod = json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": "other" },
            "spec": { "containers": [{ "name": "c" }] }
        });
        let resp = mutate_review(review_for(pod), &D(), |_, _| async {
            panic!("must not even look up the CR")
        })
        .await;
        let r = resp.pointer("/response").unwrap();
        assert_eq!(r["allowed"], true);
        assert!(r.get("patch").is_none());
    }
}
