// guarded_destroy.rs — Contract R3: the destruction chokepoint.
//
// Every op that can sever a live data path (subsystem delete, raid delete,
// lvol delete, ublk stop, controller detach) passes through one decision
// point whose inputs are LIVE probes — never the driver's own sync/intent
// record. The rules, verified against the incident history (C1-C3):
//
//   1. Live probe at the layer's native observability level: SPDK RPCs for
//      bdev/subsystem consumption, KERNEL opener probes for ublk frontends
//      (no SPDK RPC can see kernel openers — F37's probe stays kernel-side).
//      Raid consumption of an lvol is base_bdevs_list membership matched by
//      uuid AND alias AND name (the controller_reap precedent) — NOT the
//      bdev `claimed` bool, which names no claimer.
//   2. Configured-consumer authority: zero live controllers ≠ no consumer
//      (kernel initiators reconnect autonomously for up to ctrl_loss_tmo);
//      the VolumeAttachment and the subsystem's allowed-hosts list are
//      REQUIRED inputs alongside connection state.
//   3. Self-host live consumption is an ABSOLUTE veto (the F38 destroyer
//      held current authority); other-host live consumers block only when
//      still admitted (fence-then-drop handles the runf-eviction zombies).
//
// Probe failure branches on error class: target-verifiably-missing allows
// the idempotent no-op (else NodeUnstage wedges forever after a tgt
// restart — the reason F9 historically failed open); transport/unknown
// errors DEFER (F37's "never reap blind", never F9's fail-open).
//
// Operation-scoped objects (`:hotrejoin:` subsystems, `_hrpad` pads, `_hr`
// scratch heads) are exempt at the boundary: their lifecycles are owned by
// flows the per-volume claim registry serializes, and their teardown
// legitimately races connection state the volume-class rules would refuse.
//
// Enforcement is structural: the destructive RPC method names live here as
// constants, and the CI lint (`scripts/lint-guarded-destroy.sh`) fails the
// build on raw string literals anywhere else.

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// The destructive method names. The ONLY place these strings may appear.
// ---------------------------------------------------------------------------
pub const RPC_NVMF_DELETE_SUBSYSTEM: &str = "nvmf_delete_subsystem";
pub const RPC_BDEV_RAID_DELETE: &str = "bdev_raid_delete";
pub const RPC_BDEV_LVOL_DELETE: &str = "bdev_lvol_delete";
pub const RPC_UBLK_STOP_DISK: &str = "ublk_stop_disk";
pub const RPC_BDEV_NVME_DETACH_CONTROLLER: &str = "bdev_nvme_detach_controller";

/// Methods the /api/spdk/rpc boundary intercepts.
pub const GUARDED_METHODS: &[&str] = &[
    RPC_NVMF_DELETE_SUBSYSTEM,
    RPC_BDEV_RAID_DELETE,
    RPC_BDEV_LVOL_DELETE,
    RPC_UBLK_STOP_DISK,
    RPC_BDEV_NVME_DETACH_CONTROLLER,
];

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Destruction may proceed.
    Allow,
    /// Target verifiably absent — the destructive call is an idempotent
    /// no-op; let it through so retry loops converge.
    AllowIdempotentNoop,
    /// Live consumer evidence — destruction refused. The message must NOT
    /// match any benign error classifier (is_missing / already-exists), so
    /// callers surface it as a real failure and retry loops stay honest.
    Refuse(String),
    /// Probe inconclusive (transport error, unknown state) — fail closed,
    /// retry next cycle. Distinct from Refuse for observability.
    Defer(String),
}

impl Verdict {
    pub fn blocked(&self) -> Option<&str> {
        match self {
            Verdict::Refuse(r) | Verdict::Defer(r) => Some(r),
            _ => None,
        }
    }
}

/// Error classing for probe failures (correction C1-3): "missing" means no
/// consumer can exist and the destructive call is an idempotent no-op —
/// treating it as a Defer would wedge NodeUnstage forever after a tgt
/// restart. Everything else is transport/unknown → Defer.
pub fn probe_error_is_missing(msg: &str) -> bool {
    msg.contains("No such device")
        || msg.contains("Code=-19")
        || msg.contains("No such file or directory")
        || msg.contains("does not exist")
        || msg.contains("not found")
        || msg.contains("Code=-2")
}

