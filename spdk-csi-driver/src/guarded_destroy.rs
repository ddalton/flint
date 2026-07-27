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

// ---------------------------------------------------------------------------
// Contract R3's OTHER half: writer-admitting CONSTRUCTION (v1.20.0 items
// #7/#8, docs/f43-rwx-replacement-admission.md §6.4 + item #8).
//
// SPDK will not stop either hazard (verified at v26.05.1-pre source):
//   - ublk never claims the bdev it serves (lib/ublk/ublk.c write-open, no
//     claim call in the module), and both raid base-add and nvmf add-ns use
//     the legacy v1 claim whose only check is claim_type != NONE — so
//     constructing a raid or namespace over a ublk-served bdev silently
//     yields TWO LIVE WRITERS (#7);
//   - a fresh raid1 create takes min() over base sizes with no error path
//     (raid1_start assigns blockcnt directly, pre-registration, so the
//     bdev-layer shrink guard is structurally unreachable) — a stale
//     short leg at reassembly is a SILENT SHRINK under an already-grown
//     filesystem (#8).
//
// Both guards run here, at the same boundary that guards destruction: the
// probes execute on the node where the RPC lands, which is exactly the
// spdk-tgt whose ublk table and bdev sizes matter. The node agent's own
// LOCAL add_ns path (nvmeof_export::ensure_export over the unix socket)
// bypasses this boundary and carries its own pre-add probe.
//
// Kill switch: FLINT_CONSTRUCTION_GUARD=disabled (new refusals on the
// staging path get a standing off-switch, the FLINT_VOLUME_LOCK pattern).
// ---------------------------------------------------------------------------
pub const RPC_BDEV_RAID_CREATE: &str = "bdev_raid_create";
pub const RPC_BDEV_RAID_ADD_BASE_BDEV: &str = "bdev_raid_add_base_bdev";
pub const RPC_NVMF_SUBSYSTEM_ADD_NS: &str = "nvmf_subsystem_add_ns";

/// Construction methods the /api/spdk/rpc boundary intercepts.
pub const CONSTRUCTION_GUARDED_METHODS: &[&str] = &[
    RPC_BDEV_RAID_CREATE,
    RPC_BDEV_RAID_ADD_BASE_BDEV,
    RPC_NVMF_SUBSYSTEM_ADD_NS,
];

pub fn construction_guard_enabled() -> bool {
    !std::env::var("FLINT_CONSTRUCTION_GUARD").is_ok_and(|v| v.eq_ignore_ascii_case("disabled"))
}

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

/// Identity forms of a bdev PLUS its byte size (`num_blocks * block_size`)
/// from the same single `bdev_get_bdevs` row — one probe pass serves both
/// the ublk-consumer match (#7, needs the forms) and the leg-size guard
/// (#8, needs BYTES: block_size is auto-detected per lvstore, so
/// num_blocks alone is not comparable across legs). Ok(None) = bdev absent
/// (let the construction call fail downstream); size None = row present
/// but unsized (never seen live; treated as unknown, not as mismatch).
pub async fn bdev_identity_and_bytes(
    probe: &dyn SpdkProbe,
    name: &str,
) -> Result<Option<(Vec<String>, Option<u64>)>, String> {
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
            let bytes = b
                .get("num_blocks")
                .and_then(|v| v.as_u64())
                .zip(b.get("block_size").and_then(|v| v.as_u64()))
                .map(|(nb, bs)| nb.saturating_mul(bs));
            Ok(Some((forms, bytes)))
        }
        Err(e) if probe_error_is_missing(&e.to_string()) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// #7 companion, the pre-assembly CONVERGER's decision core (review
