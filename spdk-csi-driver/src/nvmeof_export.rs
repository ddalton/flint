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
    let name = ns.get("bdev_name").and_then(|b| b.as_str()).unwrap_or("");
    let uuid = ns.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
    name == spec.bdev_name
        || uuid == spec.bdev_name
        || spec.bdev_aliases.iter().any(|a| *a == name || *a == uuid)
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
    }

    impl FakeRpc {
        fn new(subsystem: Option<Value>) -> Self {
            Self {
                calls: Mutex::new(vec![]),
                subsystem: Mutex::new(subsystem),
                fail_methods: vec![],
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
}
