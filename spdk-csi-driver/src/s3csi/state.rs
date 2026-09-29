//! Per-volume state on the node: `<plugin>/volumes/<volume_id>/state.json`.
//!
//! `NodeUnpublishVolume` receives only `volume_id` and `target_path`
//! (csi.proto), so everything teardown needs — which worker to delete,
//! which source to unmount, how long a lean drain may take — is written
//! here at publish time. The file is also how a restarted plugin
//! re-adopts the node's live volumes (`node.rs::adopt_existing`).
//!
//! Absence of the file on unpublish means "nothing to do" (idempotent):
//! the ephemeral-marker pattern of the block driver.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantRef {
    pub namespace: String,
    pub pod: String,
    pub pod_uid: String,
    pub service_account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VolumeState {
    pub version: u32,
    pub volume_id: String,
    /// `passthrough` | `lean`
    pub mode: String,
    pub cr: String,
    pub tenant: TenantRef,
    pub target_path: String,
    /// The plugin-owned mount source: the FUSE mount (passthrough) or
    /// the tree (lean). Bind-mounted to `target_path`.
    pub src: String,
    pub worker_namespace: String,
    pub worker_name: String,
    #[serde(default)]
    pub worker_uid: Option<String>,
    /// `publishing` until the bind to `target_path` succeeded.
    pub phase: String,
    pub credential_mode: String,
    /// Per-volume registration nonce (design §4.2): the worker sends it
    /// as `RoleSessionName` / the door checks it as the auth token.
    pub nonce: String,
    #[serde(default)]
    pub creds_expiration: Option<String>,
    #[serde(default)]
    pub token_expiration: Option<String>,
    #[serde(default)]
    pub last_probe_ok: Option<bool>,
    #[serde(default)]
    pub published_unix: Option<u64>,
    pub read_only: bool,
    pub owner_uid: u32,
    pub owner_gid: u32,
    /// Lean: the derived drain budget handed to the syncer's delete.
    #[serde(default)]
    pub grace_secs: Option<u64>,
    /// Lean: the loop image backing the tree, if quota mode is on.
    #[serde(default)]
    pub tree_image: Option<String>,
    /// Lean: when the unpublish drain (syncer delete with grace) began,
    /// so retried unpublishes can enforce the hard ceiling.
    #[serde(default)]
    pub drain_started_unix: Option<u64>,
    /// Lean: the syncer's non-secret `FLINT_SYNC_*` list as stamped at
    /// publish, so a worker lost at the pod level (evicted, node
    /// pressure) can be relaunched from a republish without re-deriving
    /// it from the CR.
    #[serde(default)]
    pub sync_env: Option<BTreeMap<String, String>>,
    /// `chert.us/on-behalf-of` as the pod said it, kept so a
    /// re-registration after a broker restart carries it again. Audit only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
    /// Passthrough: the shared read-only mount this volume is a MEMBER of
    /// ([`SharedMount::hash`]), when the CR opted its read-only consumers
    /// into one mounter per node (`spec.sharing.readOnly`). `src`,
    /// `worker_name` and `worker_uid` then name the SHARED mount's, and
    /// teardown is the shared record's last-member gate, not this
    /// volume's. Absent: the volume owns its mount, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
}

/// The pod-bound ServiceAccount token kubelet delivered with the latest
/// publish, kept beside the state. `NodeUnpublishVolume` carries no
/// token, and the final lean drain needs one to exchange for keys that
/// outlive the drain (design §5, final-barrier row). Root only, like
/// everything under the plugin directory; 0600 regardless.
pub const TOKEN_FILE: &str = "token";

