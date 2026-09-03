//! What arrives in `NodePublishVolumeRequest.volume_context`, sorted into
//! what the POD AUTHOR wrote and what KUBELET asserted (design §3.4).
//!
//! `volumeAttributes` are attacker-controlled input to a privileged
//! process. Exactly two selector keys and two integer keys are accepted;
//! every other pod-authored key is refused BY NAME. Bucket, prefix,
//! endpoint, region, image and credentials are never accepted from the
//! pod — the CR is the policy object.
//!
//! Kubelet's keys (`csi.storage.k8s.io/*`) are written OVER anything the
//! pod put there (`csi_util.go` `mergeMap`), so they are the only inputs
//! the authorization step trusts.

use std::collections::HashMap;

use super::DRIVER_NAME;

pub const ATTR_MOUNT: &str = "flint.io/mount";
pub const ATTR_WORKSPACE: &str = "flint.io/workspace";
pub const ATTR_UID: &str = "flint.io/uid";
pub const ATTR_GID: &str = "flint.io/gid";

pub const K_POD_NAME: &str = "csi.storage.k8s.io/pod.name";
pub const K_POD_NAMESPACE: &str = "csi.storage.k8s.io/pod.namespace";
pub const K_POD_UID: &str = "csi.storage.k8s.io/pod.uid";
pub const K_SA_NAME: &str = "csi.storage.k8s.io/serviceAccount.name";
pub const K_EPHEMERAL: &str = "csi.storage.k8s.io/ephemeral";
pub const K_SA_TOKENS: &str = "csi.storage.k8s.io/serviceAccount.tokens";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// `flint.io/mount: <FlintPassthroughMount name>`
    Mount(String),
    /// `flint.io/workspace: <FlintLeanWorkspace name>`
    Workspace(String),
}

impl Selector {
    pub fn name(&self) -> &str {
        match self {
            Selector::Mount(n) | Selector::Workspace(n) => n,
        }
    }
    pub fn mode(&self) -> &'static str {
        match self {
            Selector::Mount(_) => "passthrough",
            Selector::Workspace(_) => "lean",
        }
    }
}

/// A pod-bound ServiceAccount token kubelet minted for this publish.
#[derive(Clone, PartialEq, Eq)]
pub struct SaToken {
    pub token: String,
    pub expiration: String,
}

impl std::fmt::Debug for SaToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token itself.
        write!(f, "SaToken(len={}, exp={})", self.token.len(), self.expiration)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRequest {
    pub selector: Selector,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub pod_name: String,
    pub pod_namespace: String,
    pub pod_uid: String,
    pub service_account: String,
    pub token: Option<SaToken>,
}

fn name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 253
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && !n.starts_with(['-', '.'])
        && !n.ends_with(['-', '.'])
}

fn parse_id(key: &str, v: &str) -> Result<u32, String> {
    v.trim()
        .parse::<u32>()
        .map_err(|_| format!("volumeAttributes.{key} {v:?} is not a non-negative integer"))
}