/// Operation-scoped object shapes exempt from the volume-class boundary
/// rules (their flows hold the per-volume claim and a quiesce lease).
pub fn is_operation_scoped(identifier: &str) -> bool {
    identifier.contains(":hotrejoin:")
        || identifier.contains("_hrpad")
        || identifier.ends_with("_hr")
        || identifier.contains("_hr_")
        || identifier.contains("hotrejoin")
}

// ---------------------------------------------------------------------------
// Pure decision cores — the unit-tested tables.
// ---------------------------------------------------------------------------

/// Subsystem delete: the three-valued hostnqn rule (C1-5).
///
/// - live SELF-host controller → absolute veto (this node is serving the
///   chain — the F38 shape; no authority overrides it);
/// - live OTHER-host controller that is still ADMITTED (in allowed-hosts,
///   or the subsystem allows any host) → refuse (it is the rightful
///   consumer — the F9 shape) ;
/// - live other-host controller NOT admitted → stale by fence rules (the
///   runf-eviction zombie): does not block;
/// - zero live controllers but the VolumeAttachment says another node owns
///   the volume → refuse (configured consumer mid-reconnect, C3);
/// - VA lookup ERRORED (as opposed to "unattached") → defer, fail closed.
pub fn subsystem_delete_verdict(
    live_hostnqns: &[String],
    allowed_hosts: &[String],
    allow_any_host: bool,
    own_host_nqn: &str,
    va_owner: Option<&str>,
    va_lookup_errored: bool,
    self_node: &str,
) -> Verdict {
    for h in live_hostnqns {
        if h == own_host_nqn {
            return Verdict::Refuse(format!(
                "guarded_destroy: live SELF-host controller {} on this subsystem — this node is \
                 serving the chain; destruction vetoed unconditionally",
                h
            ));
        }
    }
    for h in live_hostnqns {
        let admitted = allow_any_host || allowed_hosts.iter().any(|a| a == h);
        if admitted {
            return Verdict::Refuse(format!(
                "guarded_destroy: live ADMITTED foreign controller {} — rightful consumer \
                 elsewhere (F9 shape); leak-and-reconcile beats a cross-node data-plane kill",
                h
            ));
        }
    }
    if va_lookup_errored {
        return Verdict::Defer(
            "guarded_destroy: VolumeAttachment lookup errored — cannot rule out a configured \
             consumer mid-reconnect; failing closed this cycle"
                .to_string(),
        );
    }
    if let Some(owner) = va_owner {
        if owner != self_node {
            return Verdict::Refuse(format!(
                "guarded_destroy: VolumeAttachment owned by {} — configured consumer may be \
                 mid-reconnect (zero live controllers is not evidence of absence)",
                owner
            ));
        }
    }
    Verdict::Allow
}

/// Lvol delete: refuse while any raid claims it as a base, or any export
/// namespacing it holds live controllers (F36 guard-a generalized; D-class).
pub fn lvol_delete_verdict(
    raid_consumers: &[String],
    exports_with_live_controllers: &[String],
) -> Verdict {
    if let Some(raid) = raid_consumers.first() {
        return Verdict::Refuse(format!(
            "guarded_destroy: lvol is a base of raid {} — deleting a live raid's leg severs the \
             chain; remove it from the raid first",
            raid
        ));
    }
    if let Some(nqn) = exports_with_live_controllers.first() {
        return Verdict::Refuse(format!(
            "guarded_destroy: lvol is namespaced by subsystem {} with live controller(s) — the \
             F36 guard-a shape; destruction refused",
            nqn
        ));
    }
    Verdict::Allow
}

/// Raid delete: refuse while a frontend consumes the raid (ublk disk over
/// it, or an export namespacing it with live controllers). An ONLINE raid
/// with NO frontend consumer stays deletable — the anti-zombie and phantom
/// hygiene paths are legitimate (their raids never got a frontend); the
/// latent D2 hazard was consumers, not state.
pub fn raid_delete_verdict(
    raid_present: bool,
    ublk_consumer: Option<u64>,
    exports_with_live_controllers: &[String],
) -> Verdict {
    if !raid_present {
        return Verdict::AllowIdempotentNoop;
    }
    if let Some(id) = ublk_consumer {
        return Verdict::Refuse(format!(
            "guarded_destroy: raid is served by ublk disk {} — deleting it hot-removes the block \
             device under a mounted filesystem (D2)",
            id
        ));
    }
    if let Some(nqn) = exports_with_live_controllers.first() {
        return Verdict::Refuse(format!(
            "guarded_destroy: raid is namespaced by subsystem {} with live controller(s) (D2)",
            nqn
        ));
    }
    Verdict::Allow
}

