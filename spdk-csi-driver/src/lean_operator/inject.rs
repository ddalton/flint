//! The mutating webhook's BRAIN, as a pure function over Pod specs.
//!
//! Native sidecar containers are MANDATORY (plan §2.4): an
//! initContainer with `restartPolicy: Always` (K8s ≥ 1.29) is the only
//! shape that gives BOTH the start gate (the agent cannot start before
//! checkout-complete exists) and the stop ordering (the drain scans a
//! quiescent tree after the agent exits). A plain container with a
//! startupProbe gates nothing the design needs — kubelet starts
//! siblings on `started`, and pod deletion SIGTERMs regular containers
//! in parallel.
//!
//! The startupProbe budget is DERIVED from the declared inventory ×
//! measured rates (docs/plans/flint-lean-0b-measurements.md), plus the
//! unclean-death claim lockout, plus headroom — never a fleet
//! constant. The admission HTTP/TLS wrapper around [`inject_sidecar`]
//! is deliberately separate follow-up plumbing; everything it will do
//! to a pod is here and unit-tested.

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EmptyDirVolumeSource, EnvFromSource, EnvVar, EnvVarSource,
    ExecAction, ObjectFieldSelector, Pod, Probe, SecretEnvSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use kube::ResourceExt;

use super::crd::FlintLeanWorkspace;

pub const SIDECAR_NAME: &str = "flint-sync";
pub const VOLUME_NAME: &str = "flint-workspace";

/// Operator-level defaults the chart provides.
#[derive(Debug, Clone)]
pub struct InjectDefaults {
    pub image: String,
}

/// The derived checkout budget (plan §2.4). Rates are conservative
/// proxy-shaped planning numbers anchored on the 0b loopback floors
/// (3.3 s/GiB checkout, ~2,000 files/s sequential, 16× fan-out): a
/// re-measure through the REAL proxy replaces them, the shape stays.
pub fn checkout_budget_secs(expected_bytes: u64, expected_files: u64) -> u64 {
    /// Unclean-death claim lockout the replacement must wait out
    /// (QUIET_POLLS × heartbeat, ~60–110 s observed band).
    const LOCKOUT_SECS: u64 = 110;
    const SECS_PER_GIB: u64 = 15;
    const FILES_PER_SEC: u64 = 500;
    const FLOOR_SECS: u64 = 120;

    let gib = expected_bytes.div_ceil(1 << 30);
    let byte_secs = gib * SECS_PER_GIB;
    let file_secs = expected_files.div_ceil(FILES_PER_SEC);
    let raw = LOCKOUT_SECS + byte_secs + file_secs;
    // ×1.5 headroom, floored.
    (raw + raw / 2).max(FLOOR_SECS)
}

