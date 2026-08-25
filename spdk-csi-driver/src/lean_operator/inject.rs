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
    Container, EmptyDirVolumeSource, EnvFromSource, EnvVar, ExecAction, Pod, Probe,
    SecretEnvSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

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
    let mount = VolumeMount {
        name: VOLUME_NAME.into(),
        mount_path: s.mount_path.clone(),
        ..Default::default()
    };
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
    ];
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
        ..Default::default()
    };
    spec.init_containers.get_or_insert_with(Vec::new).push(sidecar);
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

    #[test]
    fn injection_is_idempotent() {
        let d = InjectDefaults { image: "i".into() };
        let once = inject_sidecar(&pod(), &ws(), &d).unwrap();
        let twice = inject_sidecar(&once, &ws(), &d).unwrap();
        assert_eq!(once, twice);
    }
}
