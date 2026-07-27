//! Convergent NVMe-oF export helpers (phase 0 fix).
//!
//! Every export site used to issue `nvmf_create_subsystem` /
//! `nvmf_subsystem_add_ns` / `nvmf_subsystem_add_listener` blindly and only
//! tolerated duplicates by matching an "already exists" error string that
//! SPDK never emits (duplicates return `-32602 Invalid parameters`). Any
//! partially-created export therefore poisoned every subsequent attempt and
//! NodeStage retry loops could never converge
//! (docs/phase0-hazard-repro-2026-06-10.md, bugs 2-3).
//!
//! This module replaces that with check-then-act against the live subsystem
//! state: each step inspects what exists and only issues the mutating RPC
//! when needed. On a mutate failure the state is re-read once so a concurrent
//! creator counts as success.

use async_trait::async_trait;
use serde_json::{json, Value};

pub type RpcError = Box<dyn std::error::Error + Send + Sync>;

/// Transport over which SPDK JSON-RPCs are issued; implemented for the local
/// unix-socket path (node agent) and the node-agent HTTP proxy (driver).
#[async_trait]
pub trait SpdkRpcTransport: Sync {
    async fn rpc(&self, payload: &Value) -> Result<Value, RpcError>;
}

/// Host NQN a Flint node uses for every NVMe-oF initiator connection
/// (SPDK `bdev_nvme_attach_controller hostnqn` and kernel `nvme connect -q`).
/// Predictable per-node identity is what makes host fencing possible — the
/// default initiator NQNs are random per boot/controller.
pub fn flint_host_nqn(node_name: &str) -> String {
    crate::identity::node_host_nqn(node_name)
}

/// Prefix identifying host NQNs managed by Flint. Fencing only ever removes
/// hosts under this prefix, so admin-added host entries are left alone.
pub const FLINT_HOST_NQN_PREFIX: &str = "nqn.2024-11.com.flint:node:";

/// Whether NVMe-oF host fencing is enabled (default on). Set
/// FLINT_NVMF_FENCING=disabled for mixed-version clusters during upgrade —
/// old drivers connect with random host NQNs that a fenced subsystem rejects.
pub fn fencing_enabled() -> bool {
    !std::env::var("FLINT_NVMF_FENCING")
        .map(|v| v.eq_ignore_ascii_case("disabled") || v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// F47 (§4b): true when every listener of a live subsystem is a loopback
/// address — such a subsystem has exactly ONE possible consumer, the node
/// it lives on, so that node's own unstage may delete it regardless of
/// cross-node VolumeAttachment ownership (the F9 guard's reasoning exists
/// for remote-consumable exports). A subsystem with NO listeners is
/// unreachable by anyone and counts as local-only too: deletion cannot
/// sever a consumer that cannot connect. Listener entries may carry the
/// address flat or nested under "address" (SPDK version differences —
/// same tolerance as the loss-detector's seed pass).
pub fn subsystem_is_local_only(subsystem: &serde_json::Value) -> bool {
    let Some(listeners) = subsystem.get("listen_addresses").and_then(|l| l.as_array()) else {
        return true;
    };
    listeners.iter().all(|l| {
        let addr = l.get("address").unwrap_or(l);
        matches!(
            addr.get("traddr").and_then(|t| t.as_str()),
            Some("127.0.0.1") | Some("::1")
        )
    })
}

/// Desired state of one replica/volume export.
pub struct ExportSpec<'a> {
    pub nqn: &'a str,
    pub bdev_name: &'a str,
    /// Alternate names the namespace may already be registered under
    /// (e.g. lvol alias vs uuid). Any match counts as "already exported".
    pub bdev_aliases: &'a [&'a str],
    pub trtype: &'a str,
    pub traddr: &'a str,
    pub trsvcid: u16,
    /// Host NQNs allowed to connect (fencing, doc §3). Semantics:
    /// - `None`: legacy wide-open export (`allow_any_host: true`).
    /// - `Some(list)`: default-closed; exactly these Flint hosts are
    ///   admitted. Flint-managed hosts not in the list are removed (the
    ///   fence flip on restage); non-Flint host entries are preserved.
    ///   `Some(&[])` means nobody may connect (unattached volume).
    pub allowed_hosts: Option<&'a [String]>,
    /// Deterministic namespace identity `(uuid, nguid)`, for exports a
    /// KERNEL initiator consumes (the loopback raid export): the kernel
    /// verifies namespace identity on reconnect, and a rebuilt raid bdev
    /// gets a fresh UUID — without pinning, an in-place repair (phase-6
    /// layer 2) presents a "different" namespace and the initiator
    /// refuses to reattach. `None` = SPDK default (bdev UUID), correct
    /// for replica exports whose backing lvol identity is stable.
    pub ns_identity: Option<(&'a str, &'a str)>,
}

/// Deterministic (UUID, NGUID) for a volume's kernel-facing namespace,
/// stable across raid rebuilds and spdk-tgt restarts. UUID is RFC4122-
/// shaped; NGUID is the same 16 bytes as 32 hex chars.
pub fn stable_ns_identity(volume_id: &str) -> (String, String) {
    use std::hash::{Hash, Hasher};
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    ("flint-ns-id-a", volume_id).hash(&mut h1);
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    ("flint-ns-id-b", volume_id).hash(&mut h2);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&h1.finish().to_be_bytes());
    bytes[8..].copy_from_slice(&h2.finish().to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC4122 variant
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    (uuid, hex)
}