/// Inject the lean sidecar into a pod. Idempotent: a pod that already
/// carries the sidecar is returned unchanged. Returns the mutated pod.
pub fn inject_sidecar(
    pod: &Pod,
    ws: &FlintLeanWorkspace,
    defaults: &InjectDefaults,
) -> Result<Pod, String> {
    let mut pod = pod.clone();
    let spec = pod.spec.as_mut().ok_or("pod has no spec")?;

    let already = spec
        .init_containers
        .as_ref()
        .map(|cs| cs.iter().any(|c| c.name == SIDECAR_NAME))
        .unwrap_or(false);
    if already {
        return Ok(pod);
    }

    let s = &ws.spec;
    let marker = format!("{}/.flint-sync/checkout-complete", s.mount_path);

    // The workspace volume.
    let size_limit = if s.size_limit_gib > 0 {
        Some(Quantity(format!("{}Gi", s.size_limit_gib)))
    } else {
        None
    };
    spec.volumes.get_or_insert_with(Vec::new).push(Volume {
        name: VOLUME_NAME.into(),
        empty_dir: Some(EmptyDirVolumeSource { medium: None, size_limit }),
        ..Default::default()
    });

    // Mount it into every app container.
    //
    // REFUSE rather than push blindly if a container already mounts the
    // workspace path. Pushing produced an API-server rejection —
    // "volumeMounts[2].mountPath: Invalid value: \"/workspace\": must be
    // unique" — that names neither flint nor the knob that fixes it, and
    // a pod author who happens to use /workspace has no way to read that
    // error and act on it.
    //
    // The alternative — silently skipping the push — is worse and is
    // deliberately not taken: the container's OWN volume would then
    // shadow the workspace, and the agent would run ungated against the
    // wrong directory while every marker and probe still looked healthy.
    // That is the silent-winner class this codebase refuses everywhere
    // else, and §2.5's admission rule is explicit that an opted-in pod
    // either gets its sidecar or does not schedule.
    let mount = VolumeMount {
        name: VOLUME_NAME.into(),
        mount_path: s.mount_path.clone(),
        ..Default::default()
    };
    for c in spec.containers.iter() {
        if let Some(existing) = c.volume_mounts.as_ref() {
            if let Some(m) = existing.iter().find(|m| m.mount_path == s.mount_path) {
                return Err(format!(
                    "container {:?} already mounts {:?} (volume {:?}); the lean workspace \
                     needs that path. Move one of them: set spec.mountPath on \
                     FlintLeanWorkspace {:?} to a path the pod does not use, or drop the \
                     pod's own mount",
                    c.name, s.mount_path, m.name, ws.metadata.name.as_deref().unwrap_or("?")
                ));
            }
        }
    }
    for c in spec.containers.iter_mut() {
        c.volume_mounts.get_or_insert_with(Vec::new).push(mount.clone());
    }

    // The derived start gate.
    let bytes = s.expected_bytes.unwrap_or(s.max_bytes);
    let files = s.expected_files.unwrap_or(s.max_files);
    let budget = checkout_budget_secs(bytes, files);
    let period: i32 = 5;
    let failure_threshold = i32::try_from(budget.div_ceil(period as u64))
        .map_err(|_| "checkout budget overflows the probe threshold")?;

    let mut env = vec![
        ev("FLINT_SYNC_BUCKET", &s.bucket),
        ev("FLINT_SYNC_PREFIX", &s.key_prefix),
        ev("FLINT_SYNC_ROOT", &s.mount_path),
        ev("FLINT_SYNC_FLOOR_SECS", &s.floor_secs.to_string()),
        ev("FLINT_SYNC_MAX_BYTES", &s.max_bytes.to_string()),
        ev("FLINT_SYNC_MAX_FILES", &s.max_files.to_string()),
        ev("FLINT_SYNC_FANOUT", &s.fanout.to_string()),
        ev("FLINT_SYNC_FETCH_INFLIGHT_MB", &s.fetch_inflight_mb.to_string()),
        // Boundary verbs (§2.6). Every one of these is stamped
        // unconditionally, defaults included: the sidecar reads a FIXED
        // env list, so a knob that is only stamped when it differs from
        // the default is a knob whose absence and whose default look
        // identical to the binary AND to anyone debugging the pod.
        ev("FLINT_SYNC_BOUNDARY_MODE", &s.boundary_mode),
        ev("FLINT_SYNC_SENTINELS", &s.sentinels),
        ev(
            "FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS",
            &s.sentinel_min_interval_secs.to_string(),
        ),
        ev("FLINT_SYNC_SENTINEL_HOURLY_BUDGET", &s.sentinel_hourly_budget.to_string()),
        ev("FLINT_SYNC_QUIESCE_BOUND_SECS", &s.quiesce_bound_secs.to_string()),
        ev(
            "FLINT_SYNC_STAGED_BACKLOG_CAP_OBJECTS",
            &s.staged_backlog_cap_objects.to_string(),
        ),
        ev("FLINT_SYNC_STAGED_BACKLOG_CAP_BYTES", &s.staged_backlog_cap_bytes.to_string()),
        ev("FLINT_SYNC_NONCURRENT_RETENTION_DAYS", &s.noncurrent_retention_days.to_string()),
        ev("FLINT_SYNC_UDS_DOOR", if s.uds_door { "true" } else { "false" }),
        ev("FLINT_SYNC_METRICS", if s.metrics.enabled { "true" } else { "false" }),
        ev("FLINT_SYNC_METRICS_PORT", &s.metrics.port.to_string()),
        // The only two labels any series carries (D15). The namespace
        // comes from the downward API rather than the CR, because the
        // CR does not know where the POD landed.
        ev("FLINT_SYNC_WORKSPACE", &ws.name_any()),
    ];
    env.push(EnvVar {
        name: "FLINT_SYNC_NAMESPACE".into(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: "metadata.namespace".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    });
    // The one knob with no default: gated REFUSES to start without it,
    // and stamping an invented value would defeat that refusal.
    if let Some(lag) = s.visibility_lag_bound_secs {
        env.push(ev("FLINT_SYNC_VISIBILITY_LAG_BOUND_SECS", &lag.to_string()));
    }
    if let Some(endpoint) = &s.endpoint {
        env.push(ev("FLINT_SYNC_ENDPOINT", endpoint));
    }
    let env_from = s.credentials_secret_ref.as_ref().map(|name| {
        vec![EnvFromSource {
            secret_ref: Some(SecretEnvSource { name: name.clone(), optional: Some(false) }),
            ..Default::default()
        }]
    });

    let sidecar = Container {
        name: SIDECAR_NAME.into(),
        image: Some(s.image.clone().unwrap_or_else(|| defaults.image.clone())),
        args: Some(vec!["run".into()]),
        env: Some(env),
        env_from,
        volume_mounts: Some(vec![mount]),
        // Native sidecar: the start gate AND the stop ordering.
        restart_policy: Some("Always".into()),
        startup_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec!["test".into(), "-f".into(), marker]),
            }),
            period_seconds: Some(period),
            failure_threshold: Some(failure_threshold),
            ..Default::default()
        }),
        ports: if s.metrics.enabled {
            Some(vec![ContainerPort {
                name: Some("metrics".into()),
                container_port: i32::try_from(s.metrics.port).unwrap_or(9847),
                protocol: Some("TCP".into()),
                ..Default::default()
            }])
        } else {
            None
        },
        ..Default::default()
    };
    spec.init_containers.get_or_insert_with(Vec::new).push(sidecar);

    // Scrape annotations, stamped only when exposition is on: an
    // annotation that advertises a port nothing listens on is a
    // permanently-down target in somebody's alerting.
    if s.metrics.enabled {
        let ann = pod.metadata.annotations.get_or_insert_with(Default::default);
        ann.insert("prometheus.io/scrape".into(), "true".into());
        ann.insert("prometheus.io/port".into(), s.metrics.port.to_string());
        ann.insert("prometheus.io/path".into(), "/metrics".into());
    }

    // D10 rule 3: the drain has to FIT. Today's injected sidecar sets no
    // terminationGracePeriodSeconds at all, so every workspace drains
    // inside the 30 s default nobody chose — and native-sidecar ordering
    // spends the agent's share of that budget first. Derived from the
    // workspace's own knobs, and only ever UPWARD: a pod template that
    // asks for more grace than we derive knows something we do not.
    let derived = i64::try_from(super::boundary::derived_grace_secs(s))
        .map_err(|_| "derived grace overflows")?;
    let current = spec.termination_grace_period_seconds.unwrap_or(30);
    if derived > current {
        spec.termination_grace_period_seconds = Some(derived);
    }
    Ok(pod)
}

