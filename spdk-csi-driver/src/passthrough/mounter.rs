//! The mount-s3 argument vector for a FlintPassthroughMount — the one
//! piece of the retired sidecar injector the CSI delivery still needs.
//! The node plugin (`s3csi::node`) performs the `mount(2)` itself and
//! hands the FUSE fd to an unprivileged worker, which execs Mountpoint
//! on `/dev/fd/3` with exactly this argv (design §3.4 step 9). Never
//! concatenated into a shell string: the worker passes it as ARGUMENTS.

use super::spec::{quantity_mib, MountSpec};

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
/// Where Mountpoint's block cache goes: `/tmp` in the worker — the
/// `scratch` emptyDir (`worker.rs`, sized by `workers.scratchSize`), or,
/// with `workers.cacheHostPath` set, the worker's PRIVATE directory on
/// the device the operator named (`<root>/<worker>`, 0700 for its uid,
/// made and removed by the plugin), hostPath'd at the same `/tmp` so the
/// argv — and the class key — do not depend on where the cache lives.
/// Never a SHARED host directory: AWS moved their own cache off the host
/// for isolation, and one directory for every tenant would be a
/// cross-tenant read channel; a per-worker 0700 directory is as private
/// as the emptyDir it replaces (root on the node reads both).
///
/// The scratch ROOT, not a subdirectory of it: mount-s3 creates its own
/// `mountpoint-cache-<id>` under the directory it is given and does not
/// create that directory's parents, so `/tmp/mountpoint-cache` (the
/// path until 2026-09-29) made every cached mount die at start with
/// "creation of cache sub-directory failed: No such file or directory"
/// — found on the kind rig the first time a cached mount was actually
/// published.
pub const CACHE_DIR: &str = "/tmp";

/// mount-s3 refuses a `--memory-target` below this (`cli.rs`,
/// `value_parser!(u64).range(512..)`), and its own default is the same
/// floor: `max(95% of the cgroup limit, 512 MiB)`.
pub const MEMORY_TARGET_FLOOR_MIB: u64 = 512;

/// The mounter's `--memory-target`, from the worker's memory LIMIT.
///
/// Mountpoint 1.24 sizes its read and write buffers against a target
/// that defaults to 95% of the cgroup limit — and says of it "not a
/// guaranteed limit". At the chart's 1Gi that leaves ~50 MiB for the
/// process's own heap, the cache index and the TLS buffers, and under
/// sharing every member's prefetch rides on that one budget; an
/// OOM-killed mounter is a dead mount for every pod behind it. Two
/// thirds of the limit leaves the other third for everything the target
/// does not count. Never below mount-s3's floor (it would refuse the
/// argument); `None` when there is no limit to derive from, so mount-s3
/// keeps its own default.
pub fn memory_target_mib(worker_memory_limit: Option<&str>) -> Option<u64> {
    let limit = quantity_mib(worker_memory_limit?)?;
    if limit == 0 {
        return None;
    }
    Some((limit * 2 / 3).max(MEMORY_TARGET_FLOOR_MIB))
}

/// Does the CR's `mountOptions` already carry `flag` (`--x` or `--x=v`)?
/// mount-s3's parser refuses an argument given twice, so a flag the CR
/// names is the CR's to set.
fn names_flag(options: &[String], flag: &str) -> bool {
    options.iter().any(|o| o == flag || o.starts_with(&format!("{flag}=")))
}

