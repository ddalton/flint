//! The mount-s3 argument vector for a FlintPassthroughMount — the one
//! piece of the retired sidecar injector the CSI delivery still needs.
//! The node plugin (`s3csi::node`) performs the `mount(2)` itself and
//! hands the FUSE fd to an unprivileged worker, which execs Mountpoint
//! on `/dev/fd/3` with exactly this argv (design §3.4 step 9). Never
//! concatenated into a shell string: the worker passes it as ARGUMENTS.

use super::spec::MountSpec;

/// `owner` is the resolved (uid, gid) the mount presents; `target` is
/// the mount point argument (`{FUSE_FD}` in the CSI delivery, which the
/// worker rewrites to `/dev/fd/3`).
///
/// `--allow-other` is passed in BOTH shapes, and in fd mode it is not
/// redundant with the kernel's `allow_other` mount option: Mountpoint's
/// FUSE session (fuser) enforces its OWN owner-only ACL and answers
/// every lookup/getattr/open/statfs from any uid other than the
/// daemon's with EACCES unless the flag is given. Measured on the kind
/// rig: with the daemon at uid 1001 and no flag, root's `statfs` on
/// the mount is refused, so the driver's readiness probe can never
/// pass; with the flag, root, the owner and a third uid all read.
pub fn mounter_args_for(spec: &MountSpec, owner: (Option<i64>, Option<i64>), target: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![spec.bucket.clone(), target.to_string(), "--foreground".into(), "--allow-other".into()];
    if let Some(p) = spec.key_prefix.as_deref().filter(|p| !p.is_empty()) {
        a.push("--prefix".into());
        // mount-s3 requires the trailing slash and rejects the prefix
        // without it.
        a.push(format!("{}/", p.trim_end_matches('/')));
    }
    if let Some(url) = &spec.endpoint {
        a.push("--endpoint-url".into());
        a.push(url.clone());
    }
    if spec.use_path_style() {
        a.push("--force-path-style".into());
    }
    if let Some(r) = &spec.region {
        a.push("--region".into());
        a.push(r.clone());
    }
    if spec.read_only {
        a.push("--read-only".into());
    } else {
        // Mountpoint refuses to delete or overwrite unless told twice.
        // Without these a read-write mount silently has no way to
        // replace a file, which reads as a permissions bug rather than
        // a design limit. It still cannot rename or append — see
        // `spec`'s header.
        a.push("--allow-delete".into());
        a.push("--allow-overwrite".into());
    }
    if let Some(uid) = owner.0 {
        a.push("--uid".into());
        a.push(uid.to_string());
    }
    if let Some(gid) = owner.1 {
        a.push("--gid".into());
        a.push(gid.to_string());
    }
    a.extend(spec.mount_options.iter().cloned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MountSpec {
        serde_json::from_value(serde_json::json!({
            "bucket": "agentws",
            "keyPrefix": "tenants/proj1",
            "endpoint": "http://minio.flint-system.svc:9000",
            "region": "us-east-1"
        }))
        .unwrap()
    }

    /// The prefix is an object-key prefix and mount-s3 REJECTS it
    /// without the trailing slash, so the CR's slashless form and the
    /// mounter's form are not the same string. An endpoint forces
    /// path-style addressing (a bucket name is not a DNS label there).
    #[test]
    fn the_args_address_the_subtree_and_force_path_style_behind_an_endpoint() {
        let a = mounter_args_for(&spec(), (None, None), "{FUSE_FD}");
        assert_eq!(a[0], "agentws");
        assert_eq!(a[1], "{FUSE_FD}");
        let i = a.iter().position(|x| x == "--prefix").unwrap();
        assert_eq!(a[i + 1], "tenants/proj1/");
        assert!(a.contains(&"--force-path-style".to_string()));
        assert!(a.contains(&"--endpoint-url".to_string()));
        assert!(a.contains(&"--foreground".to_string()));
    }

    /// fd mode MUST still pass `--allow-other`: the kernel option admits
    /// other uids to the mount, but Mountpoint's own session ACL refuses
    /// them — root's readiness `statfs` included — unless the daemon is
    /// told too (measured on kind: EACCES for every uid but the daemon's).
    #[test]
    fn allow_other_is_passed_for_the_daemon_side_acl() {
        let a = mounter_args_for(&spec(), (Some(1001), Some(1001)), "{FUSE_FD}");
        assert!(a.contains(&"--allow-other".to_string()));
        let i = a.iter().position(|x| x == "--uid").unwrap();
        assert_eq!(a[i + 1], "1001");
        let i = a.iter().position(|x| x == "--gid").unwrap();
        assert_eq!(a[i + 1], "1001");
    }

    /// A read-write mount that cannot replace a file looks like a
    /// permissions bug. Read-only must not carry the flags.
    #[test]
    fn write_flags_track_read_only() {
        let mut s = spec();
        let rw = mounter_args_for(&s, (None, None), "t");
        assert!(rw.contains(&"--allow-delete".to_string()));
        assert!(rw.contains(&"--allow-overwrite".to_string()));
        assert!(!rw.contains(&"--read-only".to_string()));
        s.read_only = true;
        let ro = mounter_args_for(&s, (None, None), "t");
        assert!(ro.contains(&"--read-only".to_string()));
        assert!(!ro.contains(&"--allow-delete".to_string()));
        assert!(!ro.contains(&"--allow-overwrite".to_string()));
    }

    /// Extra mount options from the CR survive verbatim as ARGUMENTS —
    /// a shell metacharacter in one cannot become a command.
    #[test]
    fn mount_options_survive_verbatim_as_arguments() {
        let mut s = spec();
        s.mount_options = vec!["--metadata-ttl".into(), "5; rm -rf /".into()];
        let a = mounter_args_for(&s, (None, None), "t");
        assert!(a.contains(&"5; rm -rf /".to_string()), "it must survive verbatim as an ARGUMENT");
    }
}
