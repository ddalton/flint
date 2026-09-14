//! The syncer's environment: the FIXED `FLINT_SYNC_*` list a
//! FlintLeanWorkspace turns into, and the derived checkout budget. The
//! s3.csi.chert.us node plugin (`crate::s3csi::node`) hands this list to
//! the lean worker over the launch socket; it is what the retired
//! webhook used to stamp on an injected syncer.

use kube::ResourceExt;

use super::crd::FlintLeanWorkspace;

/// The derived checkout budget (plan §2.4). Rates are conservative
/// proxy-shaped planning numbers anchored on the 0b loopback floors
/// (3.3 s/GiB checkout, ~2,000 files/s sequential, 16× fan-out): a
/// re-measure through the REAL proxy replaces them, the shape stays.
pub fn checkout_budget_secs(expected_bytes: u64, expected_files: u64) -> u64 {
    /// Unclean-death claim lockout the replacement must wait out
    /// (QUIET_POLLS × their spacing, ~60–110 s observed band).
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

/// The FIXED `FLINT_SYNC_*` list, every knob stamped, defaults included:
/// the binary reads a fixed env list, so a knob that is only stamped
/// when it differs from the default is a knob whose absence and whose
/// default look identical to the binary AND to anyone debugging the pod.
/// `root` is where the syncer sees the tree. `FLINT_SYNC_NAMESPACE` is
/// NOT here — the CSI node driver stamps the tenant namespace as a
/// literal (design §5), never from a downward-API ref that would name
/// the WORKER's namespace.
pub fn sync_env(ws: &FlintLeanWorkspace, root: &str) -> Vec<(String, String)> {
    let s = &ws.spec;
    let p = |k: &str, v: String| (k.to_string(), v);
    let mut env = vec![
        p("FLINT_SYNC_BUCKET", s.bucket.clone()),
        p("FLINT_SYNC_PREFIX", s.key_prefix.clone()),
        p("FLINT_SYNC_PROJECT_ID", s.project_id.clone()),
        p("FLINT_SYNC_ROOT", root.to_string()),
        p("FLINT_SYNC_FLOOR_SECS", s.floor_secs.to_string()),
        p("FLINT_SYNC_MAX_BYTES", s.max_bytes.to_string()),
        p("FLINT_SYNC_MAX_FILES", s.max_files.to_string()),
        p("FLINT_SYNC_FANOUT", s.fanout.to_string()),
        p("FLINT_SYNC_FETCH_INFLIGHT_MB", s.fetch_inflight_mb.to_string()),
        p("FLINT_SYNC_FETCH_DRIVERS", s.fetch_drivers.to_string()),
        p("FLINT_SYNC_UPLOAD_PART_PARALLELISM", s.upload_part_parallelism.to_string()),
        p("FLINT_SYNC_UPLOAD_INFLIGHT_MB", s.upload_inflight_mb.to_string()),
        p("FLINT_SYNC_EVENT_TRACE", if s.event_trace { "true" } else { "false" }.to_string()),
        p("FLINT_SYNC_RAW_READS", s.raw_reads.to_string()),
        // Boundary verbs (§2.6), unconditionally.
        p("FLINT_SYNC_SENTINELS", s.sentinels.clone()),
        p("FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS", s.sentinel_min_interval_secs.to_string()),
        p("FLINT_SYNC_SENTINEL_HOURLY_BUDGET", s.sentinel_hourly_budget.to_string()),
        p("FLINT_SYNC_UDS_DOOR", if s.uds_door { "true" } else { "false" }.to_string()),
        p("FLINT_SYNC_METRICS", if s.metrics.enabled { "true" } else { "false" }.to_string()),
        p("FLINT_SYNC_METRICS_PORT", s.metrics.port.to_string()),
        // One of the only two labels any series carries (D15).
        p("FLINT_SYNC_WORKSPACE", ws.name_any()),
    ];
    if let Some(endpoint) = &s.endpoint {
        env.push(p("FLINT_SYNC_ENDPOINT", endpoint.clone()));
    }
    // The bucket's region, when the CR names one. The node plugin turns
    // it into the worker's AWS_REGION ahead of its node-wide default
    // (`creds::effective_region`); absent, the default stands.
    if let Some(region) = &s.region {
        env.push(p("FLINT_SYNC_REGION", region.clone()));
    }
    env
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::lean_operator::crd::FlintLeanWorkspaceSpec;

    fn ws() -> FlintLeanWorkspace {
        FlintLeanWorkspace::new(
            "proj1",
            serde_json::from_value::<FlintLeanWorkspaceSpec>(serde_json::json!({
                "projectId": "team-a/proj1",
                "bucket": "agentws",
                "keyPrefix": "tenants/proj1",
                "endpoint": "http://proxy:9000",
                "region": "us-west-1",
                "expectedBytes": 2147483648u64,
                "expectedFiles": 50000u64,
            }))
            .unwrap(),
        )
    }

    /// Every knob is stamped, defaults included, so an absent knob and a
    /// default knob never look alike to the binary or to a debugger.
    #[test]
    fn the_env_is_the_fixed_list_with_defaults_stamped() {
        let env = sync_env(&ws(), "/workspace");
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("FLINT_SYNC_BUCKET").as_deref(), Some("agentws"));
        assert_eq!(get("FLINT_SYNC_PREFIX").as_deref(), Some("tenants/proj1"));
        assert_eq!(get("FLINT_SYNC_ROOT").as_deref(), Some("/workspace"));
        assert_eq!(get("FLINT_SYNC_ENDPOINT").as_deref(), Some("http://proxy:9000"));
        assert_eq!(get("FLINT_SYNC_WORKSPACE").as_deref(), Some("proj1"));
        assert_eq!(get("FLINT_SYNC_REGION").as_deref(), Some("us-west-1"));
        // The 8-wide per-object upload is the default now that the
        // upload window carries a byte bound; both are stamped together.
        assert_eq!(get("FLINT_SYNC_UPLOAD_PART_PARALLELISM").as_deref(), Some("8"));
        assert_eq!(get("FLINT_SYNC_UPLOAD_INFLIGHT_MB").as_deref(), Some("256"));
        // Off unless the CR asks, and stamped either way (the fixed list).
        assert_eq!(get("FLINT_SYNC_EVENT_TRACE").as_deref(), Some("false"));
        for k in ["FLINT_SYNC_FLOOR_SECS", "FLINT_SYNC_MAX_FILES", "FLINT_SYNC_FANOUT", "FLINT_SYNC_FETCH_DRIVERS", "FLINT_SYNC_UPLOAD_PART_PARALLELISM", "FLINT_SYNC_UPLOAD_INFLIGHT_MB", "FLINT_SYNC_EVENT_TRACE", "FLINT_SYNC_RAW_READS", "FLINT_SYNC_SENTINELS", "FLINT_SYNC_METRICS_PORT"] {
            assert!(get(k).is_some(), "{k} must be stamped even at its default");
        }
        assert!(get("FLINT_SYNC_NAMESPACE").is_none(), "the namespace is the caller's literal, never this list's");
        assert!(get("FLINT_SYNC_BOUNDARY_MODE").is_none(), "the retired mode knob is not stamped");
    }

    /// A region is stamped only when the CR names one: the plugin's
    /// node-wide default must stay distinguishable from a CR's choice.
    #[test]
    fn region_is_stamped_only_when_named() {
        let ws = FlintLeanWorkspace::new(
            "proj2",
            serde_json::from_value::<FlintLeanWorkspaceSpec>(serde_json::json!({
                "projectId": "team-a/proj2", "bucket": "agentws", "keyPrefix": "tenants/proj2",
            }))
            .unwrap(),
        );
        assert!(!sync_env(&ws, "/workspace").iter().any(|(k, _)| k == "FLINT_SYNC_REGION"));
    }

    /// Budget grows with the inventory and never drops below the floor.
    #[test]
    fn checkout_budget_is_floored_and_monotonic() {
        let small = checkout_budget_secs(1 << 30, 1_000);
        let big = checkout_budget_secs(10 << 30, 100_000);
        assert!(big > small);
        assert!(small >= 120);
        assert!(checkout_budget_secs(0, 0) >= 120);
    }
}
