//! Block-layout (scsi) export reconciler — design doc §5, "spdk-tgt as
//! the target fleet".
//!
//! A block-class volume's data lives in ONE lvol behind ONE
//! subsystem-per-volume NVMe-oF export; every granted client connects to
//! that export from its own node. This module owns the target side of
//! that sentence: the lvol exists, the subsystem exists, the namespace
//! carries the volume's NGUID (the one-identity rule: the same 16 bytes
//! GETDEVICEINFO advertises as deviceid and designator — the kernel's
//! blocklayout device matching succeeds by construction), the listener is
//! reachable, and the allow-list holds exactly the hosts the durable
//! `block_hosts` table says it should.
//!
//! Everything is LEVEL-TRIGGERED convergence, `nvmeof_export`-style: the
//! desired state is read fresh from sqlite inside a per-volume lock right
//! before converging, never carried in from the caller — two concurrent
//! admits with each other's rows uncommitted would otherwise converge
//! onto each other's stale snapshots and one admission would be yanked
//! until its client's next LAYOUTGET (a lost-admission race this
//! structure makes unrepresentable).
//!
//! Boundary note (ADR 0001): this is pNFS *consuming* the SPDK control
//! plane — the permitted direction. The allocator does not leak into
//! `nvmeof_export.rs`; this module composes its primitives.

use std::sync::Arc;

use crate::nvmeof_export::{ensure_export, get_subsystem, ExportSpec, SpdkRpcTransport};
use serde_json::json;