/// finding 2026-07-26): a stale direct-serve disk of the volume's OWN
/// previous stage attempt over a base leg would 409 the raid create at the
/// boundary with no healer — nothing else stops it (the rehydrate reaper
/// deliberately skips multi-replica PVs; the F37 stranger-reap only fires
/// for same-bdev strangers). Before the create, the driver stops disks
/// that are (a) ATTRIBUTABLE to this volume — id ∈ its expected set (the
/// `flint.io/ublk-id` annotation + the hash fallback) — and (b) NOT the
/// volume's legitimate end-state disk (bdev_name == the raid itself, which
/// a restage-reuse must keep). The stop routes through the node agent's
/// guarded ublk_stop_disk, whose F37 kernel-opener probe refuses a disk
/// with live openers — a genuinely-consumed disk keeps its veto and the
/// stage then fails loudly instead of corrupting. Foreign-id disks are
/// never touched: the boundary refusal (with its named reason) is the
/// correct outcome for those.
pub fn stale_own_ublk_disks(
    disks: &[Value],
    expected_ids: &[u32],
    raid_name: &str,
) -> Vec<(u64, String)> {
    disks
        .iter()
        .filter_map(|d| {
            let id = d.get("id").or_else(|| d.get("ublk_id")).and_then(|i| i.as_u64())?;
            let bdev = d.get("bdev_name").and_then(|b| b.as_str())?;
            let ours = expected_ids.iter().any(|e| u64::from(*e) == id);
            if ours && bdev != raid_name {
                Some((id, bdev.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// #7 pure core: a live ublk disk over the construction target is
/// disqualifying in EVERY flow — no flint flow deliberately builds a second
/// writer over a bdev this node is serving; a match means a stale/rogue
/// disk survived into reconstruction (the phantom/re-mint family).
pub fn construction_over_ublk_verdict(
    method: &str,
    bdev: &str,
    ublk_consumer: Option<u64>,
) -> Verdict {
    match ublk_consumer {
        Some(id) => Verdict::Refuse(format!(
            "guarded_construct: refusing {} over {} — ublk disk {} is live on this bdev right \
             now; a second writer would corrupt silently (F43 doc §6.4). Stop the stale ublk \
             disk first",
            method, bdev, id
        )),
        None => Verdict::Allow,
    }
}

/// #8 pure core, create path: every sized base of a raid1 create must agree
/// in BYTES. SPDK constructs the raid at min(size) with zero errors — under
/// a filesystem grown to max(size) that is silent corruption, so a
/// mismatched set is refused outright here (the driver's assembly belt
/// drops mismatched legs GRACEFULLY before ever issuing the create; a
/// refusal here means some path skipped that hygiene). Unknown sizes are
/// not treated as mismatches.
pub fn raid_create_size_verdict(sized_bases: &[(String, u64)]) -> Verdict {
    let mut distinct: Vec<u64> = sized_bases.iter().map(|(_, b)| *b).collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() <= 1 {
        return Verdict::Allow;
    }
    let detail: Vec<String> = sized_bases
        .iter()
        .map(|(n, b)| format!("{}={}B", n, b))
        .collect();
    Verdict::Refuse(format!(
        "guarded_construct: refusing bdev_raid_create — base bdevs disagree in size ({}); \
         SPDK would assemble at the minimum with no error, silently shrinking the device \
         under its filesystem (F43 doc item #8). Exclude or repair the divergent leg first",
        detail.join(", ")
    ))
}

/// #8 pure core, hot-add path: a SHORT leg is refused by SPDK anyway
/// (-EINVAL, opaque and indistinguishable from a parked standby) — this
/// upgrade makes it a NAMED refusal. A LONGER leg is allowed (SPDK caps at
/// the raid's data_size; the tail is waste, not risk).
pub fn raid_add_size_verdict(base: &str, base_bytes: u64, raid: &str, raid_bytes: u64) -> Verdict {
    if base_bytes < raid_bytes {
        return Verdict::Refuse(format!(
            "guarded_construct: refusing bdev_raid_add_base_bdev — leg {} is {}B but raid {} \
             is {}B; a short leg can never join and would park the standby with an opaque \
             EINVAL (F43 doc item #8). Rebuild the leg at full size first",
            base, base_bytes, raid, raid_bytes
        ));
    }
    Verdict::Allow
}

/// Guard one intercepted CONSTRUCTION RPC (contract R3's writer-admitting
/// half). Returns None when the method is out of scope or the guard is
/// disabled. Probe policy mirrors the destruction boundary: ublk
/// Method-not-found = no ublk support = no consumer (nvmeof-backend
/// clusters must not defer every stage); absent target bdevs fail the
/// construction downstream on their own; transport/unknown errors DEFER —
/// never fail open (F37).
///
/// Two independent gates, matching the two hazards' kill switches (both
/// read on the NODE AGENT process — the boundary runs there):
/// FLINT_CONSTRUCTION_GUARD governs the #7 ublk two-writer arms;
/// FLINT_LEG_SIZE_GUARD governs the #8 size arms — so disabling the size
/// guard stands down ALL its layers coherently (the driver belt would
/// stop excluding short legs; the boundary hard-refusing the resulting
/// mixed-size create would turn the documented escape hatch into a brick).
pub async fn construction_boundary_verdict(
    probe: &dyn SpdkProbe,
    method: &str,
    params: &Value,
) -> Option<Verdict> {
    construction_boundary_verdict_gated(
        probe,
        method,
        params,
        construction_guard_enabled(),
        crate::leg_size_guard::enabled(),
    )
    .await
}

/// The env-free core (tests inject the gates; production reads them once
/// in the wrapper above — no test ever mutates process env for these).
pub async fn construction_boundary_verdict_gated(
    probe: &dyn SpdkProbe,
    method: &str,
    params: &Value,
    construction_on: bool,
    size_on: bool,
) -> Option<Verdict> {
    if !construction_on {
        return None;
    }
    match method {
        m if m == RPC_BDEV_RAID_CREATE => {
            let bases: Vec<String> = params
                .get("base_bdevs")?
                .as_array()?
                .iter()
                .filter_map(|b| b.as_str().map(str::to_string))
                .collect();
            let mut sized: Vec<(String, u64)> = Vec::new();
            for base in &bases {
                let (forms, bytes) = match bdev_identity_and_bytes(probe, base).await {
                    Ok(Some(fb)) => fb,
                    Ok(None) => continue, // absent: the create fails downstream
                    Err(e) => {
                        return Some(Verdict::Defer(format!(
                            "guarded_construct: identity probe inconclusive for base {}: {} — \
                             failing closed",
                            base, e
                        )))
                    }
                };
                match ublk_consumer_of(probe, &forms).await {
                    Ok(Some(id)) => {
                        return Some(construction_over_ublk_verdict(m, base, Some(id)))
                    }
                    Ok(None) => {}
                    // No ublk support ⇒ no ublk consumer.
                    Err(e) if e.contains("Method not found") => {}
                    Err(e) => {
                        return Some(Verdict::Defer(format!(
                            "guarded_construct: ublk probe inconclusive for base {}: {} — \
                             failing closed",
                            base, e
                        )))
                    }
                }
                if let Some(b) = bytes {
                    sized.push((base.clone(), b));
                }
            }
            if !size_on {
                return Some(Verdict::Allow);
            }
            Some(raid_create_size_verdict(&sized))
        }
        m if m == RPC_BDEV_RAID_ADD_BASE_BDEV => {
            let base = params.get("base_bdev")?.as_str()?;
            let raid = params.get("raid_bdev")?.as_str()?;
            let (forms, base_bytes) = match bdev_identity_and_bytes(probe, base).await {
                Ok(Some(fb)) => fb,
                Ok(None) => return Some(Verdict::Allow), // absent: fails downstream
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_construct: identity probe inconclusive for base {}: {} — \
                         failing closed",
                        base, e
                    )))
                }
            };
            match ublk_consumer_of(probe, &forms).await {
                Ok(Some(id)) => return Some(construction_over_ublk_verdict(m, base, Some(id))),
                Ok(None) => {}
                Err(e) if e.contains("Method not found") => {}
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_construct: ublk probe inconclusive for base {}: {} — failing \
                         closed",
                        base, e
                    )))
                }
            }
            if !size_on {
                return Some(Verdict::Allow);
            }
            let raid_bytes = match bdev_identity_and_bytes(probe, raid).await {
                Ok(Some((_, b))) => b,
                Ok(None) => None, // raid absent: the add fails downstream
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_construct: raid size probe inconclusive for {}: {} — failing \
                         closed",
                        raid, e
                    )))
                }
            };
            match (base_bytes, raid_bytes) {
                (Some(b), Some(r)) => Some(raid_add_size_verdict(base, b, raid, r)),
                _ => Some(Verdict::Allow),
            }
        }
        m if m == RPC_NVMF_SUBSYSTEM_ADD_NS => {
            let bdev = params.get("namespace")?.get("bdev_name")?.as_str()?;
            let forms = match bdev_identity_and_bytes(probe, bdev).await {
                Ok(Some((f, _))) => f,
                Ok(None) => return Some(Verdict::Allow), // absent: fails downstream
                Err(e) => {
                    return Some(Verdict::Defer(format!(
                        "guarded_construct: identity probe inconclusive for {}: {} — failing \
                         closed",
                        bdev, e
                    )))
                }
            };
            match ublk_consumer_of(probe, &forms).await {
                Ok(Some(id)) => Some(construction_over_ublk_verdict(m, bdev, Some(id))),
                Ok(None) => Some(Verdict::Allow),
                Err(e) if e.contains("Method not found") => Some(Verdict::Allow),
                Err(e) => Some(Verdict::Defer(format!(
                    "guarded_construct: ublk probe inconclusive for {}: {} — failing closed",
                    bdev, e
                ))),
            }
        }
        _ => None,
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
        // idempotent no-op by half the call sites. Construction refusals
        // additionally must not read as EEXIST (ensure_raid1_bdev retries
        // "File exists"/Code=-17) or EBUSY (hot_rejoin's is_busy add-retry
        // loop would spin on them).
        let refusals = [
            subsystem_delete_verdict(&s(&[OWN]), &[], false, OWN, None, false, "n"),
            subsystem_delete_verdict(&s(&[OTHER]), &s(&[OTHER]), false, OWN, None, false, "n"),
            lvol_delete_verdict(&s(&["r"]), &[]),
            raid_delete_verdict(true, Some(1), &[]),
            detach_controller_verdict("c", &s(&["cn1"])),
            construction_over_ublk_verdict(RPC_BDEV_RAID_CREATE, "lvs/leg1", Some(3)),
            raid_create_size_verdict(&[("a".into(), 100), ("b".into(), 90)]),
            raid_add_size_verdict("lvs/leg1", 90, "raid_v", 100),
        ];
        for v in refusals {
            let msg = v.blocked().expect("is a refusal").to_string();
            assert!(!probe_error_is_missing(&msg), "refusal reads as benign: {msg}");
            assert!(!msg.contains("already exists"), "reads as EEXIST: {msg}");
            assert!(!msg.contains("File exists"), "reads as EEXIST: {msg}");
            assert!(!msg.contains("Code=-17"), "reads as EEXIST: {msg}");
            assert!(!msg.to_lowercase().contains("busy"), "reads as EBUSY: {msg}");
            assert!(!msg.contains("Code=-16"), "reads as EBUSY: {msg}");
        }
    }

    // ---- construction boundary (#7 ublk two-writer, #8 leg-size) ----

    #[test]
    fn converger_stops_only_own_stale_disks_never_the_raid_disk_or_foreign_ids() {
        let disks = vec![
            json!({ "id": 7, "bdev_name": "uuid-leg-a" }),   // ours, stale (leg)
            json!({ "id": 7, "bdev_name": "raid_vol1" }),    // ours, LEGIT end-state
            json!({ "id": 9, "bdev_name": "uuid-leg-b" }),   // foreign id — boundary's job
            json!({ "ublk_id": 12, "bdev_name": "lvs/x" }),  // ours via annotation, alt key
        ];
        let stale = stale_own_ublk_disks(&disks, &[7, 12], "raid_vol1");
        assert_eq!(
            stale,
            vec![(7, "uuid-leg-a".to_string()), (12, "lvs/x".to_string())],
            "stop exactly the volume's own non-raid disks"
        );
        assert!(stale_own_ublk_disks(&disks, &[], "raid_vol1").is_empty());
    }

    #[tokio::test]
    async fn construction_refuses_raid_create_over_ublk_served_base() {
        // The §6.4 hazard shape: a stale direct-serve ublk disk survived
        // into a 2-leg reassembly; SPDK would silently accept two writers.
        let probe = canned(json!({
            "bdev_get_bdevs": { "result": [
                { "name": "lvs/leg1", "uuid": "uuid-leg1", "aliases": ["lvs/leg1"],
                  "num_blocks": 262144, "block_size": 4096 }
            ]},
            "ublk_get_disks": { "result": [
                { "id": 7, "bdev_name": "uuid-leg1" }
            ]}
        }));
        let v = construction_boundary_verdict(
            &probe,
            RPC_BDEV_RAID_CREATE,
            &json!({ "name": "raid_v", "base_bdevs": ["lvs/leg1", "pvc-x_r2n1"] }),
        )
        .await
        .unwrap();
        // Matched via the uuid FORM, not the spelled name (F36 lesson).
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("ublk disk 7")));
    }

    #[tokio::test]
    async fn construction_refuses_add_ns_over_ublk_and_allows_clean_bdev() {
        let served = canned(json!({
            "bdev_get_bdevs": { "result": [
                { "name": "raid_v", "uuid": "uuid-raid", "num_blocks": 262144, "block_size": 4096 }
            ]},
            "ublk_get_disks": { "result": [ { "id": 2, "bdev_name": "raid_v" } ] }
        }));
        let v = construction_boundary_verdict(
            &served,
            RPC_NVMF_SUBSYSTEM_ADD_NS,
            &json!({ "nqn": "nqn.x", "namespace": { "bdev_name": "raid_v" } }),
        )
        .await
        .unwrap();
        assert!(matches!(v, Verdict::Refuse(_)));

        let clean = canned(json!({
            "bdev_get_bdevs": { "result": [
                { "name": "lvs/leg2", "uuid": "uuid-leg2", "num_blocks": 262144, "block_size": 4096 }
            ]},
            "ublk_get_disks": { "result": [ { "id": 2, "bdev_name": "something-else" } ] }
        }));
        let v = construction_boundary_verdict(
            &clean,
            RPC_NVMF_SUBSYSTEM_ADD_NS,
            &json!({ "nqn": "nqn.x", "namespace": { "bdev_name": "lvs/leg2" } }),
        )
        .await
        .unwrap();
        assert_eq!(v, Verdict::Allow);
    }

    #[tokio::test]
    async fn construction_no_ublk_support_is_not_a_consumer() {
        // nvmeof-backend clusters run spdk-tgt without the ublk target:
        // ublk_get_disks = Method not found. Deferring here would brick
        // every NodeStage on those clusters.
        let probe = canned(json!({
            "bdev_get_bdevs": { "result": [
                { "name": "lvs/leg1", "uuid": "u1", "num_blocks": 262144, "block_size": 4096 }
            ]}
            // no ublk_get_disks arm → canned returns Method not found
        }));
        let v = construction_boundary_verdict(
            &probe,
            RPC_BDEV_RAID_CREATE,
            &json!({ "name": "raid_v", "base_bdevs": ["lvs/leg1"] }),
        )
        .await
        .unwrap();
        assert_eq!(v, Verdict::Allow);
    }

    #[tokio::test]
    async fn construction_refuses_size_mismatched_create_and_short_hot_add() {
        // #8 backstop: the C2-B silent shrink — one stale old-size leg in a
        // fresh create. SPDK would assemble at min() with zero errors.
        // canned() keys by method only, so use a per-name closure.
        let by_name = |req: Value| {
            Box::pin(async move {
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                match method {
                    "ublk_get_disks" => Ok(json!({ "result": [] })),
                    "bdev_get_bdevs" => {
                        let name =
                            req["params"]["name"].as_str().unwrap_or("").to_string();
                        let (blocks, bs) = match name.as_str() {
                            "lvs/grown" => (524288u64, 4096u64),
                            "raid_v" => (524288, 4096),
                            _ => (262144, 4096), // the stale short leg
                        };
                        Ok(json!({ "result": [
                            { "name": name, "uuid": format!("uuid-{}", name),
                              "num_blocks": blocks, "block_size": bs }
                        ]}))
                    }
                    _ => Err(format!("Method not found: {method}").into()),
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = SpdkResult> + Send>>
        };
        let v = construction_boundary_verdict(
            &by_name,
            RPC_BDEV_RAID_CREATE,
            &json!({ "name": "raid_v", "base_bdevs": ["lvs/grown", "lvs/stale"] }),
        )
        .await
        .unwrap();
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("disagree in size")));

        // Short leg hot-add: named refusal instead of SPDK's opaque EINVAL.
        let v = construction_boundary_verdict(
            &by_name,
            RPC_BDEV_RAID_ADD_BASE_BDEV,
            &json!({ "raid_bdev": "raid_v", "base_bdev": "lvs/stale", "skip_rebuild": true }),
        )
        .await
        .unwrap();
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("short leg")));

        // Equal-size add passes both hazards.
        let v = construction_boundary_verdict(
            &by_name,
            RPC_BDEV_RAID_ADD_BASE_BDEV,
            &json!({ "raid_bdev": "raid_v", "base_bdev": "lvs/grown", "skip_rebuild": true }),
        )
        .await
        .unwrap();
        assert_eq!(v, Verdict::Allow);
    }

    #[tokio::test]
    async fn construction_defers_on_transport_error_never_fails_open() {
        let probe = |_req: Value| {
            Box::pin(async move {
                Err::<Value, Box<dyn std::error::Error + Send + Sync>>(
                    "SPDK RPC 'bdev_get_bdevs' timed out after 30s (socket /var/tmp/spdk.sock)"
                        .into(),
                )
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = SpdkResult> + Send>>
        };
        let v = construction_boundary_verdict(
            &probe,
            RPC_BDEV_RAID_CREATE,
            &json!({ "name": "raid_v", "base_bdevs": ["lvs/leg1"] }),
        )
        .await
        .unwrap();
        assert!(matches!(v, Verdict::Defer(_)));
    }

    #[tokio::test]
    async fn construction_kill_switch_stands_down() {
        // Env-free: the gates are injected (review finding 2026-07-26 —
        // mutating process env raced env readers in other test modules).
        let probe = canned(json!({}));
        let v = construction_boundary_verdict_gated(
            &probe,
            RPC_BDEV_RAID_CREATE,
            &json!({ "name": "raid_v", "base_bdevs": ["lvs/leg1"] }),
            false,
            true,
        )
        .await;
        assert!(v.is_none(), "disabled construction guard must not intercept");
    }

    /// FLINT_LEG_SIZE_GUARD=disabled must stand down the boundary's SIZE
    /// arms too (review finding 2026-07-26: with only the driver belt
    /// gated, the documented escape hatch let the un-filtered mixed-size
    /// base list reach a boundary that still hard-refused it — disabling
    /// the guard made staging strictly WORSE). The #7 ublk arms stay up.
    #[tokio::test]
    async fn size_kill_switch_stands_down_boundary_size_arms_but_not_ublk() {
        let by_name = |req: Value| {
            Box::pin(async move {
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                match method {
                    "ublk_get_disks" => Ok(json!({ "result": [ { "id": 9, "bdev_name": "lvs/served" } ] })),
                    "bdev_get_bdevs" => {
                        let name = req["params"]["name"].as_str().unwrap_or("").to_string();
                        let blocks = if name == "lvs/grown" || name == "raid_v" { 524288u64 } else { 262144 };
                        Ok(json!({ "result": [
                            { "name": name, "uuid": format!("uuid-{}", name),
                              "num_blocks": blocks, "block_size": 4096 }
                        ]}))
                    }
                    _ => Err(format!("Method not found: {method}").into()),
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = SpdkResult> + Send>>
        };
        // Mismatched create passes with the size guard off...
        let v = construction_boundary_verdict_gated(
            &by_name,
            RPC_BDEV_RAID_CREATE,
            &json!({ "name": "raid_v", "base_bdevs": ["lvs/grown", "lvs/stale"] }),
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(v, Verdict::Allow, "size arms must stand down with the guard");
        // ...and so does a short hot-add...
        let v = construction_boundary_verdict_gated(
            &by_name,
            RPC_BDEV_RAID_ADD_BASE_BDEV,
            &json!({ "raid_bdev": "raid_v", "base_bdev": "lvs/stale", "skip_rebuild": true }),
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(v, Verdict::Allow);
        // ...but the #7 two-writer refusal still fires.
        let v = construction_boundary_verdict_gated(
            &by_name,
            RPC_BDEV_RAID_CREATE,
            &json!({ "name": "raid_v", "base_bdevs": ["lvs/served"] }),
            true,
            false,
        )
        .await
        .unwrap();
        assert!(matches!(v, Verdict::Refuse(ref r) if r.contains("ublk disk 9")));
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

    /// Contract R3, construction half: a writer-admitting RPC literal
    /// outside the sanctioned files is a new unguarded construction
    /// surface. Sanctioned:
    ///   - guarded_destroy.rs (the constants + boundary live here);
    ///   - spdk_native.rs — raw transport;
    ///   - remote senders whose requests EXECUTE through the node agent's
    ///     boundary (driver.rs ensure_raid1_bdev + setup paths, hot_rejoin
    ///     window RPCs, raid_service via injected HTTP transport);
    ///   - nvmeof_export.rs — the ONE local executor, self-guarded: its
    ///     ensure_export probes ublk before the add_ns it issues;
    ///   - freshness_gate.rs — an error CLASSIFIER quotes the method name,
    ///     no RPC is issued;
    ///   - controller_operator.rs — dead code (bin commented out).
    #[test]
    fn no_construction_rpc_literals_outside_the_chokepoint() {
        let allowed_files = [
            "guarded_destroy.rs",
            "spdk_native.rs",
            "driver.rs",
            "hot_rejoin.rs",
            "nvmeof_export.rs",
            "freshness_gate.rs",
            "raid_service.rs",
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
                if line.contains("guarded-construct-lint: allow") {
                    continue;
                }
                for m in CONSTRUCTION_GUARDED_METHODS {
                    if line.contains(&format!("\"{}\"", m)) {
                        violations.push(format!("{}:{}: {}", f.display(), lineno + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "construction RPC literal(s) outside the chokepoint — route through the node \
             agent boundary or self-guard like ensure_export (or document a \
             `guarded-construct-lint: allow`):\n{}",
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
