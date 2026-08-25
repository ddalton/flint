//! The mutating-admission wrapper around `inject` — self-contained
//! cert plumbing, registration, and the AdmissionReview handler.
//!
//! Cert posture (no cert-manager dependency): the operator generates a
//! CA + serving cert once and persists them in a Secret; every replica
//! serves from the same Secret, and the MutatingWebhookConfiguration's
//! caBundle is (re)applied at startup. `failurePolicy: Fail` +
//! `objectSelector: flint.io/lean-workspace Exists` — an opted-in pod
//! either gets its sidecar or does not schedule; it NEVER starts
//! ungated (the review's clobbered-scaffold finding). Un-labeled pods
//! are never touched.

use std::sync::Arc;

use base64::Engine;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::Client;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::crd::{FlintLeanWorkspace, INJECT_LABEL};
use super::inject::{inject_sidecar, InjectDefaults};

pub const CERT_SECRET: &str = "flint-lean-webhook-cert";
pub const WEBHOOK_CONFIG: &str = "flint-lean-inject";
pub const MUTATE_PATH: &str = "mutate";

// ── the AdmissionReview core (pure over an injected lookup) ──────────

/// Handle one AdmissionReview. `lookup` resolves (namespace, name) to
/// the workspace CR; injected so the core is unit-testable without a
/// cluster.
pub async fn mutate_review<F, Fut>(
    review: Value,
    defaults: &InjectDefaults,
    lookup: F,
) -> Value
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<FlintLeanWorkspace>, String>>,
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
        respond(json!({
            "uid": uid,
            "allowed": false,
            "status": { "message": msg },
        }))
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
    let Some(ws_name) = pod
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(INJECT_LABEL))
        .cloned()
    else {
        // Not opted in (the objectSelector should have filtered this;
        // belt anyway): never touch it.
        return allow_unchanged;
    };

    let ws = match lookup(namespace.clone(), ws_name.clone()).await {
        Ok(Some(ws)) => ws,
        Ok(None) => {
            return deny(
                &uid,
                format!(
                    "pod opts into lean workspace {namespace}/{ws_name}, but no such \
                     FlintLeanWorkspace exists — refusing to start the pod ungated"
                ),
            );
        }
        Err(e) => return deny(&uid, format!("workspace lookup failed: {e}")),
    };
    if ws.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Refused") {
        return deny(
            &uid,
            format!(
                "lean workspace {namespace}/{ws_name} is Refused (foreign claim on its \
                 prefix) — resolve the claim before scheduling pods"
            ),
        );
    }

    let mutated = match inject_sidecar(&pod, &ws, defaults) {
        Ok(m) => m,
        Err(e) => return deny(&uid, format!("injection failed: {e}")),
    };
    if mutated == pod {
        return allow_unchanged;
    }
    let patch = json!([
        { "op": "replace", "path": "/spec", "value": mutated.spec }
    ]);
    let patch_b64 =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&patch).unwrap());
    respond(json!({
        "uid": uid,
        "allowed": true,
        "patchType": "JSONPatch",
        "patch": patch_b64,
    }))
}

// ── cert material ────────────────────────────────────────────────────

pub struct CertBundle {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate a CA + serving cert for `<service>.<namespace>.svc`.
pub fn generate_cert(service: &str, namespace: &str) -> Result<CertBundle, String> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
    let e = |e: rcgen::Error| e.to_string();

    let ca_key = KeyPair::generate().map_err(e)?;
    let mut ca_params = CertificateParams::new(vec![]).map_err(e)?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, "flint-lean-webhook-ca");
    let ca_cert = ca_params.self_signed(&ca_key).map_err(e)?;

    let sans = vec![
        format!("{service}.{namespace}.svc"),
        format!("{service}.{namespace}.svc.cluster.local"),
        service.to_string(),
    ];
    let server_key = KeyPair::generate().map_err(e)?;
    let mut params = CertificateParams::new(sans).map_err(e)?;
    params.distinguished_name.push(DnType::CommonName, format!("{service}.{namespace}.svc"));
    let server_cert = params.signed_by(&server_key, &ca_cert, &ca_key).map_err(e)?;

    Ok(CertBundle {
        ca_pem: ca_cert.pem(),
        cert_pem: server_cert.pem(),
        key_pem: server_key.serialize_pem(),
    })
}

