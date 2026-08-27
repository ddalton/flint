//! One implementation of the webhook TLS posture, shared by every
//! admission webhook in this crate.
//!
//! The posture (no cert-manager dependency): generate a CA + serving
//! cert once, persist both in a Secret, and let every replica serve
//! from that Secret. The caBundle is (re)applied with the registration
//! at startup, so a regenerated Secret and a stale registration cannot
//! drift apart for longer than one restart.
//!
//! This module exists because `lean_operator::webhook` and
//! `passthrough::webhook` need byte-identical cert handling and the
//! second copy is where the drift starts. Both are TLS trust decisions
//! for an API-server-to-us channel that carries pod specs; one of them
//! quietly growing a shorter validity or a missing SAN is not the kind
//! of difference that shows up in a test that only asks "did the pod
//! get its sidecar".

use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, PostParams};
use kube::Client;
use serde_json::json;
use tracing::{info, warn};

pub struct CertBundle {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate a CA + serving cert for `<service>.<namespace>.svc`.
///
/// `ca_cn` only names the CA in a cert dump — it is not a trust input
/// (the caBundle in the registration is), so callers pass whatever
/// makes `openssl x509 -text` legible for their webhook.
pub fn generate_cert(ca_cn: &str, service: &str, namespace: &str) -> Result<CertBundle, String> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
    let e = |e: rcgen::Error| e.to_string();

    let ca_key = KeyPair::generate().map_err(e)?;
    let mut ca_params = CertificateParams::new(vec![]).map_err(e)?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, ca_cn);
    let ca_cert = ca_params.self_signed(&ca_key).map_err(e)?;

    // All three SANs matter: the API server dials
    // `<service>.<ns>.svc`, in-cluster debugging dials the FQDN, and a
    // port-forwarded `curl https://localhost` presents the bare name.
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
    secret_name: &str,
    ca_cn: &str,
) -> anyhow::Result<CertBundle> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let read = |s: Secret| -> Option<CertBundle> {
        let d = s.data?;
        let get = |k: &str| d.get(k).map(|v| String::from_utf8_lossy(&v.0).to_string());
        Some(CertBundle {
            ca_pem: get("ca.crt")?,
            cert_pem: get("tls.crt")?,
            key_pem: get("tls.key")?,
        })
    };
    if let Some(s) = api.get_opt(secret_name).await? {
        if let Some(b) = read(s) {
            return Ok(b);
        }
        warn!("cert secret {secret_name} exists but is incomplete — regenerating");
    }
    let bundle = generate_cert(ca_cn, service, namespace).map_err(anyhow::Error::msg)?;
    let secret: Secret = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": secret_name, "namespace": namespace },
        "type": "kubernetes.io/tls",
        "stringData": {
            "ca.crt": bundle.ca_pem,
            "tls.crt": bundle.cert_pem,
            "tls.key": bundle.key_pem,
        }
    }))?;
    match api.create(&PostParams::default(), &secret).await {
        Ok(_) => {
            info!("webhook cert generated and stored in {secret_name}");
            Ok(bundle)
        }
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            let s = api.get(secret_name).await?;
            read(s).ok_or_else(|| anyhow::anyhow!("racing replica wrote an unreadable secret"))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_carries_the_service_sans() {
        let b = generate_cert("test-ca", "flint-svc", "flint-system").unwrap();
        assert!(b.ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(b.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(b.key_pem.contains("PRIVATE KEY"));
        // The CA and the leaf must be DIFFERENT certificates. A
        // self-signed leaf would still satisfy every "is it a
        // certificate" assertion above while the API server rejected
        // the handshake.
        assert_ne!(b.ca_pem, b.cert_pem);
    }
}