/// Fetch the subsystem record for `nqn`, or None if it does not exist.
/// SPDK returns `-19 No such device` for a missing nqn; that is not an error
/// here.
pub async fn get_subsystem(
    rpc: &dyn SpdkRpcTransport,
    nqn: &str,
) -> Result<Option<Value>, RpcError> {
    let payload = json!({
        "method": "nvmf_get_subsystems",
        "params": { "nqn": nqn }
    });
    match rpc.rpc(&payload).await {
        Ok(response) => {
            let sub = response
                .get("result")
                .and_then(|r| r.as_array())
                .and_then(|subs| subs.first())
                .cloned();
            Ok(sub)
        }
        // Missing subsystem surfaces as an RPC error (-19); treat any lookup
        // failure as "absent" — the subsequent create will surface real
        // transport problems.
        Err(_) => Ok(None),
    }
}

fn ns_matches(ns: &Value, spec: &ExportSpec<'_>) -> bool {
    ns_matches_names(ns, spec.bdev_name, spec.bdev_aliases)
}

fn ns_matches_names(ns: &Value, bdev_name: &str, aliases: &[&str]) -> bool {
    let name = ns.get("bdev_name").and_then(|b| b.as_str()).unwrap_or("");
    let uuid = ns.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
    name == bdev_name || uuid == bdev_name || aliases.iter().any(|a| *a == name || *a == uuid)
}

fn subsystem_holds_ns(sub: &Value, bdev_name: &str, aliases: &[&str]) -> bool {
    sub.get("namespaces")
        .and_then(|n| n.as_array())
        .map(|nss| nss.iter().any(|ns| ns_matches_names(ns, bdev_name, aliases)))
        .unwrap_or(false)
}

fn subsystem_ns_empty(sub: &Value) -> bool {
    sub.get("namespaces").and_then(|n| n.as_array()).map(|n| n.is_empty()).unwrap_or(true)
}

/// F46/F45-S3 transition belt: which NQN should this node's export of a
/// replica leg SERVE under right now?
///
/// The canonical shape is the inner-domain [`identity::replica_export_nqn`].
/// But a node running pre-unification code may already export the leg under
/// the legacy wrapper shape with a live consumer attached through it —
/// re-minting canonically then fails claim-shaped (`nvmf_subsystem_add_ns
/// -32602`, the F46 wedge in mirror image), and migrating the namespace
/// would sever that consumer mid-I/O. So: serve whichever subsystem already
/// holds the leg's namespace, mint canonically when neither does, and retire
/// an EMPTY legacy shell when found (the F46 fingerprint: a wrapper
/// subsystem whose namespace was long since claimed by its inner sibling —
/// left standing, it convinces name-keyed probes the head is "exported to
/// somebody else" and wedges assembly forever).
pub async fn resolve_replica_export_nqn(
    rpc: &dyn SpdkRpcTransport,
    volume_id: &str,
    replica_index: usize,
    bdev_name: &str,
    bdev_aliases: &[&str],
) -> Result<String, RpcError> {
    let canonical = crate::identity::replica_export_nqn(volume_id, replica_index);
    let legacy = crate::identity::legacy_replica_export_nqn(volume_id, replica_index);

    if let Some(sub) = get_subsystem(rpc, &canonical).await? {
        if subsystem_holds_ns(&sub, bdev_name, bdev_aliases) {
            retire_empty_shell(rpc, &legacy).await;
            return Ok(canonical);
        }
    }
    if let Some(sub) = get_subsystem(rpc, &legacy).await? {
        if subsystem_holds_ns(&sub, bdev_name, bdev_aliases) {
            tracing::info!(
                nqn = %legacy,
                "adopting legacy wrapper-domain leg export — a live consumer may be \
                 attached through it; canonical takes over on the next fresh mint"
            );
            return Ok(legacy);
        }
        if subsystem_ns_empty(&sub) {
            retire_empty_shell(rpc, &legacy).await;
        }
        // A legacy subsystem holding a DIFFERENT namespace is not ours to
        // touch — fall through and let ensure_export converge canonically.
    }
    Ok(canonical)
}

/// Best-effort delete of an empty leg-export shell. Empty = no namespace =
/// nothing any initiator can do I/O through, so deletion cannot sever a
/// data path; a connected controller (if any) was already dead weight.
async fn retire_empty_shell(rpc: &dyn SpdkRpcTransport, nqn: &str) {
    let Ok(Some(sub)) = get_subsystem(rpc, nqn).await else { return };
    if !subsystem_ns_empty(&sub) {
        return;
    }
    // The guard's question for subsystem deletion is "does it still serve a
    // consumer" — answered by the empty-namespace read above: no namespace,
    // no data path. A concurrent pre-F46 driver re-minting this shape during
    // a mixed-version roll loses the race benignly (its ensure retries and
    // recreates).
    let delete = json!({ "method": "nvmf_delete_subsystem", "params": { "nqn": nqn } }); // guarded-destroy-lint: allow
    match rpc.rpc(&delete).await {
        Ok(_) => tracing::info!(nqn, "retired empty leg-export shell (F46 residue)"),
        Err(e) => tracing::debug!(nqn, error = %e, "empty-shell retire failed (continuing)"),
    }
}

