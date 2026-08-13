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

/// The node whose spdk-tgt this MDS drives, from the pod's own
/// downward-API `spec.nodeName` (chart: `FLINT_NODE_NAME` on the MDS
/// container). The export socket is a shared hostPath, so the MDS pod
/// and the tgt it converges are on the same node BY CONSTRUCTION — the
/// MDS's own node name IS the export node, with no lookup and nothing
/// to drift.
///
/// Empty when the env var is absent (a chart older than this, or a
/// non-Kubernetes rig). Callers must have a fallback — the roller
/// resolves the listener address against the Node objects instead —
/// and must never read "" as "no node".
pub fn export_node_name() -> String {
    std::env::var("FLINT_NODE_NAME").unwrap_or_default()
}

/// This MDS's TARGET ID — the name its spdk-tgt is known by in the
/// `block_targets` registry, and what a volume's seat names as its
/// composer (design §12).
///
/// It identifies the TARGET, not the MDS. Today they resolve to the same
/// string because the export socket is a shared hostPath, so the MDS and
/// the tgt it drives are co-located by construction (see
/// `export_node_name`). When the MDS is un-pinned from the tgt node —
/// owed work, and the whole reason the registry is an indirection — this
/// is the function that learns to name the tgt some other way; nothing
/// downstream changes, because nothing downstream holds an address.
///
/// The fallback is for rigs with no downward API (lima, plain-process
/// runs): a stable per-shard name, never the empty string. A deployment
/// that GAINS `FLINT_NODE_NAME` after seating volumes under the fallback
/// renames its target, and every seat then names a composer with no
/// registry row — a loud refusal at the dial sites, not a silent
/// mis-dial, which is the trade this whole table exists to make.
pub fn target_id() -> String {
    let node = export_node_name();
    if !node.is_empty() {
        return node;
    }
    let shard = std::env::var("FLINT_MDS_SHARD_ID")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    format!("mds-shard-{shard}")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn env_u64(var: &str, default: u64) -> u64 {
    std::env::var(var).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default)
}

/// Consecutive failed probes before a target is called unreachable.
fn verdict_strikes() -> u32 {
    env_u64("FLINT_PNFS_BLOCK_UNREACHABLE_STRIKES", 3) as u32
}

/// Is a failed wire probe evidence of a broken LISTENER ADDRESS rather
/// than of a target that is down? Only when both hold: the target is
/// ours, so we have a second opinion at all, and its process answered
/// that second opinion. A predicate rather than an `if` buried in a log
/// call, because the message makes a claim ("configuration fault, not a
/// dead target") and a claim is worth pinning.
fn listener_is_misconfigured(is_self: bool, admin_ok: bool) -> bool {
    is_self && admin_ok
}

/// How long one target's probe may take. Bounded and short: a
/// partitioned node black-holes, and the pass must not wait out the
/// kernel's SYN retries behind the very node that stopped answering.
fn probe_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(env_u64("FLINT_PNFS_BLOCK_PROBE_TIMEOUT_SECS", 5))
}

/// And the wall-clock floor those strikes must span. BOTH conditions,
/// because a count alone is a statement about loop cadence rather than
/// about the target: the F60 lesson is that a pass's real period is the
/// whole loop's duration, not its `interval`, and a loop that speeds up
/// would otherwise start declaring targets dead faster.
fn verdict_min_secs() -> i64 {
    env_u64("FLINT_PNFS_BLOCK_UNREACHABLE_MIN_SECS", 30) as i64
}

/// THE UNREACHABILITY VERDICT — and the whole point is what it CANNOT
/// say (design §12; `FlintComposition`'s `tgt[t] \in {"part", "dead"}`).
///
/// A target that stops answering this MDS may be dead, or may be
/// perfectly alive and still serving every one of its clients over paths
/// the MDS cannot see. Nothing available here distinguishes those, and
/// the model is built on the assumption that nothing ever will: the
/// composition machine's every belt exists because promotion happens
/// under an unreachability verdict that might be WRONG about death.
/// `Unreachable` therefore names reachability, never liveness, and no
/// consumer may read it as "the target has stopped writing".
///
/// The other half of the exclusion is the composer's own dead-man
/// (`DeadmanGate`), which is the only thing that can reach a partitioned
/// composer's LOCAL leg — owed, and named here so the asymmetry is not
/// mistaken for completeness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    Reachable,
    /// Failing, but not yet for long enough or often enough to say so.
    Suspect { strikes: u32, since_unix: i64 },
    /// The verdict. Deliberately fallible; see the type's doc.
    Unreachable { strikes: u32, since_unix: i64 },
}

impl Reachability {
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable { .. })
    }
}

#[derive(Debug, Clone, Default)]
struct ProbeState {
    strikes: u32,
    first_fail_unix: i64,
    last_ok_unix: i64,
}