/// Write the token if it differs from what is on disk. `Ok(true)` when
/// it was written.
pub fn save_token_if_changed(dir: &Path, token: &str) -> std::io::Result<bool> {
    let p = dir.join(TOKEN_FILE);
    if std::fs::read_to_string(&p).map(|t| t == token).unwrap_or(false) {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{TOKEN_FILE}.tmp"));
    std::fs::write(&tmp, token)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(tmp, p)?;
    Ok(true)
}

pub fn load_token(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(TOKEN_FILE))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Where an UNDRAINED lean tree is moved at unpublish instead of being
/// removed: `<plugin>/undrained/<volume dir>-<unix>`. Deliberately
/// outside `volumes/`, so startup adoption never sees it and no retried
/// unpublish touches it. The bytes in it exist nowhere else.
pub fn undrained_dir(plugin_root: &Path, volume_id: &str, unix: u64) -> PathBuf {
    plugin_root.join("undrained").join(format!("{}-{unix}", dir_name(volume_id)))
}

/// Volume ids are `csi-<sha256 hex>` from kubelet, but the directory
/// name is built defensively: anything outside `[A-Za-z0-9._-]` is
/// replaced, and a leading dot is refused, so a hostile id cannot walk
/// out of the plugin directory.
pub fn dir_name(volume_id: &str) -> String {
    let mut s: String = volume_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if s.is_empty() || s.starts_with('.') {
        s = format!("v_{s}");
    }
    s
}

pub fn volume_dir(plugin_root: &Path, volume_id: &str) -> PathBuf {
    plugin_root.join("volumes").join(dir_name(volume_id))
}

impl VolumeState {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("state.json")
    }

    pub fn load(dir: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(Self::path(dir)) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Atomic: write `state.json.tmp`, rename over.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, Self::path(dir))
    }

    /// Every `volumes/*/state.json` under the plugin root, for adoption.
    pub fn list(plugin_root: &Path) -> Vec<(PathBuf, Self)> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(plugin_root.join("volumes")) else {
            return out;
        };
        for e in rd.flatten() {
            let d = e.path();
            if let Ok(Some(s)) = Self::load(&d) {
                out.push((d, s));
            }
        }
        out
    }
}

// ── shared read-only mounts ──────────────────────────────────────────

/// One `mount-s3` shared by every read-only consumer of one CR on this
/// node, when the CR opted into it (`spec.sharing.readOnly`):
/// `<plugin>/shared/<hash>/state.json`, with the FUSE mount at
/// `<plugin>/shared/<hash>/src`. Design of record:
/// docs/plans/passthrough-read-only-mount-sharing.md.
///
/// The per-member [`VolumeState`] still exists, one per pod, and keeps
/// every invariant it had (its own nonce, registration, target, token);
/// it points here through `shared`. What this record adds is the member
/// set: the mounter, its worker and its source mount come down when the
/// LAST member leaves, and only then.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SharedMount {
    pub version: u32,
    /// 16 hex of sha256 over `key`: the directory name, the worker's
    /// name and its volume-id annotation.
    pub hash: String,
    /// The equality class, readable:
    /// `node/namespace/cr/uid:gid/mode/argv-<16 hex>`.
    pub key: String,
    pub namespace: String,
    pub cr: String,
    pub src: String,
    pub worker_namespace: String,
    pub worker_name: String,
    #[serde(default)]
    pub worker_uid: Option<String>,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub credential_mode: String,
    /// `publishing` until the first member's mount served and was bound.
    pub phase: String,
    /// The volume ids bound to it, in join order. Empty ⇒ nothing holds it.
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub created_unix: Option<u64>,
}

/// `<plugin>/shared/<hash>`: outside `volumes/`, so volume adoption never
/// mistakes a shared record for a volume.
pub fn shared_dir(plugin_root: &Path, hash: &str) -> PathBuf {
    plugin_root.join("shared").join(dir_name(hash))
}