fn listener_matches(listener: &Value, spec: &ExportSpec<'_>) -> bool {
    let addr = listener.get("address").unwrap_or(listener);
    let get = |k: &str| addr.get(k).and_then(|v| v.as_str()).unwrap_or("");
    get("trtype").eq_ignore_ascii_case(spec.trtype)
        && get("traddr") == spec.traddr
        && get("trsvcid") == spec.trsvcid.to_string()
}

/// Bring the export described by `spec` into existence, converging from any
/// partial state. Safe to call repeatedly and concurrently with itself.
pub async fn ensure_export(
    rpc: &dyn SpdkRpcTransport,
    spec: &ExportSpec<'_>,
) -> Result<(), RpcError> {
    // ---- subsystem ----
    let mut subsystem = get_subsystem(rpc, spec.nqn).await?;
    if subsystem.is_none() {
        let create = json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": spec.nqn,
                "allow_any_host": spec.allowed_hosts.is_none(),
                "serial_number": serial_for_nqn(spec.nqn),
                "model_number": "SPDK CSI Volume"
            }
        });
        if let Err(e) = rpc.rpc(&create).await {
            // Lost a race with a concurrent creator? Re-read before failing.
            subsystem = get_subsystem(rpc, spec.nqn).await?;
            if subsystem.is_none() {
                return Err(format!("Failed to create subsystem {}: {}", spec.nqn, e).into());
            }
        } else {
            subsystem = get_subsystem(rpc, spec.nqn).await?;
        }
    }

    // ---- host fencing ----
    if let Some(allowed) = spec.allowed_hosts {
        converge_hosts(rpc, spec.nqn, subsystem.as_ref(), allowed).await?;
    }

    // ---- namespace ----
    let namespaces = subsystem
        .as_ref()
        .and_then(|s| s.get("namespaces"))
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();

    let ns_present = namespaces.iter().any(|ns| ns_matches(ns, spec));
    if !ns_present {
        // A namespace pointing at a *different* bdev means the lvol was
        // re-created while a stale export lingered; remove it so the add
        // below can take its place at a free nsid.
        for ns in &namespaces {
            if !ns_matches(ns, spec) {
                if let Some(nsid) = ns.get("nsid").and_then(|n| n.as_u64()) {
                    let remove = json!({
                        "method": "nvmf_subsystem_remove_ns",
                        "params": { "nqn": spec.nqn, "nsid": nsid }
                    });
                    // Best effort; the add below is what must succeed.
                    let _ = rpc.rpc(&remove).await;
                }
            }
        }
        // Contract R3, construction half (F43 doc #7): this is the ONE
        // add_ns path that executes over the local unix socket instead of
        // the node agent's guarded /api/spdk/rpc boundary — and it is
        // reached from exactly the post-restart rehydration flows the
        // stale-ublk hazard targets. Probe before admitting a second
        // writer: SPDK itself accepts an add_ns over a ublk-served bdev
        // silently (ublk never claims; nvmf's legacy claim checks only
        // claim_type != NONE). Guarded only when the namespace is actually
        // being ADDED — an already-matching namespace above is a no-op.
        if crate::guarded_destroy::construction_guard_enabled() {
            let probe = |req: Value| async move { rpc.rpc(&req).await };
            match crate::guarded_destroy::bdev_identity_and_bytes(&probe, spec.bdev_name).await {
                Ok(Some((forms, _))) => {
                    match crate::guarded_destroy::ublk_consumer_of(&probe, &forms).await {
                        Ok(Some(id)) => {
                            return Err(format!(
                                "guarded_construct: refusing nvmf_subsystem_add_ns over {} — \
                                 ublk disk {} is live on this bdev right now; a second writer \
                                 would corrupt silently (F43 doc §6.4). Stop the stale ublk \
                                 disk first",
                                spec.bdev_name, id
                            )
                            .into());
                        }
                        Ok(None) => {}
                        // No ublk support ⇒ no ublk consumer.
                        Err(e) if e.contains("Method not found") => {}
                        Err(e) => {
                            return Err(format!(
                                "guarded_construct: ublk probe inconclusive for {}: {} — \
                                 failing closed (F37: never admit a writer blind)",
                                spec.bdev_name, e
                            )
                            .into());
                        }
                    }
                }
                Ok(None) => {} // bdev absent — the add below fails naturally
                Err(e) => {
                    return Err(format!(
                        "guarded_construct: identity probe inconclusive for {}: {} — failing \
                         closed (F37: never admit a writer blind)",
                        spec.bdev_name, e
                    )
                    .into());
                }
            }
        }

        let mut ns_obj = json!({ "bdev_name": spec.bdev_name });
        if let Some((uuid, nguid)) = spec.ns_identity {
            ns_obj["uuid"] = json!(uuid);
            ns_obj["nguid"] = json!(nguid);
        }
        let add = json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": spec.nqn,
                "namespace": ns_obj
            }
        });
        if let Err(e) = rpc.rpc(&add).await {
            // Re-verify: a concurrent ensure_export may have added it.
            let current = get_subsystem(rpc, spec.nqn).await?;
            let present = current
                .as_ref()
                .and_then(|s| s.get("namespaces"))
                .and_then(|n| n.as_array())
                .map(|nss| nss.iter().any(|ns| ns_matches(ns, spec)))
                .unwrap_or(false);
            if !present {
                return Err(format!(
                    "Failed to add namespace {} to {}: {}",
                    spec.bdev_name, spec.nqn, e
                )
                .into());
            }
        }
    }

    // ---- listener ----
    let listeners = subsystem
        .as_ref()
        .and_then(|s| s.get("listen_addresses"))
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();

    let listener_present = listeners.iter().any(|l| listener_matches(l, spec));
    if !listener_present {
        let add = json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": spec.nqn,
                "listen_address": {
                    "trtype": spec.trtype.to_uppercase(),
                    "traddr": spec.traddr,
                    "trsvcid": spec.trsvcid.to_string(),
                    "adrfam": "ipv4"
                }
            }
        });
        if let Err(e) = rpc.rpc(&add).await {
            let current = get_subsystem(rpc, spec.nqn).await?;
            let present = current
                .as_ref()
                .and_then(|s| s.get("listen_addresses"))
                .and_then(|l| l.as_array())
                .map(|ls| ls.iter().any(|l| listener_matches(l, spec)))
                .unwrap_or(false);
            if !present {
                return Err(format!(
                    "Failed to add listener {}:{} to {}: {}",
                    spec.traddr, spec.trsvcid, spec.nqn, e
                )
                .into());
            }
        }
    }

    Ok(())
}

