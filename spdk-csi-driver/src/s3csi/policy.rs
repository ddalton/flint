//! The policy block both CRDs gain under the CSI delivery (design §3.3):
//! who may mount this CR, and how the worker gets its credential.
//!
//! Shared by the hand-written passthrough CRD (`passthrough/spec.rs`,
//! plain serde) and the schemars-derived lean CRD (`lean_operator/crd.rs`),
//! so it derives both.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which ServiceAccounts (in the CR's own namespace) may mount it.
///
/// ABSENT = DENY in csi mode. The webhook delivery trusted "any pod in
/// the namespace"; that posture is available only as an explicit
/// `["*"]`, so a namespace opts into it rather than inheriting it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Consumers {
    #[serde(default)]
    pub service_accounts: Vec<String>,
}

impl Consumers {
    /// `true` when `sa` is listed, or the list is the explicit wildcard.
    pub fn allows(&self, sa: &str) -> bool {
        self.service_accounts.iter().any(|s| s == "*" || s == sa)
    }
}

/// How the worker obtains its S3 credential.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// `broker` (default): the node plugin exchanges the pod-bound
    /// ServiceAccount token at `flint-s3-broker` for short-lived keys
    /// and serves them to the worker over its loopback door.
    /// `webIdentity`: the worker itself calls the broker's STS façade
    /// (needs TLS trust in the mounter image).
    /// `static`: the pod's `nodePublishSecretRef` (AWS_* keys verbatim)
    /// — the interim arm, today's trust level.
    /// `ambient`: nothing; the worker's own AWS chain.
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "broker".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMode {
    Broker,
    WebIdentity,
    Static,
    Ambient,
}

impl CredentialMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "broker" | "" => Ok(Self::Broker),
            "webIdentity" => Ok(Self::WebIdentity),
            "static" => Ok(Self::Static),
            "ambient" => Ok(Self::Ambient),
            other => Err(format!(
                "identity.mode {other:?} is not one of broker | webIdentity | static | ambient"
            )),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Broker => "broker",
            Self::WebIdentity => "webIdentity",
            Self::Static => "static",
            Self::Ambient => "ambient",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_consumers_deny_and_wildcard_is_explicit() {
        let none = Consumers::default();
        assert!(!none.allows("alice"));
        let some = Consumers { service_accounts: vec!["alice".into()] };
        assert!(some.allows("alice"));
        assert!(!some.allows("bob"));
        let all = Consumers { service_accounts: vec!["*".into()] };
        assert!(all.allows("bob"));
    }

    #[test]
    fn identity_mode_defaults_to_broker_and_refuses_unknown() {
        let i: Identity = serde_json::from_str("{}").unwrap();
        assert_eq!(CredentialMode::parse(&i.mode).unwrap(), CredentialMode::Broker);
        assert!(CredentialMode::parse("knox").unwrap_err().contains("knox"));
        for m in ["broker", "webIdentity", "static", "ambient"] {
            assert_eq!(CredentialMode::parse(m).unwrap().as_str(), m);
        }
    }
}