/// Controller detach: refuse while the controller's namespace bdev is a
/// base of any raid (severs a live leg — the controller_reap exclusion,
/// promoted to every detach path).
pub fn detach_controller_verdict(controller_name: &str, raid_base_names: &[String]) -> Verdict {
    let ns_bdev = format!("{}n1", controller_name);
    if raid_base_names.iter().any(|b| b == &ns_bdev || b == controller_name) {
        return Verdict::Refuse(format!(
            "guarded_destroy: controller {}'s namespace is a live raid base — detach severs the \
             leg; remove it from the raid first",
            controller_name
        ));
    }
    Verdict::Allow
}

// ---------------------------------------------------------------------------
// Probe helpers over the node-local SPDK transport.
// ---------------------------------------------------------------------------

type SpdkResult = Result<Value, Box<dyn std::error::Error + Send + Sync>>;

/// The transport seam: node-side callers pass a closure over their
/// MinimalDiskService; tests pass a canned-response closure.
#[async_trait::async_trait]
pub trait SpdkProbe: Send + Sync {
    async fn rpc(&self, request: &Value) -> SpdkResult;
}

#[async_trait::async_trait]
impl<F, Fut> SpdkProbe for F
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = SpdkResult> + Send,
{
    async fn rpc(&self, request: &Value) -> SpdkResult {
        self(request.clone()).await
    }
}