/// `worker_memory_limit` is the worker pod's memory limit as a Kubernetes
/// quantity (`workers.resources.limits.memory`), or `None` when the chart
/// set no limit.
pub fn mounter_args_for(
    spec: &MountSpec,
    owner: (Option<i64>, Option<i64>),
    target: &str,
    worker_memory_limit: Option<&str>,
) -> Vec<String> {
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
    // Mountpoint's block cache, in the worker's own scratch emptyDir.
    // Opt-in, a sharing CR included: the cardinality of a block-per-object
    // cache is a real cost for a mount with many small objects, and the
    // emptyDir is the node's root disk, where a working set larger than
    // the cache pays every fetched block to a 125 MiB/s volume and warms
    // nothing (5× slower than S3 on EC2, 2026-09-30, sharing design §11).
    // The operator asks for it per mount, for a set that fits.
    if let Some(c) = spec.cache.as_ref().filter(|c| c.enabled) {
        a.push("--cache".into());
        a.push(CACHE_DIR.into());
        if let Some(mib) = c.max_size_mib {
            a.push("--max-cache-size".into());
            a.push(mib.to_string());
        }
    }
    // The memory target follows the worker's cgroup limit (see
    // `memory_target_mib`); a CR that names its own keeps it.
    if !names_flag(&spec.mount_options, "--memory-target") {
        if let Some(mib) = memory_target_mib(worker_memory_limit) {
            a.push("--memory-target".into());
            a.push(mib.to_string());
        }
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
        let a = mounter_args_for(&spec(), (None, None), "{FUSE_FD}", None);
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
        let a = mounter_args_for(&spec(), (Some(1001), Some(1001)), "{FUSE_FD}", None);
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
        let rw = mounter_args_for(&s, (None, None), "t", None);
        assert!(rw.contains(&"--allow-delete".to_string()));
        assert!(rw.contains(&"--allow-overwrite".to_string()));
        assert!(!rw.contains(&"--read-only".to_string()));
        s.read_only = true;
        let ro = mounter_args_for(&s, (None, None), "t", None);
        assert!(ro.contains(&"--read-only".to_string()));
        assert!(!ro.contains(&"--allow-delete".to_string()));
        assert!(!ro.contains(&"--allow-overwrite".to_string()));
    }

    /// The `scratch` emptyDir has always been provisioned at `/tmp` and
    /// documented in values.yaml as "mount-s3's cache" — and `--cache`
    /// was never passed, so every repeated read re-fetched from S3 and
    /// the gigabyte was dead weight (found reviewing passthrough,
    /// 2026-09-22). Opt-in per mount, and OFF unless asked.
    #[test]
    fn the_cache_is_wired_to_the_scratch_dir_when_asked_and_absent_otherwise() {
        let mut s = spec();
        let off = mounter_args_for(&s, (None, None), "t", None);
        assert!(!off.iter().any(|a| a == "--cache"), "the cache must stay OPT-IN: {off:?}");
        assert!(!off.iter().any(|a| a == "--max-cache-size"));

        s.cache = Some(crate::passthrough::spec::CacheSpec { enabled: true, max_size_mib: Some(512) });
        let on = mounter_args_for(&s, (None, None), "t", None);
        let i = on.iter().position(|x| x == "--cache").expect("--cache is not passed");
        assert_eq!(on[i + 1], CACHE_DIR, "the cache must live in the worker's writable scratch");
        let j = on.iter().position(|x| x == "--max-cache-size").expect("--max-cache-size is not passed");
        assert_eq!(on[j + 1], "512", "an unbounded cache fills the emptyDir and gets the worker EVICTED");

        // Asked for but disabled is the same as not asked for.
        s.cache = Some(crate::passthrough::spec::CacheSpec { enabled: false, max_size_mib: Some(512) });
        let disabled = mounter_args_for(&s, (None, None), "t", None);
        assert!(!disabled.iter().any(|a| a == "--cache"), "{disabled:?}");
    }

    /// A CR that shares its mounter inherits no cache either. For one day
    /// (2026-09-29) the plugin defaulted a sharing CR to three quarters of
    /// the scratch; measured on EC2 the next day (sharing design §11), that
    /// cache on the emptyDir — the node's gp3 root, 125 MiB/s — made a
    /// 6 GiB read five times slower than no cache, cold and warm alike,
    /// because Mountpoint writes every fetched block to the disk under the
    /// cache directory. Off until the cache can be placed on a device
    /// faster than S3; a sharing CR that wants one names it, and gets
    /// exactly what it named — the ceiling is the CR's, not a fraction of
    /// the scratch.
    #[test]
    fn a_sharing_cr_that_names_no_cache_mounts_without_one() {
        let mut s = spec();
        s.read_only = true;
        s.sharing = Some(crate::passthrough::spec::SharingSpec { read_only: true });
        assert!(s.shares_read_only());
        let bare = mounter_args_for(&s, (None, None), "t", Some("1Gi"));
        assert!(!bare.iter().any(|a| a == "--cache"), "a sharing CR must not inherit a cache: {bare:?}");
        assert!(!bare.iter().any(|a| a == "--max-cache-size"), "{bare:?}");

        s.cache = Some(crate::passthrough::spec::CacheSpec { enabled: true, max_size_mib: Some(4096) });
        let named = mounter_args_for(&s, (None, None), "t", Some("1Gi"));
        let i = named.iter().position(|x| x == "--cache").expect("a named cache is passed");
        assert_eq!(named[i + 1], CACHE_DIR);
        let j = named.iter().position(|x| x == "--max-cache-size").expect("the named ceiling is passed");
        assert_eq!(named[j + 1], "4096", "the ceiling is the CR's, not three quarters of the scratch");
    }

    /// Extra mount options from the CR survive verbatim as ARGUMENTS —
    /// a shell metacharacter in one cannot become a command.
    #[test]
    fn mount_options_survive_verbatim_as_arguments() {
        let mut s = spec();
        s.mount_options = vec!["--metadata-ttl".into(), "5; rm -rf /".into()];
        let a = mounter_args_for(&s, (None, None), "t", None);
        assert!(a.contains(&"5; rm -rf /".to_string()), "it must survive verbatim as an ARGUMENT");
    }

    fn memory_target(a: &[String]) -> Option<String> {
        a.iter().position(|x| x == "--memory-target").map(|i| a[i + 1].clone())
    }

    /// Mountpoint's own default target is 95% of the cgroup limit and
    /// "not a guaranteed limit": at 1Gi that leaves ~50 MiB for the rest
    /// of the process, and under sharing every member's prefetch rides on
    /// it. The argv pins the target to two thirds of the worker's limit,
    /// never below mount-s3's 512 MiB floor (it refuses less), passes
    /// nothing without a limit, and defers to a CR that names its own.
    #[test]
    fn the_memory_target_is_two_thirds_of_the_worker_limit() {
        let s = spec();
        assert_eq!(memory_target(&mounter_args_for(&s, (None, None), "t", Some("1Gi"))).as_deref(), Some("682"));
        assert_eq!(memory_target(&mounter_args_for(&s, (None, None), "t", Some("3Gi"))).as_deref(), Some("2048"));
        assert_eq!(memory_target(&mounter_args_for(&s, (None, None), "t", Some("768Mi"))).as_deref(), Some("512"), "exactly at the floor");
        assert_eq!(memory_target(&mounter_args_for(&s, (None, None), "t", Some("600Mi"))).as_deref(), Some("512"), "clamped UP to the floor: mount-s3 refuses less");
        assert_eq!(memory_target(&mounter_args_for(&s, (None, None), "t", None)), None, "no limit: mount-s3 keeps its default");
        assert_eq!(memory_target(&mounter_args_for(&s, (None, None), "t", Some(""))), None, "an unreadable quantity derives nothing");
        assert_eq!(memory_target(&mounter_args_for(&s, (None, None), "t", Some("0"))), None);
        // A CR that names its own target keeps it — mount-s3 refuses a flag
        // given twice, so ours must not be there at all.
        let mut named = spec();
        named.mount_options = vec!["--memory-target".into(), "900".into()];
        let a = mounter_args_for(&named, (None, None), "t", Some("1Gi"));
        assert_eq!(a.iter().filter(|x| *x == "--memory-target").count(), 1);
        assert_eq!(memory_target(&a).as_deref(), Some("900"));
        named.mount_options = vec!["--memory-target=900".into()];
        let a = mounter_args_for(&named, (None, None), "t", Some("1Gi"));
        assert!(!a.iter().any(|x| x == "--memory-target"), "{a:?}");
        assert_eq!(memory_target_mib(Some("1Gi")), Some(682));
        assert_eq!(MEMORY_TARGET_FLOOR_MIB, 512);
    }
}
