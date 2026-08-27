//! The mutating webhook's BRAIN, as a pure function over Pod specs.
//!
//! The shape, and why it is this shape:
//!
//! - **A native sidecar** (initContainer with `restartPolicy: Always`,
//!   K8s ≥ 1.29), PREPENDED. Native sidecars start before regular init
//!   containers and their `startupProbe` gates everything after them,
//!   so prepending is what makes the bucket visible to an init
//!   container as well as to the app. A plain container gates nothing:
//!   kubelet starts siblings on `started`, and an app container that
//!   wins that race sees an EMPTY DIRECTORY — not an error, not a
//!   hang, just a bucket that appears to have no objects in it. That
//!   is the failure this ordering exists to make impossible.
//!
//! - **The sidecar is PRIVILEGED.** This is the price of the
//!   sidecar-only design and it is not reducible: `/dev/fuse` alone
//!   would take `CAP_SYS_ADMIN` plus a hostPath device, but the mount
//!   has to REACH THE APP CONTAINER, and that needs `mountPropagation:
//!   Bidirectional`, which the API server allows only on a privileged
//!   container. Everything else here is unprivileged; the app
//!   containers get `HostToContainer` and no capabilities at all. If
//!   the namespace enforces PodSecurity `baseline` or `restricted`,
//!   the mutated pod is REJECTED — correctly. The way out is a CSI
//!   driver holding the mount on the node (what gcsfuse and
//!   mountpoint's own CSI driver do), which is a different, much
//!   larger deployment; flint-lean's checkout sidecar is the other way
//!   out and needs no privilege at all, because it copies bytes
//!   instead of mounting.
//!
//! - **Argv, never a shell string.** Everything from the CR that the
//!   mounter sees is passed as `args` and reaches the binary through
//!   `"$@"`. The `command` script is a fixed, operator-authored
//!   constant. A CR author can already ask for a privileged container
//!   in their own namespace; they should not additionally be able to
//!   choose what runs in it.
//!
//! - **One mounter: `mount-s3`.** It takes the standard AWS chain
//!   natively, so a Secret of verbatim AWS_* keys and an ambient IRSA
//!   identity are both just `envFrom` — the launcher re-exports
//!   nothing and the credential story has no shell in it at all.

use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvFromSource, EnvVar, ExecAction, Lifecycle,
    LifecycleHandler, Pod, Probe, ResourceRequirements, SecretEnvSource, SecurityContext, Volume,
    VolumeMount,
};

use super::spec::MountSpec;

pub const SIDECAR_NAME: &str = "flint-passthrough";
pub const VOLUME_NAME: &str = "flint-passthrough";
pub const INJECT_LABEL: &str = "flint.io/passthrough-mount";
/// The mount path, handed to the fixed shell fragments as an env var so
/// they can quote it instead of interpolating it.
pub const MOUNT_ENV: &str = "FLINT_PT_MOUNT";

/// Where the sidecar mounts the shared volume, and the subdirectory of
/// it that actually carries the FUSE filesystem.
///
/// The FUSE mount is deliberately NOT at the volume root, for two
/// independent reasons — and the second is the load-bearing one:
///
/// 1. mount-s3 REFUSES a target that is already a mount point ("mount
///    point /mnt/mp is already mounted"), and kubelet's volume bind
///    makes every mountPath exactly that.
///
/// 2. A DEAD FUSE MOUNT AT THE VOLUME ROOT WEDGES THE POD FOREVER.
///    When the mounter dies its mount stays on the node, and every
///    subsequent container creation fails in the runtime, before any
///    of this code runs: `failed to apply OCI options: failed to stat
///    ".../volumes/kubernetes.io~empty-dir/flint-passthrough":
///    transport endpoint is not connected`. The replacement mounter
///    can never start to clean up the corpse that is blocking it, so
///    the sidecar sits in CrashLoopBackOff permanently AND deleting
///    the pod HANGS until someone unmounts it on the node by hand.
///    Moved one level down, the volume root stays an ordinary
///    directory the runtime can always stat: the replacement starts,
///    its guarded cleanup unmounts the corpse, and it remounts.
///
/// Measured on kind 2026-08-27, same kill on both shapes: root shape
/// → CreateContainerError forever, dead mount left on the node, pod
/// deletion hung; subdirectory shape → mounter restarted and running,
/// node mount live again, pod deleted in 1 second.
pub const VOLUME_ROOT: &str = "/flint-passthrough-vol";
pub const FUSE_SUBDIR: &str = "root";

pub const STATE_VOLUME: &str = "flint-passthrough-state";
pub const STATE_PATH: &str = "/flint-passthrough-state";

/// Mounting moves no data, so this is a liveness budget for "did the
/// mounter come up and authenticate", not a transfer budget — the one
/// place this design is legitimately simpler than flint-lean, whose
/// probe threshold has to be DERIVED from the declared inventory
/// because it is waiting for gigabytes to land.
const PROBE_PERIOD_SECS: i32 = 2;
const PROBE_FAILURE_THRESHOLD: i32 = 30; // 60s