/// Converge the subsystem's admitted-host state onto `allowed` (fencing,
/// doc §3): default-closed, exactly the listed Flint hosts admitted. Only
/// hosts under FLINT_HOST_NQN_PREFIX are ever removed. After removing a
/// host, polls until its controllers are actually gone — SPDK's disconnect
/// on host removal is asynchronous, and the §3 fence is only real once the
/// old consumer's qpairs are torn down.
async fn converge_hosts(
    rpc: &dyn SpdkRpcTransport,
    nqn: &str,
    subsystem: Option<&Value>,
    allowed: &[String],
) -> Result<(), RpcError> {
    // allow_any_host must be off for the host list to mean anything.
    let any_host = subsystem
        .and_then(|s| s.get("allow_any_host"))
        .and_then(|a| a.as_bool())
        .unwrap_or(true);
    if any_host {
        let disable = json!({
            "method": "nvmf_subsystem_allow_any_host",
            "params": { "nqn": nqn, "allow_any_host": false }
        });
        rpc.rpc(&disable)
            .await
            .map_err(|e| format!("Failed to disable allow_any_host on {}: {}", nqn, e))?;
    }

    let current_hosts: Vec<String> = subsystem
        .and_then(|s| s.get("hosts"))
        .and_then(|h| h.as_array())
        .map(|hosts| {
            hosts
                .iter()
                .filter_map(|h| h.get("nqn").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Add missing allowed hosts first (avoid a window with nobody admitted
    // during a same-consumer re-stage).
    for host in allowed {
        if !current_hosts.contains(host) {
            let add = json!({
                "method": "nvmf_subsystem_add_host",
                "params": { "nqn": nqn, "host": host }
            });
            if let Err(e) = rpc.rpc(&add).await {
                // Duplicate from a concurrent ensure counts as success.
                let now = get_subsystem(rpc, nqn).await?;
                let present = now
                    .as_ref()
                    .and_then(|s| s.get("hosts"))
                    .and_then(|h| h.as_array())
                    .map(|hs| hs.iter().any(|h| h.get("nqn").and_then(|n| n.as_str()) == Some(host)))
                    .unwrap_or(false);
                if !present {
                    return Err(format!("Failed to add host {} to {}: {}", host, nqn, e).into());
                }
            }
        }
    }

    // Fence out Flint hosts that are no longer allowed (the restage flip).
    let mut removed: Vec<&str> = Vec::new();
    for host in &current_hosts {
        if host.starts_with(FLINT_HOST_NQN_PREFIX) && !allowed.contains(host) {
            let remove = json!({
                "method": "nvmf_subsystem_remove_host",
                "params": { "nqn": nqn, "host": host }
            });
            match rpc.rpc(&remove).await {
                Ok(_) => removed.push(host),
                Err(e) => return Err(format!("Failed to remove host {} from {}: {}", host, nqn, e).into()),
            }
        }
    }

    // Post-fence verification: wait (bounded) for the removed hosts'
    // controllers to drain.
    if !removed.is_empty() {
        for _ in 0..20 {
            let ctrlrs = json!({
                "method": "nvmf_subsystem_get_controllers",
                "params": { "nqn": nqn }
            });
            let still_connected = match rpc.rpc(&ctrlrs).await {
                Ok(resp) => resp
                    .get("result")
                    .and_then(|r| r.as_array())
                    .map(|cs| {
                        cs.iter().any(|c| {
                            c.get("hostnqn")
                                .and_then(|h| h.as_str())
                                .map(|h| removed.contains(&h))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false),
                Err(_) => false,
            };
            if !still_connected {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        return Err(format!(
            "Fenced host(s) {:?} still have live controllers on {} after 10s",
            removed, nqn
        )
        .into());
    }

    Ok(())
}

/// Local unix-socket transport (node agent side).
#[async_trait]
impl SpdkRpcTransport for crate::minimal_disk_service::MinimalDiskService {
    async fn rpc(&self, payload: &Value) -> Result<Value, RpcError> {
        self.call_spdk_rpc(payload).await
    }
}

/// Node-agent HTTP proxy transport (driver/controller side).
pub struct NodeAgentTransport<'a> {
    pub driver: &'a crate::driver::SpdkCsiDriver,
    pub node_name: &'a str,
}

#[async_trait]
impl SpdkRpcTransport for NodeAgentTransport<'_> {
    async fn rpc(&self, payload: &Value) -> Result<Value, RpcError> {
        self.driver
            .call_node_agent(self.node_name, "/api/spdk/rpc", payload)
            .await
    }
}

/// Stable serial number derived from the NQN, so retries don't mint a new
/// serial each attempt (the previous code used the wall clock).
fn serial_for_nqn(nqn: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    nqn.hash(&mut hasher);
    format!("SPDK{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scripted transport: records calls, returns canned responses per method.
    struct FakeRpc {
        calls: Mutex<Vec<Value>>,
        subsystem: Mutex<Option<Value>>,
        fail_methods: Vec<&'static str>,
        /// Rows served by `bdev_get_bdevs` (matched on name/uuid/aliases).
        bdevs: Vec<Value>,
        /// Disks served by `ublk_get_disks` — seeds the construction guard.
        ublk_disks: Vec<Value>,
    }

    impl FakeRpc {
        fn new(subsystem: Option<Value>) -> Self {
            Self {
                calls: Mutex::new(vec![]),
                subsystem: Mutex::new(subsystem),
                fail_methods: vec![],
                bdevs: vec![],
                ublk_disks: vec![],
            }
        }

        fn method_calls(&self, method: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c["method"] == method)
                .count()
        }
    }

    #[async_trait]
    impl SpdkRpcTransport for FakeRpc {
        async fn rpc(&self, payload: &Value) -> Result<Value, RpcError> {
            self.calls.lock().unwrap().push(payload.clone());
            let method = payload["method"].as_str().unwrap();
            if self.fail_methods.contains(&method) {
                return Err("SPDK RPC error: Code=-32602 Msg=Invalid parameters".into());
            }
            match method {
                "nvmf_get_subsystems" => {
                    let sub = self.subsystem.lock().unwrap();
                    match &*sub {
                        Some(s) => Ok(json!({ "result": [s] })),
                        None => Err("Code=-19 Msg=No such device".into()),
                    }
                }
                "nvmf_create_subsystem" => {
                    *self.subsystem.lock().unwrap() = Some(json!({
                        "nqn": payload["params"]["nqn"],
                        "namespaces": [],
                        "listen_addresses": []
                    }));
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_add_ns" => {
                    let mut sub = self.subsystem.lock().unwrap();
                    if let Some(s) = sub.as_mut() {
                        let nss = s["namespaces"].as_array_mut().unwrap();
                        if nss
                            .iter()
                            .any(|n| n["bdev_name"] == payload["params"]["namespace"]["bdev_name"])
                        {
                            return Err("Code=-32602 Msg=Invalid parameters".into());
                        }
                        nss.push(json!({
                            "nsid": nss.len() + 1,
                            "bdev_name": payload["params"]["namespace"]["bdev_name"]
                        }));
                    }
                    Ok(json!({ "result": 1 }))
                }
                "nvmf_subsystem_add_listener" => {
                    let mut sub = self.subsystem.lock().unwrap();
                    if let Some(s) = sub.as_mut() {
                        let ls = s["listen_addresses"].as_array_mut().unwrap();
                        let new = &payload["params"]["listen_address"];
                        if ls.iter().any(|l| {
                            l["traddr"] == new["traddr"] && l["trsvcid"] == new["trsvcid"]
                        }) {
                            return Err("Code=-32602 Msg=Invalid parameters".into());
                        }
                        ls.push(new.clone());
                    }
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_remove_ns" => Ok(json!({ "result": true })),
                "nvmf_subsystem_allow_any_host" => {
                    let mut sub = self.subsystem.lock().unwrap();
                    if let Some(s) = sub.as_mut() {
                        s["allow_any_host"] = payload["params"]["allow_any_host"].clone();
                    }
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_add_host" => {
                    let mut sub = self.subsystem.lock().unwrap();
                    if let Some(s) = sub.as_mut() {
                        let hosts = s["hosts"].as_array_mut().unwrap();
                        hosts.push(json!({ "nqn": payload["params"]["host"] }));
                    }
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_remove_host" => {
                    let mut sub = self.subsystem.lock().unwrap();
                    if let Some(s) = sub.as_mut() {
                        let hosts = s["hosts"].as_array_mut().unwrap();
                        hosts.retain(|h| h["nqn"] != payload["params"]["host"]);
                    }
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_get_controllers" => Ok(json!({ "result": [] })),
                "bdev_get_bdevs" => {
                    let name = payload["params"]["name"].as_str().unwrap_or("");
                    let hit: Vec<Value> = self
                        .bdevs
                        .iter()
                        .filter(|b| {
                            b["name"] == name
                                || b["uuid"] == name
                                || b["aliases"]
                                    .as_array()
                                    .map(|a| a.iter().any(|x| x == name))
                                    .unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                    Ok(json!({ "result": hit }))
                }
                "ublk_get_disks" => Ok(json!({ "result": self.ublk_disks })),
                _ => Ok(json!({ "result": null })),
            }
        }
    }

    fn spec<'a>() -> ExportSpec<'a> {
        ExportSpec {
            nqn: "nqn.2024-11.com.flint:volume:test_1",
            bdev_name: "11111111-2222-3333-4444-555555555555",
            bdev_aliases: &[],
            trtype: "TCP",
            traddr: "10.0.0.2",
            trsvcid: 4420,
            allowed_hosts: None,
            ns_identity: None,
        }
    }

    #[tokio::test]
    async fn creates_everything_from_scratch() {
        let rpc = FakeRpc::new(None);
        ensure_export(&rpc, &spec()).await.unwrap();
        assert_eq!(rpc.method_calls("nvmf_create_subsystem"), 1);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_ns"), 1);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_listener"), 1);
    }

    #[test]
    fn stable_ns_identity_is_deterministic_and_well_formed() {
        let (u1, g1) = stable_ns_identity("pvc-abc");
        let (u2, g2) = stable_ns_identity("pvc-abc");
        assert_eq!((u1.clone(), g1.clone()), (u2, g2));
        assert_ne!(stable_ns_identity("pvc-other").0, u1);
        // RFC4122 shape: 8-4-4-4-12, version 4, variant bits.
        let parts: Vec<&str> = u1.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!("89ab".contains(&parts[3][0..1]));
        // NGUID = same 16 bytes, 32 hex chars, no dashes.
        assert_eq!(g1.len(), 32);
        assert_eq!(g1, u1.replace('-', ""));
    }

    #[tokio::test]
    async fn pinned_ns_identity_reaches_add_ns() {
        let rpc = FakeRpc::new(None);
        let (uuid, nguid) = stable_ns_identity("test_1");
        let s = ExportSpec { ns_identity: Some((&uuid, &nguid)), ..spec() };
        ensure_export(&rpc, &s).await.unwrap();
        let calls = rpc.calls.lock().unwrap();
        let add = calls
            .iter()
            .find(|c| c["method"] == "nvmf_subsystem_add_ns")
            .expect("add_ns issued");
        assert_eq!(add["params"]["namespace"]["uuid"], json!(uuid));
        assert_eq!(add["params"]["namespace"]["nguid"], json!(nguid));
        // Unpinned spec omits both (SPDK default = bdev identity).
        let rpc2 = FakeRpc::new(None);
        ensure_export(&rpc2, &spec()).await.unwrap();
        let calls2 = rpc2.calls.lock().unwrap();
        let add2 = calls2
            .iter()
            .find(|c| c["method"] == "nvmf_subsystem_add_ns")
            .unwrap();
        assert!(add2["params"]["namespace"].get("uuid").is_none());
    }

    #[tokio::test]
    async fn fully_present_is_a_noop() {
        let rpc = FakeRpc::new(Some(json!({
            "nqn": "nqn.2024-11.com.flint:volume:test_1",
            "namespaces": [{ "nsid": 1, "bdev_name": "11111111-2222-3333-4444-555555555555" }],
            "listen_addresses": [{ "trtype": "TCP", "traddr": "10.0.0.2", "trsvcid": "4420" }]
        })));
        ensure_export(&rpc, &spec()).await.unwrap();
        assert_eq!(rpc.method_calls("nvmf_create_subsystem"), 0);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_ns"), 0);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_listener"), 0);
    }

    /// The exact state the live repro produced: subsystem + namespace exist
    /// (previous partial attempt), listener missing. The old code failed
    /// permanently on add_ns; convergent code must skip the ns and add only
    /// the listener.
    #[tokio::test]
    async fn converges_from_ns_present_listener_missing() {
        let rpc = FakeRpc::new(Some(json!({
            "nqn": "nqn.2024-11.com.flint:volume:test_1",
            "namespaces": [{ "nsid": 1, "bdev_name": "11111111-2222-3333-4444-555555555555" }],
            "listen_addresses": []
        })));
        ensure_export(&rpc, &spec()).await.unwrap();
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_ns"), 0);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_listener"), 1);
    }

    /// Inverse partial state: listener present, namespace missing.
    #[tokio::test]
    async fn converges_from_listener_present_ns_missing() {
        let rpc = FakeRpc::new(Some(json!({
            "nqn": "nqn.2024-11.com.flint:volume:test_1",
            "namespaces": [],
            "listen_addresses": [{ "trtype": "TCP", "traddr": "10.0.0.2", "trsvcid": "4420" }]
        })));
        ensure_export(&rpc, &spec()).await.unwrap();
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_ns"), 1);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_listener"), 0);
    }

    /// Fence flip on restage: previous consumer's Flint host is removed,
    /// the new consumer admitted, allow_any_host turned off, and a non-Flint
    /// (admin-added) host entry preserved.
    #[tokio::test]
    async fn fences_out_previous_consumer() {
        let rpc = FakeRpc::new(Some(json!({
            "nqn": "nqn.2024-11.com.flint:volume:test_1",
            "allow_any_host": true,
            "hosts": [
                { "nqn": "nqn.2024-11.com.flint:node:old-node" },
                { "nqn": "nqn.2014-08.org.example:admin-host" }
            ],
            "namespaces": [{ "nsid": 1, "bdev_name": "11111111-2222-3333-4444-555555555555" }],
            "listen_addresses": [{ "trtype": "TCP", "traddr": "10.0.0.2", "trsvcid": "4420" }]
        })));
        let allowed = vec![flint_host_nqn("new-node")];
        let mut s = spec();
        s.allowed_hosts = Some(&allowed);
        ensure_export(&rpc, &s).await.unwrap();

        assert_eq!(rpc.method_calls("nvmf_subsystem_allow_any_host"), 1);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_host"), 1);
        assert_eq!(rpc.method_calls("nvmf_subsystem_remove_host"), 1);
        let sub = rpc.subsystem.lock().unwrap();
        let hosts: Vec<String> = sub.as_ref().unwrap()["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["nqn"].as_str().unwrap().to_string())
            .collect();
        assert!(hosts.contains(&flint_host_nqn("new-node")));
        assert!(hosts.contains(&"nqn.2014-08.org.example:admin-host".to_string()));
        assert!(!hosts.contains(&flint_host_nqn("old-node")));
    }

    /// Stale namespace from a re-created lvol gets replaced.
    #[tokio::test]
    async fn replaces_stale_namespace() {
        let rpc = FakeRpc::new(Some(json!({
            "nqn": "nqn.2024-11.com.flint:volume:test_1",
            "namespaces": [{ "nsid": 1, "bdev_name": "99999999-old-old-old-999999999999" }],
            "listen_addresses": [{ "trtype": "TCP", "traddr": "10.0.0.2", "trsvcid": "4420" }]
        })));
        ensure_export(&rpc, &spec()).await.unwrap();
        assert_eq!(rpc.method_calls("nvmf_subsystem_remove_ns"), 1);
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_ns"), 1);
    }

    /// F43 doc #7, the local-transport half: the rehydration flows call
    /// ensure_export over the unix socket, bypassing the node agent's
    /// guarded boundary — the pre-add probe here is their only defense
    /// against admitting a second writer over a ublk-served bdev.
    #[tokio::test]
    async fn refuses_add_ns_over_ublk_served_bdev() {
        let mut rpc = FakeRpc::new(None);
        rpc.bdevs = vec![json!({
            "name": "lvs/vol1", "uuid": "11111111-2222-3333-4444-555555555555",
            "aliases": ["lvs/vol1"], "num_blocks": 262144, "block_size": 4096
        })];
        // A stale ublk disk still serves the bdev — by ALIAS, not the uuid
        // the spec spells (the F36 name-agnostic lesson).
        rpc.ublk_disks = vec![json!({ "id": 5, "bdev_name": "lvs/vol1" })];
        let err = ensure_export(&rpc, &spec())
            .await
            .expect_err("must refuse a second writer");
        assert!(err.to_string().contains("guarded_construct"), "{err}");
        assert!(err.to_string().contains("ublk disk 5"), "{err}");
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_ns"), 0, "add_ns must not fire");
    }

    #[tokio::test]
    async fn add_ns_proceeds_when_no_ublk_disk_serves_the_bdev() {
        let mut rpc = FakeRpc::new(None);
        rpc.bdevs = vec![json!({
            "name": "lvs/vol1", "uuid": "11111111-2222-3333-4444-555555555555",
            "aliases": ["lvs/vol1"], "num_blocks": 262144, "block_size": 4096
        })];
        rpc.ublk_disks = vec![json!({ "id": 5, "bdev_name": "some-other-bdev" })];
        ensure_export(&rpc, &spec()).await.unwrap();
        assert_eq!(rpc.method_calls("nvmf_subsystem_add_ns"), 1);
    }

    // ── F46 transition belt: resolve_replica_export_nqn ─────────────────

    /// Per-NQN fake (the FakeRpc above models a single subsystem; the belt
    /// reasons about the canonical/legacy PAIR).
    struct SubsByNqn {
        subs: Mutex<std::collections::HashMap<String, Value>>,
        deletes: Mutex<Vec<String>>,
    }

    impl SubsByNqn {
        fn new(entries: &[(&str, Value)]) -> Self {
            Self {
                subs: Mutex::new(
                    entries.iter().map(|(n, s)| (n.to_string(), s.clone())).collect(),
                ),
                deletes: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl SpdkRpcTransport for SubsByNqn {
        async fn rpc(&self, payload: &Value) -> Result<Value, RpcError> {
            match payload["method"].as_str().unwrap() {
                "nvmf_get_subsystems" => {
                    let nqn = payload["params"]["nqn"].as_str().unwrap();
                    match self.subs.lock().unwrap().get(nqn) {
                        Some(s) => Ok(json!({ "result": [s] })),
                        None => Err("Code=-19 Msg=No such device".into()),
                    }
                }
                "nvmf_delete_subsystem" => {
                    let nqn = payload["params"]["nqn"].as_str().unwrap().to_string();
                    self.subs.lock().unwrap().remove(&nqn);
                    self.deletes.lock().unwrap().push(nqn);
                    Ok(json!({ "result": true }))
                }
                m => panic!("belt should not issue {m}"),
            }
        }
    }

    const VOL: &str = "pvc-5574462a";
    const HEAD: &str = "faa78582-aaaa-bbbb-cccc-000000000001";

    fn ns_sub(nqn: &str, bdev: &str) -> (String, Value) {
        (nqn.to_string(), json!({ "nqn": nqn, "namespaces": [{ "nsid": 1, "bdev_name": bdev }] }))
    }
    fn empty_sub(nqn: &str) -> (String, Value) {
        (nqn.to_string(), json!({ "nqn": nqn, "namespaces": [] }))
    }

    /// The exact run-3 F46 state: inner subsystem holds the head, wrapper
    /// sibling is an empty shell. The belt must serve canonically AND
    /// retire the shell that was wedging name-keyed probes.
    #[tokio::test]
    async fn f46_state_resolves_canonical_and_retires_the_shell() {
        let canonical = crate::identity::replica_export_nqn(VOL, 1);
        let legacy = crate::identity::legacy_replica_export_nqn(VOL, 1);
        let inner = ns_sub(&canonical, HEAD);
        let shell = empty_sub(&legacy);
        let rpc = SubsByNqn::new(&[(&inner.0, inner.1.clone()), (&shell.0, shell.1.clone())]);

        // Staged (wrapper) handle in, canonical nqn out — domain normalized.
        let staged = crate::identity::backing_handle(VOL);
        let nqn = resolve_replica_export_nqn(&rpc, &staged, 1, HEAD, &[]).await.unwrap();
        assert_eq!(nqn, canonical);
        assert_eq!(*rpc.deletes.lock().unwrap(), vec![legacy], "the F46 shell must be retired");
    }

    /// Mid-upgrade: the leg is still served by a pre-unification wrapper
    /// export with a live consumer attached through it. Adopt — re-minting
    /// canonically would fail claim-shaped and migrating would sever I/O.
    #[tokio::test]
    async fn live_legacy_export_is_adopted_not_fought() {
        let legacy = crate::identity::legacy_replica_export_nqn(VOL, 1);
        let serving = ns_sub(&legacy, HEAD);
        let rpc = SubsByNqn::new(&[(&serving.0, serving.1.clone())]);

        let nqn = resolve_replica_export_nqn(&rpc, VOL, 1, HEAD, &[]).await.unwrap();
        assert_eq!(nqn, legacy, "must adopt the serving legacy export");
        assert!(rpc.deletes.lock().unwrap().is_empty(), "adoption must not delete anything");
    }

    /// Fresh node: nothing exists — mint canonically; a legacy subsystem
    /// holding a FOREIGN namespace is not ours to touch.
    #[tokio::test]
    async fn fresh_mint_is_canonical_and_foreign_legacy_ns_is_left_alone() {
        let rpc = SubsByNqn::new(&[]);
        let nqn = resolve_replica_export_nqn(&rpc, VOL, 1, HEAD, &[]).await.unwrap();
        assert_eq!(nqn, crate::identity::replica_export_nqn(VOL, 1));

        let legacy = crate::identity::legacy_replica_export_nqn(VOL, 1);
        let foreign = ns_sub(&legacy, "some-other-bdev");
        let rpc = SubsByNqn::new(&[(&foreign.0, foreign.1.clone())]);
        let nqn = resolve_replica_export_nqn(&rpc, VOL, 1, HEAD, &[]).await.unwrap();
        assert_eq!(nqn, crate::identity::replica_export_nqn(VOL, 1));
        assert!(rpc.deletes.lock().unwrap().is_empty(), "foreign ns must survive");
    }

    // ── F47: local-only subsystem detection (F9 guard exception) ────────

    /// A subsystem whose every listener is loopback is deletable by its own
    /// node's unstage regardless of VA ownership; any remote listener keeps
    /// the guard. Both SPDK listener encodings (flat and nested under
    /// "address") must be understood, and no listeners at all counts as
    /// local-only (nobody can be severed).
    #[test]
    fn local_only_subsystem_detection() {
        let flat = serde_json::json!({
            "nqn": "nqn.x",
            "listen_addresses": [{ "trtype": "TCP", "traddr": "127.0.0.1", "trsvcid": "4420" }]
        });
        assert!(subsystem_is_local_only(&flat));

        let nested = serde_json::json!({
            "nqn": "nqn.x",
            "listen_addresses": [{ "address": { "traddr": "127.0.0.1" }, "trtype": "TCP" }]
        });
        assert!(subsystem_is_local_only(&nested));

        let v6 = serde_json::json!({
            "nqn": "nqn.x",
            "listen_addresses": [{ "traddr": "::1" }]
        });
        assert!(subsystem_is_local_only(&v6));

        let remote = serde_json::json!({
            "nqn": "nqn.x",
            "listen_addresses": [{ "traddr": "172.31.15.167" }]
        });
        assert!(!subsystem_is_local_only(&remote));

        // Mixed: one remote listener makes it remote-consumable.
        let mixed = serde_json::json!({
            "nqn": "nqn.x",
            "listen_addresses": [{ "traddr": "127.0.0.1" }, { "traddr": "10.0.0.7" }]
        });
        assert!(!subsystem_is_local_only(&mixed));

        // No listeners (empty or missing key): unreachable, local-only.
        let empty = serde_json::json!({ "nqn": "nqn.x", "listen_addresses": [] });
        assert!(subsystem_is_local_only(&empty));
        let missing = serde_json::json!({ "nqn": "nqn.x" });
        assert!(subsystem_is_local_only(&missing));

        // A listener with no readable traddr is NOT provably loopback.
        let unreadable = serde_json::json!({
            "nqn": "nqn.x",
            "listen_addresses": [{ "trtype": "TCP" }]
        });
        assert!(!subsystem_is_local_only(&unreadable));
    }
}