/// Sort the context. Every refusal names the key it is about.
pub fn parse(ctx: &HashMap<String, String>) -> Result<PublishRequest, String> {
    let mut mount = None;
    let mut workspace = None;
    let mut uid = None;
    let mut gid = None;
    let mut unknown = Vec::new();
    for (k, v) in ctx {
        match k.as_str() {
            ATTR_MOUNT => mount = Some(v.clone()),
            ATTR_WORKSPACE => workspace = Some(v.clone()),
            ATTR_UID => uid = Some(parse_id(ATTR_UID, v)?),
            ATTR_GID => gid = Some(parse_id(ATTR_GID, v)?),
            k if k.starts_with("csi.storage.k8s.io/") => {}
            k => unknown.push(k.to_string()),
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        return Err(format!(
            "volumeAttributes {} not accepted — the pod may only name the CR ({ATTR_MOUNT} or \
             {ATTR_WORKSPACE}) and a presentation uid/gid ({ATTR_UID}, {ATTR_GID}); bucket, \
             prefix, endpoint, image and credentials come from the CR",
            unknown.join(", ")
        ));
    }
    let selector = match (mount, workspace) {
        (Some(m), None) => Selector::Mount(m),
        (None, Some(w)) => Selector::Workspace(w),
        (Some(_), Some(_)) => {
            return Err(format!(
                "volumeAttributes set both {ATTR_MOUNT} and {ATTR_WORKSPACE}; exactly one is required"
            ))
        }
        (None, None) => {
            return Err(format!(
                "volumeAttributes must name the CR: {ATTR_MOUNT}: <FlintPassthroughMount> or \
                 {ATTR_WORKSPACE}: <FlintLeanWorkspace>"
            ))
        }
    };
    if !name_ok(selector.name()) {
        return Err(format!(
            "volumeAttributes {} {:?} is not a valid resource name",
            match selector {
                Selector::Mount(_) => ATTR_MOUNT,
                Selector::Workspace(_) => ATTR_WORKSPACE,
            },
            selector.name()
        ));
    }

    let need = |k: &str| -> Result<String, String> {
        ctx.get(k)
            .filter(|v| !v.is_empty())
            .cloned()
            .ok_or_else(|| format!("{k} missing from volume_context — is CSIDriver.spec.podInfoOnMount true?"))
    };
    if ctx.get(K_EPHEMERAL).map(|v| v.as_str()) != Some("true") {
        return Err(format!(
            "{K_EPHEMERAL} is not \"true\": {DRIVER_NAME} serves ephemeral inline volumes only"
        ));
    }
    let pod_name = need(K_POD_NAME)?;
    let pod_namespace = need(K_POD_NAMESPACE)?;
    let pod_uid = need(K_POD_UID)?;
    let service_account = need(K_SA_NAME)?;
    let token = match ctx.get(K_SA_TOKENS) {
        None => None,
        Some(raw) => parse_tokens(raw)?,
    };
    Ok(PublishRequest { selector, uid, gid, pod_name, pod_namespace, pod_uid, service_account, token })
}

/// `{"<audience>": {"token": "...", "expirationTimestamp": "..."}}`
/// (kubelet `csi_mounter.go`). We want OUR audience.
fn parse_tokens(raw: &str) -> Result<Option<SaToken>, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("{K_SA_TOKENS} is not JSON: {e}"))?;
    let Some(entry) = v.get(DRIVER_NAME) else {
        return Ok(None);
    };
    let token = entry
        .get("token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("{K_SA_TOKENS}[{DRIVER_NAME}] has no token"))?;
    let expiration = entry
        .get("expirationTimestamp")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(Some(SaToken { token: token.to_string(), expiration }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kubelet(mut m: HashMap<String, String>) -> HashMap<String, String> {
        m.insert(K_POD_NAME.into(), "agent".into());
        m.insert(K_POD_NAMESPACE.into(), "team-a".into());
        m.insert(K_POD_UID.into(), "uid-1".into());
        m.insert(K_SA_NAME.into(), "trainer".into());
        m.insert(K_EPHEMERAL.into(), "true".into());
        m
    }

    #[test]
    fn a_mount_selector_with_kubelet_keys_parses() {
        let ctx = kubelet(HashMap::from([(ATTR_MOUNT.into(), "datasets".into())]));
        let r = parse(&ctx).unwrap();
        assert_eq!(r.selector, Selector::Mount("datasets".into()));
        assert_eq!(r.pod_namespace, "team-a");
        assert_eq!(r.service_account, "trainer");
        assert!(r.token.is_none());
    }

    #[test]
    fn unknown_attributes_are_refused_by_name() {
        let ctx = kubelet(HashMap::from([
            (ATTR_MOUNT.into(), "d".into()),
            ("bucket".into(), "other".into()),
            ("image".into(), "evil".into()),
        ]));
        let e = parse(&ctx).unwrap_err();
        assert!(e.contains("bucket") && e.contains("image"), "{e}");
    }

    #[test]
    fn exactly_one_selector() {
        let e = parse(&kubelet(HashMap::new())).unwrap_err();
        assert!(e.contains(ATTR_MOUNT) && e.contains(ATTR_WORKSPACE), "{e}");
        let e = parse(&kubelet(HashMap::from([
            (ATTR_MOUNT.into(), "a".into()),
            (ATTR_WORKSPACE.into(), "b".into()),
        ])))
        .unwrap_err();
        assert!(e.contains("both"), "{e}");
    }

    #[test]
    fn selector_names_are_resource_names() {
        for bad in ["", "-x", "x-", "Upper", "a/b", "a b", "../x"] {
            let e = parse(&kubelet(HashMap::from([(ATTR_MOUNT.into(), bad.into())]))).unwrap_err();
            assert!(e.contains("not a valid resource name"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn uid_gid_must_be_integers() {
        let e = parse(&kubelet(HashMap::from([
            (ATTR_MOUNT.into(), "d".into()),
            (ATTR_UID.into(), "root".into()),
        ])))
        .unwrap_err();
        assert!(e.contains(ATTR_UID), "{e}");
        let r = parse(&kubelet(HashMap::from([
            (ATTR_WORKSPACE.into(), "w".into()),
            (ATTR_UID.into(), "1001".into()),
            (ATTR_GID.into(), " 1002 ".into()),
        ])))
        .unwrap();
        assert_eq!((r.uid, r.gid), (Some(1001), Some(1002)));
        assert_eq!(r.selector.mode(), "lean");
    }

    #[test]
    fn non_ephemeral_and_missing_pod_info_are_refused() {
        let mut ctx = kubelet(HashMap::from([(ATTR_MOUNT.into(), "d".into())]));
        ctx.insert(K_EPHEMERAL.into(), "false".into());
        assert!(parse(&ctx).unwrap_err().contains("ephemeral"));
        let mut ctx = kubelet(HashMap::from([(ATTR_MOUNT.into(), "d".into())]));
        ctx.remove(K_SA_NAME);
        assert!(parse(&ctx).unwrap_err().contains(K_SA_NAME));
    }

    #[test]
    fn our_audience_token_is_picked_and_never_debug_printed() {
        let mut ctx = kubelet(HashMap::from([(ATTR_MOUNT.into(), "d".into())]));
        ctx.insert(
            K_SA_TOKENS.into(),
            r#"{"other":{"token":"zzz"},"s3.flint.io":{"token":"eyJ.secret","expirationTimestamp":"2026-09-02T00:00:00Z"}}"#.into(),
        );
        let r = parse(&ctx).unwrap();
        let t = r.token.clone().unwrap();
        assert_eq!(t.token, "eyJ.secret");
        assert!(!format!("{r:?}").contains("secret"), "Debug must redact the token");
        ctx.insert(K_SA_TOKENS.into(), r#"{"other":{"token":"zzz"}}"#.into());
        assert!(parse(&ctx).unwrap().token.is_none());
        ctx.insert(K_SA_TOKENS.into(), "not json".into());
        assert!(parse(&ctx).unwrap_err().contains("not JSON"));
    }
}