/// Read-or-create the cert Secret (two replicas race safely: the loser
/// of the create adopts the winner's material).
pub async fn ensure_cert_secret(
    client: &Client,
    namespace: &str,
    service: &str,
) -> anyhow::Result<CertBundle> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let read = |s: Secret| -> Option<CertBundle> {
        let d = s.data?;
        let get = |k: &str| d.get(k).map(|v| String::from_utf8_lossy(&v.0).to_string());
        Some(CertBundle { ca_pem: get("ca.crt")?, cert_pem: get("tls.crt")?, key_pem: get("tls.key")? })
    };
    if let Some(s) = api.get_opt(CERT_SECRET).await? {
        if let Some(b) = read(s) {
            return Ok(b);
        }
        warn!("cert secret {CERT_SECRET} exists but is incomplete — regenerating");
    }
    let bundle = generate_cert(service, namespace).map_err(anyhow::Error::msg)?;
    let secret: Secret = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": CERT_SECRET, "namespace": namespace },
        "type": "kubernetes.io/tls",
        "stringData": {
            "ca.crt": bundle.ca_pem,
            "tls.crt": bundle.cert_pem,
            "tls.key": bundle.key_pem,
        }
    }))?;
    match api.create(&PostParams::default(), &secret).await {
        Ok(_) => {
            info!("webhook cert generated and stored in {CERT_SECRET}");
            Ok(bundle)
        }
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            let s = api.get(CERT_SECRET).await?;
            read(s).ok_or_else(|| anyhow::anyhow!("racing replica wrote an unreadable secret"))
        }
        Err(e) => Err(e.into()),
    }
}

/// Server-side apply of the MutatingWebhookConfiguration.
pub async fn ensure_webhook_config(
    client: &Client,
    namespace: &str,
    service: &str,
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
            "name": "inject.lean.flint.io",
            "admissionReviewVersions": ["v1"],
            "sideEffects": "None",
            // An opted-in pod either gets its sidecar or does not
            // schedule — never starts ungated (plan §2.4).
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
                "service": { "name": service, "namespace": namespace, "path": format!("/{MUTATE_PATH}"), "port": 9443 },
                "caBundle": ca_b64,
            }
        }]
    }))?;
    api.patch(
        WEBHOOK_CONFIG,
        &PatchParams::apply("flint-lean-operator").force(),
        &Patch::Apply(&cfg),
    )
    .await?;
    info!("mutating webhook {WEBHOOK_CONFIG} applied");
    Ok(())
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
                    let api: Api<FlintLeanWorkspace> = Api::namespaced((*client).clone(), &ns);
                    api.get_opt(&name).await.map_err(|e| e.to_string())
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
    use crate::lean_operator::crd::FlintLeanWorkspaceSpec;

    fn ws() -> FlintLeanWorkspace {
        FlintLeanWorkspace::new(
            "proj1",
            serde_json::from_value::<FlintLeanWorkspaceSpec>(serde_json::json!({
                "projectId": "team-a/proj1",
                "bucket": "agentws",
                "keyPrefix": "tenants/proj1",
            }))
            .unwrap(),
        )
    }

    fn review_for(pod: serde_json::Value) -> Value {
        json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "review-1",
                "namespace": "agents",
                "object": pod,
            }
        })
    }

    fn labeled_pod() -> Value {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": "agent-1", "labels": { INJECT_LABEL: "proj1" } },
            "spec": { "containers": [{ "name": "agent", "image": "agent:1" }] }
        })
    }

    const D: fn() -> InjectDefaults = || InjectDefaults { image: "flint-sync:test".into() };

    #[tokio::test]
    async fn labeled_pod_gets_a_jsonpatch_with_the_sidecar() {
        let resp =
            mutate_review(review_for(labeled_pod()), &D(), |_, _| async { Ok(Some(ws())) }).await;
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
        assert_eq!(patch[0]["op"], "replace");
        assert_eq!(patch[0]["path"], "/spec");
        let init = &patch[0]["value"]["initContainers"][0];
        assert_eq!(init["name"], "flint-sync");
        assert_eq!(init["restartPolicy"], "Always");
    }

    /// failurePolicy Fail end-to-end: a pod opting into a MISSING
    /// workspace is denied — it must never start ungated.
    #[tokio::test]
    async fn missing_workspace_denies_the_pod() {
        let resp =
            mutate_review(review_for(labeled_pod()), &D(), |_, _| async { Ok(None) }).await;
        let r = resp.pointer("/response").unwrap();
        assert_eq!(r["allowed"], false);
        assert!(r["status"]["message"].as_str().unwrap().contains("no such FlintLeanWorkspace"));
    }

    #[tokio::test]
    async fn refused_workspace_denies_the_pod() {
        let mut w = ws();
        w.status = Some(crate::lean_operator::crd::FlintLeanWorkspaceStatus {
            phase: Some("Refused".into()),
            ..Default::default()
        });
        let resp =
            mutate_review(review_for(labeled_pod()), &D(), move |_, _| async move { Ok(Some(w)) })
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
            panic!("must not even look up the workspace")
        })
        .await;
        let r = resp.pointer("/response").unwrap();
        assert_eq!(r["allowed"], true);
        assert!(r.get("patch").is_none());
    }

    #[test]
    fn cert_carries_the_service_sans() {
        let b = generate_cert("flint-lean-operator", "flint-system").unwrap();
        assert!(b.ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(b.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(b.key_pem.contains("PRIVATE KEY"));
    }
}