/// True only when a FUSE filesystem is mounted at exactly
/// `$FLINT_PT_MOUNT`.
///
/// /proc/mounts rather than /proc/self/mountinfo, because its fields
/// are `source mountpoint fstype opts` — so `" <path> fuse"` is a
/// single fixed-string match for "a FUSE filesystem is mounted exactly
/// here". mountinfo puts the fstype after a ` - ` separator, where the
/// only cheap match is on the mount point alone, and a match on the
/// mount point alone is TRUE OF THE BARE emptyDir BIND. Both users of
/// this test — the startup gate and the stale-mount cleanup — are
/// wrong in a different direction if they accept that.
fn fuse_mounted_test() -> String {
    format!("grep -qF \" ${MOUNT_ENV} fuse\" /proc/mounts")
}

/// Drop a mount left behind by a previous instance of this container.
/// With `Bidirectional` propagation the mount outlives the container
/// that made it, so a crash-restarted sidecar would otherwise mount
/// over its own corpse and the app container would keep reading the
/// dead one (ENOTCONN on every call).
///
/// THE GUARD IS THE WHOLE POINT. `fusermount -u -z` run as root does
/// NOT check that its target is a FUSE mount: pointed at the ordinary
/// bind mount kubelet makes for the volume, it unmounts it and exits
/// 0. Unguarded, this line ran on every start and detached the
/// sidecar's mount point from the shared peer group it had just been
/// given — after which the mounter mounted onto the container's own
/// overlay root, propagated to nobody, and every app container read an
/// EMPTY DIRECTORY while the sidecar looked perfectly healthy.
///
/// Measured on kind 2026-08-27, when this image still carried s3fs and
/// the rig used it: sidecar `/mnt/s3` = fuse.s3fs on parent `/`, app
/// container `/mnt/s3` = ext4, zero empty-dir binds in the sidecar.
/// The mounter is incidental to the finding — `fusermount` is what
/// detaches the peer group, and it does that whatever mounted next.
fn stale_unmount() -> String {
    let m = format!("\"${MOUNT_ENV}\"");
    format!(
        "if {test}; then fusermount -u -z {m} 2>/dev/null || \
         fusermount3 -u -z {m} 2>/dev/null || umount -l {m} 2>/dev/null || true; fi",
        test = fuse_mounted_test()
    )
}

/// Operator-level defaults the chart provides.
#[derive(Debug, Clone, Default)]
pub struct InjectDefaults {
    pub image: String,
    pub resources: Option<ResourceRequirements>,
}

/// Where the MOUNTER puts the FUSE filesystem — never the path the
/// workload sees. See [`VOLUME_ROOT`].
pub fn fuse_target(_spec: &MountSpec) -> String {
    format!("{VOLUME_ROOT}/{FUSE_SUBDIR}")
}

/// Where the SIDECAR mounts the shared volume.
fn sidecar_volume_path(_spec: &MountSpec) -> String {
    VOLUME_ROOT.to_string()
}

/// The subdirectory of the volume a CONSUMER mounts.
///
/// A subPath bind is resolved once, when the consumer container
/// starts. That is enough — the native sidecar's startup gate
/// guarantees the FUSE mount is already there — and it costs nothing
/// that the alternative would have saved: a consumer mounting the
/// volume root would ALSO be stranded by a mounter crash, for the
/// reason at `restart_detector`, and would additionally see the
/// bucket one directory below the mountPath it asked for.
fn consumer_sub_path(_spec: &MountSpec) -> Option<String> {
    Some(FUSE_SUBDIR.to_string())
}

/// Detect that THIS mounter is a replacement for one that died, and
/// leave a flag the readiness probe reads.
///
/// A mounter crash is not recoverable for containers already running
/// in the pod, and that is a property of Linux rather than of this
/// code: the consumer's view of the FUSE filesystem is a private copy
/// the runtime made when the container started, so the unmount does
/// not reach it (it goes ENOTCONN and stays there) and the
/// replacement's fresh mount does not either. Measured on kind
/// 2026-08-27: kill the mounter container, the app container reads
/// `Transport endpoint is not connected`, the sidecar restarts, and
/// the app container reads it still.
///
/// The start gate is what makes this unavoidable. A consumer that
/// mounted BEFORE the FUSE mount existed would hold a propagation
/// slave and would recover — and would also be free to start against
/// an empty directory, which is the failure this whole design exists
/// to prevent. Clean path plus start gate costs restart recovery; the
/// pod has to be recreated.
///
/// So the least this can do is stop being SILENT. The replacement
/// mounts anyway (a container created later in the same pod still
/// wants it), and reports itself NOT READY, which takes the pod out of
/// its Service endpoints and puts the reason in `kubectl describe`.
fn restart_detector() -> String {
    format!(
        "if [ -f {STATE_PATH}/started ]; then touch {STATE_PATH}/stale-consumers; fi; \
         touch {STATE_PATH}/started"
    )
}