fn result_array(v: &Value) -> Vec<Value> {
    v.get("result")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

/// All identity forms of a bdev (name, uuid, aliases) — the F36
/// name-agnostic lesson: raids reference bases by whatever identifier they
/// were created with.
pub async fn bdev_identity_forms(
    probe: &dyn SpdkProbe,
    name: &str,
) -> Result<Option<Vec<String>>, String> {
    match probe
        .rpc(&json!({ "method": "bdev_get_bdevs", "params": { "name": name } }))
        .await
    {
        Ok(resp) => {
            let rows = result_array(&resp);
            let Some(b) = rows.first() else { return Ok(None) };
            let mut forms: Vec<String> = vec![name.to_string()];
            for k in ["name", "uuid"] {
                if let Some(s) = b.get(k).and_then(|v| v.as_str()) {
                    forms.push(s.to_string());
                }
            }
            if let Some(aliases) = b.get("aliases").and_then(|v| v.as_array()) {
                forms.extend(aliases.iter().filter_map(|a| a.as_str().map(str::to_string)));
            }
            forms.sort();
            forms.dedup();
            Ok(Some(forms))
        }
        Err(e) if probe_error_is_missing(&e.to_string()) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Raids whose base_bdevs_list references any of the given identity forms.
pub async fn raids_consuming(
    probe: &dyn SpdkProbe,
    forms: &[String],
) -> Result<Vec<String>, String> {
    let resp = probe
        .rpc(&json!({ "method": "bdev_raid_get_bdevs", "params": { "category": "all" } }))
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for raid in result_array(&resp) {
        let bases = raid.get("base_bdevs_list").and_then(|b| b.as_array());
        let hit = bases
            .map(|bs| {
                bs.iter().any(|b| {
                    ["name", "uuid"].iter().any(|k| {
                        b.get(*k)
                            .and_then(|v| v.as_str())
                            .map(|s| forms.iter().any(|f| f == s))
                            .unwrap_or(false)
                    })
                })
            })
            .unwrap_or(false);
        if hit {
            if let Some(n) = raid.get("name").and_then(|n| n.as_str()) {
                out.push(n.to_string());
            }
        }
    }
    Ok(out)
}

/// Subsystems namespacing any of the identity forms that hold ≥1 live
/// controller. Namespace match is name-agnostic (bdev_name / uuid / name).
pub async fn exports_with_live_controllers(
    probe: &dyn SpdkProbe,
    forms: &[String],
) -> Result<Vec<String>, String> {
    let subs = probe
        .rpc(&json!({ "method": "nvmf_get_subsystems" }))
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for sub in result_array(&subs) {
        let Some(nqn) = sub.get("nqn").and_then(|n| n.as_str()) else { continue };
        let namespaces = sub.get("namespaces").and_then(|n| n.as_array());
        let matches = namespaces
            .map(|nss| {
                nss.iter().any(|ns| {
                    ["bdev_name", "uuid", "name"].iter().any(|k| {
                        ns.get(*k)
                            .and_then(|v| v.as_str())
                            .map(|s| forms.iter().any(|f| f == s))
                            .unwrap_or(false)
                    })
                })
            })
            .unwrap_or(false);
        if !matches {
            continue;
        }
        match probe
            .rpc(&json!({
                "method": "nvmf_subsystem_get_controllers",
                "params": { "nqn": nqn }
            }))
            .await
        {
            Ok(resp) => {
                if !result_array(&resp).is_empty() {
                    out.push(nqn.to_string());
                }
            }
            Err(e) if probe_error_is_missing(&e.to_string()) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

/// Live controllers + admission config of one subsystem, for the
/// three-valued rule. Returns (live_hostnqns, allowed_hosts,
/// allow_any_host); None when the subsystem is verifiably absent.
pub async fn subsystem_consumers(
    probe: &dyn SpdkProbe,
    nqn: &str,
) -> Result<Option<(Vec<String>, Vec<String>, bool)>, String> {
    let subs = probe
        .rpc(&json!({ "method": "nvmf_get_subsystems" }))
        .await
        .map_err(|e| e.to_string())?;
    let Some(sub) = result_array(&subs)
        .into_iter()
        .find(|s| s.get("nqn").and_then(|n| n.as_str()) == Some(nqn))
    else {
        return Ok(None);
    };
    let allowed: Vec<String> = sub
        .get("hosts")
        .and_then(|h| h.as_array())
        .map(|hs| {
            hs.iter()
                .filter_map(|h| h.get("nqn").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let any_host = sub.get("allow_any_host").and_then(|a| a.as_bool()).unwrap_or(false);
    let live: Vec<String> = match probe
        .rpc(&json!({
            "method": "nvmf_subsystem_get_controllers",
            "params": { "nqn": nqn }
        }))
        .await
    {
        Ok(resp) => result_array(&resp)
            .iter()
            .filter_map(|c| c.get("hostnqn").and_then(|h| h.as_str()).map(str::to_string))
            .collect(),
        Err(e) if probe_error_is_missing(&e.to_string()) => Vec::new(),
        Err(e) => return Err(e.to_string()),
    };
    Ok(Some((live, allowed, any_host)))
}

/// ublk disk id (if any) serving one of the identity forms.
pub async fn ublk_consumer_of(
    probe: &dyn SpdkProbe,
    forms: &[String],
) -> Result<Option<u64>, String> {
    let resp = probe
        .rpc(&json!({ "method": "ublk_get_disks" }))
        .await
        .map_err(|e| e.to_string())?;
    for d in result_array(&resp) {
        let bdev = d.get("bdev_name").and_then(|b| b.as_str()).unwrap_or("");
        if forms.iter().any(|f| f == bdev) {
            let id = d
                .get("id")
                .or_else(|| d.get("ublk_id"))
                .and_then(|i| i.as_u64());
            return Ok(id.or(Some(0)));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Boundary verdicts: probe + decide for each guarded method. `va_lookup`
// supplies (owner, errored) for volume-class subsystems; the caller resolves
// it because kube access lives with the caller.
// ---------------------------------------------------------------------------

pub struct BoundaryContext<'a> {
    pub own_host_nqn: &'a str,
    pub self_node: &'a str,
    /// (owner_node, lookup_errored) for the volume owning the target NQN;
    /// None when the target is not a volume-class subsystem.
    pub va: Option<(Option<String>, bool)>,
}

/// Guard one intercepted RPC. Returns None when the method/target is out of
/// scope (not guarded, operation-scoped, or malformed params — malformed
/// requests fail downstream anyway).
pub async fn boundary_verdict(
    probe: &dyn SpdkProbe,
    method: &str,
    params: &Value,
    ctx: &BoundaryContext<'_>,
) -> Option<Verdict> {
    match method {
        m if m == RPC_NVMF_DELETE_SUBSYSTEM => {
            let nqn = params.get("nqn")?.as_str()?;
            if is_operation_scoped(nqn) {
                return None;
            }
            let (live, allowed, any_host) = match subsystem_consumers(probe, nqn).await {
                Ok(Some(t)) => t,
                Ok(None) => return Some(Verdict::AllowIdempotentNoop),
                Err(e) if probe_error_is_missing(&e) => return Some(Verdict::AllowIdempotentNoop),
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: consumer probe inconclusive for {}: {} — failing closed",
                        nqn, e
                    )))
                }
            };
            let (va_owner, va_errored) = match &ctx.va {
                Some((owner, errored)) => (owner.as_deref(), *errored),
                None => (None, false),
            };
            Some(subsystem_delete_verdict(
                &live,
                &allowed,
                any_host,
                ctx.own_host_nqn,
                va_owner,
                va_errored,
                ctx.self_node,
            ))
        }
        m if m == RPC_BDEV_LVOL_DELETE => {
            let name = params.get("name")?.as_str()?;
            if is_operation_scoped(name) {
                return None;
            }
            let forms = match bdev_identity_forms(probe, name).await {
                Ok(Some(f)) => f,
                Ok(None) => return Some(Verdict::AllowIdempotentNoop),
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: identity probe inconclusive for {}: {} — failing closed",
                        name, e
                    )))
                }
            };
            let raids = match raids_consuming(probe, &forms).await {
                Ok(r) => r,
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: raid-consumption probe inconclusive for {}: {}",
                        name, e
                    )))
                }
            };
            let exports = match exports_with_live_controllers(probe, &forms).await {
                Ok(x) => x,
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: export-consumption probe inconclusive for {}: {}",
                        name, e
                    )))
                }
            };
            Some(lvol_delete_verdict(&raids, &exports))
        }
        m if m == RPC_BDEV_RAID_DELETE => {
            let name = params.get("name")?.as_str()?;
            let forms = vec![name.to_string()];
            let present = match probe
                .rpc(&json!({ "method": "bdev_raid_get_bdevs", "params": { "category": "all" } }))
                .await
            {
                Ok(resp) => result_array(&resp)
                    .iter()
                    .any(|r| r.get("name").and_then(|n| n.as_str()) == Some(name)),
                Err(e) if probe_error_is_missing(&e.to_string()) => false,
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: raid presence probe inconclusive for {}: {}",
                        name, e
                    )))
                }
            };
            if !present {
                return Some(Verdict::AllowIdempotentNoop);
            }
            let ublk = match ublk_consumer_of(probe, &forms).await {
                Ok(u) => u,
                // No ublk support ⇒ no ublk consumer.
                Err(e) if e.contains("Method not found") => None,
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: ublk-consumption probe inconclusive for {}: {}",
                        name, e
                    )))
                }
            };
            let exports = match exports_with_live_controllers(probe, &forms).await {
                Ok(x) => x,
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: export-consumption probe inconclusive for {}: {}",
                        name, e
                    )))
                }
            };
            Some(raid_delete_verdict(true, ublk, &exports))
        }
        m if m == RPC_BDEV_NVME_DETACH_CONTROLLER => {
            let name = params.get("name")?.as_str()?;
            if is_operation_scoped(name) {
                return None;
            }
            let resp = match probe
                .rpc(&json!({ "method": "bdev_raid_get_bdevs", "params": { "category": "all" } }))
                .await
            {
                Ok(r) => r,
                Err(e) if probe_error_is_missing(&e.to_string()) => return Some(Verdict::Allow),
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_destroy: raid-base probe inconclusive for {}: {}",
                        name, e
                    )))
                }
            };
            let mut bases = Vec::new();
            for raid in result_array(&resp) {
                if let Some(bs) = raid.get("base_bdevs_list").and_then(|b| b.as_array()) {
                    for b in bs {
                        if let Some(n) = b.get("name").and_then(|v| v.as_str()) {
                            bases.push(n.to_string());
                        }
                    }
                }
            }
            Some(detach_controller_verdict(name, &bases))
        }
        // ublk_stop_disk is guarded at its call sites with the kernel opener
        // probe (F37) — the boundary cannot run kernel probes for a REMOTE
        // caller's request, but the request executes on THIS node, so the
        // route handler performs the same node-local probe before stopping.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "nqn.2024-11.com.flint:node:self-node";
    const OTHER: &str = "nqn.2024-11.com.flint:node:other-node";

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ---- the decision table: {live-self, live-other-admitted,
    // live-other-stale, configured-idle, missing, transport-error} ----

    #[test]
    fn live_self_host_is_an_absolute_veto() {
        let v = subsystem_delete_verdict(&s(&[OWN]), &s(&[OWN]), false, OWN, None, false, "self-node");
        assert!(matches!(v, Verdict::Refuse(_)));
        // Even when every other signal says delete (VA elsewhere, admitted
        // others) the self-host veto wins — the F38 destroyer held current
        // authority; no token overrides live self consumption.
        let v = subsystem_delete_verdict(
            &s(&[OWN, OTHER]),
            &s(&[OTHER]),
            false,
            OWN,
            Some("other-node"),
            false,
            "self-node",
        );
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("SELF-host")));
    }

    #[test]
    fn live_admitted_foreign_controller_refuses_the_f9_shape() {
        let v = subsystem_delete_verdict(&s(&[OTHER]), &s(&[OTHER]), false, OWN, None, false, "self-node");
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("ADMITTED foreign")));
        // allow_any_host counts as admitted (fencing disabled has no live
        // rightfulness signal — refuse destructive automation).
        let v = subsystem_delete_verdict(&s(&[OTHER]), &[], true, OWN, None, false, "self-node");
        assert!(matches!(v, Verdict::Refuse(_)));
    }

    #[test]
    fn live_unadmitted_foreign_controller_is_the_runf_zombie_and_does_not_block() {
        // Fenced-out prior consumer's controller persists live — the runf
        // eviction shape. It is NOT admitted, so it must not block.
        let v = subsystem_delete_verdict(&s(&[OTHER]), &s(&[OWN]), false, OWN, None, false, "self-node");
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn configured_idle_consumer_blocks_despite_zero_live_controllers() {
        // C3: kernel initiators reconnect for up to ctrl_loss_tmo — zero
        // live controllers is not absence when the VA names another owner.
        let v = subsystem_delete_verdict(&[], &s(&[OTHER]), false, OWN, Some("other-node"), false, "self-node");
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("VolumeAttachment")));
        // Own VA ownership allows.
        let v = subsystem_delete_verdict(&[], &s(&[OWN]), false, OWN, Some("self-node"), false, "self-node");
        assert_eq!(v, Verdict::Allow);
        // Unattached allows.
        let v = subsystem_delete_verdict(&[], &[], false, OWN, None, false, "self-node");
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn va_lookup_error_defers_instead_of_failing_open() {
        // The delete_phantom_raid_local bug class: an API error must not
        // read as "unattached".
        let v = subsystem_delete_verdict(&[], &[], false, OWN, None, true, "self-node");
        assert!(matches!(v, Verdict::Defer(_)));
    }

    #[test]
    fn lvol_delete_refuses_raid_base_and_live_export() {
        let v = lvol_delete_verdict(&s(&["raid_vol1"]), &[]);
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("raid_vol1")));
        let v = lvol_delete_verdict(&[], &s(&["nqn.2024-11.com.flint:volume:vol1"]));
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("guard-a")));
        assert_eq!(lvol_delete_verdict(&[], &[]), Verdict::Allow);
    }

    #[test]
    fn raid_delete_refuses_frontend_consumers_but_allows_zombies() {
        // D2: ublk or live-export frontend blocks.
        let v = raid_delete_verdict(true, Some(3), &[]);
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("ublk disk 3")));
        let v = raid_delete_verdict(true, None, &s(&["nqn.x"]));
        assert!(matches!(v, Verdict::Refuse(_)));
        // An ONLINE raid with no frontend is the anti-zombie / phantom
        // hygiene case — legitimate to delete.
        assert_eq!(raid_delete_verdict(true, None, &[]), Verdict::Allow);
        assert_eq!(raid_delete_verdict(false, None, &[]), Verdict::AllowIdempotentNoop);
    }

    #[test]
    fn detach_refuses_live_raid_base() {
        let bases = s(&["nvme_remote_leg_1n1", "uuid-x"]);
        let v = detach_controller_verdict("nvme_remote_leg_1", &bases);
        assert!(matches!(v, Verdict::Refuse(_)));
        assert_eq!(detach_controller_verdict("nvme_copy_source", &bases), Verdict::Allow);
    }

    #[test]
    fn error_classing_missing_vs_transport() {
        for m in [
            "SPDK RPC error: Code=-19 Msg=No such device",
            "subsystem does not exist",
            "Lvol x not found in SPDK",
            "No such file or directory",
        ] {
            assert!(probe_error_is_missing(m), "{m}");
        }
        for m in [
            "SPDK RPC 'nvmf_get_subsystems' timed out after 30s (socket /var/tmp/spdk.sock)",
            "Failed to connect to SPDK socket: Connection refused",
            "Node agent HTTP call failed: 500",
        ] {
            assert!(!probe_error_is_missing(m), "{m}");
        }
    }

    #[test]
    fn operation_scoped_shapes_are_exempt() {
        assert!(is_operation_scoped("nqn.2024-11.com.flint:hotrejoin:pvc-1"));
        assert!(is_operation_scoped("vol1_hrpad2"));
        assert!(is_operation_scoped("vol1_replica_2_hr"));
        assert!(!is_operation_scoped("nqn.2024-11.com.flint:volume:pvc-1"));
        assert!(!is_operation_scoped("pvc-abc123"));
        assert!(!is_operation_scoped("vol1_replica_2"));
    }

    #[test]
    fn refusal_texts_never_match_benign_classifiers() {
        // A Refuse that reads as "does not exist" would be swallowed as an
        // idempotent no-op by half the call sites.
        let refusals = [
            subsystem_delete_verdict(&s(&[OWN]), &[], false, OWN, None, false, "n"),
            subsystem_delete_verdict(&s(&[OTHER]), &s(&[OTHER]), false, OWN, None, false, "n"),
            lvol_delete_verdict(&s(&["r"]), &[]),
            raid_delete_verdict(true, Some(1), &[]),
            detach_controller_verdict("c", &s(&["cn1"])),
        ];
        for v in refusals {
            let msg = v.blocked().expect("is a refusal").to_string();
            assert!(!probe_error_is_missing(&msg), "refusal reads as benign: {msg}");
            assert!(!msg.contains("already exists"), "reads as EEXIST: {msg}");
        }
    }

    // ---- boundary probe plumbing over a canned SPDK ----

    fn canned(responses: Value) -> impl Fn(Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = SpdkResult> + Send>> {
        move |req: Value| {
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
            let resp = responses.get(&method).cloned();
            Box::pin(async move {
                match resp {
                    Some(r) => Ok(r),
                    None => Err(format!("SPDK RPC error: Code=-32601 Msg=Method not found: {method}").into()),
                }
            })
        }
    }

    #[tokio::test]
    async fn boundary_blocks_lvol_delete_of_raid_base_and_allows_snapshot() {
        let probe = canned(json!({
            "bdev_get_bdevs": { "result": [
                { "name": "lvs/leg1", "uuid": "uuid-leg1", "aliases": ["lvs/leg1"] }
            ]},
            "bdev_raid_get_bdevs": { "result": [
                { "name": "raid_vol1", "state": "online",
                  "base_bdevs_list": [ { "name": "uuid-leg1" }, { "name": "nvme_xn1" } ] }
            ]},
            "nvmf_get_subsystems": { "result": [] }
        }));
        let ctx = BoundaryContext { own_host_nqn: OWN, self_node: "self-node", va: None };
        let v = boundary_verdict(&probe, RPC_BDEV_LVOL_DELETE, &json!({ "name": "lvs/leg1" }), &ctx)
            .await
            .unwrap();
        assert!(matches!(v, Verdict::Refuse(_)));

        // A snapshot (not a raid base, not exported) passes — the
        // snapshot-class exemption needs no special case.
        let probe = canned(json!({
            "bdev_get_bdevs": { "result": [ { "name": "lvs/vol1_e42", "uuid": "uuid-snap" } ] },
            "bdev_raid_get_bdevs": { "result": [] },
            "nvmf_get_subsystems": { "result": [] }
        }));
        let v = boundary_verdict(&probe, RPC_BDEV_LVOL_DELETE, &json!({ "name": "lvs/vol1_e42" }), &ctx)
            .await
            .unwrap();
        assert_eq!(v, Verdict::Allow);
    }

    #[tokio::test]
    async fn boundary_missing_target_is_idempotent_noop_and_hotrejoin_is_exempt() {
        let probe = canned(json!({
            "bdev_get_bdevs": { "result": [] },
            "nvmf_get_subsystems": { "result": [] }
        }));
        let ctx = BoundaryContext { own_host_nqn: OWN, self_node: "self-node", va: None };
        let v = boundary_verdict(&probe, RPC_BDEV_LVOL_DELETE, &json!({ "name": "gone" }), &ctx)
            .await
            .unwrap();
        assert_eq!(v, Verdict::AllowIdempotentNoop);
        let v = boundary_verdict(
            &probe,
            RPC_NVMF_DELETE_SUBSYSTEM,
            &json!({ "nqn": "nqn.2024-11.com.flint:volume:gone" }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v, Verdict::AllowIdempotentNoop);
        // Operation-scoped: out of boundary scope entirely.
        assert!(boundary_verdict(
            &probe,
            RPC_NVMF_DELETE_SUBSYSTEM,
            &json!({ "nqn": "nqn.2024-11.com.flint:hotrejoin:pvc-1" }),
            &ctx,
        )
        .await
        .is_none());
    }

    /// Contract R3 lint (identity.rs Phase-4 pattern): a destructive RPC
    /// method literal outside this module is a new unguarded destruction
    /// surface. Allowed files, each with a documented reason:
    ///   - spdk_native.rs — the raw transport layer (guarding happens above
    ///     it; its typed wrappers are not new call sites);
    ///   - remote senders whose requests EXECUTE through the node agent's
    ///     boundary-guarded /api/spdk/rpc (catchup, epoch_scheduler,
    ///     hot_rejoin, replica_replace, driver, dashboard, raid_service via
    ///     injected HTTP transport);
    ///   - snapshot-class deletes (snapshot/*) — SPDK clone-pinning EPERM is
    ///     the intrinsic guard; snapshots have no controllers to probe.
    /// The two LOCAL executors — node_agent.rs and minimal_disk_service.rs —
    /// are deliberately NOT allowed: every local destructive call must go
    /// through this module's verdicts. Deliberate exceptions carry a
    /// `guarded-destroy-lint: allow` comment on the same line.
    #[test]
    fn no_destructive_rpc_literals_outside_the_chokepoint() {
        let allowed_files = [
            "guarded_destroy.rs",
            "spdk_native.rs",
            "catchup.rs",
            "epoch_scheduler.rs",
            "hot_rejoin.rs",
            "replica_replace.rs",
            "driver.rs",
            "spdk_dashboard_backend_minimal.rs",
            "raid_service.rs",
            "multi_replica.rs",
            "snapshot_service.rs",
            "controller_operator.rs", // dead code (bin commented out of Cargo.toml)
        ];
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        walk(&src_dir, &mut files);
        assert!(files.len() > 20, "source walk looks broken: {} files", files.len());

        let mut violations = Vec::new();
        for f in files {
            let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if allowed_files.contains(&name) {
                continue;
            }
            let text = std::fs::read_to_string(&f).unwrap();
            let prod = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text[..],
            };
            for (lineno, line) in prod.lines().enumerate() {
                if line.contains("guarded-destroy-lint: allow") {
                    continue;
                }
                for m in GUARDED_METHODS {
                    if line.contains(&format!("\"{}\"", m)) {
                        violations.push(format!("{}:{}: {}", f.display(), lineno + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "destructive RPC literal(s) outside guarded_destroy — route through the chokepoint \
             (or document a `guarded-destroy-lint: allow`):\n{}",
            violations.join("\n")
        );
    }

    #[tokio::test]
    async fn boundary_defers_on_transport_error_never_fails_open() {
        let probe = |_req: Value| {
            Box::pin(async move {
                Err::<Value, Box<dyn std::error::Error + Send + Sync>>(
                    "SPDK RPC 'bdev_get_bdevs' timed out after 30s (socket /var/tmp/spdk.sock)".into(),
                )
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = SpdkResult> + Send>>
        };
        let ctx = BoundaryContext { own_host_nqn: OWN, self_node: "self-node", va: None };
        let v = boundary_verdict(&probe, RPC_BDEV_LVOL_DELETE, &json!({ "name": "x" }), &ctx)
            .await
            .unwrap();
        assert!(matches!(v, Verdict::Defer(_)));
        let v = boundary_verdict(&probe, RPC_BDEV_RAID_DELETE, &json!({ "name": "raid_x" }), &ctx)
            .await
            .unwrap();
        assert!(matches!(v, Verdict::Defer(_)));
    }
}