/// The result of one promotion attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionOutcome {
    Promoted { from: String, to: String, epoch: i64 },
    /// The election gate had nobody to elect. Carries WHY, because on a
    /// single-replica volume this is the permanent and correct answer.
    NoCandidate { reason: String },
    /// The CAS lost — the seat had already moved.
    Raced { epoch: i64, composer: String },
    Refused(String),
}

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
    /// Probe history per TARGET, keyed by target id even though this
    /// MDS probes exactly one today. A scalar here would be an assertion
    /// that there is only ever one target — the A2 tranche's lesson,
    /// where a scalar `raidHost` made "two compositions exist"
    /// unrepresentable and a green run meaningless.
    probes: dashmap::DashMap<String, ProbeState>,
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
            probes: dashmap::DashMap::new(),
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

    /// This reconciler's OWN listener coordinates — where the tgt it
    /// drives answers. Configuration, not a routing decision: use it to
    /// describe this node (self-registration, `BlockExportStatus`), and
    /// `listener_for` to answer "where does THIS VOLUME live", which is
    /// a question only the record may answer.
    pub fn listener(&self) -> (&str, u16) {
        (&self.traddr, self.trsvcid)
    }

    /// Announce this target's coordinates in the registry (design §12).
    /// Idempotent and level-triggered — called at MDS start and on every
    /// reconcile pass, so a chart change to the listener converges with
    /// no operator step and a target returning on a new address updates
    /// its own row.
    pub async fn self_register(&self) -> Result<(), String> {
        let id = target_id();
        match self
            .backend
            .block_target_register(&id, &self.traddr, self.trsvcid, now_unix())
            .await
        {
            Ok(Ok(())) => {
                tracing::debug!(
                    "block target '{}' registered at {}:{}",
                    id,
                    self.traddr,
                    self.trsvcid
                );
                Ok(())
            }
            Ok(Err(e)) => Err(format!("target registration refused: {e}")),
            Err(e) => Err(format!("target registration failed: {e}")),
        }
    }

    /// Seat a volume at THIS target if it has no seat, and return the
    /// seat that stands. Provision-time only: `INSERT`-if-absent, so a
    /// seat naming someone else comes back unchanged and the caller
    /// refuses rather than adopting a volume by writing a row. Moving a
    /// seat is promotion's job.
    async fn seat_here(
        &self,
        volume: &str,
    ) -> Result<crate::state_backend::extent_alloc::BlockSeat, String> {
        let me = target_id();
        let seat = match self.backend.block_seat_volume(volume, &me, now_unix()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(format!("seating '{volume}' refused: {e}")),
            Err(e) => return Err(format!("seating '{volume}' failed: {e}")),
        };
        if seat.composer != me {
            return Err(format!(
                "'{}' is already seated at composer '{}' (epoch {}) — this target is '{}' and \
                 will not adopt a volume the record gives to someone else",
                volume, seat.composer, seat.epoch, me
            ));
        }
        Ok(seat)
    }

    /// THE RESOLUTION: where does this volume's target answer? Reads the
    /// seat and the composer's registry row — never the constructor's
    /// address.
    ///
    /// `FlintCompositionStaticTraddr.cfg` is why: aim the preempt at a
    /// configured address instead of at what the record names and every
    /// post-failover fence confirmation dials a dead node forever, so
    /// `delivered_unix` stays 0 and the quarantine sweep's ranges park
    /// permanently. A fallback here would restore that lasso exactly, so
    /// there is none — an unresolvable volume is a refusal.
    pub async fn resolve(
        &self,
        volume: &str,
    ) -> Result<(crate::state_backend::extent_alloc::BlockSeat, String, u16), String> {
        match self.backend.block_resolve_target(volume).await {
            Ok(Ok((seat, target))) => {
                let (traddr, trsvcid) = (target.traddr, target.trsvcid);
                Ok((seat, traddr, trsvcid))
            }
            Ok(Err(e)) => Err(format!(
                "cannot resolve the target serving '{volume}': {e}"
            )),
            Err(e) => Err(format!("target resolution for '{volume}' failed: {e}")),
        }
    }

    /// What `AttachBlockNode` hands a csi-node for its `nvme connect` —
    /// the volume's OWN listener, resolved through the record.
    pub async fn listener_for(&self, volume: &str) -> Result<(String, u16), String> {
        let (_seat, traddr, trsvcid) = self.resolve(volume).await?;
        Ok((traddr, trsvcid))
    }

    /// Record one observation of a target and return the verdict that
    /// now stands. THE single mutation of probe history: `probe_target`
    /// calls it for the local tgt, and the remote prober the failover
    /// work owes will call it for everyone else — so there is one place
    /// where a verdict can be reached, and one set of thresholds.
    ///
    /// A success resets everything. Strikes must clear BOTH the count
    /// and the wall-clock floor; see `verdict_min_secs`.
    pub fn observe(&self, target_id: &str, ok: bool, now_unix: i64) -> Reachability {
        let mut st = self.probes.entry(target_id.to_string()).or_default();
        if ok {
            st.strikes = 0;
            st.first_fail_unix = 0;
            st.last_ok_unix = now_unix;
            return Reachability::Reachable;
        }
        if st.strikes == 0 {
            st.first_fail_unix = now_unix;
        }
        st.strikes = st.strikes.saturating_add(1);
        let since = st.first_fail_unix;
        if st.strikes >= verdict_strikes() && now_unix - since >= verdict_min_secs() {
            Reachability::Unreachable { strikes: st.strikes, since_unix: since }
        } else {
            Reachability::Suspect { strikes: st.strikes, since_unix: since }
        }
    }

    /// The verdict standing for a target, without probing. `None` = never
    /// observed, which is NOT "reachable": a target this MDS has never
    /// heard from is not a target it may promote a volume onto.
    pub fn reachability(&self, target_id: &str) -> Option<Reachability> {
        self.reachability_at(target_id, now_unix())
    }

    /// `reachability` with the clock passed in — the wall-clock floor is
    /// half the verdict, so anything that judges must be able to say
    /// WHEN it is judging.
    pub fn reachability_at(&self, target_id: &str, now_unix: i64) -> Option<Reachability> {
        let st = self.probes.get(target_id)?;
        if st.strikes == 0 {
            return Some(Reachability::Reachable);
        }
        let since = st.first_fail_unix;
        Some(
            if st.strikes >= verdict_strikes() && now_unix - since >= verdict_min_secs() {
                Reachability::Unreachable { strikes: st.strikes, since_unix: since }
            } else {
                Reachability::Suspect { strikes: st.strikes, since_unix: since }
            },
        )
    }

    /// Probe ONE target at the coordinates the registry holds for it,
    /// and fold the result into its verdict.
    ///
    /// The probe is the DATA path (`resv_fence::probe_nvme_tcp`), and
    /// uniformly so — for this MDS's own target exactly as for a remote
    /// one. That uniformity is the point: "reachable" has to mean one
    /// thing, and the thing the verdict licenses is a decision about who
    /// SERVES a volume. A control-plane probe over the local RPC socket
    /// answers a different question — whether the tgt can still be
    /// administered — and a target whose process is fine while its nvmf
    /// listener is wedged would pass it while serving nobody. Asking
    /// each target a different question would make the verdict's meaning
    /// depend on which target it looked at.
    ///
    /// The local RPC socket does keep one job here, as a DIAGNOSTIC: if
    /// our own listener does not answer while the process does, the
    /// configured `traddr` is not reachable from this MDS — and it is
    /// the address every csi-node dials, so that is a live
    /// misconfiguration and worth naming rather than leaving as a
    /// mysterious verdict. It never overrides the verdict: the address
    /// really is broken.
    pub async fn probe_one(&self, target_id_: &str, traddr: &str, trsvcid: u16) -> Reachability {
        let res = super::resv_fence::probe_nvme_tcp(traddr, trsvcid, probe_timeout()).await;
        if let Err(ref why) = res {
            let admin_ok = if target_id_ == target_id() {
                self.rpc
                    .rpc(&json!({ "method": "spdk_get_version", "params": {} }))
                    .await
                    .is_ok()
            } else {
                // Not ours to administer; no second opinion available.
                false
            };
            if listener_is_misconfigured(target_id_ == target_id(), admin_ok) {
                tracing::error!(
                    "target '{}' answers its RPC socket but NOT its own listener {}:{} ({}) — \
                     that is the address every csi-node dials, so this is a configuration \
                     fault, not a dead target",
                    target_id_,
                    traddr,
                    trsvcid,
                    why
                );
            }
        }
        self.observe(target_id_, res.is_ok(), now_unix())
    }

    /// THE REMOTE PROBER: probe every registered target, concurrently,
    /// and return the verdict standing for each.
    ///
    /// Concurrent because the timeout is the cost: a partitioned target
    /// costs a full `probe_timeout()`, and probing serially would make
    /// the pass's duration proportional to how many targets are down —
    /// which is backwards, since that is exactly when the pass has work
    /// to do.
    ///
    /// A target with no registry row is not probed and gets no verdict,
    /// which is the correct answer rather than a gap: this MDS has no
    /// address for it, and inventing one is what the registry exists to
    /// prevent.
    pub async fn probe_all_targets(&self) -> Vec<(String, Reachability)> {
        let targets = match self.backend.block_target_list().await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                tracing::error!("target registry unreadable, no verdicts this pass: {e}");
                return Vec::new();
            }
            Err(e) => {
                tracing::error!("target registry unreadable, no verdicts this pass: {e}");
                return Vec::new();
            }
        };
        let probes = targets.into_iter().map(|t| async move {
            let v = self.probe_one(&t.target_id, &t.traddr, t.trsvcid).await;
            (t.target_id, v)
        });
        futures::future::join_all(probes).await
    }

    /// Every seat this MDS holds, paired with whether its composer is
    /// registered — the startup audit's input. Reported rather than
    /// repaired: a seat naming an unknown composer is a fact an operator
    /// must see, and inventing a registration for it would be the
    /// adoption this whole mechanism refuses.
    pub async fn seat_audit(
        &self,
    ) -> Result<Vec<(crate::state_backend::extent_alloc::BlockSeat, bool)>, String> {
        let seats = match self.backend.block_seat_list().await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(format!("seat list refused: {e}")),
            Err(e) => return Err(format!("seat list failed: {e}")),
        };
        let targets = match self.backend.block_target_list().await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(format!("target list refused: {e}")),
            Err(e) => return Err(format!("target list failed: {e}")),
        };
        let known: std::collections::HashSet<&str> =
            targets.iter().map(|t| t.target_id.as_str()).collect();
        Ok(seats
            .into_iter()
            .map(|s| {
                let ok = known.contains(s.composer.as_str());
                (s, ok)
            })
            .collect())
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

    /// The backing lvolstore's totals, straight from SPDK
    /// (`bdev_lvol_get_lvstores`): `(total_bytes, free_bytes)`.
    ///
    /// `None` when the store cannot be read. Callers treat that as
    /// "unknown", never "empty" — refusing every provision because one
    /// RPC blipped is worse than the oversubscription it guards.
    async fn lvstore_totals(&self) -> Option<(u64, u64)> {
        let resp = self
            .rpc
            .rpc(&json!({
                "method": "bdev_lvol_get_lvstores",
                "params": { "lvs_name": self.lvstore }
            }))
            .await
            .ok()?;
        // Find OUR store by name rather than taking the first entry:
        // the driver's RPC shim ignores the `lvs_name` filter and hands
        // back every lvstore, so trusting position would size the gate
        // against somebody else's store.
        let s = resp
            .get("result")
            .and_then(|r| r.as_array())?
            .iter()
            .find(|s| s.get("name").and_then(|v| v.as_str()) == Some(self.lvstore.as_str()))?;
        let cluster = s.get("cluster_size").and_then(|v| v.as_u64())?;
        // SPDK spells it `total_data_clusters`; the shim also emits
        // `total_clusters`. Accept either — reading only one of them is
        // how this number came back 0 and silently disabled the gate on
        // its first rig run.
        let total = s
            .get("total_data_clusters")
            .or_else(|| s.get("total_clusters"))
            .and_then(|v| v.as_u64())
            .filter(|t| *t > 0)?;
        let free = s.get("free_clusters").and_then(|v| v.as_u64())?;
        Some((total.checked_mul(cluster)?, free.checked_mul(cluster)?))
    }

    /// Bytes this store has already PROMISED: the sum of every lvol's
    /// logical size, read from one `bdev_get_bdevs` and filtered to this
    /// lvolstore by alias.
    ///
    /// Logical, not allocated, and that distinction is the whole gate.
    /// `free_clusters` cannot answer this question: a thin lvol consumes
    /// no clusters until it is written, so ten 1 GiB volumes on a 1 GiB
    /// store leave the store reporting itself nearly empty right up
    /// until the writes land. The oversubscription is invisible in the
    /// physical numbers and visible in the logical ones.
    async fn committed_logical_bytes(&self) -> Option<u64> {
        let resp = self
            .rpc
            .rpc(&json!({ "method": "bdev_get_bdevs" }))
            .await
            .ok()?;
        let prefix = format!("{}/", self.lvstore);
        let mut sum = 0u64;
        for b in resp.get("result")?.as_array()? {
            let mine = b
                .get("aliases")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .any(|v| v.starts_with(&prefix))
                })
                .unwrap_or(false);
            if !mine {
                continue;
            }
            let bs = b.get("block_size").and_then(|v| v.as_u64()).unwrap_or(0);
            let nb = b.get("num_blocks").and_then(|v| v.as_u64()).unwrap_or(0);
            sum = sum.saturating_add(bs.saturating_mul(nb));
        }
        Some(sum)
    }

    /// Is the operator deliberately overcommitting the lvolstore?
    ///
    /// Thin provisioning makes logical-beyond-physical *possible*, and
    /// some fleets want it. Default OFF because the failure it permits
    /// is silent and lands on the application, not the operator: the PVC
    /// reports its full size and the write fails at the device. Loud
    /// when set, for the same reason the kernel-floor override is.
    fn overcommit_allowed() -> bool {
        std::env::var("FLINT_PNFS_BLOCK_OVERCOMMIT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    /// Refuse to promise capacity the lvolstore does not have.
    ///
    /// `need` is the NEW promise — a create's full size, a grow's DELTA
    /// — added to what the store has already promised. SPDK will not
    /// make this check for us: `blob_resize` skips its free-cluster
    /// check entirely for thin blobs (`lib/blob/blobstore.c:2292`,
    /// `spdk_blob_is_thin_provisioned(blob) == false`), and every flint
    /// block volume is thin. So without this, a create or grow beyond
    /// the store's capacity SUCCEEDS at the device, the arena ceiling
    /// follows it, the PVC reports the full size, and the application
    /// discovers the truth at write time as an lvol-level failure
    /// instead of the honest NFS4ERR_NOSPC the arena would have given.
    async fn check_store_has_room(&self, volume: &str, need: u64) -> Result<(), String> {
        if need == 0 {
            return Ok(());
        }
        let (Some((total, free)), Some(committed)) =
            (self.lvstore_totals().await, self.committed_logical_bytes().await)
        else {
            tracing::warn!(
                "📏 block volume '{}': lvolstore '{}' capacity UNREADABLE — proceeding \
                 without the capacity gate (a write may still hit the store's real limit)",
                volume, self.lvstore
            );
            return Ok(());
        };
        let after = committed.saturating_add(need);
        if after <= total {
            tracing::info!(
                "📏 block volume '{}': lvolstore '{}' promises {} of {} bytes after this \
                 {} request ({} physically free)",
                volume, self.lvstore, after, total, need, free
            );
            return Ok(());
        }
        if Self::overcommit_allowed() {
            tracing::warn!(
                "📏 block volume '{}': OVERCOMMITTING lvolstore '{}' — {} promised of {} \
                 after a {} request (FLINT_PNFS_BLOCK_OVERCOMMIT=1). The PVC will report \
                 its full size and a write past the store's real capacity fails at the \
                 device, NOT as ENOSPC",
                volume, self.lvstore, after, total, need
            );
            return Ok(());
        }
        Err(format!(
            "lvolstore '{}' has already promised {} of {} bytes and this needs {} more — \
             refusing rather than handing out capacity the store does not have. SPDK will \
             NOT stop this on its own (a thin blob's resize skips the free-cluster check), \
             so the volume would report its full size and fail at WRITE time instead. Grow \
             the lvolstore, delete a volume, or set FLINT_PNFS_BLOCK_OVERCOMMIT=1 to accept \
             the risk deliberately",
            self.lvstore, committed, total, need
        ))
    }

    /// A live bdev's capacity in bytes, from its own `bdev_get_bdevs`
    /// record. Read back rather than computed from what we asked for:
    /// lvol sizes round up to the lvolstore's cluster size, so the
    /// device is usually BIGGER than the request, and the number the
    /// allocator's ceiling is checked against has to be the real one.
    fn bdev_capacity(resp: &serde_json::Value) -> Option<u64> {
        let b = resp.get("result").and_then(|r| r.as_array())?.first()?;
        let block_size = b.get("block_size").and_then(|v| v.as_u64())?;
        let num_blocks = b.get("num_blocks").and_then(|v| v.as_u64())?;
        block_size.checked_mul(num_blocks)
    }

    /// Grow the volume's lvol to at least `new_size_bytes` — the DEVICE
    /// half of a CSI expand, and the half that must land first (the
    /// allocator's ceiling may never outrun the namespace; see
    /// `extent_alloc::expand_volume`). Returns the capacity in force
    /// afterwards.
    ///
    /// Idempotent: a device already big enough is left alone, so a
    /// re-driven ExpandVolume costs one RPC and changes nothing.
    ///
    /// The namespace follows for free — SPDK turns the bdev resize into
    /// `nvmf_ns_resize`, which bumps the namespace and sends the
    /// ns-changed AEN, and connected kernels rescan (v26.05
    /// `lib/nvmf/subsystem.c`; growth needs no I/O quiesce, which is why
    /// this is safe under live initiators). Shrinking has no path here
    /// at all — CSI forbids it and the extents would be unaddressable.
    pub async fn grow(&self, volume: &str, new_size_bytes: u64) -> Result<u64, String> {
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        let bdev = self.bdev_name(volume);
        let probe = json!({ "method": "bdev_get_bdevs", "params": { "name": bdev } });

        let before = self
            .rpc
            .rpc(&probe)
            .await
            .map_err(|e| format!("bdev_get_bdevs {bdev}: {e}"))?;
        let current = Self::bdev_capacity(&before).ok_or_else(|| {
            format!("bdev_get_bdevs {bdev}: reply carries no block_size/num_blocks")
        })?;
        if current >= new_size_bytes {
            return Ok(current);
        }

        // The store must be able to fund the GROWTH. Checked before the
        // resize because after it the promise is already made: SPDK
        // accepts a thin resize regardless of free clusters, so there is
        // no later point at which the truth surfaces except a failing
        // write in the application.
        self.check_store_has_room(volume, new_size_bytes - current).await?;

        let size_mib = new_size_bytes.div_ceil(1024 * 1024).max(1);
        let resize = json!({
            "method": "bdev_lvol_resize",
            "params": { "name": bdev, "size_in_mib": size_mib }
        });
        self.rpc
            .rpc(&resize)
            .await
            .map_err(|e| format!("bdev_lvol_resize {bdev} to {size_mib} MiB: {e}"))?;

        let after = self
            .rpc
            .rpc(&probe)
            .await
            .map_err(|e| format!("bdev_get_bdevs {bdev} (post-resize): {e}"))?;
        let grown = Self::bdev_capacity(&after).ok_or_else(|| {
            format!("bdev_get_bdevs {bdev} (post-resize): reply carries no block_size/num_blocks")
        })?;
        // Belt: an acked resize that did not actually grow the device
        // must not become a raised ceiling. Refusing here leaves the
        // volume at its old (working) size for the resizer to retry.
        if grown < new_size_bytes {
            return Err(format!(
                "lvol {bdev} is {grown} bytes after a resize to {new_size_bytes} — \
                 refusing to raise the allocation ceiling past the device"
            ));
        }
        tracing::info!(
            "📏 block volume '{}': lvol grown {} → {} bytes (namespace follows via \
             the SPDK resize AEN)",
            volume, current, grown
        );
        Ok(grown)
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
        // ---- the seat, BEFORE any device state (design §12) ----
        //
        // Provision seats the volume here; reconcile requires a seat
        // that already names this target. The order matters: seat then
        // converge means a crash in between leaves a seat with no
        // export, which the next pass converges. The reverse would leave
        // an export no record names — a target serving bytes nobody
        // elected it to serve.
        //
        // The reconcile-side refusal is `RecordAssemblyOnly` shipped
        // early and for free. Its mutation (`FlintCompositionAssembly.
        // cfg`) is the healed composer whose reconciler re-converges the
        // same subnqn and NGUID over its stale leg and serves it; the
        // record is the only door, and this is the door. It cannot fire
        // today (every volume is seated where it was provisioned), which
        // is exactly why it should exist before promotion can move a
        // seat rather than after.
        // Note this reads the SEAT, not the full resolution: converging
        // needs to know WHO serves the volume, never WHERE — this
        // reconciler can only ever configure the tgt on the other end of
        // its own socket. Keeping the address out of the converge path
        // is what keeps the registry needed exactly where an address is
        // needed (the fence lane, the attach answer) and nowhere else.
        let me = target_id();
        let seat = if size_bytes.is_some() {
            // Announce before seating: the volume is about to be seated
            // at this target, and the very next thing a new volume gets
            // is a ControllerPublish that must resolve an address. The
            // reconcile pass would get there eventually; a CreateVolume
            // immediately followed by an attach would not wait for it.
            self.self_register().await?;
            self.seat_here(volume).await?
        } else {
            match self.backend.block_volume_seat(volume).await {
                Ok(Ok(Some(seat))) => seat,
                Ok(Ok(None)) => {
                    return Err(format!(
                        "refusing to converge '{volume}': no serving-target seat — this target \
                         ('{me}') does not serve a volume the record does not give it"
                    ))
                }
                Ok(Err(e)) => return Err(format!("seat lookup for '{volume}' refused: {e}")),
                Err(e) => return Err(format!("seat lookup for '{volume}' failed: {e}")),
            }
        };
        if seat.composer != me {
            return Err(format!(
                "refusing to converge '{}': the record seats it at composer '{}' (epoch {}), \
                 not at this target ('{}')",
                volume, seat.composer, seat.epoch, me
            ));
        }

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
            self.check_store_has_room(volume, bytes).await?;
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
        tracing::debug!("fence_preempt {}: converging export", volume);
        self.ensure_locked(volume, None).await?;
        tracing::debug!("fence_preempt {}: converged, resolving nsid", volume);
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
        // WHERE to preempt comes from the record, never from this
        // reconciler's own configuration — see `resolve`. The epoch
        // rides into the log so a fence can be read against the
        // composition it was aimed at.
        let (seat, traddr, trsvcid) = self.resolve(volume).await?;
        tracing::debug!(
            "fence_preempt {}: nsid={}, opening NVMe session to composer '{}' at {}:{} (epoch {})",
            volume,
            nsid,
            seat.composer,
            traddr,
            trsvcid,
            seat.epoch
        );
        let ep = super::resv_fence::ResvEndpoint {
            traddr,
            trsvcid,
            subnqn: nqn,
            hostnqn: crate::identity::block_mds_host_nqn(),
            hostid: crate::identity::BLOCK_MDS_HOST_ID,
            nsid: nsid as u32,
        };
        let out = ep
            .fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, victim_key)
            .await?;
        Ok(format!(
            "victim={victim_key:#x} preempted={} (registered={} acquired={}) \
             composer={} epoch={} resv: {}",
            out.preempted,
            out.registered,
            out.acquired,
            seat.composer,
            seat.epoch,
            out.after.summary()
        ))
    }

    /// The fence's inverse (the UnfenceBlockClient release arm): drop
    /// the EA-RO reservation the MDS holds on the volume's namespace,
    /// so non-registrant I/O — every kernel blocklayout client — flows
    /// again. The caller decides WHETHER releasing is safe (no other
    /// client still fenced on the volume); this method only delivers
    /// it. Same converge-first shape as `fence_preempt`: the fence
    /// lane's host must be on the allow-list to connect.
    pub async fn fence_release(&self, volume: &str) -> Result<String, String> {
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        self.ensure_locked(volume, None).await?;
        let nqn = crate::identity::block_volume_export_nqn(volume);
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
            .ok_or_else(|| format!("{} carries no namespace to release on", nqn))?;
        // Record-driven for the same reason the preempt is: releasing at
        // a stale address would report a clean unfence while the
        // reservation that actually excludes the client stands at the
        // composer the record names.
        let (seat, traddr, trsvcid) = self.resolve(volume).await?;
        let ep = super::resv_fence::ResvEndpoint {
            traddr,
            trsvcid,
            subnqn: nqn,
            hostnqn: crate::identity::block_mds_host_nqn(),
            hostid: crate::identity::BLOCK_MDS_HOST_ID,
            nsid: nsid as u32,
        };
        let out = ep.release(crate::identity::BLOCK_MDS_PR_KEY).await?;
        Ok(format!(
            "released={} composer={} epoch={} resv: {}",
            out.released,
            seat.composer,
            seat.epoch,
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

    /// What one promotion attempt did. Every non-`Promoted` arm is a
    /// REFUSAL with its reason kept, because the reasons are the
    /// interesting part today: a single-copy volume has no candidate,
    /// and saying so plainly is `WaitsPrice`'s bill arriving in the log
    /// instead of in a design doc.
    pub async fn attempt_promotion(&self, volume: &str) -> PromotionOutcome {
        let seat = match self.backend.block_volume_seat(volume).await {
            Ok(Ok(Some(s))) => s,
            Ok(Ok(None)) => return PromotionOutcome::Refused("no seat".into()),
            Ok(Err(e)) => return PromotionOutcome::Refused(format!("seat read refused: {e}")),
            Err(e) => return PromotionOutcome::Refused(format!("seat read failed: {e}")),
        };
        // Never promote away from a composer this MDS has not actually
        // condemned: the model's CAS is guarded on the verdict, and a
        // promotion without one is a failover invented out of nothing.
        match self.reachability(&seat.composer) {
            Some(v) if v.is_unreachable() => {}
            other => {
                return PromotionOutcome::Refused(format!(
                    "composer '{}' is not under an unreachability verdict ({:?})",
                    seat.composer, other
                ))
            }
        }

        let legs = match self.backend.block_legs(volume).await {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => return PromotionOutcome::Refused(format!("legs unreadable: {e}")),
            Err(e) => return PromotionOutcome::Refused(format!("legs unreadable: {e}")),
        };
        let in_sync: Vec<&crate::state_backend::extent_alloc::BlockLeg> = legs
            .iter()
            .filter(|l| {
                l.target_id != seat.composer
                    && l.sync_state == crate::state_backend::extent_alloc::LEG_INSYNC
            })
            .collect();
        if in_sync.is_empty() {
            // The honest single-replica answer, and the degraded-volume
            // answer too: `ElectInSync` refuses a stale survivor, so the
            // volume waits rather than serving a leg that is missing
            // acked bytes. Availability spent on durability.
            return PromotionOutcome::NoCandidate {
                reason: format!(
                    "no in-sync leg other than '{}' ({} leg(s) recorded)",
                    seat.composer,
                    legs.len()
                ),
            };
        }
        // A candidate must be one this MDS has affirmatively heard from.
        // "Never observed" is not "fine": promoting onto a target whose
        // reachability is unknown is how a volume lands somewhere that
        // cannot serve it.
        let Some(candidate) = in_sync
            .iter()
            .find(|l| matches!(self.reachability(&l.target_id), Some(Reachability::Reachable)))
        else {
            return PromotionOutcome::NoCandidate {
                reason: format!(
                    "in-sync leg(s) {:?} exist, but none is affirmatively reachable — a remote \
                     target prober is owed",
                    in_sync.iter().map(|l| &l.target_id).collect::<Vec<_>>()
                ),
            };
        };

        match self
            .backend
            .block_promote(volume, seat.epoch, &seat.composer, &candidate.target_id, now_unix())
            .await
        {
            Ok(Ok(new_seat)) => PromotionOutcome::Promoted {
                from: seat.composer,
                to: new_seat.composer,
                epoch: new_seat.epoch,
            },
            Ok(Err(crate::state_backend::extent_alloc::ExtentAllocError::PromotionRaced {
                epoch,
                composer,
            })) => PromotionOutcome::Raced { epoch, composer },
            Ok(Err(e)) => PromotionOutcome::Refused(format!("CAS refused: {e}")),
            Err(e) => PromotionOutcome::Refused(format!("CAS failed: {e}")),
        }
    }

    /// MDS-start replay: converge every known block-class volume. Runs
    /// with `size_bytes: None` (never mints an lvol — see `ensure`).
    /// Failures are LOUD but non-fatal: a down tgt must not crashloop
    /// the MDS out of serving its file-class volumes. A tgt restart
    /// while the MDS stays up is NOT covered here — that needs the
    /// periodic reconcile loop (next tranche); until then the runbook's
    /// answer is an MDS rollout after any tgt restart.
    pub async fn reconcile_all(&self, volumes: &[String]) {
        // Announce first: the registry is level-triggered like everything
        // else here, so a listener change in the chart converges on the
        // next pass rather than needing an operator. Once per pass, not
        // once per volume — the row is about this target, not about any
        // volume.
        if let Err(e) = self.self_register().await {
            tracing::error!(
                "block target registration failed: {} — fences and attach answers for this \
                 target's volumes cannot resolve an address until this succeeds",
                e
            );
        }
        for v in volumes {
            if let Err(e) = self.ensure(v, None).await {
                tracing::error!(
                    "block-export reconcile of '{}' failed: {} — the volume's clients \
                     cannot connect until this converges",
                    v,
                    e
                );
            } else {
                // debug, not info: this line is per-volume per-pass and
                // the periodic reconcile loop runs it every tick.
                tracing::debug!("block-export reconcile of '{}' converged", v);
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
        /// alias → capacity in bytes. Real lvols round UP to the
        /// lvolstore's cluster size, so the expand path must read the
        /// device's own number back rather than trust its arithmetic —
        /// this map is what lets a test prove it does.
        pub(crate) bdev_bytes: Mutex<std::collections::HashMap<String, u64>>,
        pub(crate) subsystems: Mutex<std::collections::HashMap<String, Value>>,
        /// Free clusters the fake lvolstore reports. `None` = the
        /// `bdev_lvol_get_lvstores` RPC FAILS, which is the "unknown"
        /// case the capacity gate must not read as "empty".
        pub(crate) free_clusters: Mutex<Option<u64>>,
        pub(crate) total_clusters: Mutex<u64>,
    }

    /// The fake lvolstore's cluster size (SPDK's default is 4 MiB).
    pub(crate) const FAKE_CLUSTER: u64 = 4 * 1024 * 1024;

    /// The block size the fake reports; 4 KiB, as an lvolstore on a
    /// modern NVMe namespace does.
    const FAKE_BLOCK_SIZE: u64 = 4096;

    impl FakeTgt {
        pub(crate) fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                bdevs: Mutex::new(Default::default()),
                bdev_bytes: Mutex::new(Default::default()),
                subsystems: Mutex::new(Default::default()),
                // Roomy by default so every pre-existing test keeps its
                // exact behaviour; the gate's own tests set it.
                free_clusters: Mutex::new(Some(1 << 20)),
                total_clusters: Mutex::new(1 << 20),
            }
        }
        /// Size the fake store: `(total_clusters, free_clusters)`.
        pub(crate) fn set_store(&self, total: u64, free: u64) {
            *self.total_clusters.lock().unwrap() = total;
            *self.free_clusters.lock().unwrap() = Some(free);
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
                // The reachability probe: cheapest proof the process is
                // answering, and it asserts nothing about its state.
                "spdk_get_version" => Ok(json!({ "result": { "version": "SPDK v26.05" } })),
                "bdev_get_bdevs" => {
                    let bdevs = self.bdevs.lock().unwrap();
                    // No `name` = list everything (what the capacity
                    // gate uses to sum promised logical bytes).
                    let Some(name) = p["name"].as_str() else {
                        let sizes = self.bdev_bytes.lock().unwrap();
                        let all: Vec<Value> = bdevs
                            .iter()
                            .map(|(alias, canonical)| {
                                let bytes = sizes.get(alias).copied().unwrap_or(0);
                                json!({
                                    "name": canonical,
                                    "uuid": canonical,
                                    "aliases": [alias],
                                    "block_size": FAKE_BLOCK_SIZE,
                                    "num_blocks": bytes / FAKE_BLOCK_SIZE,
                                })
                            })
                            .collect();
                        return Ok(json!({ "result": all }));
                    };
                    let bytes =
                        self.bdev_bytes.lock().unwrap().get(name).copied().unwrap_or(0);
                    match bdevs.get(name) {
                        Some(canonical) => Ok(json!({ "result": [{
                            "name": canonical,
                            "uuid": canonical,
                            "aliases": [name],
                            "block_size": FAKE_BLOCK_SIZE,
                            "num_blocks": bytes / FAKE_BLOCK_SIZE,
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
                    self.bdevs.lock().unwrap().insert(alias.clone(), canonical.clone());
                    let mib = p["size_in_mib"].as_u64().unwrap_or(0);
                    self.bdev_bytes.lock().unwrap().insert(alias, mib * 1024 * 1024);
                    Ok(json!({ "result": canonical }))
                }
                "bdev_lvol_get_lvstores" => {
                    match *self.free_clusters.lock().unwrap() {
                        // Two entries, and the caller's filter is
                        // IGNORED — exactly what the driver's RPC shim
                        // does, so the lookup-by-name is exercised
                        // rather than assumed.
                        Some(free) => Ok(json!({ "result": [
                            {
                                "name": "someone-elses-store",
                                "cluster_size": FAKE_CLUSTER,
                                "free_clusters": 0u64,
                                "total_data_clusters": 1u64,
                            },
                            {
                                "name": "lvs_test",
                                "cluster_size": FAKE_CLUSTER,
                                "free_clusters": free,
                                "total_data_clusters": *self.total_clusters.lock().unwrap(),
                            }
                        ]})),
                        None => Err("lvolstore unreadable".into()),
                    }
                }
                "bdev_lvol_resize" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    if !self.bdevs.lock().unwrap().contains_key(&name) {
                        return Err("No such device".into());
                    }
                    let mib = p["size_in_mib"].as_u64().unwrap_or(0);
                    self.bdev_bytes.lock().unwrap().insert(name, mib * 1024 * 1024);
                    Ok(json!({ "result": true }))
                }
                "bdev_lvol_delete" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    self.bdevs.lock().unwrap().remove(&name);
                    self.bdev_bytes.lock().unwrap().remove(&name);
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

    /// The device half of a CSI expand: the lvol grows, rounded UP to
    /// MiB, and the capacity reported back is the DEVICE's own.
    #[tokio::test]
    async fn grow_resizes_the_lvol_and_reports_the_device_capacity() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));
        r.ensure("pvc-1", Some(4 * 1024 * 1024)).await.expect("provision");
        tgt.calls.lock().unwrap().clear();

        let got = r.grow("pvc-1", 10 * 1024 * 1024 + 1).await.expect("grow");
        let calls = tgt.calls.lock().unwrap().clone();
        let resize = tgt.call_with_method(&calls, "bdev_lvol_resize").expect("resized");
        assert_eq!(resize["params"]["name"], "lvs_test/pvc-1");
        assert_eq!(resize["params"]["size_in_mib"], 11, "rounded UP to MiB");
        assert_eq!(got, 11 * 1024 * 1024, "the DEVICE's capacity, not the request");
    }

    /// Idempotent: a re-driven ExpandVolume (external-resizer does this
    /// freely) must not re-resize a device that is already big enough.
    #[tokio::test]
    async fn grow_is_a_no_op_when_the_device_already_fits() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));
        r.ensure("pvc-1", Some(16 * 1024 * 1024)).await.expect("provision");
        tgt.calls.lock().unwrap().clear();

        let got = r.grow("pvc-1", 8 * 1024 * 1024).await.expect("grow");
        assert_eq!(got, 16 * 1024 * 1024);
        assert!(
            !tgt.methods().contains(&"bdev_lvol_resize".to_string()),
            "a device that already fits must not be resized: {:?}",
            tgt.methods()
        );
    }

    /// A missing lvol is a hard error, never a silent success — the
    /// ceiling must not move for a device that isn't there.
    #[tokio::test]
    async fn grow_refuses_when_the_lvol_is_absent() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));
        let e = r.grow("pvc-ghost", 1 << 20).await.expect_err("must refuse");
        assert!(e.contains("bdev_get_bdevs"), "got: {e}");
    }

    /// THE CAPACITY GATE. SPDK will not stop an oversubscribed thin
    /// provision on its own — `blob_resize` skips its free-cluster check
    /// entirely for thin blobs — so a create or grow the store cannot
    /// fund succeeds at the device and fails later, in the application,
    /// as an lvol-level error instead of ENOSPC. The gate asks SPDK the
    /// question SPDK declines to ask itself.
    #[tokio::test]
    async fn create_refuses_what_the_lvolstore_cannot_fund() {
        let tgt = Arc::new(FakeTgt::new());
        tgt.set_store(16, 16); // 64 MiB total, all of it free
        let r = reconciler(Arc::clone(&tgt));

        // 48 MiB fits.
        r.ensure("pvc-a", Some(48 * 1024 * 1024)).await.expect("first fits");
        tgt.calls.lock().unwrap().clear();

        // ...and the store is STILL physically empty (thin lvols consume
        // no clusters until written), which is exactly why the gate
        // cannot be a free-space check: 48 MiB is already promised.
        assert_eq!(
            *tgt.free_clusters.lock().unwrap(),
            Some(16),
            "a thin create must not have consumed a single cluster"
        );
        let e = r
            .ensure("pvc-b", Some(32 * 1024 * 1024))
            .await
            .expect_err("48 + 32 > 64 must be refused even though the store reads empty");
        assert!(e.contains("promised") && e.contains("OVERCOMMIT"), "got: {e}");
        assert!(
            !tgt.methods().iter().any(|m| m == "bdev_lvol_create"),
            "the refusal must come BEFORE the create, not after"
        );

        // What still fits within the promise budget provisions fine.
        r.ensure("pvc-c", Some(8 * 1024 * 1024))
            .await
            .expect("a request inside the remaining budget must succeed");
    }

    /// The opt-out exists because thin provisioning legitimately means
    /// overcommitting — but it must be deliberate and loud, never the
    /// default, because the cost lands on an application as a failed
    /// write rather than on the operator as a refused PVC.
    #[tokio::test]
    async fn overcommit_is_allowed_only_when_asked_for() {
        let tgt = Arc::new(FakeTgt::new());
        tgt.set_store(4, 4); // 16 MiB
        let r = reconciler(Arc::clone(&tgt));
        assert!(
            r.ensure("pvc-over", Some(64 * 1024 * 1024)).await.is_err(),
            "default must refuse"
        );
        // The env-free core is what the test drives; the wrapper is one
        // std::env read (the F43 shape).
        assert!(!BlockExportReconciler::overcommit_allowed());
    }

    /// The grow half gates on the DELTA, not the new size: a volume
    /// already 32 MiB growing to 40 MiB needs 8 MiB of new space, and
    /// charging it 40 would refuse expansions the store can easily fund.
    #[tokio::test]
    async fn grow_gates_on_the_delta_and_refuses_before_the_resize() {
        let tgt = Arc::new(FakeTgt::new());
        tgt.set_store(12, 12); // 48 MiB
        let r = reconciler(Arc::clone(&tgt));
        r.ensure("pvc-g", Some(32 * 1024 * 1024)).await.expect("create");

        tgt.calls.lock().unwrap().clear();
        // 32 MiB promised of 48 total: +8 MiB fits, even though the NEW
        // SIZE (40 MiB) plus the old promise would not.
        r.grow("pvc-g", 40 * 1024 * 1024).await.expect("the delta fits");
        assert!(tgt.methods().iter().any(|m| m == "bdev_lvol_resize"));

        // ...and a delta that does NOT fit is refused before the resize.
        tgt.calls.lock().unwrap().clear();
        let e = r
            .grow("pvc-g", 512 * 1024 * 1024)
            .await
            .expect_err("a delta beyond free space must be refused");
        assert!(e.contains("refusing"), "got: {e}");
        assert!(
            !tgt.methods().iter().any(|m| m == "bdev_lvol_resize"),
            "the promise must not be made at the device first"
        );
    }

    /// UNREADABLE IS NOT EMPTY — the same rule the roller's block read
    /// follows, pointing the other way. There the safe default was to
    /// refuse; here it is to proceed, because a blipped RPC must not
    /// block every provision in the fleet, and the pre-existing
    /// behaviour (no gate at all) is exactly what proceeding restores.
    #[tokio::test]
    async fn an_unreadable_lvolstore_does_not_block_provisioning() {
        let tgt = Arc::new(FakeTgt::new());
        *tgt.free_clusters.lock().unwrap() = None; // the lvstore RPC fails
        let r = reconciler(Arc::clone(&tgt));
        r.ensure("pvc-blind", Some(64 * 1024 * 1024))
            .await
            .expect("an unreadable store must not refuse the provision");
        assert!(tgt.methods().iter().any(|m| m == "bdev_lvol_create"));
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
        // Provision first, then lose the lvol behind the reconciler's
        // back. The volume must be SEATED here for this test to be about
        // what it claims to be about: reconciling an unseated volume is
        // refused a step earlier (the record is the only door), and a
        // test that passed on THAT refusal would stop watching the F67
        // one entirely.
        r.ensure("pvc-3", Some(1024 * 1024)).await.expect("provision");
        tgt.bdevs.lock().unwrap().clear();
        tgt.calls.lock().unwrap().clear();
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

    /// The release seam: after a fence, `fence_release` drops the
    /// reservation (non-registrants may I/O again) but keeps the MDS
    /// registration, and a replay is the idempotent no-op.
    #[tokio::test]
    async fn fence_release_drops_the_reservation_and_replays_as_a_noop() {
        let nvme = crate::pnfs::mds::resv_fence::tests::FakeNvmeTarget::spawn().await;
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
        r.ensure("pvc-r", Some(1024 * 1024)).await.expect("provision");
        r.fence_preempt("pvc-r", 42).await.expect("fence");
        {
            let st = nvme.state.lock().unwrap();
            assert!(st.registrants.iter().any(|(_, _, h)| *h), "fence holds");
        }

        let summary = r.fence_release("pvc-r").await.expect("release");
        assert!(summary.contains("released=true"), "got: {summary}");
        {
            let st = nvme.state.lock().unwrap();
            assert!(!st.registrants.iter().any(|(_, _, h)| *h), "no holder");
            assert_eq!(st.rtype, 0);
            assert!(
                st.registrants
                    .iter()
                    .any(|(k, _, _)| *k == crate::identity::BLOCK_MDS_PR_KEY),
                "MDS registration kept"
            );
        }

        let again = r.fence_release("pvc-r").await.expect("replay");
        assert!(again.contains("released=false"), "got: {again}");
    }

    /// THE ACCEPTANCE TEST for `FlintCompositionStaticTraddr.cfg`.
    ///
    /// That run is required to FAIL: with the preempt aimed at the
    /// address the reconciler was constructed with, every post-failover
    /// fence dials a node that no longer serves the volume, `delivered`
    /// never becomes nonzero, and the quarantine sweep's ranges park
    /// forever. Here the constructed address is deliberately DEAD and
    /// the registry names the live one — the shape a failover produces —
    /// and the fence must land at the address the RECORD gives.
    ///
    /// The constructor address is 127.0.0.1:1 rather than a black hole
    /// on purpose: a regression to it fails this test in milliseconds
    /// with a refused connection instead of hanging on a timeout.
    #[tokio::test]
    async fn the_fence_dials_the_record_not_the_constructor() {
        let nvme = crate::pnfs::mds::resv_fence::tests::FakeNvmeTarget::spawn().await;
        nvme.state.lock().unwrap().registrants.push((42, [0xcc; 16], false));

        let tgt = Arc::new(FakeTgt::new());
        let backend = crate::state_backend::memory_backend();
        let r = BlockExportReconciler::new(
            Arc::clone(&tgt) as Arc<dyn SpdkRpcTransport + Send + Sync>,
            Arc::clone(&backend),
            "lvs_test".into(),
            "127.0.0.1".into(),
            1,
            "/var/tmp".into(),
        );
        r.ensure("pvc-moved", Some(1024 * 1024)).await.expect("provision");

        // The composer announces new coordinates — a target coming back
        // somewhere else, which after failover is the ordinary case.
        backend
            .block_target_register(
                &target_id(),
                &nvme.addr.ip().to_string(),
                nvme.addr.port(),
                1_000,
            )
            .await
            .unwrap()
            .unwrap();

        let (traddr, trsvcid) = r.listener_for("pvc-moved").await.expect("resolves");
        assert_eq!(
            (traddr.as_str(), trsvcid),
            (nvme.addr.ip().to_string().as_str(), nvme.addr.port()),
            "the attach answer follows the record, not the reconciler's config"
        );

        let summary = r.fence_preempt("pvc-moved", 42).await.expect("fence converges");
        assert!(summary.contains("preempted=true"), "got: {summary}");
        assert!(summary.contains("epoch=1"), "the fence names its composition: {summary}");
        let st = nvme.state.lock().unwrap();
        assert!(
            !st.registrants.iter().any(|(k, _, _)| *k == 42),
            "the victim was preempted at the address the record named"
        );
    }

    /// Fail-closed, both shapes, with the constructor's address sitting
    /// right there unused. An unresolvable volume is a refusal — the
    /// moment it becomes a fallback, StaticTraddr's lasso is back.
    #[tokio::test]
    async fn an_unresolvable_volume_is_refused_never_defaulted() {
        let tgt = Arc::new(FakeTgt::new());
        let backend = crate::state_backend::memory_backend();
        let r = BlockExportReconciler::new(
            Arc::clone(&tgt) as Arc<dyn SpdkRpcTransport + Send + Sync>,
            Arc::clone(&backend),
            "lvs_test".into(),
            "10.0.0.9".into(),
            4420,
            "/var/tmp".into(),
        );

        let e = r.listener_for("pvc-unseated").await.expect_err("must refuse");
        assert!(e.contains("no serving-target seat"), "got: {e}");
        assert!(!e.contains("10.0.0.9"), "a refusal must not leak a guess: {e}");

        // Seated at a composer that never registered: the other refusal,
        // naming who is missing so an operator can go look for it.
        backend
            .block_seat_volume("pvc-elsewhere", "node-b", 100)
            .await
            .unwrap()
            .unwrap();
        let e = r.listener_for("pvc-elsewhere").await.expect_err("must refuse");
        assert!(e.contains("node-b"), "got: {e}");
    }

    /// THE VERDICT's two conditions, each shown to be load-bearing on
    /// its own. Strikes alone would make the verdict a statement about
    /// loop cadence rather than about the target (F60's lesson: a pass's
    /// real period is the whole loop's duration, not its `interval`),
    /// and the window alone would condemn a target on one blip.
    #[tokio::test]
    async fn the_verdict_needs_both_the_strikes_and_the_window() {
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));

        // Enough strikes, no elapsed time: SUSPECT, not condemned.
        assert!(matches!(r.observe("node-x", false, 1_000), Reachability::Suspect { .. }));
        assert!(matches!(r.observe("node-x", false, 1_000), Reachability::Suspect { .. }));
        let v = r.observe("node-x", false, 1_000);
        assert!(
            matches!(v, Reachability::Suspect { strikes: 3, .. }),
            "three strikes inside one second is a fast loop, not a dead target: {v:?}"
        );

        // The same strike count once the window has passed: condemned.
        let v = r.observe("node-x", false, 1_000 + verdict_min_secs());
        assert!(matches!(v, Reachability::Unreachable { strikes: 4, .. }), "got {v:?}");

        // Elapsed time with too few strikes is not a verdict either.
        assert!(matches!(r.observe("node-y", false, 1_000), Reachability::Suspect { .. }));
        let v = r.observe("node-y", false, 1_000 + 10 * verdict_min_secs());
        assert!(
            matches!(v, Reachability::Suspect { strikes: 2, .. }),
            "a long-ago blip plus one is not a pattern: {v:?}"
        );

        // One success clears everything — including the clock.
        assert_eq!(r.observe("node-x", true, 2_000), Reachability::Reachable);
        assert!(matches!(
            r.observe("node-x", false, 2_000 + 10 * verdict_min_secs()),
            Reachability::Suspect { strikes: 1, .. }
        ));

        // A target nobody has ever probed has NO verdict — which is not
        // the same as being fine, and the promotion path treats it that
        // way.
        assert!(r.reachability("node-never-seen").is_none());
    }

    /// THE PROBE ITSELF, over the wire it is a claim about. A real
    /// NVMe/TCP target answers the initialize-connection exchange; a
    /// closed port does not; strikes accumulate and the verdict lands.
    ///
    /// The instrument is the DATA path deliberately — that is what a
    /// verdict is used to decide about (who serves, and where a fence
    /// can be delivered), and an RPC-socket probe would be answering
    /// "can I still administer it", which a target with a wedged
    /// listener passes while serving nobody.
    #[tokio::test]
    async fn a_target_that_stops_answering_the_wire_earns_the_verdict() {
        let nvme = crate::pnfs::mds::resv_fence::tests::FakeNvmeTarget::spawn().await;
        let tgt = Arc::new(FakeTgt::new());
        let r = reconciler(Arc::clone(&tgt));

        let live = r.probe_one("node-live", &nvme.addr.ip().to_string(), nvme.addr.port()).await;
        assert_eq!(live, Reachability::Reachable, "a real target answers ICReq");

        // Port 1: nothing listens, so the connection is refused at once
        // — the "dead" shape. The "partitioned" shape is a black hole
        // and times out; the verdict folds them together on purpose,
        // which is the whole premise of the composition machine.
        for _ in 0..verdict_strikes() + 1 {
            let v = r.probe_one("node-gone", "127.0.0.1", 1).await;
            assert!(!v.is_unreachable(), "these all land in one second: {v:?}");
        }
        let v = r
            .reachability_at("node-gone", now_unix() + verdict_min_secs())
            .expect("observed");
        assert!(v.is_unreachable(), "got {v:?}");

        // And recovery needs no operator: one answer clears it.
        let back = r.probe_one("node-gone", &nvme.addr.ip().to_string(), nvme.addr.port()).await;
        assert_eq!(back, Reachability::Reachable);
    }

    /// The misconfiguration diagnostic makes a CLAIM — "configuration
    /// fault, not a dead target" — so the predicate behind it is pinned
    /// rather than left inside a log call. It must never fire for a
    /// remote target (we have no second opinion about someone else's
    /// process) nor for a target whose process is also silent (that IS a
    /// dead target, and saying otherwise would send an operator to
    /// audit their network for nothing).
    #[test]
    fn the_misconfiguration_claim_needs_a_second_opinion_that_agrees() {
        assert!(listener_is_misconfigured(true, true), "ours, and its process answers");
        assert!(!listener_is_misconfigured(true, false), "ours and silent = down, not misconfigured");
        assert!(!listener_is_misconfigured(false, true), "never claimed about a remote target");
        assert!(!listener_is_misconfigured(false, false));
    }

    /// A black hole must cost the pass a BOUNDED wait, not the kernel's
    /// SYN-retry budget — the listener here accepts and then says
    /// nothing, which is what a half-open path looks like from a live
    /// TCP stack.
    #[tokio::test]
    async fn a_silent_target_times_out_instead_of_stalling_the_pass() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and hold: never answer the ICReq.
            let _held = listener.accept().await;
            std::future::pending::<()>().await;
        });

        let started = std::time::Instant::now();
        let e = crate::pnfs::mds::resv_fence::probe_nvme_tcp(
            &addr.ip().to_string(),
            addr.port(),
            std::time::Duration::from_millis(150),
        )
        .await
        .expect_err("a silent target is not a reachable one");
        assert!(e.contains("within"), "the error names the bound: {e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "bounded");
    }

    /// The prober is level-triggered over the REGISTRY: it probes what
    /// is registered, at the coordinates registered for it, and returns
    /// one verdict per target. A target with no row is not probed — this
    /// MDS has no address for it, and inventing one is what the registry
    /// exists to prevent.
    #[tokio::test]
    async fn the_prober_covers_every_registered_target() {
        let nvme = crate::pnfs::mds::resv_fence::tests::FakeNvmeTarget::spawn().await;
        let tgt = Arc::new(FakeTgt::new());
        let backend = crate::state_backend::memory_backend();
        let r = BlockExportReconciler::new(
            Arc::clone(&tgt) as Arc<dyn SpdkRpcTransport + Send + Sync>,
            Arc::clone(&backend),
            "lvs_test".into(),
            nvme.addr.ip().to_string(),
            nvme.addr.port(),
            "/var/tmp".into(),
        );
        r.self_register().await.expect("register self");
        backend
            .block_target_register("node-gone", "127.0.0.1", 1, now_unix())
            .await
            .unwrap()
            .unwrap();

        let verdicts = r.probe_all_targets().await;
        assert_eq!(verdicts.len(), 2, "one verdict per registered target");
        let mine = verdicts.iter().find(|(id, _)| *id == target_id()).expect("self probed");
        assert_eq!(mine.1, Reachability::Reachable);
        let theirs = verdicts.iter().find(|(id, _)| id == "node-gone").expect("peer probed");
        assert!(matches!(theirs.1, Reachability::Suspect { strikes: 1, .. }), "{:?}", theirs.1);
    }

    /// The verdict drives the CAS, and on a single-copy volume the
    /// answer is a REFUSAL with a reason — `WaitsPrice`'s bill arriving
    /// in the log rather than in a design doc. Then a second in-sync,
    /// reachable leg turns the same call into a promotion.
    #[tokio::test]
    async fn promotion_waits_on_a_single_copy_and_fires_once_a_survivor_exists() {
        use crate::state_backend::extent_alloc::LEG_INSYNC;
        let tgt = Arc::new(FakeTgt::new());
        let backend = crate::state_backend::memory_backend();
        let r = BlockExportReconciler::new(
            Arc::clone(&tgt) as Arc<dyn SpdkRpcTransport + Send + Sync>,
            Arc::clone(&backend),
            "lvs_test".into(),
            "10.0.0.9".into(),
            4420,
            "/var/tmp".into(),
        );
        r.ensure("pvc-p", Some(1 << 20)).await.expect("provision");
        let me = target_id();

        // No verdict yet ⇒ no promotion. A healthy composer is not
        // deposed because someone asked.
        match r.attempt_promotion("pvc-p").await {
            PromotionOutcome::Refused(reason) => {
                assert!(reason.contains("not under an unreachability verdict"), "{reason}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // Condemn the composer (strikes long ago ⇒ the window is past).
        let long_ago = now_unix() - 10 * verdict_min_secs();
        for _ in 0..verdict_strikes() + 1 {
            r.observe(&me, false, long_ago);
        }
        assert!(r.reachability(&me).unwrap().is_unreachable());

        // Under the verdict, and still nowhere to go: one copy.
        match r.attempt_promotion("pvc-p").await {
            PromotionOutcome::NoCandidate { reason } => {
                assert!(reason.contains("no in-sync leg"), "{reason}")
            }
            other => panic!("expected NoCandidate, got {other:?}"),
        }

        // A survivor appears, in sync — but never probed. Still no
        // promotion: unknown reachability is not permission.
        backend
            .block_target_register("node-b", "10.0.0.2", 4420, now_unix())
            .await
            .unwrap()
            .unwrap();
        backend
            .block_leg_mark("pvc-p", "node-b", LEG_INSYNC, now_unix())
            .await
            .unwrap()
            .unwrap();
        match r.attempt_promotion("pvc-p").await {
            PromotionOutcome::NoCandidate { reason } => {
                assert!(reason.contains("affirmatively reachable"), "{reason}")
            }
            other => panic!("expected NoCandidate, got {other:?}"),
        }

        // Heard from, and healthy. Now the CAS fires.
        r.observe("node-b", true, now_unix());
        match r.attempt_promotion("pvc-p").await {
            PromotionOutcome::Promoted { from, to, epoch } => {
                assert_eq!((from.as_str(), to.as_str(), epoch), (me.as_str(), "node-b", 2))
            }
            other => panic!("expected a promotion, got {other:?}"),
        }

        // The record moved, so the dial sites follow it — this is the
        // whole reason the registry went in first.
        let (traddr, trsvcid) = r.listener_for("pvc-p").await.expect("resolves");
        assert_eq!((traddr.as_str(), trsvcid), ("10.0.0.2", 4420));

        // And the retry is not a second promotion.
        match r.attempt_promotion("pvc-p").await {
            PromotionOutcome::Refused(reason) => {
                assert!(reason.contains("not under an unreachability verdict"), "{reason}")
            }
            other => panic!("the new composer is healthy; got {other:?}"),
        }
    }

    /// `RecordAssemblyOnly`'s door, shipped before promotion can open
    /// it. `FlintCompositionAssembly.cfg` is the healed composer whose
    /// reconciler re-converges the same subnqn and NGUID over its stale
    /// leg and serves it; the record is the only door, and converge is
    /// where that door lives.
    #[tokio::test]
    async fn a_volume_seated_elsewhere_is_neither_converged_nor_adopted() {
        let tgt = Arc::new(FakeTgt::new());
        let backend = crate::state_backend::memory_backend();
        let r = BlockExportReconciler::new(
            Arc::clone(&tgt) as Arc<dyn SpdkRpcTransport + Send + Sync>,
            Arc::clone(&backend),
            "lvs_test".into(),
            "10.0.0.9".into(),
            4420,
            "/var/tmp".into(),
        );
        backend
            .block_seat_volume("pvc-theirs", "node-b", 100)
            .await
            .unwrap()
            .unwrap();

        let e = r.ensure("pvc-theirs", None).await.expect_err("reconcile must refuse");
        assert!(e.contains("node-b"), "the refusal names the composer: {e}");

        // And the provision shape does not take it either: seating is
        // insert-if-absent, so re-provisioning someone else's volume
        // reports the standing seat instead of stealing it.
        let e = r
            .ensure("pvc-theirs", Some(1 << 20))
            .await
            .expect_err("provision must refuse");
        assert!(e.contains("will not adopt"), "got: {e}");

        assert!(
            tgt.methods().iter().all(|m| m.starts_with("bdev_get_") || m.starts_with("nvmf_get_")),
            "a refused volume must not have touched the target: {:?}",
            tgt.methods()
        );
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