fn ev(name: &str, value: &str) -> EnvVar {
    EnvVar { name: name.into(), value: Some(value.into()), ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lean_operator::crd::{FlintLeanWorkspace, FlintLeanWorkspaceSpec};
    use k8s_openapi::api::core::v1::PodSpec;

    fn ws() -> FlintLeanWorkspace {
        FlintLeanWorkspace::new(
            "proj1",
            serde_json::from_value::<FlintLeanWorkspaceSpec>(serde_json::json!({
                "projectId": "team-a/proj1",
                "bucket": "agentws",
                "keyPrefix": "tenants/proj1",
                "endpoint": "http://proxy:9000",
                "credentialsSecretRef": "proj1-proxy-creds",
                "expectedBytes": 2147483648u64, // 2 GiB
                "expectedFiles": 50000u64,
            }))
            .unwrap(),
        )
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

    /// The derived budget: lockout + bytes + files, ×1.5, floored —
    /// never a fleet constant (the hub's 600 s default is the
    /// cautionary tale).
    #[test]
    fn derived_budget_scales_with_inventory() {
        // Tiny workspace: the floor.
        assert_eq!(checkout_budget_secs(0, 0), 165); // (110)*1.5
        // 20 GiB: the hub's fleet-constant 600 s would kill this.
        let big = checkout_budget_secs(20 << 30, 0);
        assert!(big > 600, "20 GiB budget must exceed the hub's old constant: {big}");
        // Files dominate at high counts.
        let many = checkout_budget_secs(0, 250_000);
        assert!(many >= 750, "250k files at 500/s needs >= 500s pre-headroom: {many}");
    }

    #[test]
    fn injects_native_sidecar_with_gate_and_mounts() {
        let out = inject_sidecar(&pod(), &ws(), &InjectDefaults { image: "flint-sync:test".into() })
            .unwrap();
        let spec = out.spec.unwrap();

        let sc = &spec.init_containers.as_ref().unwrap()[0];
        assert_eq!(sc.name, SIDECAR_NAME);
        assert_eq!(sc.restart_policy.as_deref(), Some("Always"), "must be a NATIVE sidecar");
        let probe = sc.startup_probe.as_ref().unwrap();
        let cmd = probe.exec.as_ref().unwrap().command.as_ref().unwrap();
        assert_eq!(cmd[2], "/workspace/.flint-sync/checkout-complete");
        // 2 GiB + 50k files: 110 + 2*15 + 100 = 240 → ×1.5 = 360 s / 5 s.
        assert_eq!(probe.failure_threshold, Some(72));

        // The agent container mounts the workspace.
        assert!(spec.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == VOLUME_NAME && m.mount_path == "/workspace"));
        // Volume present with the sizeLimit.
        assert!(spec.volumes.as_ref().unwrap().iter().any(|v| v.name == VOLUME_NAME));
        // Env carries the subtree address; creds ride the secret ref.
        let env = sc.env.as_ref().unwrap();
        assert!(env.iter().any(|e| e.name == "FLINT_SYNC_PREFIX"
            && e.value.as_deref() == Some("tenants/proj1")));
        assert!(sc.env_from.is_some());
    }

    fn sidecar_of(pod: &Pod) -> &Container {
        &pod.spec.as_ref().unwrap().init_containers.as_ref().unwrap()[0]
    }

    fn env_of(pod: &Pod) -> std::collections::BTreeMap<String, String> {
        sidecar_of(pod)
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect()
    }

    /// The sidecar's own source, read at COMPILE time. The contract
    /// between these two crates is a list of env names in two files,
    /// and every previous version of that contract drifted: a knob the
    /// CRD offers and the binary never reads is the "knobs that exist
    /// and do NOTHING" class, and a knob the binary reads and the
    /// webhook never stamps is the same bug facing the other way.
    const SIDECAR_MAIN: &str = include_str!("../../../lean/sidecar/src/bin/flint_sync.rs");

    #[test]
    fn every_knob_the_sidecar_reads_is_stamped_by_the_webhook() {
        // Names the binary actually parses (not the doc-comment block).
        let mut read: std::collections::BTreeSet<String> = Default::default();
        for line in SIDECAR_MAIN.lines() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for (i, _) in code.match_indices("\"FLINT_SYNC_") {
                let rest = &code[i + 1..];
                if let Some(end) = rest.find('"') {
                    read.insert(rest[..end].to_string());
                }
            }
        }
        assert!(read.len() > 10, "the source scan found almost nothing: {read:?}");

        let out = inject_sidecar(&pod(), &ws(), &InjectDefaults { image: "i".into() }).unwrap();
        let stamped = env_of(&out);
        // Declared exceptions, each with a reason. Anything else that
        // shows up here is drift, and the test says which side.
        let exceptions: &[(&str, &str)] = &[
            ("FLINT_SYNC_ENDPOINT", "stamped only when the CR overrides it"),
            ("FLINT_SYNC_SENTINEL_POLL_SECS", "env-only by design — not a fleet contract"),
            (
                "FLINT_SYNC_VISIBILITY_LAG_BOUND_SECS",
                "no default: gated refuses without it, and stamping an invented value \
                 would defeat that refusal",
            ),
        ];
        for name in &read {
            if exceptions.iter().any(|(n, _)| n == name) {
                continue;
            }
            assert!(
                stamped.contains_key(name),
                "the sidecar reads {name} and the webhook never stamps it — the workspace \
                 silently runs the binary default instead of the CR"
            );
        }
        for name in stamped.keys() {
            assert!(
                read.contains(name.as_str()),
                "the webhook stamps {name} and the sidecar never reads it — a knob that \
                 exists and does NOTHING"
            );
        }
    }

    /// The gated lag bound has no default anywhere, so stamping it when
    /// the CR does not set it would hand the sidecar a number nobody
    /// chose and defeat its startup refusal.
    #[test]
    fn the_lag_bound_is_stamped_only_when_the_cr_sets_it() {
        let out = inject_sidecar(&pod(), &ws(), &InjectDefaults { image: "i".into() }).unwrap();
        assert!(!env_of(&out).contains_key("FLINT_SYNC_VISIBILITY_LAG_BOUND_SECS"));

        let mut gated = ws();
        gated.spec.boundary_mode = "gated".into();
        gated.spec.visibility_lag_bound_secs = Some(300);
        let out = inject_sidecar(&pod(), &gated, &InjectDefaults { image: "i".into() }).unwrap();
        let env = env_of(&out);
        assert_eq!(env["FLINT_SYNC_VISIBILITY_LAG_BOUND_SECS"], "300");
        assert_eq!(env["FLINT_SYNC_BOUNDARY_MODE"], "gated");
    }

    /// D10 rule 3. The failing control is the FIRST assertion: today the
    /// injected sidecar sets no grace at all, so every workspace drains
    /// inside the 30 s default nobody chose.
    #[test]
    fn the_drain_gets_a_derived_grace_and_a_bigger_one_is_never_lowered() {
        let out = inject_sidecar(&pod(), &ws(), &InjectDefaults { image: "i".into() }).unwrap();
        let grace = out.spec.as_ref().unwrap().termination_grace_period_seconds;
        assert!(grace.is_some_and(|g| g >= 30), "no derived grace: {grace:?}");

        // The derivation must TRACK the knobs it claims to be derived
        // from — a "derived" number that ignores them is a fleet
        // constant wearing a derivation's clothes, which is the hazard
        // the startupProbe budget was written against.
        let mut gated = ws();
        gated.spec.boundary_mode = "gated".into();
        gated.spec.visibility_lag_bound_secs = Some(300);
        let out = inject_sidecar(&pod(), &gated, &InjectDefaults { image: "i".into() }).unwrap();
        let small = out.spec.as_ref().unwrap().termination_grace_period_seconds.unwrap();

        let mut bigger = gated.clone();
        bigger.spec.staged_backlog_cap_bytes *= 2;
        let out = inject_sidecar(&pod(), &bigger, &InjectDefaults { image: "i".into() }).unwrap();
        let large = out.spec.as_ref().unwrap().termination_grace_period_seconds.unwrap();
        assert!(large > small, "doubling the backlog cap did not move the grace: {small} → {large}");

        // …and the cadence estimate tracks the floor, which is the only
        // bound that mode has: nothing stages, so the drain repeats at
        // most one floor's barrier.
        let mut slow = ws();
        slow.spec.floor_secs = 600;
        let out = inject_sidecar(&pod(), &slow, &InjectDefaults { image: "i".into() }).unwrap();
        assert!(
            out.spec.unwrap().termination_grace_period_seconds.unwrap() > grace.unwrap(),
            "a 10-minute floor drains no slower than a 1-minute one?"
        );

        // A pod that asks for more knows something we do not.
        let mut big = pod();
        big.spec.as_mut().unwrap().termination_grace_period_seconds = Some(3600);
        let out = inject_sidecar(&big, &gated, &InjectDefaults { image: "i".into() }).unwrap();
        assert_eq!(
            out.spec.unwrap().termination_grace_period_seconds,
            Some(3600),
            "the webhook LOWERED a grace period the pod author chose"
        );
    }

    /// D15 is opt-in, and "opt-in" has to mean the pod is untouched:
    /// an annotation advertising a port nothing listens on is a
    /// permanently-down target in somebody's alerting.
    #[test]
    fn metrics_plumbing_appears_only_when_metrics_are_enabled() {
        let out = inject_sidecar(&pod(), &ws(), &InjectDefaults { image: "i".into() }).unwrap();
        assert!(out.metadata.annotations.is_none() || out.metadata.annotations.unwrap().is_empty());
        let sc = &out.spec.as_ref().unwrap().init_containers.as_ref().unwrap()[0];
        assert!(sc.ports.is_none(), "a port was declared with exposition off");
        let out2 = inject_sidecar(&pod(), &ws(), &InjectDefaults { image: "i".into() }).unwrap();
        assert_eq!(env_of(&out2)["FLINT_SYNC_METRICS"], "false");

        let mut on = ws();
        on.spec.metrics.enabled = true;
        on.spec.metrics.port = 9911;
        let out = inject_sidecar(&pod(), &on, &InjectDefaults { image: "i".into() }).unwrap();
        let ann = out.metadata.annotations.clone().unwrap();
        assert_eq!(ann["prometheus.io/scrape"], "true");
        assert_eq!(ann["prometheus.io/port"], "9911");
        assert_eq!(ann["prometheus.io/path"], "/metrics");
        let sc = &out.spec.as_ref().unwrap().init_containers.as_ref().unwrap()[0];
        assert_eq!(sc.ports.as_ref().unwrap()[0].container_port, 9911);
        let env = env_of(&out);
        assert_eq!(env["FLINT_SYNC_METRICS"], "true");
        assert_eq!(env["FLINT_SYNC_METRICS_PORT"], "9911");
        // The two metric labels: the workspace from the CR, the
        // namespace from the downward API — the CR does not know where
        // the pod landed.
        assert_eq!(env["FLINT_SYNC_WORKSPACE"], "proj1");
        let ns = sc
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "FLINT_SYNC_NAMESPACE")
            .expect("no namespace label source");
        assert_eq!(
            ns.value_from.as_ref().unwrap().field_ref.as_ref().unwrap().field_path,
            "metadata.namespace"
        );
    }

    #[test]
    /// A pod that already mounts the workspace path is REFUSED with a
    /// message naming the knob, not pushed into an API-server rejection
    /// that mentions neither flint nor `spec.mountPath`.
    ///
    /// Refusing is the point. Skipping the push would let the pod's own
    /// volume shadow the workspace and run the agent ungated against the
    /// wrong directory, with every marker and probe still looking
    /// healthy — a silent winner, which §2.5 forbids at admission.
    #[test]
    fn a_pod_that_already_mounts_the_workspace_path_is_refused_by_name() {
        let d = InjectDefaults { image: "i".into() };
        let mut p = pod();
        p.spec.as_mut().unwrap().containers[0].volume_mounts = Some(vec![VolumeMount {
            name: "my-scratch".into(),
            mount_path: "/workspace".into(),
            ..Default::default()
        }]);
        let err = inject_sidecar(&p, &ws(), &d)
            .expect_err("a colliding mount must be refused, not silently shadowed");
        assert!(err.contains("/workspace"), "must name the path: {err}");
        assert!(err.contains("spec.mountPath"), "must name the knob that fixes it: {err}");
        assert!(err.contains("my-scratch"), "must name the offending volume: {err}");
    }

    /// …and the refusal tracks the CONFIGURED path, not a hardcoded
    /// "/workspace": a workspace moved to /flint collides on /flint and
    /// is fine on /workspace. That is the fix the message recommends, so
    /// it has to actually work.
    #[test]
    fn the_collision_check_follows_spec_mount_path() {
        let d = InjectDefaults { image: "i".into() };
        let mut w = ws();
        w.spec.mount_path = "/flint".into();

        let mut keeps_own = pod();
        keeps_own.spec.as_mut().unwrap().containers[0].volume_mounts = Some(vec![VolumeMount {
            name: "my-scratch".into(),
            mount_path: "/workspace".into(),
            ..Default::default()
        }]);
        inject_sidecar(&keeps_own, &w, &d)
            .expect("/workspace is free real estate once the workspace moved to /flint");

        let mut collides = pod();
        collides.spec.as_mut().unwrap().containers[0].volume_mounts = Some(vec![VolumeMount {
            name: "mine".into(),
            mount_path: "/flint".into(),
            ..Default::default()
        }]);
        let err = inject_sidecar(&collides, &w, &d).expect_err("collision on the moved path");
        assert!(err.contains("/flint"), "{err}");
    }

    #[test]
    fn injection_is_idempotent() {
        let d = InjectDefaults { image: "i".into() };
        let once = inject_sidecar(&pod(), &ws(), &d).unwrap();
        let twice = inject_sidecar(&once, &ws(), &d).unwrap();
        assert_eq!(once, twice);
    }
}