/// The equality class of a shared mount, and its hash. Everything that
/// reaches the mounter's argv or the `mount(2)` call is in it: the node
/// (the worker's NAME must be unique cluster-wide), the namespace and
/// CR, the effective uid and gid (one owner per daemon — the argv
/// `--uid`, the kernel's `allow_other` owner and the uid the worker runs
/// as), the credential mode, and the whole argument vector, so a CR
/// edited under running members yields a NEW class for new members
/// rather than an old mount serving a new spec.
pub fn share_key(node: &str, namespace: &str, cr: &str, uid: u32, gid: u32, mode: &str, argv: &[String]) -> (String, String) {
    let mut h = Sha256::new();
    for a in argv {
        h.update(a.as_bytes());
        h.update([0u8]);
    }
    let argv_hash = hex16(&h.finalize());
    let key = format!("{node}/{namespace}/{cr}/{uid}:{gid}/{mode}/argv-{argv_hash}");
    let hash = hex16(&Sha256::digest(key.as_bytes()));
    (hash, key)
}

fn hex16(digest: &[u8]) -> String {
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

impl SharedMount {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("state.json")
    }

    pub fn load(dir: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(Self::path(dir)) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Atomic: write `state.json.tmp`, rename over.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, Self::path(dir))
    }

    /// Every `shared/*/state.json` under the plugin root, for adoption.
    pub fn list(plugin_root: &Path) -> Vec<(PathBuf, Self)> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(plugin_root.join("shared")) else {
            return out;
        };
        for e in rd.flatten() {
            let d = e.path();
            if let Ok(Some(s)) = Self::load(&d) {
                out.push((d, s));
            }
        }
        out
    }

    /// Idempotent: a retried join is one member, not two.
    pub fn add_member(&mut self, volume_id: &str) {
        if !self.members.iter().any(|m| m == volume_id) {
            self.members.push(volume_id.to_string());
        }
    }

    /// `true` when it was a member.
    pub fn remove_member(&mut self, volume_id: &str) -> bool {
        let before = self.members.len();
        self.members.retain(|m| m != volume_id);
        before != self.members.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_names_cannot_escape() {
        assert_eq!(dir_name("csi-abc"), "csi-abc");
        assert_eq!(dir_name("../x"), "v_.._x");
        assert_eq!(dir_name("a/b"), "a_b");
        assert_eq!(dir_name(""), "v_");
        assert!(!dir_name("..").starts_with('.'));
    }

    #[test]
    fn round_trips_and_lists() {
        let root = tempfile::tempdir().unwrap();
        let s = VolumeState {
            version: STATE_VERSION,
            volume_id: "csi-1".into(),
            mode: "passthrough".into(),
            cr: "datasets".into(),
            tenant: TenantRef {
                namespace: "team-a".into(),
                pod: "p".into(),
                pod_uid: "u".into(),
                service_account: "sa".into(),
            },
            target_path: "/t".into(),
            src: "/s".into(),
            worker_namespace: "flint-workers".into(),
            worker_name: "s3w-x".into(),
            worker_uid: None,
            phase: "publishing".into(),
            credential_mode: "broker".into(),
            nonce: "n".into(),
            creds_expiration: None,
            token_expiration: None,
            last_probe_ok: None,
            published_unix: None,
            read_only: false,
            owner_uid: 1001,
            owner_gid: 1001,
            grace_secs: None,
            tree_image: None,
            drain_started_unix: None,
            sync_env: Some(BTreeMap::from([("FLINT_SYNC_ROOT".to_string(), "/workspace".to_string())])),
            on_behalf_of: Some("alice@example.com".into()),
            shared: None,
        };
        let d = volume_dir(root.path(), "csi-1");
        assert!(VolumeState::load(&d).unwrap().is_none());
        s.save(&d).unwrap();
        assert_eq!(VolumeState::load(&d).unwrap().unwrap(), s);
        let all = VolumeState::list(root.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.volume_id, "csi-1");
    }

    /// A state file written before `sync_env` existed still loads: the
    /// field is optional, and a relaunch on such a volume is refused
    /// with a message rather than a deserialization error.
    #[test]
    fn older_state_without_sync_env_still_loads() {
        let root = tempfile::tempdir().unwrap();
        let d = volume_dir(root.path(), "csi-old");
        std::fs::create_dir_all(&d).unwrap();
        let mut v: serde_json::Value = serde_json::json!({
            "version": 1, "volumeId": "csi-old", "mode": "lean", "cr": "ws",
            "tenant": {"namespace": "t", "pod": "p", "pod_uid": "u", "service_account": "s"},
            "targetPath": "/t", "src": "/s", "workerNamespace": "flint-workers", "workerName": "s3w-x",
            "phase": "published", "credentialMode": "broker", "nonce": "n",
            "readOnly": false, "ownerUid": 1001, "ownerGid": 1001
        });
        v.as_object_mut().unwrap().remove("syncEnv");
        std::fs::write(VolumeState::path(&d), serde_json::to_vec(&v).unwrap()).unwrap();
        let s = VolumeState::load(&d).unwrap().unwrap();
        assert!(s.sync_env.is_none());
        assert_eq!(s.phase, "published");
    }

    /// The preserved-tree directory lives OUTSIDE `volumes/`: adoption
    /// lists nothing there, so a preserved tree is never adopted,
    /// cleaned up, or retried.
    #[test]
    fn undrained_trees_are_invisible_to_adoption() {
        let root = tempfile::tempdir().unwrap();
        let dest = undrained_dir(root.path(), "csi-1", 1_700_000_000);
        assert!(!dest.starts_with(root.path().join("volumes")));
        assert_eq!(dest.file_name().unwrap().to_str().unwrap(), "csi-1-1700000000");
        let s = VolumeState {
            version: STATE_VERSION,
            volume_id: "csi-1".into(),
            mode: "lean".into(),
            cr: "ws".into(),
            tenant: TenantRef { namespace: "t".into(), pod: "p".into(), pod_uid: "u".into(), service_account: "s".into() },
            target_path: "/t".into(),
            src: "/s".into(),
            worker_namespace: "flint-workers".into(),
            worker_name: "s3w-x".into(),
            worker_uid: None,
            phase: "published".into(),
            credential_mode: "broker".into(),
            nonce: "n".into(),
            creds_expiration: None,
            token_expiration: None,
            last_probe_ok: None,
            published_unix: None,
            read_only: false,
            owner_uid: 1001,
            owner_gid: 1001,
            grace_secs: None,
            tree_image: None,
            drain_started_unix: None,
            sync_env: None,
            on_behalf_of: None,
            shared: None,
        };
        s.save(&dest).unwrap();
        assert!(VolumeState::list(root.path()).is_empty(), "a preserved tree must not be listed for adoption");
        // A hostile id cannot walk out of undrained/ either.
        assert!(undrained_dir(root.path(), "../x", 1).starts_with(root.path().join("undrained")));
    }

    #[test]
    fn token_is_written_once_at_0600_and_reloaded() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let d = volume_dir(root.path(), "csi-1");
        assert!(load_token(&d).is_none());
        assert!(save_token_if_changed(&d, "eyJ.a").unwrap(), "first write");
        assert!(!save_token_if_changed(&d, "eyJ.a").unwrap(), "same token: no rewrite");
        assert!(save_token_if_changed(&d, "eyJ.b").unwrap(), "rotated token: rewritten");
        assert_eq!(load_token(&d).as_deref(), Some("eyJ.b"));
        let mode = std::fs::metadata(d.join(TOKEN_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!d.join(format!("{TOKEN_FILE}.tmp")).exists());
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;

    fn argv() -> Vec<String> {
        vec!["b".into(), "{FUSE_FD}".into(), "--foreground".into(), "--read-only".into()]
    }

    /// Two publishes share a mounter only when everything that reaches
    /// the mounter or the mount(2) call is equal — and the node, because
    /// the worker's name is the hash and pod names are per namespace,
    /// not per node.
    #[test]
    fn the_class_is_node_namespace_cr_owner_mode_and_argv() {
        let (h, k) = share_key("n1", "team-a", "datasets", 1001, 1001, "broker", &argv());
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "{h}");
        assert!(k.starts_with("n1/team-a/datasets/1001:1001/broker/argv-"), "{k}");
        assert_eq!(share_key("n1", "team-a", "datasets", 1001, 1001, "broker", &argv()).0, h, "stable");
        let mut more = argv();
        more.extend(["--metadata-ttl".to_string(), "60".to_string()]);
        let others = [
            ("node", share_key("n2", "team-a", "datasets", 1001, 1001, "broker", &argv())),
            ("namespace", share_key("n1", "team-b", "datasets", 1001, 1001, "broker", &argv())),
            ("cr", share_key("n1", "team-a", "datasets-2", 1001, 1001, "broker", &argv())),
            ("uid", share_key("n1", "team-a", "datasets", 1002, 1001, "broker", &argv())),
            ("gid", share_key("n1", "team-a", "datasets", 1001, 1002, "broker", &argv())),
            ("mode", share_key("n1", "team-a", "datasets", 1001, 1001, "ambient", &argv())),
            ("argv", share_key("n1", "team-a", "datasets", 1001, 1001, "broker", &more)),
        ];
        for (what, (oh, _)) in others {
            assert_ne!(oh, h, "{what} must separate the class");
        }
    }

    #[test]
    fn a_shared_mount_round_trips_and_membership_is_a_set() {
        let root = tempfile::tempdir().unwrap();
        let (h, k) = share_key("n1", "team-a", "datasets", 1001, 1001, "broker", &argv());
        let d = shared_dir(root.path(), &h);
        assert!(SharedMount::load(&d).unwrap().is_none());
        let mut sm = SharedMount {
            version: STATE_VERSION,
            hash: h.clone(),
            key: k,
            namespace: "team-a".into(),
            cr: "datasets".into(),
            src: d.join("src").display().to_string(),
            worker_namespace: "flint-workers".into(),
            worker_name: format!("s3w-{h}"),
            worker_uid: None,
            owner_uid: 1001,
            owner_gid: 1001,
            credential_mode: "broker".into(),
            phase: "publishing".into(),
            members: vec![],
            created_unix: Some(1),
        };
        sm.add_member("csi-1");
        sm.add_member("csi-2");
        sm.add_member("csi-1");
        assert_eq!(sm.members, vec!["csi-1", "csi-2"], "a retried join is one member");
        sm.save(&d).unwrap();
        assert_eq!(SharedMount::load(&d).unwrap().unwrap(), sm);
        let all = SharedMount::list(root.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.hash, h);
        assert!(sm.remove_member("csi-1"));
        assert!(!sm.remove_member("csi-1"), "leaving twice is one leave");
        assert_eq!(sm.members, vec!["csi-2"]);
        assert!(sm.remove_member("csi-2"));
        assert!(sm.members.is_empty(), "the last member out leaves nothing holding it");
        // Outside volumes/: volume adoption never lists a shared record.
        assert!(VolumeState::list(root.path()).is_empty());
        assert!(shared_dir(root.path(), "../x").starts_with(root.path().join("shared")));
    }

    /// A state file written before `shared` existed is a volume that
    /// owns its mount.
    #[test]
    fn older_state_without_shared_owns_its_mount() {
        let root = tempfile::tempdir().unwrap();
        let d = volume_dir(root.path(), "csi-old");
        std::fs::create_dir_all(&d).unwrap();
        let v = serde_json::json!({
            "version": 1, "volumeId": "csi-old", "mode": "passthrough", "cr": "datasets",
            "tenant": {"namespace": "t", "pod": "p", "pod_uid": "u", "service_account": "s"},
            "targetPath": "/t", "src": "/s", "workerNamespace": "flint-workers", "workerName": "s3w-x",
            "phase": "published", "credentialMode": "broker", "nonce": "n",
            "readOnly": true, "ownerUid": 1001, "ownerGid": 1001
        });
        std::fs::write(VolumeState::path(&d), serde_json::to_vec(&v).unwrap()).unwrap();
        let s = VolumeState::load(&d).unwrap().unwrap();
        assert!(s.shared.is_none());
        // and the field is not written when absent, so the file stays readable by an older plugin
        s.save(&d).unwrap();
        assert!(!std::fs::read_to_string(VolumeState::path(&d)).unwrap().contains("\"shared\""));
    }
}
