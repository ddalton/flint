//! The policy block both CRDs gain under the CSI delivery (design §3.3):
//! who may mount this CR, at which access, and how the worker gets its
//! credential.
//!
//! Shared by the hand-written passthrough CRD (`passthrough/spec.rs`,
//! plain serde) and the schemars-derived lean CRD (`lean_operator/crd.rs`),
//! so it derives both. `FlintRepo` (forge) takes [`Consumers`], the list
//! without an access level: its door has no read-only posture, and a
//! `readOnlyServiceAccounts` it would accept and ignore is the kind of
//! field a reader trusts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which ServiceAccounts (in the CR's own namespace) forge's door lets
/// through. See [`MountConsumers`] for the mount CRDs' version.
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

/// What a mount may do with the bucket (per-user access design §4.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    Read,
    /// The default on the wire: a registration from a plugin that
    /// predates the field asserted nothing narrower, and the broker
    /// narrows it by the CR in any case.
    #[default]
    ReadWrite,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::ReadWrite => "readWrite",
        }
    }
    pub fn is_read(self) -> bool {
        self == Access::Read
    }
}

/// Which ServiceAccounts (in the CR's own namespace) may mount a
/// `FlintLeanWorkspace` or a `FlintPassthroughMount`, and at which access.
///
/// ABSENT = DENY in csi mode. The webhook delivery trusted "any pod in
/// the namespace"; that posture is available only as an explicit
/// `["*"]`, so a namespace opts into it rather than inheriting it.
///
/// Two lists rather than a list of objects: a string-or-object union is
/// a schema junctor, which Kubernetes refuses in a structural CRD.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MountConsumers {
    /// May mount read-write, or read-only when the pod's volume says
    /// `readOnly: true`.
    #[serde(default)]
    pub service_accounts: Vec<String>,
    /// May mount read-only and never wider: `readOnly: false` is narrowed,
    /// not refused. The syncer follows the workspace and publishes
    /// nothing, the tree is bound read-only, and the broker issues a
    /// credential that cannot write where its backend can scope one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only_service_accounts: Vec<String>,
}

impl MountConsumers {
    /// The access `sa` gets, or `None` for a refusal.
    ///
    /// The most specific entry decides, and at equal specificity the
    /// narrower one: a name before the wildcard, and the read-only list
    /// before the read-write one. So `serviceAccounts: ["*"]` with
    /// `readOnlyServiceAccounts: [agent-ro]` is "everyone writes except
    /// agent-ro", an SA named in both lists reads, and
    /// `serviceAccounts: [editor]` with `readOnlyServiceAccounts: ["*"]`
    /// is "editor writes, everyone else reads".
    pub fn access(&self, sa: &str, read_only_requested: bool) -> Option<Access> {
        let named = |list: &[String]| list.iter().any(|s| s == sa);
        let wild = |list: &[String]| list.iter().any(|s| s == "*");
        let asked = if read_only_requested { Access::Read } else { Access::ReadWrite };
        if named(&self.read_only_service_accounts) {
            Some(Access::Read)
        } else if named(&self.service_accounts) {
            Some(asked)
        } else if wild(&self.read_only_service_accounts) {
            Some(Access::Read)
        } else if wild(&self.service_accounts) {
            Some(asked)
        } else {
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.service_accounts.is_empty() && self.read_only_service_accounts.is_empty()
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
    fn mount_access_is_decided_by_the_most_specific_then_narrowest_entry() {
        use Access::*;
        let c = |rw: &[&str], ro: &[&str]| MountConsumers {
            service_accounts: rw.iter().map(|s| s.to_string()).collect(),
            read_only_service_accounts: ro.iter().map(|s| s.to_string()).collect(),
        };
        // (consumers, sa, readOnly asked, expected)
        let table = [
            (c(&[], &[]), "a", false, None),
            (c(&["a"], &[]), "a", false, Some(ReadWrite)),
            (c(&["a"], &[]), "a", true, Some(Read)),
            (c(&["a"], &[]), "b", false, None),
            // readOnly: false never widens a read-only entry.
            (c(&[], &["a"]), "a", false, Some(Read)),
            (c(&[], &["a"]), "b", false, None),
            // Named in both: the narrower.
            (c(&["a"], &["a"]), "a", false, Some(Read)),
            // A name beats a wildcard, either way round.
            (c(&["*"], &["a"]), "a", false, Some(Read)),
            (c(&["*"], &["a"]), "b", false, Some(ReadWrite)),
            (c(&["a"], &["*"]), "a", false, Some(ReadWrite)),
            (c(&["a"], &["*"]), "b", false, Some(Read)),
            // Two wildcards: the narrower.
            (c(&["*"], &["*"]), "b", false, Some(Read)),
            (c(&["*"], &[]), "b", true, Some(Read)),
        ];
        for (i, (cons, sa, ro, want)) in table.into_iter().enumerate() {
            assert_eq!(cons.access(sa, ro), want, "row {i}: {cons:?} sa={sa} readOnly={ro}");
        }
    }

    #[test]
    fn an_old_cr_and_an_old_registration_read_as_before() {
        let c: MountConsumers = serde_json::from_str(r#"{"serviceAccounts":["a"]}"#).unwrap();
        assert_eq!(c.access("a", false), Some(Access::ReadWrite));
        // An empty read-only list is not written back into the CR.
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"serviceAccounts":["a"]}"#);
        #[derive(Deserialize)]
        struct Reg {
            #[serde(default)]
            access: Access,
        }
        assert_eq!(serde_json::from_str::<Reg>("{}").unwrap().access, Access::ReadWrite);
        assert_eq!(serde_json::from_str::<Reg>(r#"{"access":"read"}"#).unwrap().access, Access::Read);
        assert_eq!(serde_json::to_string(&Access::ReadWrite).unwrap(), r#""readWrite""#);
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