/// The fixed launcher. User data arrives as `"$@"`.
///
/// mount-s3 reads the standard AWS chain, so there is nothing to map
/// and nothing to export: the whole script is "make the subdirectory,
/// clear a corpse, record that we mounted, exec". The subdirectory
/// lives in the shared volume and survives a restart; `mkdir -p` is
/// for the first start.
fn sidecar_command() -> Vec<String> {
    let script = format!(
        "mkdir -p \"${MOUNT_ENV}\"; {}; {}; exec mount-s3 \"$@\"",
        stale_unmount(),
        restart_detector()
    );
    vec!["/bin/sh".into(), "-c".into(), script, SIDECAR_NAME.into()]
}

/// Who the mount reports as the owner of everything in it.
///
/// mount-s3 defaults this to the MOUNTING user, which is root, because
/// a privileged sidecar is the only thing that can make the mount. An
/// unprivileged workload then reads fine (`--allow-other` plus mode
/// 0755) and CANNOT CREATE A FILE — EACCES, which reads as a bucket
/// policy problem and is not one. The CR's `uid`/`gid` have always
/// been the fix; nothing told anyone to set them.
///
/// So the pod's own `securityContext.runAsUser` is the default, which
/// is the answer in every case where the question comes up at all: a
/// pod that declares who it runs as gets a mount that user owns. The
/// CR still wins when it says something, and a pod that declares
/// nothing runs as root and needs neither.
///
/// Measured on kind 2026-08-27: `runAsUser: 1001` with no uid → the
/// app container reads all 11 objects and `echo > file` is Permission
/// denied; with this default it writes through to the bucket.
fn mount_owner(pod: &Pod, spec: &MountSpec) -> (Option<i64>, Option<i64>) {
    let sc = pod.spec.as_ref().and_then(|s| s.security_context.as_ref());
    (
        spec.uid.or_else(|| sc.and_then(|c| c.run_as_user)),
        spec.gid.or_else(|| sc.and_then(|c| c.run_as_group)),
    )
}