/// How the reconciler reaches its spdk-tgt plus the export coordinates it
/// converges toward. One target per MDS shard for phase 1 — allocation is
/// per-volume inside the volume's own lvol (§8), and the volume pins to
/// its shard, so the shard's tgt is the volume's tgt.
pub struct BlockExportReconciler {
    rpc: Arc<dyn SpdkRpcTransport + Send + Sync>,
    backend: Arc<dyn crate::state_backend::StateBackend>,
    /// lvolstore backing the per-volume lvols (`<lvstore>/<volume>`).
    lvstore: String,
    /// Listener address kernel initiators dial. Advertised nowhere in
    /// NFS (RFC 8154 device addresses carry designators, not netaddrs —
    /// clients connect out of band), so this only has to be right for
    /// the nodes' `nvme connect`.
    traddr: String,
    trsvcid: u16,
    /// Directory (ON THE TGT HOST) for per-namespace reservation PTPL
    /// files. Mandatory-by-kernel: see `ExportSpec::ptpl_file`. Must
    /// outlive tgt restarts in production (a lost PTPL file silently
    /// unregisters every client's reservation key — the csi-node roll
    /// landmine, reservation edition).
    ptpl_dir: String,
    /// Per-volume serialization of converge passes (see module doc).
    locks: dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl BlockExportReconciler {
    pub fn new(
        rpc: Arc<dyn SpdkRpcTransport + Send + Sync>,
        backend: Arc<dyn crate::state_backend::StateBackend>,
        lvstore: String,
        traddr: String,
        trsvcid: u16,
        ptpl_dir: String,
    ) -> Self {
        Self {
            rpc,
            backend,
            lvstore,
            traddr,
            trsvcid,
            ptpl_dir,
            locks: dashmap::DashMap::new(),
        }
    }

    fn lock_for(&self, volume: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .entry(volume.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn bdev_name(&self, volume: &str) -> String {
        format!("{}/{}", self.lvstore, volume)
    }

    /// Desired allow-list, read fresh from sqlite. A read failure is a
    /// hard error — converging onto an EMPTY list on a read failure
    /// would evict every live client of the volume. The MDS's own fence
    /// lane host is ALWAYS desired: it must be able to connect and
    /// preempt at the exact moment clients are being thrown out, so no
    /// eviction/reconcile pass may ever sweep it.
    async fn desired_hosts(&self, volume: &str) -> Result<Vec<String>, String> {
        match self.backend.block_hosts(volume).await {
            Ok(Ok(mut hosts)) => {
                let mds = crate::identity::block_mds_host_nqn();
                if !hosts.contains(&mds) {
                    hosts.push(mds);
                }
                Ok(hosts)
            }
            Ok(Err(e)) => Err(format!("block_hosts read refused: {e}")),
            Err(e) => Err(format!("block_hosts read failed: {e}")),
        }
    }

    /// Converge the volume's whole export chain. `size_bytes = Some(n)`
    /// is the PROVISION shape: a missing lvol is created (thin, n bytes
    /// rounded up to MiB). `None` is the RECONCILE shape: a missing lvol
    /// is a hard, screaming error — the arena's extent rows may reference
    /// real bytes, and silently minting a fresh empty lvol under them
    /// would serve zeros for committed extents (F67's exact shape, one
    /// layer down).
    pub async fn ensure(&self, volume: &str, size_bytes: Option<u64>) -> Result<(), String> {
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        self.ensure_locked(volume, size_bytes).await
    }

    /// Re-read the desired allow-list and converge the export onto it.
    /// The post-admit/post-evict hook.
    pub async fn reconcile_hosts(&self, volume: &str) -> Result<(), String> {
        self.ensure(volume, None).await
    }

    /// Every name the lvol answers to, from a live `bdev_get_bdevs`
    /// record: its bdev name (UUID-form for lvols), its uuid, and its
    /// aliases. The namespace record in `nvmf_get_subsystems` carries
    /// the bdev NAME, not the `lvs/vol` alias this module addresses the
    /// lvol by — matching on the alias alone made every converge pass
    /// see "a namespace pointing at a different bdev" and take
    /// ensure_export's remove-and-re-add repair arm, yanking the
    /// namespace out from under live initiators once per reconcile
    /// (rig-found: the device node vanished for ~0.5s at every admit,
    /// and the client's layout-time open raced straight into the gap).
    fn lvol_identities(resp: &serde_json::Value) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(b) = resp
            .get("result")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
        {
            for key in ["name", "uuid"] {
                if let Some(v) = b.get(key).and_then(|v| v.as_str()) {
                    ids.push(v.to_string());
                }
            }
            if let Some(aliases) = b.get("aliases").and_then(|a| a.as_array()) {
                ids.extend(aliases.iter().filter_map(|a| a.as_str().map(String::from)));
            }
        }
        ids
    }

    async fn ensure_locked(&self, volume: &str, size_bytes: Option<u64>) -> Result<(), String> {
        let bdev = self.bdev_name(volume);

        // ---- lvol ----
        let probe = json!({ "method": "bdev_get_bdevs", "params": { "name": bdev } });
        let mut probe_resp = self.rpc.rpc(&probe).await;
        let lvol_present = probe_resp.is_ok();
        if !lvol_present {
            let Some(bytes) = size_bytes else {
                return Err(format!(
                    "lvol {} is MISSING but the volume's extent arena exists — refusing to \
                     re-create it: committed extents would silently read zeros from a fresh \
                     lvol (F67). If the lvolstore was genuinely lost, the volume's data is \
                     gone; delete and re-provision the volume to say so explicitly",
                    bdev
                ));
            };
            let size_mib = bytes.div_ceil(1024 * 1024).max(1);
            let create = json!({
                "method": "bdev_lvol_create",
                "params": {
                    "lvs_name": self.lvstore,
                    "lvol_name": volume,
                    "size_in_mib": size_mib,
                    // Thin: the allocator hands out INVALID_DATA extents a
                    // client must write before reading; unwritten thin
                    // clusters read zeros, which is exactly that contract.
                    "thin_provision": true
                }
            });
            if let Err(e) = self.rpc.rpc(&create).await {
                // Concurrent creator (CreateVolume retry storm)? Re-read
                // before failing, same discipline as ensure_export.
                if self.rpc.rpc(&probe).await.is_err() {
                    return Err(format!("bdev_lvol_create {}: {}", bdev, e));
                }
            }
            probe_resp = self.rpc.rpc(&probe).await;
        }
        let lvol_ids = probe_resp.as_ref().map(Self::lvol_identities).unwrap_or_default();
        let lvol_id_refs: Vec<&str> = lvol_ids.iter().map(String::as_str).collect();

        // ---- subsystem / namespace / listener / hosts ----
        let hosts = self.desired_hosts(volume).await?;
        let nqn = crate::identity::block_volume_export_nqn(volume);
        let (uuid, nguid) = crate::nvmeof_export::stable_ns_identity(volume);
        let ptpl = format!("{}/flint-ptpl-{}.json", self.ptpl_dir, volume);
        let spec = ExportSpec {
            nqn: &nqn,
            bdev_name: &bdev,
            bdev_aliases: &lvol_id_refs,
            trtype: "TCP",
            traddr: &self.traddr,
            trsvcid: self.trsvcid,
            // Default-closed always: Some(&[]) = nobody may connect. The
            // legacy wide-open shape (None) never applies to block
            // exports — a layout-holding client has raw write reach over
            // the whole namespace (§6), so admission is the boundary.
            allowed_hosts: Some(&hosts),
            // NGUID adoption (§5): pinned, never the lvol-UUID default.
            // This is the identity GETDEVICEINFO advertises; it must
            // survive lvol rebuild and tgt restart.
            ns_identity: Some((&uuid, &nguid)),
            ptpl_file: Some(&ptpl),
        };
        ensure_export(self.rpc.as_ref(), &spec)
            .await
            .map_err(|e| format!("ensure_export {}: {}", nqn, e))
    }

    /// The PRIMARY fence (§5, RFC 9561 §2.2): connect to the volume's
    /// export as the MDS's own NVMe host and preempt the victim's
    /// reservation key. Target-side and per-command: the victim's next
    /// read or write gets RESERVATION CONFLICT no matter what state its
    /// connections, sessions, or kernel are in — client reachability is
    /// irrelevant to delivery, which is the whole point (the client
    /// being fenced is precisely the one that stopped answering).
    /// Returns a one-line summary of the resulting reservation state
    /// for the caller's log (the rig greps it).
    pub async fn fence_preempt(&self, volume: &str, victim_key: u64) -> Result<String, String> {
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        // Converge first: the fence lane's host NQN must be on the
        // allow-list to connect (`desired_hosts` always includes it),
        // and reconverging is the idempotent way to get it there for
        // volumes provisioned before the fence lane existed.
        self.ensure_locked(volume, None).await?;
        let nqn = crate::identity::block_volume_export_nqn(volume);
        // The namespace id, read from the live subsystem rather than
        // assumed: subsystem-per-volume means exactly one, but the
        // repair arms can re-mint it and auto-assignment is a tgt
        // detail, not our invariant.
        let nsid = get_subsystem(self.rpc.as_ref(), &nqn)
            .await
            .map_err(|e| format!("nvmf_get_subsystems {}: {}", nqn, e))?
            .and_then(|s| {
                s.get("namespaces")?
                    .as_array()?
                    .first()?
                    .get("nsid")?
                    .as_u64()
            })
            .ok_or_else(|| format!("{} carries no namespace to fence on", nqn))?;
        let ep = super::resv_fence::ResvEndpoint {
            traddr: self.traddr.clone(),
            trsvcid: self.trsvcid,
            subnqn: nqn,
            hostnqn: crate::identity::block_mds_host_nqn(),
            hostid: crate::identity::BLOCK_MDS_HOST_ID,
            nsid: nsid as u32,
        };
        let out = ep
            .fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, victim_key)
            .await?;
        Ok(format!(
            "victim={victim_key:#x} preempted={} (registered={} acquired={}) resv: {}",
            out.preempted,
            out.registered,
            out.acquired,
            out.after.summary()
        ))
    }

    /// DeleteVolume's teardown: subsystem first (severs every data path),
    /// then the lvol. Both tolerate absence — the sweep is unconditional
    /// and a crashed earlier attempt may have finished half of it.
    pub async fn delete_volume_export(&self, volume: &str) -> Result<(), String> {
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        let nqn = crate::identity::block_volume_export_nqn(volume);
        if get_subsystem(self.rpc.as_ref(), &nqn)
            .await
            .map_err(|e| format!("nvmf_get_subsystems {}: {}", nqn, e))?
            .is_some()
        {
            // Deleting a subsystem consumers may hold open is normally the
            // guarded-destroy question; here the volume itself is being
            // deleted under CSI authority, every layout was reclaimed by
            // the DeleteVolume sweep, and any remaining connection is by
            // definition stale.
            let delete = json!({ "method": "nvmf_delete_subsystem", "params": { "nqn": nqn } }); // guarded-destroy-lint: allow
            self.rpc
                .rpc(&delete)
                .await
                .map_err(|e| format!("nvmf_delete_subsystem {}: {}", nqn, e))?;
        }
        let bdev = self.bdev_name(volume);
        let probe = json!({ "method": "bdev_get_bdevs", "params": { "name": bdev } });
        if self.rpc.rpc(&probe).await.is_ok() {
            // Namespace gone with the subsystem above; nothing serves this
            // bdev any more. The volume's bytes die here, on DeleteVolume's
            // authority.
            let delete = json!({ "method": "bdev_lvol_delete", "params": { "name": bdev } }); // guarded-destroy-lint: allow
            self.rpc
                .rpc(&delete)
                .await
                .map_err(|e| format!("bdev_lvol_delete {}: {}", bdev, e))?;
        }
        Ok(())
    }

    /// MDS-start replay: converge every known block-class volume. Runs
    /// with `size_bytes: None` (never mints an lvol — see `ensure`).
    /// Failures are LOUD but non-fatal: a down tgt must not crashloop
    /// the MDS out of serving its file-class volumes. A tgt restart
    /// while the MDS stays up is NOT covered here — that needs the
    /// periodic reconcile loop (next tranche); until then the runbook's
    /// answer is an MDS rollout after any tgt restart.
    pub async fn reconcile_all(&self, volumes: &[String]) {
        for v in volumes {
            if let Err(e) = self.ensure(v, None).await {
                tracing::error!(
                    "block-export reconcile of '{}' failed: {} — the volume's clients \
                     cannot connect until this converges",
                    v,
                    e
                );
            } else {
                tracing::info!("block-export reconcile of '{}' converged", v);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Scriptable transport: records every RPC, answers from a mutable
    /// world (bdevs + subsystems present). `pub(crate)`: the grpc and
    /// operations tests drive their integration seams against it too.
    /// Models the real-SPDK trap rig-found on the live tgt: an lvol's
    /// CANONICAL bdev name is UUID-form, the `lvs/vol` alias is only an
    /// alias, and namespace records carry the CANONICAL name — a
    /// reconciler matching on the alias alone bounces the namespace on
    /// every pass.
    pub(crate) struct FakeTgt {
        pub(crate) calls: Mutex<Vec<Value>>,
        /// alias → canonical (UUID-form) bdev name.
        pub(crate) bdevs: Mutex<std::collections::HashMap<String, String>>,
        pub(crate) subsystems: Mutex<std::collections::HashMap<String, Value>>,
    }

    impl FakeTgt {
        pub(crate) fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                bdevs: Mutex::new(Default::default()),
                subsystems: Mutex::new(Default::default()),
            }
        }
        pub(crate) fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c["method"].as_str().unwrap().to_string())
                .collect()
        }
        pub(crate) fn call_with_method<'a>(&self, calls: &'a [Value], m: &str) -> Option<&'a Value> {
            calls.iter().find(|c| c["method"] == m)
        }
        /// Host NQNs currently on `nqn`'s allow-list.
        pub(crate) fn hosts_of(&self, nqn: &str) -> Vec<String> {
            self.subsystems
                .lock()
                .unwrap()
                .get(nqn)
                .and_then(|s| s["hosts"].as_array())
                .map(|hs| {
                    hs.iter()
                        .filter_map(|h| h["nqn"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl SpdkRpcTransport for FakeTgt {
        async fn rpc(&self, payload: &Value) -> Result<Value, crate::nvmeof_export::RpcError> {
            self.calls.lock().unwrap().push(payload.clone());
            let method = payload["method"].as_str().unwrap_or("");
            let p = &payload["params"];
            match method {
                "bdev_get_bdevs" => {
                    let name = p["name"].as_str().unwrap_or("");
                    let bdevs = self.bdevs.lock().unwrap();
                    match bdevs.get(name) {
                        Some(canonical) => Ok(json!({ "result": [{
                            "name": canonical,
                            "uuid": canonical,
                            "aliases": [name],
                        }] })),
                        None => Err("No such device".into()),
                    }
                }
                "bdev_lvol_create" => {
                    let alias = format!(
                        "{}/{}",
                        p["lvs_name"].as_str().unwrap(),
                        p["lvol_name"].as_str().unwrap()
                    );
                    let canonical = format!("uuid-of-{}", p["lvol_name"].as_str().unwrap());
                    self.bdevs.lock().unwrap().insert(alias, canonical.clone());
                    Ok(json!({ "result": canonical }))
                }
                "bdev_lvol_delete" => {
                    self.bdevs.lock().unwrap().remove(p["name"].as_str().unwrap_or(""));
                    Ok(json!({ "result": true }))
                }
                "nvmf_get_subsystems" => {
                    let nqn = p["nqn"].as_str().unwrap_or("");
                    match self.subsystems.lock().unwrap().get(nqn) {
                        Some(s) => Ok(json!({ "result": [s] })),
                        None => Err("No such device".into()),
                    }
                }
                "nvmf_create_subsystem" => {
                    let nqn = p["nqn"].as_str().unwrap().to_string();
                    self.subsystems.lock().unwrap().insert(
                        nqn.clone(),
                        json!({
                            "nqn": nqn,
                            "namespaces": [],
                            "listen_addresses": [],
                            "hosts": [],
                            "allow_any_host": p["allow_any_host"],
                        }),
                    );
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_add_ns" => {
                    let nqn = p["nqn"].as_str().unwrap();
                    // Real SPDK resolves the alias and the ns record then
                    // carries the CANONICAL bdev name — model that, or the
                    // alias-only ns_matches bug is untestable here.
                    let requested = p["namespace"]["bdev_name"].as_str().unwrap_or("");
                    let canonical = self
                        .bdevs
                        .lock()
                        .unwrap()
                        .get(requested)
                        .cloned()
                        .unwrap_or_else(|| requested.to_string());
                    let mut subs = self.subsystems.lock().unwrap();
                    let s = subs.get_mut(nqn).ok_or("no subsystem")?;
                    s["namespaces"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({
                            "nsid": 1,
                            "bdev_name": canonical,
                            "uuid": p["namespace"]["uuid"],
                            "nguid": p["namespace"]["nguid"],
                            "ptpl_file": p["namespace"]["ptpl_file"],
                        }));
                    Ok(json!({ "result": 1 }))
                }
                "nvmf_subsystem_add_listener" => {
                    let nqn = p["nqn"].as_str().unwrap();
                    let mut subs = self.subsystems.lock().unwrap();
                    let s = subs.get_mut(nqn).ok_or("no subsystem")?;
                    s["listen_addresses"]
                        .as_array_mut()
                        .unwrap()
                        .push(p["listen_address"].clone());
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_add_host" => {
                    let nqn = p["nqn"].as_str().unwrap();
                    let mut subs = self.subsystems.lock().unwrap();
                    let s = subs.get_mut(nqn).ok_or("no subsystem")?;
                    s["hosts"].as_array_mut().unwrap().push(json!({ "nqn": p["host"] }));
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_remove_host" => {
                    let nqn = p["nqn"].as_str().unwrap();
                    let host = p["host"].as_str().unwrap();
                    let mut subs = self.subsystems.lock().unwrap();
                    let s = subs.get_mut(nqn).ok_or("no subsystem")?;
                    let hosts = s["hosts"].as_array_mut().unwrap();
                    hosts.retain(|h| h["nqn"] != host);
                    Ok(json!({ "result": true }))
                }
                "nvmf_delete_subsystem" => {
                    self.subsystems.lock().unwrap().remove(p["nqn"].as_str().unwrap_or(""));
                    Ok(json!({ "result": true }))
                }
                "nvmf_subsystem_get_controllers" => Ok(json!({ "result": [] })),
                // The construction guard's ublk probe: a tgt built
                // without ublk answers exactly this, and the guard
                // treats it as "no ublk ⇒ no ublk consumer".
                "ublk_get_disks" => Err("Method not found".into()),
                other => Err(format!("unscripted method {other}").into()),
            }
        }
    }

    fn reconciler(tgt: Arc<FakeTgt>) -> BlockExportReconciler {
        BlockExportReconciler::new(
            tgt,
            crate::state_backend::memory_backend(),
            "lvs_test".into(),
            "10.0.0.9".into(),
            4420,
            "/var/tmp".into(),
        )
    }

    #[tokio::test]
    async fn provision_builds_the_whole_chain_with_pinned_nguid() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));
        r.ensure("pvc-1", Some(3 * 1024 * 1024 + 1)).await.expect("provision");

        let calls = tgt.calls.lock().unwrap().clone();
        let create = tgt.call_with_method(&calls, "bdev_lvol_create").expect("lvol created");
        assert_eq!(create["params"]["size_in_mib"], 4, "rounded UP to MiB");
        assert_eq!(create["params"]["thin_provision"], true);

        let sub = tgt.call_with_method(&calls, "nvmf_create_subsystem").expect("subsystem");
        assert_eq!(
            sub["params"]["nqn"],
            "nqn.2024-11.com.flint:block:pvc-1",
            "the :block: namespace, never :volume:"
        );
        assert_eq!(
            sub["params"]["allow_any_host"], false,
            "default-closed even with an empty allow-list"
        );

        let add_ns = tgt.call_with_method(&calls, "nvmf_subsystem_add_ns").expect("ns");
        let (uuid, nguid) = crate::nvmeof_export::stable_ns_identity("pvc-1");
        assert_eq!(add_ns["params"]["namespace"]["nguid"], nguid.as_str());
        assert_eq!(add_ns["params"]["namespace"]["uuid"], uuid.as_str());
        assert_eq!(
            add_ns["params"]["namespace"]["ptpl_file"], "/var/tmp/flint-ptpl-pvc-1.json",
            "PTPL is mandatory-by-kernel: without it the client's CPTPL=PERSIST \
             pr_register gets INVALID_FIELD and no I/O ever leaves the client"
        );

        let listener =
            tgt.call_with_method(&calls, "nvmf_subsystem_add_listener").expect("listener");
        assert_eq!(listener["params"]["listen_address"]["traddr"], "10.0.0.9");
        assert_eq!(listener["params"]["listen_address"]["trsvcid"], "4420");
    }

    #[tokio::test]
    async fn ensure_is_idempotent_no_mutations_second_time() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));
        r.ensure("pvc-2", Some(1024 * 1024)).await.expect("first");
        tgt.calls.lock().unwrap().clear();
        r.ensure("pvc-2", Some(1024 * 1024)).await.expect("second");
        let mutating: Vec<String> = tgt
            .methods()
            .into_iter()
            .filter(|m| !m.starts_with("bdev_get_") && !m.starts_with("nvmf_get_"))
            .collect();
        assert!(mutating.is_empty(), "second ensure mutated: {mutating:?}");
    }

    #[tokio::test]
    async fn reconcile_never_mints_an_lvol_over_a_lost_one() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));
        let err = r.ensure("pvc-3", None).await.expect_err("must refuse");
        assert!(err.contains("MISSING"), "got: {err}");
        assert!(
            tgt.call_with_method(&tgt.calls.lock().unwrap().clone(), "bdev_lvol_create")
                .is_none(),
            "no lvol may be minted on the reconcile path"
        );
    }

    #[tokio::test]
    async fn host_admit_and_evict_converge_the_allow_list() {
        let tgt = Arc::new(FakeTgt::new());
        let backend: Arc<dyn crate::state_backend::StateBackend> =
            Arc::new(crate::state_backend::SqliteBackend::open_in_memory().unwrap());
        let r = BlockExportReconciler::new(
            Arc::clone(&tgt) as Arc<dyn SpdkRpcTransport + Send + Sync>,
            Arc::clone(&backend),
            "lvs_test".into(),
            "10.0.0.9".into(),
            4420,
            "/var/tmp".into(),
        );
        r.ensure("pvc-h", Some(1024 * 1024)).await.expect("provision");
        let nqn = crate::identity::block_volume_export_nqn("pvc-h");
        let mds = crate::identity::block_mds_host_nqn();
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        assert_eq!(
            tgt.hosts_of(&nqn),
            vec![mds.clone()],
            "default-closed: no client admitted, only the MDS fence lane"
        );

        let h1 = crate::nvmeof_export::flint_host_nqn("node-a");
        backend.block_host_admit("pvc-h", 71, &h1, 0).await.unwrap().unwrap();
        r.reconcile_hosts("pvc-h").await.expect("admit converge");
        assert_eq!(sorted(tgt.hosts_of(&nqn)), sorted(vec![h1.clone(), mds.clone()]));

        // Second client on ANOTHER node; evicting the first must not
        // touch the second's admission.
        let h2 = crate::nvmeof_export::flint_host_nqn("node-b");
        backend.block_host_admit("pvc-h", 72, &h2, 0).await.unwrap().unwrap();
        r.reconcile_hosts("pvc-h").await.expect("second admit converge");

        let (evicted, _) = backend.block_host_evict("pvc-h", 71).await.unwrap().unwrap();
        assert_eq!(evicted, vec![h1.clone()]);
        r.reconcile_hosts("pvc-h").await.expect("evict converge");
        assert_eq!(
            sorted(tgt.hosts_of(&nqn)),
            sorted(vec![h2, mds]),
            "evicted h1, kept h2 — and NEVER the fence lane's own admission"
        );
    }

    /// The whole primary-fence seam: converge (which admits the MDS's
    /// fence-lane host), resolve the live nsid, then preempt the
    /// victim's key over a real (scripted) NVMe/TCP conversation.
    #[tokio::test]
    async fn fence_preempt_converges_then_preempts_over_nvme_tcp() {
        let nvme = crate::pnfs::mds::resv_fence::tests::FakeNvmeTarget::spawn().await;
        // The victim client (id 42) registered its pr_key kernel-style.
        nvme.state.lock().unwrap().registrants.push((42, [0xcc; 16], false));

        let tgt = Arc::new(FakeTgt::new());
        let r = BlockExportReconciler::new(
            Arc::clone(&tgt) as Arc<dyn SpdkRpcTransport + Send + Sync>,
            crate::state_backend::memory_backend(),
            "lvs_test".into(),
            nvme.addr.ip().to_string(),
            nvme.addr.port(),
            "/var/tmp".into(),
        );
        r.ensure("pvc-f", Some(1024 * 1024)).await.expect("provision");

        let summary = r.fence_preempt("pvc-f", 42).await.expect("fence converges");
        assert!(summary.contains("preempted=true"), "got: {summary}");
        assert!(summary.contains("holder"), "got: {summary}");

        // The fence lane identified itself as the MDS host on the wire,
        // and its NQN is on the export allow-list it connected through.
        let nqn = crate::identity::block_volume_export_nqn("pvc-f");
        assert!(
            tgt.hosts_of(&nqn).contains(&crate::identity::block_mds_host_nqn()),
            "fence lane host missing from the allow-list"
        );
        let st = nvme.state.lock().unwrap();
        assert!(!st.registrants.iter().any(|(k, _, _)| *k == 42), "victim key gone");
        assert!(
            st.registrants
                .iter()
                .any(|(k, _, h)| *k == crate::identity::BLOCK_MDS_PR_KEY && *h),
            "MDS key holds the reservation"
        );
        assert_eq!(st.rtype, crate::pnfs::mds::resv_fence::RTYPE_EA_REG_ONLY);
    }

    #[tokio::test]
    async fn a_shared_host_nqn_survives_one_clients_eviction() {
        let backend: Arc<dyn crate::state_backend::StateBackend> =
            Arc::new(crate::state_backend::SqliteBackend::open_in_memory().unwrap());
        let shared = crate::nvmeof_export::flint_host_nqn("node-shared");
        backend.block_host_admit("v", 1, &shared, 0).await.unwrap().unwrap();
        backend.block_host_admit("v", 2, &shared, 0).await.unwrap().unwrap();
        let (evicted, remaining) = backend.block_host_evict("v", 1).await.unwrap().unwrap();
        assert_eq!(evicted, vec![shared.clone()]);
        assert_eq!(
            remaining,
            vec![shared],
            "client 2 still holds the node's admission — the NQN must stay desired"
        );
    }

    #[tokio::test]
    async fn delete_tears_down_subsystem_then_lvol_and_tolerates_absence() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));
        r.ensure("pvc-4", Some(1024 * 1024)).await.expect("provision");
        r.delete_volume_export("pvc-4").await.expect("teardown");
        assert!(tgt.subsystems.lock().unwrap().is_empty(), "subsystem gone");
        assert!(tgt.bdevs.lock().unwrap().is_empty(), "lvol gone");
        // Second delete: nothing left, still Ok.
        r.delete_volume_export("pvc-4").await.expect("idempotent teardown");
    }
}