/// The mounter's argument vector. Never concatenated into a shell.
///
/// `owner` is the resolved (uid, gid) from [`mount_owner`] — resolved
/// by the caller because it depends on the POD, which a spec does not
/// know about.
pub fn mounter_args(spec: &MountSpec, owner: (Option<i64>, Option<i64>)) -> Vec<String> {
    let mut a: Vec<String> = vec![
        spec.bucket.clone(),
        fuse_target(spec),
        "--foreground".into(),
        // Without allow-other only the mounting uid (root) can traverse
        // the mount, and the app container is the whole point.
        // /etc/fuse.conf in the image carries user_allow_other.
        "--allow-other".into(),
    ];
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

/// Inject the passthrough sidecar into a pod. Idempotent: a pod that
/// already carries the sidecar is returned unchanged.
pub fn inject_mount(
    pod: &Pod,
    cr_name: &str,
    spec: &MountSpec,
    defaults: &InjectDefaults,
) -> Result<Pod, String> {
    // Resolved BEFORE the clone is mutated, from the pod as submitted.
    let owner = mount_owner(pod, spec);
    let mut pod = pod.clone();
    let pspec = pod.spec.as_mut().ok_or("pod has no spec")?;

    let already = pspec
        .init_containers
        .as_ref()
        .map(|cs| cs.iter().any(|c| c.name == SIDECAR_NAME))
        .unwrap_or(false);
    if already {
        return Ok(pod);
    }
    spec.validate()?;

    if let Some(vs) = pspec.volumes.as_ref() {
        if vs.iter().any(|v| v.name == VOLUME_NAME) {
            return Err(format!(
                "the pod already declares a volume named {VOLUME_NAME:?}; rename it — the \
                 passthrough mount needs that name"
            ));
        }
    }

    // REFUSE rather than push blindly if a container already mounts the
    // path, and refuse rather than skip. Skipping is the worse of the
    // two: the container's own volume would shadow the mount and the
    // workload would run against an empty directory while every probe
    // still looked healthy.
    let collides = |c: &Container| -> Option<String> {
        c.volume_mounts.as_ref()?.iter().find(|m| m.mount_path == spec.mount_path).map(|m| {
            format!(
                "container {:?} already mounts {:?} (volume {:?}); the passthrough mount \
                 needs that path. Move one of them: set spec.mountPath on \
                 FlintPassthroughMount {cr_name:?} to a path the pod does not use, or drop \
                 the pod's own mount",
                c.name, spec.mount_path, m.name
            )
        })
    };
    for c in pspec.containers.iter().chain(pspec.init_containers.iter().flatten()) {
        if let Some(msg) = collides(c) {
            return Err(msg);
        }
    }

    let volumes = pspec.volumes.get_or_insert_with(Vec::new);
    volumes.push(Volume {
        name: VOLUME_NAME.into(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    });
    volumes.push(Volume {
        name: STATE_VOLUME.into(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    });

    // The consumer side: see the sidecar's mount, never make one.
    let consumer_mount = VolumeMount {
        name: VOLUME_NAME.into(),
        mount_path: spec.mount_path.clone(),
        mount_propagation: Some("HostToContainer".into()),
        sub_path: consumer_sub_path(spec),
        read_only: Some(spec.read_only),
        ..Default::default()
    };
    for c in pspec.containers.iter_mut().chain(pspec.init_containers.iter_mut().flatten()) {
        c.volume_mounts.get_or_insert_with(Vec::new).push(consumer_mount.clone());
    }

    // The env var the launcher, the probe and the preStop all quote is
    // the MOUNTER's target, which is not always what the workload sees.
    let mut env = vec![EnvVar {
        name: MOUNT_ENV.into(),
        value: Some(fuse_target(spec)),
        ..Default::default()
    }];
    if let Some(r) = &spec.region {
        // mount-s3 takes `--region`, but the AWS SDK inside it reads
        // this for everything the flag does not cover (STS, and the
        // web-identity exchange behind IRSA).
        env.push(EnvVar {
            name: "AWS_REGION".into(),
            value: Some(r.clone()),
            ..Default::default()
        });
    }
    let env_from = spec.credentials_secret_ref.as_ref().map(|name| {
        vec![EnvFromSource {
            // optional: false — a missing Secret must hold the pod in
            // CreateContainerConfigError with the Secret's name in the
            // event, not mount the bucket anonymously.
            secret_ref: Some(SecretEnvSource { name: name.clone(), optional: Some(false) }),
            ..Default::default()
        }]
    });

    let sidecar = Container {
        name: SIDECAR_NAME.into(),
        image: Some(spec.image.clone().unwrap_or_else(|| defaults.image.clone())),
        command: Some(sidecar_command()),
        args: Some(mounter_args(spec, owner)),
        env: Some(env),
        env_from,
        volume_mounts: Some(vec![
            VolumeMount {
                name: VOLUME_NAME.into(),
                mount_path: sidecar_volume_path(spec),
                // The whole mechanism. Without this the mount exists
                // only inside this container and every consumer sees an
                // empty directory.
                mount_propagation: Some("Bidirectional".into()),
                ..Default::default()
            },
            VolumeMount {
                name: STATE_VOLUME.into(),
                mount_path: STATE_PATH.into(),
                ..Default::default()
            },
        ]),
        restart_policy: Some("Always".into()),
        security_context: Some(SecurityContext {
            // Bidirectional propagation is API-validated to require it.
            privileged: Some(true),
            run_as_user: Some(0),
            // Explicit, because a pod-level `runAsNonRoot: true` would
            // otherwise refuse to start this container at all — with a
            // kubelet message about the IMAGE running as root, which
            // names neither FUSE nor the reason. Container-level wins.
            run_as_non_root: Some(false),
            ..Default::default()
        }),
        // A functional gate: the mount must be PRESENT in the sidecar's
        // mount table. `ls` would pass on the bare emptyDir and gate
        // nothing.
        startup_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    // Mount PRESENT and mount ANSWERING. The fstype
                    // half is what stops the bare emptyDir bind from
                    // satisfying the gate; the `test -d` half is what
                    // stops a mount whose daemon has died (ENOTCONN)
                    // from doing so.
                    format!("{} && test -d \"${MOUNT_ENV}\"", fuse_mounted_test()),
                ]),
            }),
            period_seconds: Some(PROBE_PERIOD_SECS),
            failure_threshold: Some(PROBE_FAILURE_THRESHOLD),
            ..Default::default()
        }),
        // Not "is the mount healthy" — the mount in THIS container is
        // healthy after a restart, and the workload's is not. This
        // reports the only thing the sidecar can actually know: that
        // consumers in this pod are stranded on a mount that died.
        readiness_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("test ! -f {STATE_PATH}/stale-consumers"),
                ]),
            }),
            period_seconds: Some(10),
            ..Default::default()
        }),
        lifecycle: Some(Lifecycle {
            // preStop runs BEFORE the SIGTERM, which is the only window
            // in which the mount can be removed cleanly. A sidecar that
            // just dies leaves the mount in the node's namespace, where
            // it outlives the pod and blocks the kubelet's emptyDir
            // teardown.
            pre_stop: Some(LifecycleHandler {
                exec: Some(ExecAction {
                    command: Some(vec!["/bin/sh".into(), "-c".into(), stale_unmount()]),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        resources: defaults.resources.clone(),
        // A mounter that dies on bad credentials writes the reason to
        // stderr and nowhere else; this puts those lines in the pod's
        // status where `kubectl describe` shows them.
        termination_message_policy: Some("FallbackToLogsOnError".into()),
        ..Default::default()
    };
    pspec.init_containers.get_or_insert_with(Vec::new).insert(0, sidecar);
    Ok(pod)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PodSecurityContext, PodSpec};

    fn spec_json(v: serde_json::Value) -> MountSpec {
        serde_json::from_value(v).unwrap()
    }
    fn spec() -> MountSpec {
        spec_json(serde_json::json!({
            "bucket": "agentws",
            "keyPrefix": "tenants/proj1",
            "endpoint": "http://minio.flint-system.svc:9000",
            "credentialsSecretRef": "proj1-creds",
        }))
    }
    fn defaults() -> InjectDefaults {
        InjectDefaults { image: "flint-passthrough:test".into(), resources: None }
    }
    fn pod() -> Pod {
        Pod {
            spec: Some(PodSpec {
                containers: vec![Container { name: "agent".into(), ..Default::default() }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    fn inject(p: &Pod, s: &MountSpec) -> Pod {
        inject_mount(p, "proj1", s, &defaults()).unwrap()
    }

    #[test]
    fn the_sidecar_is_a_native_sidecar_and_comes_first() {
        let out = inject(&pod(), &spec());
        let init = out.spec.unwrap().init_containers.unwrap();
        assert_eq!(init[0].name, SIDECAR_NAME);
        assert_eq!(init[0].restart_policy.as_deref(), Some("Always"));
        assert!(init[0].startup_probe.is_some());
    }

    /// Prepending is the point: a regular init container has to see the
    /// bucket too, and it only does if the native sidecar precedes it.
    #[test]
    fn a_regular_init_container_is_ordered_after_the_mount_and_gets_it() {
        let mut p = pod();
        p.spec.as_mut().unwrap().init_containers =
            Some(vec![Container { name: "fetch".into(), ..Default::default() }]);
        let out = inject(&p, &spec());
        let init = out.spec.unwrap().init_containers.unwrap();
        assert_eq!(init[0].name, SIDECAR_NAME);
        assert_eq!(init[1].name, "fetch");
        let m = &init[1].volume_mounts.as_ref().unwrap()[0];
        assert_eq!(m.mount_path, "/mnt/s3");
        assert_eq!(m.mount_propagation.as_deref(), Some("HostToContainer"));
    }

    /// The mechanism, asserted on both sides. Either half alone is a
    /// pod where the app container reads an empty directory forever.
    #[test]
    fn propagation_is_bidirectional_on_the_mounter_and_host_to_container_on_consumers() {
        let out = inject(&pod(), &spec());
        let s = out.spec.unwrap();
        let side = &s.init_containers.unwrap()[0];
        assert_eq!(
            side.volume_mounts.as_ref().unwrap()[0].mount_propagation.as_deref(),
            Some("Bidirectional")
        );
        assert_eq!(side.security_context.as_ref().unwrap().privileged, Some(true));
        let app = &s.containers[0];
        assert_eq!(
            app.volume_mounts.as_ref().unwrap()[0].mount_propagation.as_deref(),
            Some("HostToContainer")
        );
        assert!(app.security_context.is_none(), "app containers gain no privilege");
    }

    /// Container-level runAsNonRoot=false is what lets a pod that sets
    /// runAsNonRoot=true at POD level still mount.
    #[test]
    fn the_sidecar_overrides_a_pod_level_run_as_non_root() {
        let sc = inject(&pod(), &spec()).spec.unwrap().init_containers.unwrap()[0]
            .security_context
            .clone()
            .unwrap();
        assert_eq!(sc.run_as_non_root, Some(false));
        assert_eq!(sc.run_as_user, Some(0));
    }

    /// The probe must prove a FUSE mount, not a directory and not just
    /// "some mount here". kubelet has already bind-mounted the emptyDir
    /// at this exact path before the mounter runs, so a gate that
    /// matches the mount POINT alone is satisfied before anything has
    /// been mounted — and the app container starts against an empty
    /// directory it cannot tell from an empty bucket.
    #[test]
    fn the_startup_probe_requires_a_fuse_mount_not_merely_a_mount() {
        let out = inject(&pod(), &spec());
        let probe = out.spec.unwrap().init_containers.unwrap()[0].startup_probe.clone().unwrap();
        let cmd = probe.exec.unwrap().command.unwrap().join(" ");
        assert!(cmd.contains("/proc/mounts"), "probe must read the mount table: {cmd}");
        assert!(cmd.contains(MOUNT_ENV), "probe must match THIS mount path: {cmd}");
        assert!(cmd.contains("fuse"), "probe must require the FUSE fstype: {cmd}");
        assert!(cmd.contains("test -d"), "probe must also prove the mount answers: {cmd}");
    }

    /// `fusermount -u -z` run as root does not check that its target is
    /// a FUSE mount: pointed at kubelet's ordinary bind mount for the
    /// volume it unmounts it and exits 0. Unguarded, the cleanup
    /// detached the mount point from its shared peer group and the
    /// mount reached nobody. Every place that unmounts must ask first.
    #[test]
    fn every_unmount_is_guarded_on_the_path_being_a_fuse_mount() {
        let out = inject(&pod(), &spec());
        let c = out.spec.unwrap().init_containers.unwrap()[0].clone();
        let launcher = c.command.unwrap()[2].clone();
        let pre_stop = c
            .lifecycle
            .unwrap()
            .pre_stop
            .unwrap()
            .exec
            .unwrap()
            .command
            .unwrap()
            .join(" ");
        for (what, script) in [("launcher", &launcher), ("preStop", &pre_stop)] {
            let u = script.find("fusermount").expect("{what} must unmount stale mounts");
            let g = script
                .find("/proc/mounts")
                .unwrap_or_else(|| panic!("{what} unmounts with no guard at all: {script}"));
            assert!(g < u, "{what} unmounts BEFORE it checks the fstype: {script}");
            assert!(script.contains("fuse\" /proc/mounts"), "{what} guard must match the FUSE fstype: {script}");
        }
    }

    /// Everything from the CR reaches the binary through "$@". The
    /// command script itself carries no CR-derived text at all.
    #[test]
    fn cr_data_never_lands_in_the_command_string() {
        let mut s = spec();
        s.mount_options = vec!["--metadata-ttl".into(), "60; touch /pwned".into()];
        let out = inject(&pod(), &s);
        let c = out.spec.unwrap().init_containers.unwrap()[0].clone();
        let script = &c.command.unwrap()[2];
        assert!(!script.contains("pwned"), "CR text must not reach the shell script");
        assert!(!script.contains("agentws"), "not even the bucket: {script}");
        assert!(
            c.args.unwrap().contains(&"60; touch /pwned".to_string()),
            "it must survive verbatim as an ARGUMENT"
        );
    }

    /// The prefix is an object-key prefix and mount-s3 REJECTS it
    /// without the trailing slash, so the CR's slashless form and the
    /// mounter's form are not the same string.
    #[test]
    fn the_args_address_the_subtree_and_force_path_style_behind_an_endpoint() {
        let a = mounter_args(&spec(), (None, None));
        assert_eq!(a[0], "agentws");
        assert_eq!(a[1], format!("{VOLUME_ROOT}/{FUSE_SUBDIR}"), "the mounter targets the subdirectory");
        let i = a.iter().position(|x| x == "--prefix").unwrap();
        assert_eq!(a[i + 1], "tenants/proj1/");
        assert!(a.contains(&"--force-path-style".to_string()));
        assert!(a.contains(&"--endpoint-url".to_string()));
        assert!(a.contains(&"--allow-other".to_string()));
    }

    /// A read-write mount that cannot replace a file looks like a
    /// permissions bug. Read-only must not carry the flags.
    #[test]
    fn write_flags_track_read_only() {
        let mut s = spec();
        let rw = mounter_args(&s, (None, None));
        assert!(rw.contains(&"--allow-delete".to_string()));
        assert!(rw.contains(&"--allow-overwrite".to_string()));
        s.read_only = true;
        let ro = mounter_args(&s, (None, None));
        assert!(ro.contains(&"--read-only".to_string()));
        assert!(!ro.contains(&"--allow-delete".to_string()));
    }

    /// mount-s3 resolves credentials itself. The launcher must not
    /// re-export anything into the environment — an s3fs-shaped
    /// mapping is exactly the sort of thing that turns an empty
    /// AWS_ACCESS_KEY_ID into a failed authentication instead of a
    /// fall-through to the ambient chain.
    #[test]
    fn the_launcher_touches_no_credential_environment() {
        let out = inject(&pod(), &spec());
        let script = out.spec.unwrap().init_containers.unwrap()[0].command.clone().unwrap()[2]
            .clone();
        for name in ["AWS_ACCESS_KEY_ID", "AWSACCESSKEYID", "AWS_SESSION_TOKEN", "export"] {
            assert!(!script.contains(name), "the launcher must not handle credentials ({name}): {script}");
        }
    }

    /// The mount must be OWNED by the user the pod runs as, or the
    /// workload reads everything and can create nothing — EACCES on
    /// the first write, which looks like a bucket policy problem. The
    /// CR still wins where it speaks.
    #[test]
    fn the_mount_is_owned_by_the_user_the_pod_runs_as() {
        let mut p = pod();
        p.spec.as_mut().unwrap().security_context = Some(PodSecurityContext {
            run_as_user: Some(1001),
            run_as_group: Some(2002),
            ..Default::default()
        });
        let args = inject(&p, &spec()).spec.unwrap().init_containers.unwrap()[0]
            .args
            .clone()
            .unwrap();
        let at = |flag: &str| {
            args.iter().position(|a| a == flag).map(|i| args[i + 1].clone())
        };
        assert_eq!(at("--uid").as_deref(), Some("1001"), "{args:?}");
        assert_eq!(at("--gid").as_deref(), Some("2002"), "{args:?}");

        // The CR wins: an explicit uid is the whole reason the field
        // exists, and a pod-derived default that overrode it would be
        // a knob that silently does nothing.
        let mut s = spec();
        s.uid = Some(4004);
        let args = inject(&p, &s).spec.unwrap().init_containers.unwrap()[0].args.clone().unwrap();
        let i = args.iter().position(|a| a == "--uid").unwrap();
        assert_eq!(args[i + 1], "4004");

        // A pod that declares nothing runs as root and needs neither —
        // and must not be handed a --uid 0 that means the same thing
        // as saying nothing.
        let args = inject(&pod(), &spec()).spec.unwrap().init_containers.unwrap()[0]
            .args
            .clone()
            .unwrap();
        assert!(!args.contains(&"--uid".to_string()), "{args:?}");
        assert!(!args.contains(&"--gid".to_string()), "{args:?}");
    }

    /// Path style is inferred from `endpoint`, and the inference must
    /// be overridable in BOTH directions.
    #[test]
    fn path_style_defaults_to_whether_an_endpoint_is_set() {
        let mut s = spec();
        assert!(s.use_path_style());
        s.endpoint = None;
        s.credentials_secret_ref = Some("c".into());
        assert!(!s.use_path_style());
        s.path_style = Some(true);
        assert!(s.use_path_style());
        s.endpoint = Some("http://x:9000".into());
        s.path_style = Some(false);
        assert!(!s.use_path_style());
    }

    /// The mount goes one level below the volume root, and the reason
    /// is not mount-s3's refusal to mount over kubelet's bind — that
    /// one would merely fail loudly. It is that a dead FUSE mount AT
    /// the volume root makes every later container creation fail in
    /// the runtime, before any of this code runs, so the replacement
    /// mounter can never start to clean up the corpse blocking it.
    /// Measured: root shape wedges the pod permanently and hangs its
    /// deletion; subdirectory shape restarts clean.
    #[test]
    fn the_mount_is_below_the_volume_root_and_consumers_use_a_subpath() {
        let sp = spec();
        let out = inject(&pod(), &sp);
        let pspec = out.spec.unwrap();
        let side = pspec.init_containers.unwrap()[0].clone();
        let target = format!("{VOLUME_ROOT}/{FUSE_SUBDIR}");

        let vm = side.volume_mounts.as_ref().unwrap();
        let workspace = vm.iter().find(|m| m.name == VOLUME_NAME).unwrap();
        assert_eq!(
            workspace.mount_path, VOLUME_ROOT,
            "the volume root must stay an ordinary directory the runtime can stat"
        );
        assert!(mounter_args(&sp, (None, None)).contains(&target), "the mounter must target the subdirectory");
        assert_eq!(side.env.as_ref().unwrap()[0].value.as_deref(), Some(target.as_str()));
        assert!(
            side.command.as_ref().unwrap()[2].contains("mkdir -p"),
            "the subdirectory has to exist before the mount"
        );

        let m = pspec.containers[0].volume_mounts.clone().unwrap()[0].clone();
        assert_eq!(m.mount_path, "/mnt/s3", "the workload still sees its own mountPath");
        assert_eq!(m.sub_path.as_deref(), Some(FUSE_SUBDIR));
        assert_eq!(m.mount_propagation.as_deref(), Some("HostToContainer"));
    }

    #[test]
    fn a_path_collision_is_refused_by_name() {
        let mut p = pod();
        p.spec.as_mut().unwrap().containers[0].volume_mounts = Some(vec![VolumeMount {
            name: "scratch".into(),
            mount_path: "/mnt/s3".into(),
            ..Default::default()
        }]);
        let e = inject_mount(&p, "proj1", &spec(), &defaults()).unwrap_err();
        assert!(e.contains("already mounts"), "{e}");
        assert!(e.contains("scratch"), "must name the offending volume: {e}");
        assert!(e.contains("mountPath"), "must name the knob that fixes it: {e}");
    }

    #[test]
    fn injection_is_idempotent() {
        let once = inject(&pod(), &spec());
        let twice = inject(&once, &spec());
        assert_eq!(once, twice);
        let vols = twice.spec.unwrap().volumes.unwrap();
        let names: Vec<&str> = vols.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec![VOLUME_NAME, STATE_VOLUME], "no volume is added twice");
    }

    /// A mounter crash strands every already-running consumer, and
    /// nothing the sidecar does can repair it. The one thing it must
    /// not do is stay Ready while that is true.
    #[test]
    fn a_replacement_mounter_reports_its_consumers_stranded() {
        let out = inject(&pod(), &spec());
        let c = out.spec.unwrap().init_containers.unwrap()[0].clone();
        let script = c.command.unwrap()[2].clone();
        assert!(
            script.contains(&format!("{STATE_PATH}/started")),
            "the launcher must record that a mount was made: {script}"
        );
        assert!(
            script.contains(&format!("touch {STATE_PATH}/stale-consumers")),
            "and must flag the SECOND mount as a replacement: {script}"
        );
        let probe = c.readiness_probe.expect("a replacement mounter must not stay Ready");
        let cmd = probe.exec.unwrap().command.unwrap().join(" ");
        assert!(cmd.contains("stale-consumers"), "readiness must read the flag: {cmd}");
        // The state must live somewhere the FUSE mount cannot hide it,
        // which the workspace volume is not.
        let state = c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == STATE_VOLUME)
            .expect("the sidecar needs its own state volume");
        assert_eq!(state.mount_path, STATE_PATH);
        assert!(state.mount_propagation.is_none(), "state is private to the sidecar");
    }

    /// The state volume is the sidecar's alone. A workload that could
    /// see it could clear the flag and un-strand itself on paper.
    #[test]
    fn consumers_never_mount_the_state_volume() {
        let out = inject(&pod(), &spec());
        let app = out.spec.unwrap().containers[0].clone();
        assert!(app
            .volume_mounts
            .unwrap()
            .iter()
            .all(|m| m.name != STATE_VOLUME));
    }

    #[test]
    fn a_read_only_mount_is_read_only_for_consumers_too() {
        let mut s = spec();
        s.read_only = true;
        let out = inject(&pod(), &s);
        let app = out.spec.unwrap().containers[0].clone();
        assert_eq!(app.volume_mounts.unwrap()[0].read_only, Some(true));
    }

    #[test]
    fn the_credentials_secret_is_mandatory_when_named() {
        let out = inject(&pod(), &spec());
        let ef = out.spec.unwrap().init_containers.unwrap()[0].env_from.clone().unwrap();
        assert_eq!(ef[0].secret_ref.as_ref().unwrap().optional, Some(false));
    }

    #[test]
    fn bad_specs_are_refused_with_the_field_named() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (serde_json::json!({"bucket": "b", "mountPath": "/"}), "mountPath"),
            (serde_json::json!({"bucket": "b", "mountPath": "/mnt/s3/"}), "mountPath"),
            (serde_json::json!({"bucket": "b", "mountPath": "/mnt/$(x)"}), "mountPath"),
            (serde_json::json!({"bucket": "b", "keyPrefix": "/abs"}), "keyPrefix"),
            (serde_json::json!({"bucket": "b", "keyPrefix": "a/../b"}), "keyPrefix"),
            (serde_json::json!({"bucket": "b b"}), "bucket"),
            // An s3fs option that outlived s3fs. Unrefused, this is a
            // privileged sidecar in CrashLoopBackOff and a reason that
            // never leaves the container log.
            (
                serde_json::json!({"bucket": "b", "mountOptions": ["-o", "auto_unmount"]}),
                "mountOptions",
            ),
            // The driver field is gone, not optional. A CR carrying it
            // is a CR written against a chart that offered a write
            // model this one does not have, and serde's
            // deny_unknown_fields is what says so by name.
            (serde_json::json!({"bucket": "b", "driver": "s3fs"}), "driver"),
        ];
        for (v, want) in cases {
            // A spec can be refused at either gate — serde (a field
            // that does not exist) or validate (a field with a value
            // that cannot work). Both must name the field; which one
            // fires is not the point.
            let e = match serde_json::from_value::<MountSpec>(v.clone()) {
                Err(e) => e.to_string(),
                Ok(s) => inject_mount(&pod(), "proj1", &s, &defaults())
                    .unwrap_err_or_else(|| panic!("{v} was accepted")),
            };
            assert!(e.contains(want), "error for {v} must name {want:?}: {e}");
        }
    }

    /// No Secret is a legitimate spec: mount-s3 resolves IRSA, an
    /// instance profile or anything else the AWS chain offers, so a
    /// mount with no `credentialsSecretRef` must be admitted with no
    /// `envFrom` at all rather than refused or given an empty one.
    #[test]
    fn a_mount_with_no_secret_uses_the_ambient_chain() {
        let mut s = spec();
        s.credentials_secret_ref = None;
        let out = inject_mount(&pod(), "proj1", &s, &defaults()).unwrap();
        let side = out.spec.unwrap().init_containers.unwrap()[0].clone();
        assert!(side.env_from.is_none(), "an ambient-chain mount names no Secret");
    }

    trait UnwrapErrOr<E> {
        fn unwrap_err_or_else(self, f: impl FnOnce() -> E) -> E;
    }
    impl<T, E> UnwrapErrOr<E> for Result<T, E> {
        fn unwrap_err_or_else(self, f: impl FnOnce() -> E) -> E {
            match self {
                Err(e) => e,
                Ok(_) => f(),
            }
        }
    }
}
