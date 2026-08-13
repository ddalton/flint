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

/// How long a serving lease is granted for. MUST comfortably exceed
/// several reconcile intervals: the lease is renewed by the pass, so a
/// TTL near the interval would let one slow pass expire a lease that
/// nothing is wrong with. It is also the EVICTION HORIZON — the time a
/// deposed composer is given to notice, in the worst case, before
/// anything may be torn out from under it — so raising it trades
/// failover latency for tolerance of a stalled loop.
fn lease_ttl() -> i64 {
    env_u64("FLINT_PNFS_BLOCK_LEASE_SECS", 120) as i64
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

/// How much un-copied delta the rebuild is willing to carry INTO the
/// quiesced window. The window gates every client write on the volume,
/// so this number is a latency budget spent on the clients: 64 MiB over
/// a 10 GbE leg is tens of milliseconds, well under any initiator's I/O
/// timeout. Raising it shortens the rebuild and lengthens the stall.
fn rebuild_window_max_bytes() -> u64 {
    env_u64("FLINT_PNFS_BLOCK_REBUILD_WINDOW_MAX_MIB", 64) * 1024 * 1024
}

/// How many delta rounds the ladder may run before it takes the window
/// anyway. The ladder converges when the copy outruns the writer; it
/// does NOT when the writer is faster, and without a bound that is an
/// infinite chase. Reaching the bound is not a failure — it means the
/// final window will be longer than the budget, which is logged.
fn rebuild_max_rounds() -> u32 {
    env_u64("FLINT_PNFS_BLOCK_REBUILD_MAX_ROUNDS", 6) as u32
}

/// The quiesce lease (`bdev_raid_quiesce`, flint's carried SPDK patch).
/// Auto-expiring BY DESIGN: an orchestrator that dies mid-window must
/// not leave guest I/O gated until the initiator escalates to resets.
fn rebuild_lease_ms() -> u64 {
    env_u64("FLINT_PNFS_BLOCK_REBUILD_LEASE_MS", 10_000)
}

/// Shallow-copy poll cadence.
fn rebuild_poll() -> std::time::Duration {
    std::time::Duration::from_millis(env_u64("FLINT_PNFS_BLOCK_REBUILD_POLL_MS", 500))
}

/// F39's bound, ported: a copy that stops making cluster progress for
/// this long is declared wedged. The bound is on PROGRESS, never on
/// total time — a wall-clock cap would kill a legitimate multi-hour
/// bulk copy of a large volume.
fn rebuild_stall_secs() -> u64 {
    env_u64("FLINT_PNFS_BLOCK_REBUILD_STALL_SECS", 600)
}

/// What one rebuild attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// The leg holds the volume's bytes, is a member of the composition
    /// again, and the record says so — in that order.
    Rebuilt { peer: String, rounds: u32, clusters: u64 },
    /// No stale leg to rebuild.
    NotNeeded,
    /// Not now — the peer is unreachable, the frame has no slot, the
    /// window could not be held. The next pass tries again.
    Deferred(String),
    /// Not without something changing. Distinct from `Deferred` because
    /// retrying alone will never help.
    Refused(String),
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

/// What one converge pass is FOR. The three differ only in what they
/// require of the record and what allow-list they converge toward, so
/// they share one spec builder rather than growing a second one that
/// could drift.
#[derive(Debug, Clone, Copy)]
enum ConvergeMode {
    /// CreateVolume: seat the volume here (which also grants its first
    /// lease) and mint the lvol at this size.
    Provision(u64),
    /// The steady state: the record must seat the volume here AND the
    /// lease must renew. Converging IS the assertion that this target
    /// serves the volume, so it renews rather than merely checking —
    /// otherwise an attach arriving between two passes could be refused
    /// by a lease nothing was wrong with.
    Reconcile,
    /// THE DEAD-MAN's act: converge the allow-list down to the fence
    /// lane alone, whatever the record says. Requires no seat and no
    /// lease, because it runs precisely when those are gone.
    Suspend,
}

/// What one assembly attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyOutcome {
    /// The composition is live: the deposed peer is evicted at this
    /// target's leg-export, its leg is marked stale, the epoch's lease
    /// is granted, and the export is up with standing fences replayed
    /// into its allow-list.
    Assembled { epoch: i64, deposed: Option<String> },
    /// Nothing to do — this target already holds the lease for the
    /// seated epoch.
    AlreadyAssembled { epoch: i64 },
    /// THE HORIZON has not passed. The deposed composer's lease still
    /// runs, so it may still be acking its clients' writes, and tearing
    /// its fan-in out now is what strands those writes on a doomed leg.
    AwaitingHorizon { deposed: String, until_unix: i64 },
    Refused(String),
}

/// The result of one promotion attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionOutcome {
    Promoted {
        from: String,
        to: String,
        epoch: i64,
        /// When the deposed composer's lease expires. Eviction and
        /// assembly may not begin before this: severing a still-acking
        /// zombie's fan-in strands its clients' acked writes on the
        /// doomed leg, which is why the model's order is CAS → horizon →
        /// evict → assemble and not CAS → evict.
        evict_after_unix: i64,
    },
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
    /// Volumes with a rebuild running. In memory and not durable on
    /// purpose: it answers "is this process already copying" and nothing
    /// else. A rebuild that dies with the process leaves cuts, an
    /// attached destination and possibly an unvouched member — all of
    /// which the next attempt sweeps, prunes or re-copies, and none of
    /// which a durable flag would have helped with.
    rebuilding: dashmap::DashMap<String, ()>,
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
            rebuilding: dashmap::DashMap::new(),
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
        let now = now_unix();
        let seat = match self
            .backend
            .block_seat_volume(volume, &me, now, now + lease_ttl())
            .await
        {
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
        let mode = match size_bytes {
            Some(n) => ConvergeMode::Provision(n),
            None => ConvergeMode::Reconcile,
        };
        self.ensure_locked(volume, mode).await
    }

    /// THE DEAD-MAN's act on one volume: converge the allow-list down to
    /// the fence lane, so every client's controller is torn down and
    /// this target stops serving bytes it no longer holds the right to
    /// serve. The admissions in sqlite stay — the clients are still
    /// legitimately admitted; it is this target that lost the lease.
    pub async fn suspend_export(&self, volume: &str) -> Result<(), String> {
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        self.ensure_locked(volume, ConvergeMode::Suspend).await
    }

    /// Extend this target's lease on a volume. Record-conditioned at the
    /// MDS; the refusal reason is carried up because the two refusals
    /// mean different things (deposed vs. elected-but-not-assembled).
    async fn renew_lease(
        &self,
        volume: &str,
        holder: &str,
    ) -> Result<crate::state_backend::extent_alloc::BlockLease, String> {
        match self
            .backend
            .block_lease_renew(volume, holder, now_unix() + lease_ttl())
            .await
        {
            Ok(Ok(l)) => Ok(l),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(format!("lease renewal failed: {e}")),
        }
    }

    /// Who may reach THIS target's leg copy of a volume: exactly the
    /// current composer, and nobody else.
    ///
    /// That one sentence is the whole of `legAdmit` and the whole of
    /// eviction. It is derived from the record every pass, so when the
    /// seat moves the new composer is admitted and the old one is
    /// dropped by the same converge — a deposed peer loses its reach
    /// because the record stopped naming it, not because someone
    /// remembered to revoke it.
    ///
    /// Empty when this target IS the composer: a composer mirrors onto
    /// its peers, never onto itself, and its own copy is claimed by the
    /// raid module anyway.
    fn desired_leg_hosts(&self, seat: &crate::state_backend::extent_alloc::BlockSeat) -> Vec<String> {
        if seat.composer == target_id() {
            Vec::new()
        } else {
            vec![crate::nvmeof_export::flint_host_nqn(&seat.composer)]
        }
    }

    /// Converge this target's LEG export for a volume it holds a copy of
    /// but does not compose: the copy is offered to the composer named
    /// by the record, over its own subsystem, default-closed.
    ///
    /// Level-triggered like everything else, and that is what makes the
    /// eviction automatic: the allow-list is recomputed from the seat,
    /// so a deposed composer is removed by the ordinary pass.
    pub async fn ensure_leg_export(&self, volume: &str) -> Result<(), String> {
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        let seat = match self.backend.block_volume_seat(volume).await {
            Ok(Ok(Some(s))) => s,
            Ok(Ok(None)) => return Err(format!("'{volume}' has no seat — no leg to offer")),
            Ok(Err(e)) => return Err(format!("seat unreadable: {e}")),
            Err(e) => return Err(format!("seat unreadable: {e}")),
        };
        let me = target_id();
        if seat.composer == me {
            // We compose it; the raid claims the lvol. Any leg export
            // left from when we did NOT compose it must go, or the
            // claim fails with EPERM — the collision the file tier's
            // stale-export cleanup exists for.
            return self.drop_leg_export_locked(volume).await;
        }
        let bdev = self.bdev_name(volume);
        if self.rpc.rpc(&json!({ "method": "bdev_get_bdevs", "params": { "name": bdev } }))
            .await
            .is_err()
        {
            return Err(format!(
                "leg export for '{volume}': no local copy ({bdev}) — this target holds no leg \
                 to offer, and minting one would offer zeros as if they were data"
            ));
        }
        let hosts = self.desired_leg_hosts(&seat);
        let nqn = crate::identity::block_leg_export_nqn(volume);
        // The leg carries the volume's bytes but NOT its client-facing
        // identity: the NGUID is what kernel clients resolve by-id, and
        // two namespaces answering to it — the composer's raid and a
        // peer's leg — is the one-identity rule broken. The composer
        // dials this by NQN, never by designator.
        let spec = ExportSpec {
            nqn: &nqn,
            bdev_name: &bdev,
            bdev_aliases: &[],
            trtype: "TCP",
            traddr: &self.traddr,
            trsvcid: self.trsvcid,
            allowed_hosts: Some(&hosts),
            ns_identity: None,
            ptpl_file: None,
        };
        ensure_export(self.rpc.as_ref(), &spec)
            .await
            .map_err(|e| format!("leg export {nqn}: {e}"))?;
        tracing::debug!(
            "leg export for '{}' converged, offered to composer '{}'",
            volume,
            seat.composer
        );
        Ok(())
    }

    async fn drop_leg_export_locked(&self, volume: &str) -> Result<(), String> {
        let nqn = crate::identity::block_leg_export_nqn(volume);
        if get_subsystem(self.rpc.as_ref(), &nqn)
            .await
            .map_err(|e| format!("nvmf_get_subsystems {nqn}: {e}"))?
            .is_none()
        {
            return Ok(());
        }
        // Guarded-destroy: this subsystem exists only to offer a leg to
        // one composer, and the record no longer says it should. The
        // bytes are untouched — only the door closes.
        let del = json!({ "method": "nvmf_delete_subsystem", "params": { "nqn": nqn } }); // guarded-destroy-lint: allow
        self.rpc
            .rpc(&del)
            .await
            .map_err(|e| format!("nvmf_delete_subsystem {nqn}: {e}"))?;
        tracing::info!("leg export for '{}' withdrawn — this target composes it now", volume);
        Ok(())
    }

    /// EVICTION AT THE LEG-EXPORT (`EvictAtLeg`): the deposed composer
    /// must not be able to reach this target's copy of the volume.
    ///
    /// The subject is THIS target's leg export — the door a peer
    /// composer mirrors through. Removing the deposed composer's host
    /// NQN from it is what stops its fan-in reaching the copy this
    /// target is about to serve from, and it is the only exclusion that
    /// does not depend on the deposed cooperating.
    ///
    /// The ORDER is load-bearing: this runs after the horizon and before
    /// assembly, because severing a still-acking zombie's fan-in strands
    /// its clients' acked writes on the doomed leg — CAS → horizon →
    /// evict → assemble, never CAS → evict.
    ///
    /// The ordinary converge would reach the same state, since the leg
    /// allow-list is derived from the seat and the seat no longer names
    /// the deposed. This is the explicit act because assembly must not
    /// proceed on the ASSUMPTION that a pass ran: the removal is
    /// verified here, in the order the model requires.
    async fn evict_deposed_at_leg(&self, volume: &str, deposed: &str) -> Result<(), String> {
        let nqn = crate::identity::block_leg_export_nqn(volume);
        let deposed_nqn = crate::nvmeof_export::flint_host_nqn(deposed);
        let present = get_subsystem(self.rpc.as_ref(), &nqn)
            .await
            .map_err(|e| format!("nvmf_get_subsystems {}: {}", nqn, e))?
            .and_then(|s| {
                Some(
                    s.get("hosts")?
                        .as_array()?
                        .iter()
                        .any(|h| h.get("nqn").and_then(|n| n.as_str()) == Some(&deposed_nqn)),
                )
            })
            .unwrap_or(false);
        if !present {
            tracing::debug!(
                "evict '{}': '{}' already excluded at this leg-export",
                volume,
                deposed
            );
            return Ok(());
        }
        let remove = json!({
            "method": "nvmf_subsystem_remove_host",
            "params": { "nqn": nqn, "host": deposed_nqn }
        });
        self.rpc
            .rpc(&remove)
            .await
            .map_err(|e| format!("evicting '{deposed}' from {nqn}: {e}"))?;
        tracing::warn!("⛔ evict '{}': deposed composer '{}' removed at the leg-export", volume, deposed);
        Ok(())
    }

    /// The raid bdev name for a composed volume. Derived, never stored:
    /// the composition is EPHEMERAL and re-created from the record, so
    /// the name has to be a pure function of the volume.
    fn raid_name(&self, volume: &str) -> String {
        format!("flintraid-{volume}")
    }

    /// The bdev the client-facing export should serve: the RAID when
    /// this volume has more than one leg, the bare lvol when it is
    /// solo.
    ///
    /// Switching between them is safe, and that is not a lucky accident
    /// — it is bought by `superblock: false`. SPDK's raid superblock
    /// costs `RAID_BDEV_MIN_DATA_OFFSET_SIZE` (≥1 MiB) of data offset,
    /// which would shift every byte under the volume's pinned NGUID and
    /// make composing an existing volume a data migration. Without it
    /// each base carries the volume's bytes at LBA 0, identical to the
    /// bare lvol, so a solo volume can be composed in place and a
    /// composition can fall back to solo — both without moving a byte.
    /// The file tier reached the same conclusion for its own reasons
    /// (driver.rs: snapshots and clones of superblocked bases were
    /// unmountable raw); this tier inherits the property and depends on
    /// it harder.
    ///
    /// The second thing `superblock: false` buys is the whole of
    /// `RecordAssemblyOnly`: with no superblock there is no
    /// examine-based auto-assembly to fight, so a composition exists
    /// exactly when flint builds one from the record — which is the
    /// review's "a survivor cannot self-promote" limit dissolved rather
    /// than worked around. Nothing here ever consults a superblock to
    /// decide who serves.
    /// THE FRAME, and why the composition outlives its legs: a raid
    /// bdev's SLOT COUNT is fixed at creation (`bdev_raid_create` refuses
    /// an empty base name, and `raid_bdev_add_base_bdev` only ever fills
    /// a slot some removal emptied — bdev_raid.c:3672-3680). A volume
    /// composed over one base can therefore never gain a second leg
    /// without being torn down and rebuilt, which re-points the
    /// namespace under a live client.
    ///
    /// So the frame is a function of the RECORD's leg count, not of how
    /// many legs are currently healthy: a volume the record gives two
    /// legs is served through a two-slot raid whether the peer is
    /// present, stale, or missing. A leg leaves by emptying its slot and
    /// rejoins into the same slot under a quiesce, and the client sees
    /// neither.
    async fn compose_bdev(
        &self,
        volume: &str,
        seat: &crate::state_backend::extent_alloc::BlockSeat,
    ) -> Result<String, String> {
        let local = self.bdev_name(volume);
        let legs = match self.backend.block_legs(volume).await {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => return Err(format!("legs unreadable: {e}")),
            Err(e) => return Err(format!("legs unreadable: {e}")),
        };
        if legs.len() <= 1 {
            // Genuinely solo — one leg in the record, nothing to
            // compose. Any raid left from a previous composition must
            // go, or it keeps its exclusive claim on the lvol.
            self.drop_raid(volume).await?;
            return Ok(local);
        }
        let raid = self.raid_name(volume);
        match self.get_raid(&raid).await? {
            None => self.build_frame(volume, &raid, &local, &legs).await?,
            Some(live) => self.prune_frame(volume, &raid, &local, &legs, &live).await?,
        }
        tracing::debug!(
            "'{}' composed at epoch {}: frame of {} slot(s)",
            volume,
            seat.epoch,
            legs.len()
        );
        Ok(raid)
    }

    /// Build the composition frame from nothing — a target that has just
    /// started, or a volume being composed for the first time.
    ///
    /// TWO things happen here and both are load-bearing.
    ///
    /// (1) `UncleanResync`, the write-hole belt. A composition built
    /// from nothing cannot prove that any peer's bytes equal ours: raid1
    /// acks on any-one-leg success and records the leg failure
    /// asynchronously (bdev_raid.c:705-718, 2440-2444), so a composer
    /// that died mid-write left two legs holding different bytes with
    /// nothing durable saying so, and there is no scrub or resync to
    /// find out — the only process type raid1 has is REBUILD. Code can
    /// only ever see "died while serving". So every peer is DEMOTED here
    /// and earns its place back through a rebuild. The price is a copy
    /// after an unclean restart; the alternative is reads that flap
    /// between divergent legs on LAYOUTCOMMIT-confirmed data
    /// (`Inv_NoSplitRead`).
    ///
    /// (2) The slot count. Slots cannot be created empty, so each
    /// absent leg is stood in for by a null bdev of the same geometry
    /// that is removed the instant the frame exists. That is safe
    /// because the frame is built BEFORE the export — `ensure_locked`
    /// composes and then converges the subsystem, and a raid that
    /// vanished took its namespace with it (SPDK hot-removes a
    /// namespace when its bdev unregisters) — so no read can reach the
    /// stand-in's zeros. It is a narrow argument, so the belt is
    /// explicit: if a stand-in cannot be removed, the whole frame is
    /// deleted rather than left serving a leg of zeros.
    async fn build_frame(
        &self,
        volume: &str,
        raid: &str,
        local: &str,
        legs: &[crate::state_backend::extent_alloc::BlockLeg],
    ) -> Result<(), String> {
        let me = target_id();
        for leg in legs.iter().filter(|l| l.target_id != me) {
            if leg.sync_state != crate::state_backend::extent_alloc::LEG_INSYNC {
                continue;
            }
            tracing::warn!(
                "🧬 '{}': building the composition from nothing — leg '{}' is marked in-sync but \
                 nothing here can prove its bytes equal ours (raid1 acks solo and records the \
                 failure afterwards), so it is DEMOTED and owes a rebuild",
                volume,
                leg.target_id
            );
            match self
                .backend
                .block_leg_mark(
                    volume,
                    &leg.target_id,
                    crate::state_backend::extent_alloc::LEG_STALE,
                    now_unix(),
                )
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(format!("demoting leg '{}': {e}", leg.target_id)),
                Err(e) => return Err(format!("demoting leg '{}': {e}", leg.target_id)),
            }
        }
        // Geometry for the stand-ins: the raid is sized to its smallest
        // base, so a stand-in that is one block short would silently
        // shrink the volume.
        let probe = json!({ "method": "bdev_get_bdevs", "params": { "name": local } });
        let resp = self
            .rpc
            .rpc(&probe)
            .await
            .map_err(|e| format!("bdev_get_bdevs {local}: {e}"))?;
        let b = resp.get("result").and_then(|r| r.as_array()).and_then(|a| a.first());
        let block_size = b.and_then(|b| b.get("block_size")).and_then(|v| v.as_u64());
        let num_blocks = b.and_then(|b| b.get("num_blocks")).and_then(|v| v.as_u64());
        let (Some(block_size), Some(num_blocks)) = (block_size, num_blocks) else {
            return Err(format!(
                "cannot frame '{volume}': {local} reports no block_size/num_blocks"
            ));
        };
        let mut bases = vec![local.to_string()];
        let mut pads = Vec::new();
        for slot in 1..legs.len() {
            let pad = format!("flintslot-{volume}-{slot}");
            let create = json!({
                "method": "bdev_null_create",
                "params": { "name": pad, "num_blocks": num_blocks, "block_size": block_size }
            });
            if let Err(e) = self.rpc.rpc(&create).await {
                // Leftover from a previous attempt is fine; anything
                // else is not, and framing on a stand-in of unknown
                // geometry would resize the volume.
                let probe = json!({ "method": "bdev_get_bdevs", "params": { "name": pad } });
                if self.rpc.rpc(&probe).await.is_err() {
                    for p in &pads {
                        let _ = self
                            .rpc
                            .rpc(&json!({ "method": "bdev_null_delete", "params": { "name": p } })) // guarded-destroy-lint: allow
                            .await;
                    }
                    return Err(format!("bdev_null_create {pad}: {e}"));
                }
            }
            bases.push(pad.clone());
            pads.push(pad);
        }
        // guarded-construct-lint: allow — the hazard this lint guards is
        // a raid created or REUSED over a base set nobody validated (the
        // A2 tranche's finding against the file tier's ONLINE-reuse
        // path). This site cannot reuse: it runs only when
        // `bdev_raid_get_bdevs` says no raid of this name exists, and
        // the base set it passes is derived from the record — the local
        // leg plus one stand-in per recorded peer, every one of which is
        // removed again below. With `superblock: false` there is no
        // examine-based auto-assembly to race, so nothing can exist
        // under this name for the create to adopt.
        let create = json!({
            "method": "bdev_raid_create", // guarded-construct-lint: allow
            "params": {
                "name": raid,
                "raid_level": "1",
                "base_bdevs": bases,
                "superblock": false,
            }
        });
        self.rpc
            .rpc(&create)
            .await
            .map_err(|e| format!("bdev_raid_create {raid}: {e}"))?;
        // Empty the stand-in slots. This is the fail-closed step: a
        // stand-in left in the frame is a leg that answers reads with
        // zeros.
        for pad in &pads {
            let remove = json!({
                "method": "bdev_raid_remove_base_bdev",
                "params": { "name": pad }
            });
            if let Err(e) = self.rpc.rpc(&remove).await {
                let del = json!({ "method": "bdev_raid_delete", "params": { "name": raid } }); // guarded-destroy-lint: allow
                let _ = self.rpc.rpc(&del).await;
                return Err(format!(
                    "framing '{volume}': stand-in {pad} could not be removed ({e}) — the \
                     composition was deleted rather than left serving a leg of zeros"
                ));
            }
        }
        for pad in &pads {
            let del = json!({ "method": "bdev_null_delete", "params": { "name": pad } }); // guarded-destroy-lint: allow
            let _ = self.rpc.rpc(&del).await;
        }
        tracing::info!(
            "🧬 '{}' framed: {} slot(s), {} filled — every absent leg has a slot waiting for it",
            volume,
            legs.len(),
            bases.len() - pads.len()
        );
        Ok(())
    }

    /// A live composition, converged against the record: a member the
    /// record does not vouch for is removed.
    ///
    /// The record is the authority in one direction only. A member it
    /// does not name is removed here (a degrade whose removal failed, a
    /// rebuild that crashed between the add and its mark). A leg it
    /// names that is NOT a member is left alone — putting it back is the
    /// rebuild's business, and only the rebuild can prove it belongs.
    async fn prune_frame(
        &self,
        volume: &str,
        raid: &str,
        local: &str,
        legs: &[crate::state_backend::extent_alloc::BlockLeg],
        live: &serde_json::Value,
    ) -> Result<(), String> {
        let me = target_id();
        let mut vouched = vec![local.to_string()];
        for leg in legs.iter().filter(|l| {
            l.target_id != me && l.sync_state == crate::state_backend::extent_alloc::LEG_INSYNC
        }) {
            vouched.push(self.leg_base_name(volume, &leg.target_id));
        }
        for member in Self::raid_members(live) {
            if vouched.contains(&member) {
                continue;
            }
            tracing::warn!(
                "'{}': base '{}' is a member of the composition but the record does not vouch \
                 for it — removing it (a rebuild is what puts a leg back)",
                volume,
                member
            );
            let remove = json!({
                "method": "bdev_raid_remove_base_bdev",
                "params": { "name": member }
            });
            self.rpc
                .rpc(&remove)
                .await
                .map_err(|e| format!("removing unvouched base {member} from {raid}: {e}"))?;
        }
        Ok(())
    }

    /// The base bdev name a peer's leg is attached under. One function,
    /// because the composition, the degrade and the rebuild all have to
    /// mean the same bdev.
    fn leg_base_name(&self, volume: &str, peer: &str) -> String {
        format!("flintleg-{volume}-{peer}n1")
    }

    /// Configured members of a live raid, from `bdev_raid_get_bdevs`.
    /// Empty slots carry a null name — that is how SPDK reports a slot
    /// waiting for a leg (bdev_raid.c: `raid_bdev_write_info_json`).
    fn raid_members(live: &serde_json::Value) -> Vec<String> {
        live.get("base_bdevs_list")
            .and_then(|b| b.as_array())
            .map(|bs| {
                bs.iter()
                    .filter_map(|b| b.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Slots with no leg in them.
    fn raid_empty_slots(live: &serde_json::Value) -> usize {
        live.get("base_bdevs_list")
            .and_then(|b| b.as_array())
            .map(|bs| bs.iter().filter(|b| b.get("name").map(|n| n.is_null()).unwrap_or(true)).count())
            .unwrap_or(0)
    }

    /// Attach a peer's leg export as a local bdev, dialling the address
    /// the REGISTRY holds for that target — the same rule as every other
    /// dial site, for the same reason.
    async fn attach_peer_leg(&self, volume: &str, peer: &str) -> Result<String, String> {
        let ctrl = format!("flintleg-{volume}-{peer}");
        let expected = format!("{ctrl}n1");
        if self
            .rpc
            .rpc(&json!({ "method": "bdev_get_bdevs", "params": { "name": expected } }))
            .await
            .is_ok()
        {
            return Ok(expected); // already attached
        }
        let target = match self.backend.block_target_list().await {
            Ok(Ok(t)) => t.into_iter().find(|t| t.target_id == peer),
            Ok(Err(e)) => return Err(format!("target registry unreadable: {e}")),
            Err(e) => return Err(format!("target registry unreadable: {e}")),
        };
        let Some(target) = target else {
            return Err(format!(
                "leg peer '{peer}' has no target-registry row — refusing to guess where its \
                 copy answers"
            ));
        };
        let mut params = json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": ctrl,
                "trtype": "TCP",
                "traddr": target.traddr,
                "trsvcid": target.trsvcid.to_string(),
                "subnqn": crate::identity::block_leg_export_nqn(volume),
                "adrfam": "IPv4",
                // The inter-target identity the peer's leg allow-list
                // admits. Stable per target, which is what makes the
                // peer's eviction possible at all.
                "hostnqn": crate::nvmeof_export::flint_host_nqn(&target_id()),
            }
        });
        // THE DEGRADE BARRIER's transport half: a composed leg QUEUES
        // I/O rather than failing it, so raid1 cannot ack a write only
        // one leg took. The stall that creates is bounded by the
        // mark-then-degrade loop below, not by a transport timeout —
        // see `LegTransportPolicy::composed_leg` for why that is not
        // F42 coming back.
        crate::nvme_recovery::LegTransportPolicy::composed_leg().apply(&mut params["params"]);
        let resp = self
            .rpc
            .rpc(&params)
            .await
            .map_err(|e| format!("attaching leg of '{peer}' for '{volume}': {e}"))?;
        Ok(resp
            .get("result")
            .and_then(|r| r.as_array())
            .and_then(|n| n.first())
            .and_then(|b| b.as_str())
            .map(String::from)
            .unwrap_or(expected))
    }

    async fn get_raid(&self, raid: &str) -> Result<Option<serde_json::Value>, String> {
        match self
            .rpc
            .rpc(&json!({ "method": "bdev_raid_get_bdevs", "params": { "category": "all" } }))
            .await
        {
            Ok(resp) => Ok(resp
                .get("result")
                .and_then(|r| r.as_array())
                .and_then(|rs| {
                    rs.iter()
                        .find(|r| r.get("name").and_then(|n| n.as_str()) == Some(raid))
                        .cloned()
                })),
            // A tgt without the raid module answers "method not found",
            // which is "no raid", not an error.
            Err(_) => Ok(None),
        }
    }

    async fn drop_raid(&self, volume: &str) -> Result<(), String> {
        let raid = self.raid_name(volume);
        if self.get_raid(&raid).await?.is_none() {
            return Ok(());
        }
        // Guarded-destroy: the record says this volume is solo, so the
        // composition object is what is stale, not the bytes. Deleting
        // the raid releases the lvol's claim; the lvol is untouched, and
        // with no superblock its bytes are the volume's bytes.
        let del = json!({ "method": "bdev_raid_delete", "params": { "name": raid } }); // guarded-destroy-lint: allow
        self.rpc
            .rpc(&del)
            .await
            .map_err(|e| format!("bdev_raid_delete {raid}: {e}"))?;
        tracing::info!("'{}' is solo — composition object dropped", volume);
        Ok(())
    }

    /// THE DEGRADE BARRIER (`DegradeBarrier`), record half: MARK, then
    /// degrade. Never the other way round.
    ///
    /// Stock raid1 does the opposite — it acks a solo-landing write and
    /// records the leg failure asynchronously afterwards — and the gap
    /// between those two is a window in which the MDS record still says
    /// a leg is in sync while it is already missing acked bytes. An
    /// election in that window discards them with every belt green
    /// (`FlintCompositionDegradeBlind.cfg`).
    ///
    /// flint is not on the data path, so it cannot gate an ack. It gates
    /// the ABILITY TO ACK instead: a composed leg's transport queues I/O
    /// rather than failing it (`LegTransportPolicy::composed_leg`), so
    /// while a peer is unreachable the raid cannot complete a write at
    /// all. Writes stall. Then, and only then:
    ///
    ///   1. the peer's stale mark lands DURABLY in the record;
    ///   2. the leg is removed from the raid, which lets the queued I/O
    ///      drain and the volume serve degraded.
    ///
    /// After step 1 no ack can be a lie: any write the raid completes
    /// from here is one the record already knows the peer missed. The
    /// stall between the peer's death and step 2 is the barrier's price,
    /// bounded by the unreachability verdict's own thresholds — and it
    /// is the same trade `ElectInSync` makes, availability spent on
    /// durability.
    ///
    /// If flint dies between the peer's death and step 2, writes stay
    /// stalled until it returns. That is an availability event and not a
    /// correctness one — nothing acked, so nothing was lost — and it is
    /// the honest cost of not being on the data path.
    ///
    /// Returns the legs degraded this pass.
    pub async fn degrade_pass(&self, volume: &str) -> Vec<String> {
        let me = target_id();
        let seat = match self.backend.block_volume_seat(volume).await {
            Ok(Ok(Some(s))) if s.composer == me => s,
            _ => return Vec::new(), // not ours to compose
        };
        let legs = match self.backend.block_legs(volume).await {
            Ok(Ok(l)) => l,
            _ => return Vec::new(),
        };
        let mut degraded = Vec::new();
        for leg in legs.iter().filter(|l| {
            l.target_id != me && l.sync_state == crate::state_backend::extent_alloc::LEG_INSYNC
        }) {
            // Only an affirmative verdict degrades a leg. A peer that is
            // merely SUSPECT keeps its place: the transport is queueing,
            // so nothing can be acked behind its back, and degrading on
            // a blip would spend a rebuild on a target that never left.
            match self.reachability(&leg.target_id) {
                Some(v) if v.is_unreachable() => {}
                _ => continue,
            }
            tracing::error!(
                "🔻 '{}' epoch {}: leg '{}' is unreachable — marking it STALE before degrading, \
                 so no ack can outrun the record",
                volume,
                seat.epoch,
                leg.target_id
            );
            // (1) THE MARK, first and durable.
            match self
                .backend
                .block_leg_mark(
                    volume,
                    &leg.target_id,
                    crate::state_backend::extent_alloc::LEG_STALE,
                    now_unix(),
                )
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    // The mark did not land, so the barrier is not up.
                    // Leave the leg in the raid: I/O stays stalled,
                    // which is the safe side of this trade.
                    tracing::error!(
                        "'{}' leg '{}': stale mark FAILED ({}) — leaving the leg in the \
                         composition, so writes stall rather than acking behind the record",
                        volume, leg.target_id, e
                    );
                    continue;
                }
                Err(e) => {
                    tracing::error!(
                        "'{}' leg '{}': stale mark FAILED ({}) — writes stall rather than \
                         acking behind the record",
                        volume, leg.target_id, e
                    );
                    continue;
                }
            }
            // (2) Only now may the composition degrade. The slot the
            // removal empties is the one the leg rejoins through — the
            // frame outlives the leg (see `compose_bdev`).
            let base = self.leg_base_name(volume, &leg.target_id);
            let remove = json!({
                "method": "bdev_raid_remove_base_bdev",
                "params": { "name": base }
            });
            match self.rpc.rpc(&remove).await {
                Ok(_) => {
                    tracing::warn!(
                        "'{}' now serving DEGRADED without leg '{}' — a rebuild is owed before \
                         that leg can be elected again",
                        volume,
                        leg.target_id
                    );
                    degraded.push(leg.target_id.clone());
                }
                // The mark stands, which is the important half: the leg
                // is already unelectable. The removal retries next pass;
                // until it succeeds, writes stall.
                Err(e) => tracing::error!(
                    "'{}' leg '{}': marked stale but NOT removed from the raid ({}) — writes \
                     stay stalled until the next pass",
                    volume, leg.target_id, e
                ),
            }
        }
        degraded
    }

    /// THE REBUILD (`RebuildStart` → `RebuildComplete`): how a stale leg
    /// earns its place back — and the ONLY door through which a leg ever
    /// becomes a member of a live composition.
    ///
    /// The copy engine is SPARSE BY CONSTRUCTION and it is not ours:
    /// SPDK's `bdev_lvol_start_shallow_copy` walks the source blob's
    /// cluster map and skips every cluster the blob does not own
    /// (blobstore.c, `bs_shallow_copy_cluster_find_next`), writing the
    /// rest at identical offsets on the destination. That is the whole
    /// of §12's "flint-driven and sparse-aware, never raid1's own
    /// process": raid1 rebuilds by walking the arena from zero with no
    /// zero-detection (raid1.c:564-584), which densifies the thin target
    /// leg — full logical size, hours at multi-TB, and cross-AZ egress
    /// on every byte that was never written.
    ///
    /// The copy's source must be READ ONLY (blobstore.c: a shallow copy
    /// of a writable blob is -EPERM). That is not an obstacle but the
    /// mechanism: each round snapshots the live head, and the snapshot's
    /// OWN clusters are exactly the bytes written since the previous
    /// round. The blobstore's copy-on-write IS the dirty-region
    /// tracking, so flint keeps none of its own and cannot get it wrong.
    ///
    ///   round 1   cut the head, copy the cut     — the volume, sparsely
    ///   round n   cut again, copy the cut        — that round's writes
    ///   window    quiesce, cut, copy, ADD, mark  — the last delta
    ///
    /// The ladder converges when the copy outruns the writer, and is
    /// bounded when it does not (`rebuild_max_rounds`) because the
    /// window's price is a stall on every client of the volume.
    ///
    /// THE WINDOW is flint's carried SPDK patch, and its contract is
    /// enforced target-side: `bdev_raid_add_base_bdev --skip-rebuild`
    /// refuses unless a `bdev_raid_quiesce` lease is HELD, because the
    /// cut that produced the base and the add that admits it must sit
    /// inside ONE quiesce or the writes between them exist nowhere on
    /// the new leg. The lease auto-expires, so an orchestrator that dies
    /// mid-window cannot leave guest I/O gated behind it.
    ///
    /// THE ORDER is the degrade barrier's, mirrored. There the record
    /// went stale BEFORE the composition degraded; here the leg becomes
    /// a member BEFORE the record calls it in sync. One rule stated
    /// twice: THE RECORD'S OPTIMISM TRAILS REALITY. A mark that landed
    /// first and then crashed would leave an electable leg missing the
    /// final delta, and `ElectInSync` would hand it the composition in
    /// good faith.
    ///
    /// Note which of the model's two rejoin doors this is. It is always
    /// `RebuildComplete` — the full copy of the source's allocated set —
    /// and never `DeltaRejoin`, which copies only what changed since the
    /// stale leg's own cut and therefore needs `AncestryGuard` to prove
    /// the leg is still AT that cut. flint does not take that door, so
    /// it does not need that proof: every round after the first is a
    /// delta of OUR OWN cuts, laid over a destination this rebuild
    /// wrote from the beginning.
    pub async fn rebuild_leg(&self, volume: &str, peer: &str) -> RebuildOutcome {
        if self.rebuilding.insert(volume.to_string(), ()).is_some() {
            return RebuildOutcome::Deferred(format!(
                "a rebuild of '{volume}' is already running"
            ));
        }
        let out = self.rebuild_inner(volume, peer).await;
        self.rebuilding.remove(volume);
        out
    }

    async fn rebuild_inner(&self, volume: &str, peer: &str) -> RebuildOutcome {
        let me = target_id();
        // ---- the record is the only door ----
        let seat = match self.backend.block_volume_seat(volume).await {
            Ok(Ok(Some(s))) => s,
            Ok(Ok(None)) => {
                return RebuildOutcome::Refused(format!("'{volume}' has no seat"))
            }
            Ok(Err(e)) => return RebuildOutcome::Deferred(format!("seat unreadable: {e}")),
            Err(e) => return RebuildOutcome::Deferred(format!("seat unreadable: {e}")),
        };
        if seat.composer != me {
            return RebuildOutcome::Refused(format!(
                "'{volume}' is seated at '{}', not here — only the composer may rebuild a leg, \
                 because only its bytes are the volume's",
                seat.composer
            ));
        }
        let legs = match self.backend.block_legs(volume).await {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => return RebuildOutcome::Deferred(format!("legs unreadable: {e}")),
            Err(e) => return RebuildOutcome::Deferred(format!("legs unreadable: {e}")),
        };
        let Some(leg) = legs.iter().find(|l| l.target_id == peer) else {
            return RebuildOutcome::Refused(format!(
                "'{volume}' has no leg row for '{peer}' — a rebuild copies onto a leg the \
                 record already knows about, it does not mint one"
            ));
        };
        if leg.sync_state != crate::state_backend::extent_alloc::LEG_STALE {
            return RebuildOutcome::NotNeeded;
        }
        // ---- the peer must be AFFIRMATIVELY reachable ----
        //
        // Not merely "not condemned": a rebuild pours the whole volume
        // at this peer and ends by holding every client's I/O while it
        // finishes. Starting one at a target nothing has heard from
        // spends all of that to fail at the end.
        match self.reachability(peer) {
            Some(Reachability::Reachable) => {}
            other => {
                return RebuildOutcome::Deferred(format!(
                    "leg peer '{peer}' is {}",
                    match other {
                        Some(v) => format!("{v:?}"),
                        None => "not yet observed".to_string(),
                    }
                ))
            }
        }
        // ---- the frame must have a slot for this leg ----
        let raid = self.raid_name(volume);
        let live = match self.get_raid(&raid).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return RebuildOutcome::Deferred(format!(
                    "'{volume}' has no live composition to rejoin — the frame is built by the \
                     converge pass"
                ))
            }
            Err(e) => return RebuildOutcome::Deferred(e),
        };
        if live.get("state").and_then(|s| s.as_str()) != Some("online") {
            return RebuildOutcome::Deferred(format!(
                "composition of '{volume}' is not online — nothing to rejoin yet"
            ));
        }
        if Self::raid_empty_slots(&live) == 0 {
            // A raid's slot count is fixed at creation, so this is not a
            // wait — it is a statement that the frame was built without
            // room for this leg and only a re-frame (a converge that
            // builds the composition from nothing) can make room.
            return RebuildOutcome::Refused(format!(
                "the composition of '{volume}' has no empty slot for leg '{peer}': a raid's \
                 slots are fixed when it is created, so this leg can only join a frame built \
                 while the record already named it"
            ));
        }
        // ---- crash leftovers, then the parent guard ----
        //
        // Sweeping first is what makes the guard below meaningful: our
        // own abandoned cuts are parents too, and merging them back into
        // the head is also how the chain gets cleaned up. One mechanism,
        // both jobs.
        let head = self.bdev_name(volume);
        let swept = self.sweep_cuts(volume).await;
        if swept > 0 {
            tracing::info!("'{}': swept {} abandoned rebuild cut(s) first", volume, swept);
        }
        let info = match self.lvol_info(&head).await {
            Ok(i) => i,
            Err(e) => return RebuildOutcome::Deferred(e),
        };
        let is_clone = info
            .get("driver_specific")
            .and_then(|d| d.get("lvol"))
            .and_then(|l| l.get("clone"))
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        if is_clone {
            // A shallow copy carries only the blob's OWN clusters. If
            // the head still has an ancestor, the clusters the ancestor
            // holds are skipped and the destination reads ZEROS where
            // the volume holds data — silent, and only discovered at a
            // failover. Walking the chain oldest-first is the extension
            // (the file tier's `copy_chain_to` does exactly that);
            // refusing is the honest answer until it exists.
            return RebuildOutcome::Refused(format!(
                "'{volume}' still has a parent snapshot ({}): a sparse copy carries only the \
                 head's own clusters, so the rebuilt leg would read zeros wherever the ancestor \
                 holds the data. Refusing rather than building a leg with holes",
                info.get("driver_specific")
                    .and_then(|d| d.get("lvol"))
                    .and_then(|l| l.get("base_snapshot"))
                    .and_then(|b| b.as_str())
                    .unwrap_or("unknown")
            ));
        }
        // ---- the destination ----
        //
        // Attached but NOT a member: the raid holds a write claim on its
        // bases, and a member would be taking live writes underneath the
        // copy. It joins in the window, once it holds the bytes.
        let dst = match self.attach_peer_leg(volume, peer).await {
            Ok(d) => d,
            Err(e) => return RebuildOutcome::Deferred(e),
        };
        let cluster = self.cluster_bytes().await.unwrap_or(4 * 1024 * 1024);
        tracing::info!(
            "🔧 '{}': rebuilding leg '{}' — sparse copy onto {}, window budget {} MiB",
            volume,
            peer,
            dst,
            rebuild_window_max_bytes() / (1024 * 1024)
        );

        // ---- the ladder ----
        let max_rounds = rebuild_max_rounds();
        let budget = rebuild_window_max_bytes();
        let mut round = 0u32;
        let mut clusters = 0u64;
        loop {
            let pending = self.allocated_bytes(&head, cluster).await.unwrap_or(u64::MAX);
            if pending <= budget {
                break;
            }
            if round >= max_rounds {
                tracing::warn!(
                    "'{}': leg '{}' still has {} MiB of delta after {} round(s) — the writer is \
                     outrunning the copy, so the window will be longer than its budget",
                    volume,
                    peer,
                    pending / (1024 * 1024),
                    round
                );
                break;
            }
            round += 1;
            let cut = self.cut_name(volume, round);
            if let Err(e) = self.snapshot_lvol(&head, &cut).await {
                self.abandon_rebuild(volume, peer).await;
                return RebuildOutcome::Deferred(e);
            }
            match self.shallow_copy(&self.cut_alias(volume, round), &dst).await {
                Ok(n) => {
                    clusters += n;
                    tracing::info!(
                        "'{}': leg '{}' round {} copied {} cluster(s)",
                        volume, peer, round, n
                    );
                }
                Err(e) => {
                    self.abandon_rebuild(volume, peer).await;
                    return RebuildOutcome::Deferred(e);
                }
            }
        }

        // ---- the window ----
        //
        // Under the volume lock, so a converge cannot re-frame the
        // composition underneath it.
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        // The world may have moved during a copy that took hours. A
        // rebuild that started while we composed the volume must not
        // finish after we were deposed: the leg would be marked in sync
        // against bytes that are no longer the volume's.
        match self.backend.block_volume_seat(volume).await {
            Ok(Ok(Some(s))) if s.composer == me && s.epoch == seat.epoch => {}
            Ok(Ok(Some(s))) => {
                self.abandon_rebuild(volume, peer).await;
                return RebuildOutcome::Refused(format!(
                    "'{volume}' moved to epoch {} at '{}' during the rebuild — the copy is \
                     abandoned rather than marked in sync",
                    s.epoch, s.composer
                ));
            }
            _ => {
                self.abandon_rebuild(volume, peer).await;
                return RebuildOutcome::Deferred("seat unreadable at the window".into());
            }
        }
        match self.backend.block_lease(volume).await {
            Ok(Ok(Some(l))) if l.holder == me && l.is_live_at(now_unix()) => {}
            _ => {
                self.abandon_rebuild(volume, peer).await;
                return RebuildOutcome::Deferred(format!(
                    "'{volume}': this target does not hold a live serving lease — the exercise \
                     must not outlive the entitlement"
                ));
            }
        }
        round += 1;
        let result = self.rebuild_window(volume, peer, &raid, &head, &dst, round).await;
        // ALWAYS release. The lease would expire on its own — that is
        // what it is for — but leaving client I/O gated for the rest of
        // a lease we are finished with is a stall we chose not to spend.
        if let Err(e) = self.unquiesce(&raid).await {
            tracing::error!(
                "'{}': releasing the quiesce failed ({}) — the lease expires on its own, so \
                 client I/O resumes within {} ms",
                volume, e, rebuild_lease_ms()
            );
        }
        match result {
            Ok(n) => {
                clusters += n;
                let swept = self.sweep_cuts(volume).await;
                tracing::info!(
                    "✅ '{}': leg '{}' is IN SYNC and back in the composition — {} cluster(s) \
                     over {} round(s), {} cut(s) swept",
                    volume, peer, clusters, round, swept
                );
                RebuildOutcome::Rebuilt { peer: peer.to_string(), rounds: round, clusters }
            }
            Err(e) => {
                self.abandon_rebuild(volume, peer).await;
                RebuildOutcome::Deferred(e)
            }
        }
    }

    /// The quiesced window: cut, copy, admit, mark. Every step of it is
    /// ordered, and the ordering is the correctness argument — see
    /// `rebuild_leg`. The caller releases the quiesce whatever happens
    /// here.
    async fn rebuild_window(
        &self,
        volume: &str,
        peer: &str,
        raid: &str,
        head: &str,
        dst: &str,
        round: u32,
    ) -> Result<u64, String> {
        let lease_ms = rebuild_lease_ms();
        self.quiesce(raid, lease_ms).await?;
        let armed = std::time::Instant::now();
        // The cut MUST be inside the held quiesce: a cut taken before it
        // misses every write that landed between the two, and those
        // writes would exist nowhere on the new leg.
        let cut = self.cut_name(volume, round);
        self.snapshot_lvol(head, &cut).await?;
        let copied = self.shallow_copy(&self.cut_alias(volume, round), dst).await?;
        // Did the window outlive its own lease? A lapsed lease
        // auto-unquiesces, writes resume, and our cut is stale — and a
        // renewal after that is indistinguishable from a fresh quiesce
        // at the RPC, so the target's own contract check would pass
        // while the base silently missed the writes. Our clock is the
        // only witness, so it is the one that refuses.
        let spent = armed.elapsed().as_millis() as u64;
        if spent * 4 > lease_ms * 3 {
            return Err(format!(
                "'{volume}': the final delta took {spent} ms of a {lease_ms} ms quiesce lease — \
                 abandoning the window rather than admitting a leg whose cut may predate a \
                 lapse. The next attempt starts from a smaller delta"
            ));
        }
        // Renew immediately before the add, so the add itself runs on a
        // full lease. The target pins the lease across the add, so an
        // expiry during channel installation defers to it.
        self.quiesce(raid, lease_ms).await?;
        self.raid_add_insync(raid, dst).await?;
        // AND ONLY NOW the record. If this fails the leg is a member
        // that the record does not vouch for, which the next converge
        // prunes — the safe direction. The reverse would leave an
        // electable leg missing the final delta.
        match self
            .backend
            .block_leg_mark(
                volume,
                peer,
                crate::state_backend::extent_alloc::LEG_INSYNC,
                now_unix(),
            )
            .await
        {
            Ok(Ok(())) => Ok(copied),
            other => {
                let e = match other {
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                    Ok(Ok(())) => unreachable!(),
                };
                let remove = json!({
                    "method": "bdev_raid_remove_base_bdev",
                    "params": { "name": dst }
                });
                let _ = self.rpc.rpc(&remove).await;
                Err(format!(
                    "'{volume}': leg '{peer}' was admitted but its in-sync mark did not land \
                     ({e}) — the leg was removed again, because a member the record cannot \
                     vouch for is a member nobody can elect"
                ))
            }
        }
    }

    /// Give up on a rebuild in progress: drop the destination
    /// controller, sweep our cuts. Best-effort by design — every one of
    /// these is level-triggered and the next attempt re-does it.
    ///
    /// The detach matters more than it looks: a composed leg's transport
    /// QUEUES I/O rather than failing it (the degrade barrier's other
    /// half), so an abandoned destination whose peer never comes back
    /// holds queued writes forever.
    async fn abandon_rebuild(&self, volume: &str, peer: &str) {
        let ctrl = format!("flintleg-{volume}-{peer}");
        // guarded-destroy-lint: allow — the subject is a controller
        // THIS rebuild attached as a copy destination, and dropping it
        // destroys no data: the peer's leg keeps whatever was copied,
        // and the next attempt starts the ladder again from a cut.
        // Leaving it attached is the harmful option, not the safe one —
        // a composed leg's transport queues I/O rather than failing it.
        let detach = json!({
            "method": "bdev_nvme_detach_controller", // guarded-destroy-lint: allow
            "params": { "name": ctrl }
        });
        let _ = self.rpc.rpc(&detach).await;
        let swept = self.sweep_cuts(volume).await;
        tracing::warn!(
            "'{}': rebuild of leg '{}' abandoned — destination detached, {} cut(s) swept",
            volume, peer, swept
        );
    }

    /// Every stale leg on a volume this target composes. The pass spawns
    /// rebuilds rather than awaiting them: a full copy can take hours,
    /// and the loop that would be blocked is the one that renews every
    /// serving lease on this target.
    pub async fn rebuild_candidates(&self, volumes: &[String]) -> Vec<(String, String)> {
        let me = target_id();
        let mut out = Vec::new();
        for volume in volumes {
            match self.backend.block_volume_seat(volume).await {
                Ok(Ok(Some(s))) if s.composer == me => {}
                _ => continue,
            }
            let Ok(Ok(legs)) = self.backend.block_legs(volume).await else { continue };
            for leg in legs.iter().filter(|l| {
                l.target_id != me
                    && l.sync_state == crate::state_backend::extent_alloc::LEG_STALE
            }) {
                out.push((volume.clone(), leg.target_id.clone()));
            }
        }
        out
    }

    // ---- the rebuild's primitives ----

    fn cut_name(&self, volume: &str, round: u32) -> String {
        format!("flintcut-{volume}-{round}")
    }

    fn cut_alias(&self, volume: &str, round: u32) -> String {
        format!("{}/{}", self.lvstore, self.cut_name(volume, round))
    }

    async fn lvol_info(&self, name: &str) -> Result<serde_json::Value, String> {
        let resp = self
            .rpc
            .rpc(&json!({ "method": "bdev_get_bdevs", "params": { "name": name } }))
            .await
            .map_err(|e| format!("bdev_get_bdevs {name}: {e}"))?;
        resp.get("result")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| format!("bdev_get_bdevs {name}: empty reply"))
    }

    /// The lvolstore's cluster size — the unit everything sparse is
    /// counted in.
    async fn cluster_bytes(&self) -> Option<u64> {
        let resp = self
            .rpc
            .rpc(&json!({ "method": "bdev_lvol_get_lvstores" }))
            .await
            .ok()?;
        resp.get("result")?
            .as_array()?
            .iter()
            .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(self.lvstore.as_str()))?
            .get("cluster_size")?
            .as_u64()
    }

    /// What the head owns RIGHT NOW — which, after a cut, is exactly
    /// what has been written since it. The blobstore's copy-on-write
    /// bookkeeping is the dirty-region tracking; this reads it.
    async fn allocated_bytes(&self, lvol: &str, cluster: u64) -> Option<u64> {
        let info = self.lvol_info(lvol).await.ok()?;
        let n = info
            .get("driver_specific")?
            .get("lvol")?
            .get("num_allocated_clusters")?
            .as_u64()?;
        Some(n * cluster)
    }

    async fn snapshot_lvol(&self, lvol: &str, snap: &str) -> Result<(), String> {
        let payload = json!({
            "method": "bdev_lvol_snapshot",
            "params": { "lvol_name": lvol, "snapshot_name": snap }
        });
        match self.rpc.rpc(&payload).await {
            Ok(_) => Ok(()),
            // A resume after a crash between the cut and its copy: same
            // head, same content, and the copy below is idempotent.
            Err(e) if crate::epoch_scheduler::is_already_exists(&e.to_string()) => Ok(()),
            Err(e) => Err(format!("cutting {snap} from {lvol}: {e}")),
        }
    }

    /// One sparse copy, polled to a terminal state. Returns the clusters
    /// copied.
    ///
    /// The poll's bound is PROGRESS, not wall clock (F39): a wall-clock
    /// cap would kill a legitimate multi-hour bulk copy of a large
    /// volume, while a copy that stops moving clusters is wedged no
    /// matter how long it has been running.
    async fn shallow_copy(&self, src_lvol: &str, dst_bdev: &str) -> Result<u64, String> {
        let start = json!({
            "method": "bdev_lvol_start_shallow_copy",
            "params": { "src_lvol_name": src_lvol, "dst_bdev_name": dst_bdev }
        });
        let resp = self
            .rpc
            .rpc(&start)
            .await
            .map_err(|e| format!("shallow copy {src_lvol} → {dst_bdev}: {e}"))?;
        let op = resp
            .get("result")
            .and_then(|r| r.get("operation_id"))
            .and_then(|o| o.as_u64())
            .ok_or_else(|| format!("shallow copy of {src_lvol} returned no operation_id"))?;
        let stall = std::time::Duration::from_secs(rebuild_stall_secs());
        let poll = rebuild_poll();
        let mut last_copied: Option<u64> = None;
        let mut last_progress = std::time::Instant::now();
        loop {
            let check = json!({
                "method": "bdev_lvol_check_shallow_copy",
                "params": { "operation_id": op }
            });
            let resp = self
                .rpc
                .rpc(&check)
                .await
                .map_err(|e| format!("checking the copy of {src_lvol}: {e}"))?;
            let result = resp.get("result").cloned().unwrap_or_default();
            let copied = result.get("copied_clusters").and_then(|c| c.as_u64());
            match result.get("state").and_then(|s| s.as_str()) {
                Some("complete") => return Ok(copied.unwrap_or(0)),
                Some("in progress") => {
                    if copied != last_copied {
                        last_copied = copied;
                        last_progress = std::time::Instant::now();
                    } else if last_progress.elapsed() >= stall {
                        return Err(format!(
                            "the copy of {src_lvol} has moved no clusters for {}s (stuck at \
                             {last_copied:?}) — declaring it wedged (F39)",
                            stall.as_secs()
                        ));
                    }
                    if !poll.is_zero() {
                        tokio::time::sleep(poll).await;
                    }
                }
                // Includes the destination running out of room: abort,
                // stay stale, surface it — never retry into a full pool.
                Some("error") => {
                    let detail =
                        result.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                    return Err(format!("the copy of {src_lvol} failed: {detail}"));
                }
                other => {
                    return Err(format!(
                        "the copy of {src_lvol} returned an unexpected state {other:?}"
                    ))
                }
            }
        }
    }

    async fn quiesce(&self, raid: &str, lease_ms: u64) -> Result<(), String> {
        let payload = json!({
            "method": "bdev_raid_quiesce",
            "params": { "name": raid, "lease_ms": lease_ms }
        });
        self.rpc
            .rpc(&payload)
            .await
            .map(|_| ())
            .map_err(|e| format!("quiescing {raid}: {e}"))
    }

    async fn unquiesce(&self, raid: &str) -> Result<(), String> {
        let payload = json!({ "method": "bdev_raid_unquiesce", "params": { "name": raid } });
        self.rpc
            .rpc(&payload)
            .await
            .map(|_| ())
            .map_err(|e| format!("releasing the quiesce of {raid}: {e}"))
    }

    async fn raid_add_insync(&self, raid: &str, base: &str) -> Result<(), String> {
        // guarded-construct-lint: allow — the hazard this lint guards is
        // a composition assembled over bases nobody validated. This call
        // is the opposite: it is the ONE site that has proof, and the
        // proof is enforced target-side as well as here. `skip_rebuild`
        // is refused by flint's carried patch unless a quiesce lease is
        // held, and the caller took the cut this base was built from
        // inside that same lease. No other site may add a base at all.
        let payload = json!({
            "method": "bdev_raid_add_base_bdev", // guarded-construct-lint: allow
            "params": { "raid_bdev": raid, "base_bdev": base, "skip_rebuild": true }
        });
        self.rpc
            .rpc(&payload)
            .await
            .map(|_| ())
            .map_err(|e| format!("admitting {base} to {raid}: {e}"))
    }

    /// Merge our cuts back into the head, newest first.
    ///
    /// Deleting a snapshot with exactly one clone is a metadata act —
    /// the blobstore hands the clusters to the clone rather than copying
    /// them — so this is cheap, and it is also the crash recovery: a
    /// chain left by an abandoned rebuild is swept before the next one
    /// starts, which is what keeps the head parent-free.
    async fn sweep_cuts(&self, volume: &str) -> usize {
        let prefix = format!("{}/flintcut-{}-", self.lvstore, volume);
        let Ok(resp) = self.rpc.rpc(&json!({ "method": "bdev_get_bdevs" })).await else {
            return 0;
        };
        let mut cuts: Vec<(u32, String)> = resp
            .get("result")
            .and_then(|r| r.as_array())
            .map(|bs| {
                bs.iter()
                    .flat_map(|b| {
                        b.get("aliases")
                            .and_then(|a| a.as_array())
                            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                            .unwrap_or_default()
                    })
                    .filter_map(|alias| {
                        let round = alias.strip_prefix(prefix.as_str())?.parse::<u32>().ok()?;
                        Some((round, alias.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        cuts.sort_by_key(|c| std::cmp::Reverse(c.0));
        let mut swept = 0;
        for (_, alias) in cuts {
            // Guarded-destroy: this is one of our own cuts, named by
            // this volume and this mechanism, and its content is merged
            // into its clone rather than discarded.
            let del = json!({ "method": "bdev_lvol_delete", "params": { "name": alias } }); // guarded-destroy-lint: allow
            match self.rpc.rpc(&del).await {
                Ok(_) => swept += 1,
                Err(e) => {
                    tracing::warn!("'{}': could not sweep cut {} ({}) — leaving the rest", volume, alias, e);
                    break;
                }
            }
        }
        swept
    }

    /// ASSEMBLY (`Assemble`) — the act that makes an elected composer a
    /// serving one, and the act that GRANTS THE EPOCH'S LEASE. Those are
    /// one thing, which is tranche 3's finding (b): a composer that
    /// serves on some earlier lease's lapse gets deposed later, and the
    /// promoter reads that ancient lapse as an already-passed horizon
    /// and assembles over a still-serving zombie.
    ///
    /// In order, and the order is the model's:
    ///   1. the seat must name this target — the record is the only door;
    ///   2. THE HORIZON — the standing lease (whoever holds it) must have
    ///      expired, or belong to us already;
    ///   3. evict the deposed at this target's leg-export;
    ///   4. mark the deposed leg STALE, so the election gate will not
    ///      hand the composition back to it before a rebuild;
    ///   5. grant this epoch's lease;
    ///   6. converge the export, whose allow-list is admissions minus
    ///      fenced — the FENCE REPLAY, fail-closed by construction:
    ///      `ensure_export` creates the subsystem with
    ///      `allow_any_host: false`, converges the host list, and only
    ///      then adds the namespace and the listener, so there is no
    ///      instant at which the volume is reachable by a client the
    ///      MDS-side computation excluded. PTPL never travels, so that
    ///      allow-list is the ONLY exclusion a fenced client meets here.
    ///
    /// 5 before 6 deliberately, and this is the one place the code
    /// cannot be as atomic as the model. A crash between them leaves a
    /// lease with no export — harmless, and the next converge builds it.
    /// The other order would leave an export SERVING with no lease: the
    /// dead-man's work list is what a target holds, so it would never
    /// look at it again, and converge would refuse it forever. Exercise
    /// must never outlive entitlement.
    ///
    /// The standing lease is also what names the DEPOSED target. There
    /// is no separate "who was serving" record because the lease is
    /// exactly that record — holder plus epoch.
    pub async fn assemble(&self, volume: &str) -> AssemblyOutcome {
        let me = target_id();
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;

        let seat = match self.backend.block_volume_seat(volume).await {
            Ok(Ok(Some(s))) => s,
            Ok(Ok(None)) => return AssemblyOutcome::Refused("no seat".into()),
            Ok(Err(e)) => return AssemblyOutcome::Refused(format!("seat unreadable: {e}")),
            Err(e) => return AssemblyOutcome::Refused(format!("seat unreadable: {e}")),
        };
        if seat.composer != me {
            return AssemblyOutcome::Refused(format!(
                "the record seats '{}' at '{}' (epoch {}), not at this target ('{}')",
                volume, seat.composer, seat.epoch, me
            ));
        }

        let standing = match self.backend.block_lease(volume).await {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => return AssemblyOutcome::Refused(format!("lease unreadable: {e}")),
            Err(e) => return AssemblyOutcome::Refused(format!("lease unreadable: {e}")),
        };
        let now = now_unix();
        let deposed = match &standing {
            Some(l) if l.holder == me && l.epoch == seat.epoch => {
                return AssemblyOutcome::AlreadyAssembled { epoch: l.epoch }
            }
            // Our own lease at an older epoch: we were the one serving,
            // so there is no foreign horizon to wait out and no other
            // leg to depose.
            Some(l) if l.holder == me => None,
            Some(l) if l.is_live_at(now) => {
                return AssemblyOutcome::AwaitingHorizon {
                    deposed: l.holder.clone(),
                    until_unix: l.expires_unix,
                }
            }
            Some(l) => Some(l.holder.clone()),
            None => None,
        };

        if let Some(ref d) = deposed {
            if let Err(e) = self.evict_deposed_at_leg(volume, d).await {
                return AssemblyOutcome::Refused(format!("eviction failed: {e}"));
            }
            // The deposed leg missed everything this composition is
            // about to accept. Marking it stale is what stops the
            // election gate handing the volume straight back to it —
            // only a completed rebuild clears the mark
            // (`RecordRejoinOnly`).
            match self
                .backend
                .block_leg_mark(volume, d, crate::state_backend::extent_alloc::LEG_STALE, now)
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return AssemblyOutcome::Refused(format!("stale mark refused: {e}"))
                }
                Err(e) => return AssemblyOutcome::Refused(format!("stale mark failed: {e}")),
            }
        }

        match self
            .backend
            .block_lease_grant(volume, seat.epoch, &me, now + lease_ttl())
            .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return AssemblyOutcome::Refused(format!("lease grant refused: {e}")),
            Err(e) => return AssemblyOutcome::Refused(format!("lease grant failed: {e}")),
        }

        // Now the export, through the ordinary converge path so the
        // fence replay is the same code every client gets and cannot
        // drift into a second implementation.
        if let Err(e) = self.ensure_locked(volume, ConvergeMode::Reconcile).await {
            return AssemblyOutcome::Refused(format!(
                "lease granted but the export did not converge: {e} — the next pass retries"
            ));
        }
        AssemblyOutcome::Assembled { epoch: seat.epoch, deposed }
    }

    /// Assemble every volume seated here that this target does not yet
    /// hold the seated epoch's lease for. Returns only the volumes it
    /// acted on or is waiting on, so a steady-state pass reports
    /// nothing.
    pub async fn assembly_pass(&self, volumes: &[String]) -> Vec<(String, AssemblyOutcome)> {
        let me = target_id();
        let mut out = Vec::new();
        for v in volumes {
            let seat = match self.backend.block_volume_seat(v).await {
                Ok(Ok(Some(s))) => s,
                _ => continue,
            };
            if seat.composer != me {
                continue;
            }
            let held = matches!(
                self.backend.block_lease(v).await,
                Ok(Ok(Some(ref l))) if l.holder == me && l.epoch == seat.epoch
            );
            if held {
                continue;
            }
            out.push((v.clone(), self.assemble(v).await));
        }
        out
    }

    /// THE DEAD-MAN (design §12; `DeadmanGate`). For every volume this
    /// target holds a lease on: renew it, and if the renewal is REFUSED
    /// and the standing lease has already expired, suspend the export
    /// and surrender the lease.
    ///
    /// This is the only exclusion a composer's LOCAL leg has. Eviction
    /// at the survivor's leg-export cannot reach a partitioned composer
    /// serving its own disk to its own clients; nothing the MDS says
    /// reaches it either. It has to stop itself.
    ///
    /// Both conditions are required, and the order is what makes it
    /// safe: a renewal that SUCCEEDS proves the record still vouches for
    /// us, so a stalled loop can never suspend a healthy volume — it
    /// simply does not run, and the expiry it would have found is
    /// repaired by the very renewal that precedes the check. Suspension
    /// happens only where the record has stopped vouching or cannot be
    /// read at all.
    ///
    /// What it CANNOT promise is timeliness, and that is the model's
    /// `DeadmanCertain` axiom priced: a loop that is late leaves a
    /// window in which a deposed composer still answers reads. The Skew
    /// run puts a number on what that costs — stale READS only, because
    /// writes stay contained by eviction and the degrade barrier.
    pub async fn deadman_pass(&self) -> Vec<(String, String)> {
        let me = target_id();
        let leases = match self.backend.block_leases_held(&me).await {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => {
                tracing::error!("dead-man: lease list refused: {e}");
                return Vec::new();
            }
            Err(e) => {
                tracing::error!("dead-man: lease list unreadable: {e}");
                return Vec::new();
            }
        };
        let now = now_unix();
        let mut suspended = Vec::new();
        for lease in leases {
            let Err(why) = self.renew_lease(&lease.volume, &me).await else {
                continue;
            };
            if lease.is_live_at(now) {
                // Refused, but the horizon has not passed. Serving
                // continues, deliberately: the clients' writes are still
                // this composition's to accept until the lease it was
                // granted under actually runs out, and cutting them off
                // early is what strands acked writes on a doomed leg.
                tracing::warn!(
                    "dead-man '{}': renewal refused ({}) — suspending in {}s when the epoch-{} \
                     lease expires",
                    lease.volume,
                    why,
                    lease.expires_unix - now,
                    lease.epoch
                );
                continue;
            }
            tracing::error!(
                "⛔ dead-man '{}': the epoch-{} lease expired at {} and renewal is refused ({}) \
                 — SUSPENDING this target's export",
                lease.volume,
                lease.epoch,
                lease.expires_unix,
                why
            );
            match self.suspend_export(&lease.volume).await {
                Ok(()) => {
                    // Surrender explicitly, so nothing downstream has to
                    // tell "expired" from "expired and acted upon".
                    match self.backend.block_lease_drop(&lease.volume).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::error!(
                            "dead-man '{}': suspended but lease not surrendered: {e}",
                            lease.volume
                        ),
                        Err(e) => tracing::error!(
                            "dead-man '{}': suspended but lease not surrendered: {e}",
                            lease.volume
                        ),
                    }
                    suspended.push((lease.volume.clone(), why));
                }
                Err(e) => tracing::error!(
                    "dead-man '{}': SUSPENSION FAILED ({}) — this target may still be serving a \
                     composition it no longer holds",
                    lease.volume,
                    e
                ),
            }
        }
        suspended
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

    async fn ensure_locked(&self, volume: &str, mode: ConvergeMode) -> Result<(), String> {
        let size_bytes = match mode {
            ConvergeMode::Provision(n) => Some(n),
            _ => None,
        };
        // ---- the seat and the LEASE, BEFORE any device state (§12) ----
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
        match mode {
            ConvergeMode::Provision(_) => {
                // Announce before seating: the volume is about to be
                // seated at this target, and the very next thing a new
                // volume gets is a ControllerPublish that must resolve an
                // address. The reconcile pass would get there eventually;
                // a CreateVolume immediately followed by an attach would
                // not wait for it.
                self.self_register().await?;
                self.seat_here(volume).await?;
            }
            ConvergeMode::Reconcile => {
                // RENEW, not merely check. Converging IS the assertion
                // that this target serves the volume, and the renewal is
                // record-conditioned at the MDS — so this one call
                // enforces both doors: the record must still seat the
                // volume here, and the lease must be the one assembly
                // granted for the CURRENT epoch. A deposed composer is
                // refused here however healthy it is, and an ELECTED one
                // is refused too, because a lease is granted by assembly
                // and never taken by the holder that wants it.
                if let Err(e) = self.renew_lease(volume, &me).await {
                    return Err(format!("refusing to converge '{volume}': {e}"));
                }
            }
            // The dead-man has no record to satisfy. That is the point:
            // it runs when the record has stopped vouching for us.
            ConvergeMode::Suspend => {}
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

        // ---- the composition ----
        //
        // What the namespace serves is the RAID when the record gives
        // this volume more than one in-sync leg, and the bare lvol when
        // it is solo. Both expose the same byte space (`superblock:
        // false`), so this can change under a live volume without moving
        // data — see `compose_bdev`.
        //
        // Not under `Suspend`: a target the dead-man is closing must not
        // be reaching out to attach peer legs on its way out.
        let seat = match self.backend.block_volume_seat(volume).await {
            Ok(Ok(Some(s))) => Some(s),
            _ => None,
        };
        let served = match (mode, &seat) {
            (ConvergeMode::Suspend, _) | (_, None) => bdev.clone(),
            (_, Some(seat)) => match self.compose_bdev(volume, seat).await {
                Ok(b) => b,
                Err(e) => {
                    // A composition that will not build is a DEGRADED
                    // volume, not a dead one: the local leg carries the
                    // volume's bytes, so serve solo and say so loudly
                    // rather than refusing every client because a peer
                    // is unreachable.
                    tracing::error!(
                        "'{}' could not be composed ({}) — serving SOLO from the local leg; \
                         acked writes from here will need a rebuild before that peer can be \
                         elected",
                        volume, e
                    );
                    bdev.clone()
                }
            },
        };

        // ---- subsystem / namespace / listener / hosts ----
        //
        // Under `Suspend` the desired allow-list is the fence lane and
        // nothing else. It is expressed as DESIRED STATE rather than as
        // a one-off teardown for the reason everything here is
        // level-triggered: a suspension applied imperatively would be
        // undone by the next converge, and the admissions in sqlite are
        // deliberately NOT deleted — the clients are still legitimately
        // admitted, it is this TARGET that has lost the right to serve
        // them. Removing a host from the allow-list tears its controller
        // down, which is what makes this a real suspension and not a
        // request.
        let hosts = match mode {
            ConvergeMode::Suspend => vec![crate::identity::block_mds_host_nqn()],
            _ => self.desired_hosts(volume).await?,
        };
        let nqn = crate::identity::block_volume_export_nqn(volume);
        let (uuid, nguid) = crate::nvmeof_export::stable_ns_identity(volume);
        let ptpl = format!("{}/flint-ptpl-{}.json", self.ptpl_dir, volume);
        // The aliases belong to the bdev being SERVED, and getting this
        // wrong is a silent-divergence bug rather than a cosmetic one:
        // `ns_matches` accepts a namespace pointing at any alias of the
        // spec's bdev, so handing it the LVOL's aliases while asking for
        // the RAID makes a stale lvol-backed namespace look correct —
        // the volume would keep serving one leg while the record said
        // two, with every write landing on a single copy. Caught by
        // `a_peer_leg_composes_a_raid_and_losing_it_falls_back_to_the_lvol`.
        // A raid's name is canonical and has no aliases.
        let served_aliases: &[&str] = if served == bdev { &lvol_id_refs } else { &[] };
        let spec = ExportSpec {
            nqn: &nqn,
            bdev_name: &served,
            bdev_aliases: served_aliases,
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
        // Converge first so the fence lane's host NQN is on the
        // allow-list. NOTE the asymmetry this opens once seats can move:
        // the converge reaches the LOCAL tgt while `resolve` below dials
        // whatever the record names. They are the same target today, and
        // when they stop being, a deposed target's converge is refused
        // here (its lease will not renew) rather than fencing the wrong
        // one — fail-closed, and the remote arm belongs to the same work
        // that brings assembly.
        tracing::debug!("fence_preempt {}: converging export", volume);
        self.ensure_locked(volume, ConvergeMode::Reconcile).await?;
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
        self.ensure_locked(volume, ConvergeMode::Reconcile).await?;
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

    /// PLACEMENT, peer side: host a copy of a volume this target does
    /// NOT compose (design §12).
    ///
    /// The CSI controller calls this, because it is the only component
    /// that can see the whole fleet — an MDS shard's sqlite is its own
    /// and shares nothing with its peers, which is why a target learns
    /// about another target by being TOLD, never by discovery.
    ///
    /// Three acts, and the order is the usual one: the record first, so
    /// a crash leaves a seat with no device (which the next pass
    /// converges) rather than a device no record names.
    ///   1. seat the volume at its COMPOSER — this shard's own statement
    ///      that the volume is not its to serve, and what the leg
    ///      export's allow-list is derived from;
    ///   2. mint the lvol, thin and EMPTY. It holds none of the volume's
    ///      bytes, which is exactly why the composer records this leg as
    ///      STALE and why only a rebuild may say otherwise;
    ///   3. converge the leg export that offers it to the composer.
    ///
    /// Idempotent throughout: CreateVolume is retried by the
    /// provisioner, and placement is deterministic, so this runs again
    /// with the same arguments and must reach the same state.
    pub async fn host_leg(
        &self,
        volume: &str,
        size_bytes: u64,
        composer: &str,
    ) -> Result<(), String> {
        let me = target_id();
        if composer == me {
            return Err(format!(
                "'{volume}' is composed HERE — a target cannot also host its leg: the raid holds \
                 an exclusive claim on that lvol and a second export of it is EPERM"
            ));
        }
        {
            let lock = self.lock_for(volume);
            let _g = lock.lock().await;
            // Seat INSERT-if-absent, like every other seating: an upsert
            // here would let a stale placement call move a live volume's
            // composer, which is promotion's job and nobody else's.
            match self
                .backend
                .block_seat_volume(volume, composer, now_unix(), now_unix() + lease_ttl())
                .await
            {
                Ok(Ok(seat)) if seat.composer == composer => {}
                Ok(Ok(seat)) => {
                    return Err(format!(
                        "'{volume}' is already seated at '{}' here, not at '{composer}' — \
                         refusing to host a leg for a composer the record does not name",
                        seat.composer
                    ))
                }
                Ok(Err(e)) => return Err(format!("seating '{volume}': {e}")),
                Err(e) => return Err(format!("seating '{volume}': {e}")),
            }
            let bdev = self.bdev_name(volume);
            let probe = json!({ "method": "bdev_get_bdevs", "params": { "name": bdev } });
            if self.rpc.rpc(&probe).await.is_err() {
                self.check_store_has_room(volume, size_bytes).await?;
                let size_mib = size_bytes.div_ceil(1024 * 1024).max(1);
                let create = json!({
                    "method": "bdev_lvol_create",
                    "params": {
                        "lvs_name": self.lvstore,
                        "lvol_name": volume,
                        "size_in_mib": size_mib,
                        // Thin, like the volume's own lvol: an unwritten
                        // cluster reads zeros, and this leg is nothing
                        // BUT unwritten clusters until the rebuild runs.
                        "thin_provision": true
                    }
                });
                if let Err(e) = self.rpc.rpc(&create).await {
                    if self.rpc.rpc(&probe).await.is_err() {
                        return Err(format!("bdev_lvol_create {bdev}: {e}"));
                    }
                }
            }
        }
        self.ensure_leg_export(volume).await?;
        tracing::info!(
            "🧩 '{}': hosting a leg here for composer '{}' ({} MiB, empty until it rebuilds)",
            volume,
            composer,
            size_bytes / (1024 * 1024)
        );
        Ok(())
    }

    /// PLACEMENT, composer side: record WHERE the second copy lives.
    ///
    /// Two rows, and neither is a device act: the peer's registry row
    /// (so every dial site can resolve it, the whole reason the registry
    /// is an indirection) and the leg row, STALE. The frame and the
    /// rebuild follow from those on the next pass — nothing here builds
    /// anything, which is what makes it safe to run at CreateVolume
    /// before the peer has ever been contacted.
    pub async fn record_leg(
        &self,
        volume: &str,
        leg: &str,
        traddr: &str,
        trsvcid: u16,
    ) -> Result<(), String> {
        if leg == target_id() {
            return Err(format!(
                "'{volume}': the placed leg names this target, which composes it — a volume \
                 cannot be its own second copy"
            ));
        }
        match self.backend.block_target_register(leg, traddr, trsvcid, now_unix()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("registering leg target '{leg}': {e}")),
            Err(e) => return Err(format!("registering leg target '{leg}': {e}")),
        }
        // STALE, and this is the load-bearing word. The peer's lvol
        // exists and is EMPTY; a leg row that said in-sync would make it
        // electable, and `ElectInSync` would hand the volume to a copy
        // of nothing.
        match self
            .backend
            .block_leg_mark(volume, leg, crate::state_backend::extent_alloc::LEG_STALE, now_unix())
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("recording leg '{leg}' of '{volume}': {e}")),
            Err(e) => return Err(format!("recording leg '{leg}' of '{volume}': {e}")),
        }
        tracing::info!(
            "🧩 '{}': second copy placed at '{}' ({}:{}) — recorded STALE; the frame and the \
             rebuild follow from the record",
            volume, leg, traddr, trsvcid
        );
        Ok(())
    }

    /// Targets other than this one holding a leg of `volume`. Read
    /// before DeleteVolume sweeps the record, because afterwards
    /// nothing names the peer's lvol at all.
    pub async fn leg_targets(&self, volume: &str) -> Vec<String> {
        let me = target_id();
        match self.backend.block_legs(volume).await {
            Ok(Ok(legs)) => {
                legs.into_iter().map(|l| l.target_id).filter(|t| *t != me).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Drop a leg this target hosts: the leg export, the lvol, the
    /// record. DeleteVolume's authority, arriving second-hand.
    pub async fn drop_leg(&self, volume: &str) -> Result<(), String> {
        let me = target_id();
        if let Ok(Ok(Some(seat))) = self.backend.block_volume_seat(volume).await {
            if seat.composer == me {
                return Err(format!(
                    "'{volume}' is COMPOSED here — this is the volume itself, not a leg of it; \
                     deleting it is DeleteVolume's act, not a leg drop's"
                ));
            }
        }
        let lock = self.lock_for(volume);
        let _g = lock.lock().await;
        self.drop_leg_export_locked(volume).await?;
        let bdev = self.bdev_name(volume);
        let probe = json!({ "method": "bdev_get_bdevs", "params": { "name": bdev } });
        if self.rpc.rpc(&probe).await.is_ok() {
            // Guarded-destroy: the volume this copy belongs to is being
            // deleted, its export is already gone, and a leg whose
            // volume no longer exists is unreachable by construction —
            // no record on any target will ever name it again.
            let delete = json!({ "method": "bdev_lvol_delete", "params": { "name": bdev } }); // guarded-destroy-lint: allow
            self.rpc
                .rpc(&delete)
                .await
                .map_err(|e| format!("bdev_lvol_delete {bdev}: {e}"))?;
        }
        if let Err(e) = self.backend.extent_drop_volume(volume).await {
            tracing::warn!("'{}': leg record sweep failed ({}) — device state is gone", volume, e);
        }
        tracing::info!("🧹 '{}': leg dropped here — export, lvol and record", volume);
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

        // The horizon the CAS is about to create, read BEFORE it: the
        // deposed composer's lease is what eviction and assembly must
        // wait out. Reading it after would race the dead-man's own
        // surrender of that lease.
        let horizon = match self.backend.block_lease(volume).await {
            Ok(Ok(Some(l))) => l.expires_unix,
            // No lease means nothing is entitled to be serving, so the
            // horizon is already behind us.
            Ok(Ok(None)) => now_unix(),
            Ok(Err(e)) => return PromotionOutcome::Refused(format!("lease unreadable: {e}")),
            Err(e) => return PromotionOutcome::Refused(format!("lease unreadable: {e}")),
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
                evict_after_unix: horizon,
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
        /// raid name → its SLOT TABLE, `null` for an empty slot. The
        /// fake models the two properties the composition path depends
        /// on: a raid is an ordinary bdev once it exists (so the
        /// namespace can point at it exactly as it points at an lvol),
        /// and its slot COUNT is fixed at creation — a leg can only ever
        /// rejoin a slot some removal emptied.
        pub(crate) raids: Mutex<std::collections::HashMap<String, Vec<Option<String>>>>,
        /// Raids under a held `bdev_raid_quiesce` lease.
        pub(crate) quiesced: Mutex<std::collections::HashSet<String>>,
        /// alias → clusters the blob OWNS. Snapshotting moves them to
        /// the cut, which is what makes a shallow copy of the cut the
        /// delta and nothing more.
        pub(crate) alloc: Mutex<std::collections::HashMap<String, u64>>,
        /// alias → parent alias, for the clone/`base_snapshot` fields a
        /// rebuild has to read before it dares copy.
        pub(crate) parents: Mutex<std::collections::HashMap<String, String>>,
        /// Every shallow copy that ran: `(src, dst, clusters)`. The
        /// sparseness claim is only testable because this records what
        /// was actually moved.
        pub(crate) copies: Mutex<Vec<(String, String, u64)>>,
        /// `(head, clusters)`: a writer that dirties the head every time
        /// a copy runs. Without one the ladder converges in a single
        /// round and its bound is never exercised.
        pub(crate) writer: Mutex<Option<(String, u64)>>,
        /// method → the error it answers with. The rebuild's unwind
        /// paths are only testable if a step can be made to fail.
        pub(crate) failures: Mutex<std::collections::HashMap<String, String>>,
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
                raids: Mutex::new(Default::default()),
                quiesced: Mutex::new(Default::default()),
                alloc: Mutex::new(Default::default()),
                parents: Mutex::new(Default::default()),
                copies: Mutex::new(Default::default()),
                writer: Mutex::new(None),
                failures: Mutex::new(Default::default()),
            }
        }

        /// Make one RPC method fail from now on.
        pub(crate) fn fail(&self, method: &str, msg: &str) {
            self.failures.lock().unwrap().insert(method.to_string(), msg.to_string());
        }

        /// A client that writes `clusters` clusters to `head` during
        /// every shallow copy.
        pub(crate) fn set_writer(&self, head: &str, clusters: u64) {
            *self.writer.lock().unwrap() = Some((head.to_string(), clusters));
        }

        /// Simulate writes: the head now owns `clusters` clusters.
        pub(crate) fn set_allocated(&self, alias: &str, clusters: u64) {
            self.alloc.lock().unwrap().insert(alias.to_string(), clusters);
        }

        /// Every shallow copy that ran, in order.
        pub(crate) fn copies(&self) -> Vec<(String, String, u64)> {
            self.copies.lock().unwrap().clone()
        }

        /// The live members of a raid, empty slots dropped.
        pub(crate) fn members_of(&self, raid: &str) -> Vec<String> {
            self.raids
                .lock()
                .unwrap()
                .get(raid)
                .map(|slots| slots.iter().flatten().cloned().collect())
                .unwrap_or_default()
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
            if let Some(msg) = self.failures.lock().unwrap().get(method) {
                return Err(msg.clone().into());
            }
            match method {
                // The reachability probe: cheapest proof the process is
                // answering, and it asserts nothing about its state.
                "spdk_get_version" => Ok(json!({ "result": { "version": "SPDK v26.05" } })),
                // A peer's leg, attached as a local nvme bdev.
                "bdev_nvme_attach_controller" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    let ns = format!("{name}n1");
                    self.bdevs.lock().unwrap().insert(ns.clone(), ns.clone());
                    Ok(json!({ "result": [ns] }))
                }
                "bdev_raid_create" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    if self.raids.lock().unwrap().contains_key(&name) {
                        return Err("File exists".into());
                    }
                    // Real SPDK refuses an empty base name, which is
                    // precisely why the frame needs stand-ins.
                    let slots: Vec<Option<String>> = p["base_bdevs"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|b| b.as_str().unwrap_or("").to_string())
                        .map(|b| if b.is_empty() { None } else { Some(b) })
                        .collect();
                    if slots.iter().any(|s| s.is_none()) {
                        return Err("The base bdev name cannot be empty".into());
                    }
                    self.raids.lock().unwrap().insert(name.clone(), slots);
                    // A raid IS a bdev — that is the whole reason the
                    // namespace can be re-pointed at it.
                    self.bdevs.lock().unwrap().insert(name.clone(), name.clone());
                    Ok(json!({ "result": true }))
                }
                // Degrading: the base leaves the raid, which is what
                // lets the queued I/O drain. The SLOT survives — that
                // is what the leg rejoins through.
                "bdev_raid_remove_base_bdev" => {
                    let base = p["name"].as_str().unwrap_or("").to_string();
                    let mut raids = self.raids.lock().unwrap();
                    let mut hit = false;
                    for slots in raids.values_mut() {
                        for slot in slots.iter_mut() {
                            if slot.as_deref() == Some(base.as_str()) {
                                *slot = None;
                                hit = true;
                            }
                        }
                    }
                    if hit { Ok(json!({ "result": true })) } else { Err("no such base".into()) }
                }
                // The carried patch. Its contract is enforced HERE
                // because that is where the real target enforces it: a
                // skip_rebuild add without a held quiesce lease is
                // -EPERM, since the cut that produced the base and the
                // add that admits it must sit in one window.
                "bdev_raid_add_base_bdev" => {
                    let raid = p["raid_bdev"].as_str().unwrap_or("").to_string();
                    let base = p["base_bdev"].as_str().unwrap_or("").to_string();
                    let skip = p["skip_rebuild"].as_bool().unwrap_or(false);
                    if skip && !self.quiesced.lock().unwrap().contains(&raid) {
                        return Err(format!(
                            "skip_rebuild add requires a held bdev_raid_quiesce lease on {raid}"
                        )
                        .into());
                    }
                    let mut raids = self.raids.lock().unwrap();
                    let slots = raids.get_mut(&raid).ok_or("no such raid")?;
                    match slots.iter_mut().find(|s| s.is_none()) {
                        Some(slot) => {
                            *slot = Some(base);
                            Ok(json!({ "result": true }))
                        }
                        None => Err(format!(
                            "no empty slot found in raid bdev '{raid}' for new base bdev '{base}'"
                        )
                        .into()),
                    }
                }
                "bdev_raid_quiesce" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    if !self.raids.lock().unwrap().contains_key(&name) {
                        return Err("raid bdev not found".into());
                    }
                    self.quiesced.lock().unwrap().insert(name);
                    Ok(json!({ "result": true }))
                }
                "bdev_raid_unquiesce" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    if !self.quiesced.lock().unwrap().remove(&name) {
                        return Err("no quiesce lease held".into());
                    }
                    Ok(json!({ "result": true }))
                }
                "bdev_raid_delete" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    self.raids.lock().unwrap().remove(&name);
                    self.bdevs.lock().unwrap().remove(&name);
                    Ok(json!({ "result": true }))
                }
                "bdev_raid_get_bdevs" => {
                    let raids: Vec<Value> = self
                        .raids
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(name, slots)| {
                            let bases: Vec<Value> = slots
                                .iter()
                                .map(|b| json!({ "name": b, "is_configured": b.is_some() }))
                                .collect();
                            json!({
                                "name": name,
                                "state": "online",
                                "num_base_bdevs": slots.len(),
                                "base_bdevs_list": bases,
                            })
                        })
                        .collect();
                    Ok(json!({ "result": raids }))
                }
                // A stand-in for an absent leg's slot, and nothing else:
                // created, framed over, removed, deleted.
                "bdev_null_create" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    if self.bdevs.lock().unwrap().contains_key(&name) {
                        return Err("File exists".into());
                    }
                    let bytes = p["num_blocks"].as_u64().unwrap_or(0)
                        * p["block_size"].as_u64().unwrap_or(FAKE_BLOCK_SIZE);
                    self.bdevs.lock().unwrap().insert(name.clone(), name.clone());
                    self.bdev_bytes.lock().unwrap().insert(name, bytes);
                    Ok(json!({ "result": true }))
                }
                "bdev_null_delete" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    self.bdevs.lock().unwrap().remove(&name);
                    self.bdev_bytes.lock().unwrap().remove(&name);
                    Ok(json!({ "result": true }))
                }
                "bdev_nvme_detach_controller" => {
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    self.bdevs.lock().unwrap().remove(&format!("{name}n1"));
                    Ok(json!({ "result": true }))
                }
                // THE CUT. The clusters the head owns become the
                // snapshot's; the head starts owning nothing again and
                // accumulates only what is written from here. That is
                // the copy-on-write bookkeeping the ladder reads as its
                // dirty set, so the fake has to model it or the delta
                // rounds prove nothing.
                "bdev_lvol_snapshot" => {
                    let head = p["lvol_name"].as_str().unwrap_or("").to_string();
                    let snap = format!(
                        "{}/{}",
                        head.split('/').next().unwrap_or(""),
                        p["snapshot_name"].as_str().unwrap_or("")
                    );
                    if self.bdevs.lock().unwrap().contains_key(&snap) {
                        return Err("File exists".into());
                    }
                    let owned = self.alloc.lock().unwrap().insert(head.clone(), 0).unwrap_or(0);
                    self.alloc.lock().unwrap().insert(snap.clone(), owned);
                    let bytes =
                        self.bdev_bytes.lock().unwrap().get(&head).copied().unwrap_or(0);
                    self.bdev_bytes.lock().unwrap().insert(snap.clone(), bytes);
                    self.bdevs.lock().unwrap().insert(snap.clone(), snap.clone());
                    let mut parents = self.parents.lock().unwrap();
                    if let Some(grandparent) = parents.get(&head).cloned() {
                        parents.insert(snap.clone(), grandparent);
                    }
                    parents.insert(head, snap);
                    Ok(json!({ "result": "uuid-of-snapshot" }))
                }
                "bdev_lvol_start_shallow_copy" => {
                    let src = p["src_lvol_name"].as_str().unwrap_or("").to_string();
                    let dst = p["dst_bdev_name"].as_str().unwrap_or("").to_string();
                    if !self.bdevs.lock().unwrap().contains_key(&dst) {
                        return Err("destination does not exist".into());
                    }
                    // ONLY the clusters this blob owns — an ancestor's
                    // are skipped, which is exactly the hazard the
                    // parent guard refuses to walk into.
                    let clusters =
                        self.alloc.lock().unwrap().get(&src).copied().unwrap_or(0);
                    // A client that kept writing while the copy ran —
                    // which is the whole reason the ladder has more than
                    // one rung.
                    if let Some((head, per_copy)) = self.writer.lock().unwrap().clone() {
                        *self.alloc.lock().unwrap().entry(head).or_insert(0) += per_copy;
                    }
                    let mut copies = self.copies.lock().unwrap();
                    copies.push((src, dst, clusters));
                    Ok(json!({ "result": { "operation_id": copies.len() } }))
                }
                "bdev_lvol_check_shallow_copy" => {
                    let op = p["operation_id"].as_u64().unwrap_or(0) as usize;
                    let copies = self.copies.lock().unwrap();
                    let n = copies.get(op.saturating_sub(1)).map(|c| c.2).unwrap_or(0);
                    Ok(json!({
                        "result": { "state": "complete", "copied_clusters": n, "total_clusters": n }
                    }))
                }
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
                    let parent = self.parents.lock().unwrap().get(name).cloned();
                    let owned = self.alloc.lock().unwrap().get(name).copied().unwrap_or(0);
                    match bdevs.get(name) {
                        Some(canonical) => Ok(json!({ "result": [{
                            "name": canonical,
                            "uuid": canonical,
                            "aliases": [name],
                            "block_size": FAKE_BLOCK_SIZE,
                            "num_blocks": bytes / FAKE_BLOCK_SIZE,
                            "driver_specific": { "lvol": {
                                "num_allocated_clusters": owned,
                                "clone": parent.is_some(),
                                "base_snapshot": parent,
                            }},
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
                    // Deleting a snapshot with one clone MERGES: the
                    // clone inherits its clusters and its parent. That
                    // is what makes the cut sweep cheap, and what
                    // leaves the head parent-free afterwards.
                    let owned = self.alloc.lock().unwrap().remove(&name).unwrap_or(0);
                    let mut parents = self.parents.lock().unwrap();
                    let grandparent = parents.remove(&name);
                    let clones: Vec<String> = parents
                        .iter()
                        .filter(|(_, v)| **v == name)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for clone in clones {
                        *self.alloc.lock().unwrap().entry(clone.clone()).or_insert(0) += owned;
                        match &grandparent {
                            Some(g) => parents.insert(clone, g.clone()),
                            None => parents.remove(&clone),
                        };
                    }
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
                // Real SPDK drops the namespace; without this the fake
                // kept a stale one alongside its replacement and the
                // composition transition looked half-applied.
                "nvmf_subsystem_remove_ns" => {
                    let nqn = p["nqn"].as_str().unwrap();
                    let nsid = p["nsid"].as_u64().unwrap_or(0);
                    let mut subs = self.subsystems.lock().unwrap();
                    let s = subs.get_mut(nqn).ok_or("no subsystem")?;
                    s["namespaces"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|ns| ns["nsid"].as_u64() != Some(nsid));
                    Ok(json!({ "result": true }))
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
            .filter(|m| {
                // Reads, not mutations. `bdev_raid_get_bdevs` is the
                // composition probe every converge makes to decide
                // whether a raid should exist.
                !m.starts_with("bdev_get_")
                    && !m.starts_with("nvmf_get_")
                    && m != "bdev_raid_get_bdevs"
            })
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
            .block_seat_volume("pvc-elsewhere", "node-b", 100, 220)
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
            PromotionOutcome::Promoted { from, to, epoch, .. } => {
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

    /// THE FRAME IS DERIVED FROM THE RECORD'S LEG COUNT, not from how
    /// many legs are healthy: a volume the record gives two legs is
    /// served through a two-slot composition even while the second leg
    /// is absent, because a raid's slots are fixed when it is created
    /// and a leg can only rejoin one that some removal emptied.
    ///
    /// That last part is what `superblock: false` buys and it is the
    /// reason this tier can compose an EXISTING volume at all. SPDK's
    /// raid superblock costs ≥1 MiB of data offset, which would shift
    /// every byte under the volume's pinned NGUID and make composition a
    /// data migration. Without it, each base carries the volume's bytes
    /// at LBA 0 — identical to the bare lvol — so the namespace can be
    /// re-pointed either way.
    #[tokio::test]
    async fn a_recorded_leg_frames_the_composition_and_the_frame_outlives_it() {
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
        let now = now_unix();
        r.ensure("pvc-mir", Some(1 << 20)).await.expect("provision");
        let nqn = crate::identity::block_volume_export_nqn("pvc-mir");

        // Solo: the namespace serves the LVOL, and no raid exists.
        let ns_bdev = |t: &FakeTgt| -> String {
            t.subsystems.lock().unwrap()[&nqn]["namespaces"][0]["bdev_name"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        assert!(ns_bdev(&tgt).contains("pvc-mir"), "one leg serves the lvol");
        assert!(!tgt.methods().iter().any(|m| m == "bdev_raid_create"), "nothing to compose");

        // A peer leg appears in the record — STALE, because it has no
        // copy yet. The frame is built for it anyway.
        backend.block_target_register("node-peer", "10.0.0.2", 4420, now).await.unwrap().unwrap();
        backend
            .block_leg_mark(
                "pvc-mir",
                "node-peer",
                crate::state_backend::extent_alloc::LEG_STALE,
                now,
            )
            .await
            .unwrap()
            .unwrap();
        tgt.calls.lock().unwrap().clear();
        r.reconcile_hosts("pvc-mir").await.expect("frame");

        let calls = tgt.calls.lock().unwrap().clone();
        let create = tgt.call_with_method(&calls, "bdev_raid_create").expect("frame built");
        assert_eq!(create["params"]["raid_level"], "1");
        assert_eq!(
            create["params"]["superblock"], false,
            "a superblock would shift every byte under the pinned NGUID"
        );
        assert_eq!(
            create["params"]["base_bdevs"].as_array().unwrap().len(),
            2,
            "two slots — one per RECORDED leg, whatever their state"
        );
        // The stand-in exists only to make the slot, and it is gone
        // before anything can read zeros out of it.
        let idx = |m: &str| tgt.methods().iter().position(|x| x == m).unwrap();
        assert!(idx("bdev_null_create") < idx("bdev_raid_create"), "stand-in before the frame");
        assert!(
            idx("bdev_raid_create") < idx("bdev_raid_remove_base_bdev"),
            "and emptied immediately after"
        );
        assert!(tgt.methods().iter().any(|m| m == "bdev_null_delete"), "then deleted");
        assert_eq!(
            tgt.members_of("flintraid-pvc-mir"),
            vec![r.bdev_name("pvc-mir")],
            "the composition serves ONE leg — the local one — with a slot standing empty"
        );
        assert!(ns_bdev(&tgt).starts_with("flintraid-"), "the namespace serves the RAID now");
        assert_eq!(
            tgt.subsystems.lock().unwrap()[&nqn]["namespaces"].as_array().unwrap().len(),
            1,
            "exactly one namespace — the lvol-backed one is REPLACED, not accompanied"
        );

        // THE FRAME OUTLIVES THE LEG. A further converge with the peer
        // still absent must not tear the composition down and re-point
        // the namespace at the lvol: that slot is what the leg rejoins
        // through, and re-framing is what a live client would feel.
        tgt.calls.lock().unwrap().clear();
        r.reconcile_hosts("pvc-mir").await.expect("re-converge");
        assert!(
            !tgt.methods().iter().any(|m| m == "bdev_raid_delete"),
            "the frame stands while the leg is away: {:?}",
            tgt.methods()
        );
        assert!(ns_bdev(&tgt).starts_with("flintraid-"), "still the raid");
    }

    /// `UncleanResync`, the write-hole belt: a composition built from
    /// nothing DEMOTES every peer it cannot prove.
    ///
    /// raid1 acks on any-one-leg success and writes the leg-failed
    /// record asynchronously afterwards, so a composer that died
    /// mid-write left two legs holding different bytes with nothing
    /// durable saying which. There is no scrub and no resync — REBUILD
    /// is raid1's only process — so "the record said in sync" is not
    /// evidence, and re-composing over both would let reads flap
    /// between divergent legs on LAYOUTCOMMIT-confirmed data.
    ///
    /// A/B: keep the in-sync mark here and the leg re-enters the
    /// composition on nothing but its own last word.
    #[tokio::test]
    async fn a_composition_built_from_nothing_demotes_every_peer_it_cannot_prove() {
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
        let now = now_unix();
        r.ensure("pvc-unclean", Some(1 << 20)).await.expect("provision");
        backend.block_target_register("node-peer", "10.0.0.2", 4420, now).await.unwrap().unwrap();
        // The record's last word before the target died: both in sync.
        backend
            .block_leg_mark(
                "pvc-unclean",
                "node-peer",
                crate::state_backend::extent_alloc::LEG_INSYNC,
                now,
            )
            .await
            .unwrap()
            .unwrap();

        // The target restarts: no raid exists, so the composition is
        // built from nothing.
        r.reconcile_hosts("pvc-unclean").await.expect("frame");

        let legs = backend.block_legs("pvc-unclean").await.unwrap().unwrap();
        assert_eq!(
            legs.iter().find(|l| l.target_id == "node-peer").unwrap().sync_state,
            crate::state_backend::extent_alloc::LEG_STALE,
            "the peer is demoted: nothing here can prove its bytes equal ours"
        );
        assert_eq!(
            tgt.members_of("flintraid-pvc-unclean"),
            vec![r.bdev_name("pvc-unclean")],
            "and it is NOT a member — it owes a rebuild"
        );
        assert!(
            !tgt.methods().iter().any(|m| m == "bdev_nvme_attach_controller"),
            "an unproven peer is not even dialled: {:?}",
            tgt.methods()
        );
    }

    /// THE DEGRADE BARRIER: the stale mark lands BEFORE the leg leaves
    /// the raid, and the order is the whole property.
    ///
    /// Stock raid1 acks a solo-landing write and records the leg failure
    /// afterwards; in that gap the record still calls the missing leg
    /// in sync, so an election hands the volume to a leg already short
    /// of acked bytes. Since flint is not on the data path it cannot
    /// gate an ack — it gates the ABILITY to ack, by leaving the leg in
    /// the raid (where its transport queues rather than fails) until the
    /// mark is durable. Removing the leg first would be stock raid1's
    /// order and the counterexample's shape.
    #[tokio::test]
    async fn the_barrier_marks_the_leg_stale_before_the_raid_degrades() {
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
        let now = now_unix();
        r.ensure("pvc-deg", Some(1 << 20)).await.expect("provision");
        backend.block_target_register("node-peer", "10.0.0.2", 4420, now).await.unwrap().unwrap();
        backend
            .block_leg_mark(
                "pvc-deg",
                "node-peer",
                crate::state_backend::extent_alloc::LEG_STALE,
                now,
            )
            .await
            .unwrap()
            .unwrap();
        r.reconcile_hosts("pvc-deg").await.expect("frame");
        // The only way into a live composition: a rebuild. Setting the
        // record to in-sync by hand would be testing a state the code
        // can no longer reach.
        r.observe("node-peer", true, now);
        assert!(matches!(
            r.rebuild_leg("pvc-deg", "node-peer").await,
            RebuildOutcome::Rebuilt { .. }
        ));

        // The leg's transport QUEUES rather than fails — that is what
        // makes a solo ack impossible while the peer is missing.
        let calls = tgt.calls.lock().unwrap().clone();
        let attach = tgt.call_with_method(&calls, "bdev_nvme_attach_controller").unwrap();
        assert!(
            attach["params"].get("fast_io_fail_timeout_sec").is_none(),
            "a composed leg must not fail queued I/O — that is the ack window: {:?}",
            attach["params"]
        );
        assert_eq!(attach["params"]["ctrlr_loss_timeout_sec"], -1);

        // A peer that is merely SUSPECT keeps its place: the transport
        // is queueing, so nothing can be acked behind its back, and
        // degrading on a blip spends a rebuild for nothing.
        r.observe("node-peer", false, now);
        assert!(r.degrade_pass("pvc-deg").await.is_empty(), "a blip must not degrade");
        let legs = backend.block_legs("pvc-deg").await.unwrap().unwrap();
        assert_eq!(
            legs.iter().find(|l| l.target_id == "node-peer").unwrap().sync_state,
            crate::state_backend::extent_alloc::LEG_INSYNC
        );

        // The blip clears — which resets the failure clock, so the real
        // outage below starts its own window rather than inheriting the
        // blip's. (Getting this wrong in the test made the verdict read
        // SUSPECT and the barrier look inert.)
        r.observe("node-peer", true, now);

        // Now the verdict lands.
        let long_ago = now - 10 * verdict_min_secs();
        for _ in 0..verdict_strikes() + 1 {
            r.observe("node-peer", false, long_ago);
        }
        tgt.calls.lock().unwrap().clear();
        assert_eq!(r.degrade_pass("pvc-deg").await, vec!["node-peer".to_string()]);

        // THE ORDER. The mark is durable and the leg is gone from the
        // raid — and the raid removal is the LAST thing that happened,
        // because everything before it could still have acked.
        let legs = backend.block_legs("pvc-deg").await.unwrap().unwrap();
        assert_eq!(
            legs.iter().find(|l| l.target_id == "node-peer").unwrap().sync_state,
            crate::state_backend::extent_alloc::LEG_STALE,
            "the record knows the leg is behind"
        );
        assert!(
            tgt.methods().iter().any(|m| m == "bdev_raid_remove_base_bdev"),
            "the leg left the composition: {:?}",
            tgt.methods()
        );

        // And the leg is no longer electable — which is the point of
        // marking it at all.
        let seat = backend.block_volume_seat("pvc-deg").await.unwrap().unwrap().unwrap();
        assert!(matches!(
            backend
                .block_promote("pvc-deg", seat.epoch, &seat.composer, "node-peer", now)
                .await
                .unwrap(),
            Err(crate::state_backend::extent_alloc::ExtentAllocError::NotInSync { .. })
        ));
    }

    /// A framed volume with a stale peer, ready to be rebuilt: the
    /// record's two legs, the frame with its empty slot, the peer heard
    /// from, and `head` holding `clusters` clusters of data.
    async fn framed_with_a_stale_peer(
        volume: &str,
        clusters: u64,
    ) -> (Arc<FakeTgt>, Arc<dyn crate::state_backend::StateBackend>, BlockExportReconciler) {
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
        let now = now_unix();
        r.ensure(volume, Some(1 << 20)).await.expect("provision");
        backend.block_target_register("node-peer", "10.0.0.2", 4420, now).await.unwrap().unwrap();
        backend
            .block_leg_mark(
                volume,
                "node-peer",
                crate::state_backend::extent_alloc::LEG_STALE,
                now,
            )
            .await
            .unwrap()
            .unwrap();
        r.reconcile_hosts(volume).await.expect("frame");
        r.observe("node-peer", true, now);
        tgt.set_allocated(&r.bdev_name(volume), clusters);
        tgt.calls.lock().unwrap().clear();
        (tgt, backend, r)
    }

    /// THE REBUILD COPIES ONLY WHAT THE CUT OWNS — that is the whole
    /// sparseness claim, and it is SPDK's, not ours: a shallow copy
    /// walks the source blob's cluster map and skips every cluster the
    /// blob does not own. raid1's own rebuild would walk the arena from
    /// zero with no zero-detection and densify the thin target leg.
    ///
    /// And the ladder: round 1 carries the volume, every later round
    /// carries only what the writer dirtied while the previous round
    /// ran, because a cut takes the head's clusters with it.
    #[tokio::test]
    async fn the_rebuild_copies_only_the_clusters_each_cut_owns() {
        // 100 clusters = 400 MiB of data, well past the 64 MiB window
        // budget, so the ladder runs before the window does.
        let (tgt, backend, r) = framed_with_a_stale_peer("pvc-rb", 100).await;
        // A writer dirtying 4 clusters (16 MiB) per copy: under the
        // budget, so the ladder converges after one round.
        tgt.set_writer(&r.bdev_name("pvc-rb"), 4);

        let out = r.rebuild_leg("pvc-rb", "node-peer").await;
        assert!(matches!(out, RebuildOutcome::Rebuilt { .. }), "{out:?}");

        let copies = tgt.copies();
        assert_eq!(copies.len(), 2, "one ladder round and the window: {copies:?}");
        assert_eq!(
            (copies[0].0.as_str(), copies[0].2),
            ("lvs_test/flintcut-pvc-rb-1", 100),
            "round 1 carries the volume's allocated clusters — and nothing else"
        );
        assert_eq!(
            (copies[1].0.as_str(), copies[1].2),
            ("lvs_test/flintcut-pvc-rb-2", 4),
            "the window carries ONLY what was written while round 1 ran"
        );
        assert!(
            copies.iter().all(|c| c.1 == r.leg_base_name("pvc-rb", "node-peer")),
            "every copy lands on the peer's leg: {copies:?}"
        );

        // The leg is a member again, the record says so, and our cuts
        // are gone — a chain left behind would make the next rebuild
        // refuse, because a head with a parent cannot be copied sparsely.
        assert_eq!(
            tgt.members_of("flintraid-pvc-rb").len(),
            2,
            "the leg rejoined the slot its absence left"
        );
        let legs = backend.block_legs("pvc-rb").await.unwrap().unwrap();
        assert_eq!(
            legs.iter().find(|l| l.target_id == "node-peer").unwrap().sync_state,
            crate::state_backend::extent_alloc::LEG_INSYNC
        );
        let head = r.lvol_info(&r.bdev_name("pvc-rb")).await.unwrap();
        assert_eq!(
            head["driver_specific"]["lvol"]["clone"], false,
            "the cuts were swept back into the head"
        );
    }

    /// THE WINDOW'S CONTRACT, and the ORDER inside it.
    ///
    /// The cut that produced the leg's bytes and the add that admits it
    /// must sit inside ONE held quiesce, or the writes between them
    /// exist nowhere on the new leg — flint's carried SPDK patch refuses
    /// a `skip_rebuild` add without a held lease for exactly that
    /// reason, and the fake enforces it the same way.
    ///
    /// Then the mark, and only then: the leg becomes a member BEFORE the
    /// record calls it in sync, mirroring the degrade barrier. A mark
    /// that landed first and then crashed would leave an electable leg
    /// missing the final delta.
    ///
    /// A/B: mark before the add and the "record trails reality" order
    /// fails; drop either quiesce and the admission is refused outright.
    #[tokio::test]
    async fn the_leg_is_admitted_inside_the_quiesce_and_only_then_called_in_sync() {
        let (tgt, _backend, r) = framed_with_a_stale_peer("pvc-win", 4).await;
        assert!(matches!(
            r.rebuild_leg("pvc-win", "node-peer").await,
            RebuildOutcome::Rebuilt { .. }
        ));

        let methods = tgt.methods();
        let idx = |m: &str| methods.iter().position(|x| x == m).expect(m);
        let last = |m: &str| methods.iter().rposition(|x| x == m).expect(m);
        assert!(
            idx("bdev_raid_quiesce") < idx("bdev_lvol_snapshot"),
            "the cut is taken INSIDE the window, not before it: {methods:?}"
        );
        assert!(
            idx("bdev_lvol_snapshot") < idx("bdev_lvol_start_shallow_copy"),
            "cut, then copy"
        );
        assert!(
            last("bdev_raid_quiesce") < idx("bdev_raid_add_base_bdev"),
            "the lease is renewed immediately before the add, so the add runs on a full lease"
        );
        assert!(
            idx("bdev_raid_add_base_bdev") < idx("bdev_raid_unquiesce"),
            "and the add completes before client I/O resumes"
        );
        let add = tgt
            .call_with_method(&tgt.calls.lock().unwrap().clone(), "bdev_raid_add_base_bdev")
            .expect("admitted")
            .clone();
        assert_eq!(
            add["params"]["skip_rebuild"], true,
            "no rebuild process: the leg already holds the bytes"
        );
        assert!(
            tgt.methods().iter().any(|m| m == "bdev_raid_unquiesce"),
            "the quiesce is always released — the lease would expire anyway, but a stall we \
             are finished with is one we chose not to spend"
        );
    }

    /// THE RECORD'S OPTIMISM TRAILS REALITY — proven by the failure.
    ///
    /// If the admission itself fails, the leg must still be STALE: a
    /// mark placed in anticipation would leave an ELECTABLE leg that is
    /// missing the final delta, and `ElectInSync` would hand it the
    /// composition in good faith. That is the degrade barrier's rule
    /// pointing the other way — there the record went stale before the
    /// composition degraded, here the member comes before the mark.
    ///
    /// A/B: mark before the add and this leg comes back elected on a
    /// window that never closed.
    #[tokio::test]
    async fn an_admission_that_fails_leaves_the_leg_stale_and_the_quiesce_released() {
        let (tgt, backend, r) = framed_with_a_stale_peer("pvc-undo", 8).await;
        tgt.fail("bdev_raid_add_base_bdev", "slot vanished");

        match r.rebuild_leg("pvc-undo", "node-peer").await {
            RebuildOutcome::Deferred(why) => assert!(why.contains("admitting"), "{why}"),
            other => panic!("expected a deferral, got {other:?}"),
        }
        let legs = backend.block_legs("pvc-undo").await.unwrap().unwrap();
        assert_eq!(
            legs.iter().find(|l| l.target_id == "node-peer").unwrap().sync_state,
            crate::state_backend::extent_alloc::LEG_STALE,
            "the leg was never admitted, so nothing may say it is in sync"
        );
        assert!(
            tgt.quiesced.lock().unwrap().is_empty(),
            "and client I/O is not left gated behind a window that failed"
        );
        // The abandoned destination is dropped: a composed leg's
        // transport QUEUES rather than fails, so one left attached to a
        // peer that never returns holds queued writes forever.
        assert!(
            tgt.methods().iter().any(|m| m == "bdev_nvme_detach_controller"),
            "{:?}",
            tgt.methods()
        );
    }

    /// A HEAD WITH A PARENT IS REFUSED. A shallow copy carries only the
    /// blob's own clusters, so a head that still has an ancestor would
    /// produce a leg that reads zeros wherever the ancestor holds the
    /// data — silent, and only discovered at a failover. Walking the
    /// chain oldest-first is the extension; refusing is the honest
    /// answer until it exists.
    #[tokio::test]
    async fn a_rebuild_refuses_a_head_with_an_ancestor_rather_than_building_holes() {
        let (tgt, _backend, r) = framed_with_a_stale_peer("pvc-anc", 8).await;
        // A parent the sweep cannot merge away: not one of our cuts.
        tgt.parents
            .lock()
            .unwrap()
            .insert(r.bdev_name("pvc-anc"), "lvs_test/some-restore-source".into());

        match r.rebuild_leg("pvc-anc", "node-peer").await {
            RebuildOutcome::Refused(why) => {
                assert!(why.contains("parent snapshot"), "{why}");
                assert!(why.contains("read zeros"), "{why}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(tgt.copies().is_empty(), "and not a byte was copied");
    }

    /// A rebuild pours the whole volume at a peer and ends by holding
    /// every client's I/O while it finishes. So the peer must be
    /// AFFIRMATIVELY reachable — never-observed is not "probably fine",
    /// it is a target nothing has heard from.
    ///
    /// A/B: accept anything that is merely not-condemned and the rebuild
    /// spends the whole copy to fail at the window.
    #[tokio::test]
    async fn a_rebuild_waits_for_a_peer_it_has_actually_heard_from() {
        let (tgt, _backend, r) = framed_with_a_stale_peer("pvc-unheard", 8).await;
        r.probes.clear(); // never observed
        match r.rebuild_leg("pvc-unheard", "node-peer").await {
            RebuildOutcome::Deferred(why) => assert!(why.contains("not yet observed"), "{why}"),
            other => panic!("expected a deferral, got {other:?}"),
        }
        assert!(tgt.copies().is_empty());

        // Suspect is not enough either: the verdict has not landed, and
        // a rebuild started at a target that is on its way out is a
        // whole copy spent for nothing.
        r.observe("node-peer", false, now_unix());
        match r.rebuild_leg("pvc-unheard", "node-peer").await {
            RebuildOutcome::Deferred(why) => assert!(why.contains("Suspect"), "{why}"),
            other => panic!("expected a deferral, got {other:?}"),
        }
        assert!(tgt.copies().is_empty());
    }

    /// Only the composer may rebuild a leg, because only its bytes are
    /// the volume's. The same predicate is re-checked inside the window
    /// under the volume lock, where it catches the seat moving during a
    /// copy that took hours.
    #[tokio::test]
    async fn only_the_composer_rebuilds_and_a_leg_the_record_omits_is_not_minted() {
        let (tgt, backend, r) = framed_with_a_stale_peer("pvc-who", 8).await;
        match r.rebuild_leg("pvc-who", "node-ghost").await {
            RebuildOutcome::Refused(why) => assert!(why.contains("no leg row"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }

        // The seat moves away from us: the copy would be of bytes that
        // are no longer the volume's.
        let seat = backend.block_volume_seat("pvc-who").await.unwrap().unwrap().unwrap();
        backend
            .block_leg_mark(
                "pvc-who",
                "node-peer",
                crate::state_backend::extent_alloc::LEG_INSYNC,
                now_unix(),
            )
            .await
            .unwrap()
            .unwrap();
        backend
            .block_promote("pvc-who", seat.epoch, &seat.composer, "node-peer", now_unix())
            .await
            .unwrap()
            .unwrap();
        match r.rebuild_leg("pvc-who", "node-peer").await {
            RebuildOutcome::Refused(why) => assert!(why.contains("not here"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(tgt.copies().is_empty());
    }

    /// PLACEMENT, peer side: hosting a copy mints an EMPTY lvol and
    /// offers it to exactly one composer.
    ///
    /// Empty is the point. The leg holds none of the volume's bytes,
    /// which is why the composer records it STALE and why only a
    /// rebuild may ever say otherwise — a leg that arrived claiming to
    /// be in sync would be electable, and `ElectInSync` would hand the
    /// volume to a copy of nothing.
    #[tokio::test]
    async fn hosting_a_leg_mints_an_empty_lvol_and_offers_it_to_the_composer_alone() {
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
        r.host_leg("pvc-host", 1 << 20, "node-composer").await.expect("host");

        // The seat is this target's own statement that the volume is
        // NOT its to serve — and it is what the allow-list below is
        // derived from.
        let seat = backend.block_volume_seat("pvc-host").await.unwrap().unwrap().unwrap();
        assert_eq!(seat.composer, "node-composer");

        let created = tgt
            .call_with_method(&tgt.calls.lock().unwrap().clone(), "bdev_lvol_create")
            .expect("lvol minted")
            .clone();
        assert_eq!(created["params"]["thin_provision"], true, "an empty leg is all holes");

        let leg_nqn = crate::identity::block_leg_export_nqn("pvc-host");
        assert_eq!(
            tgt.hosts_of(&leg_nqn),
            vec![crate::nvmeof_export::flint_host_nqn("node-composer")],
            "offered to the composer the record names, and to nobody else"
        );
        assert!(
            tgt.subsystems
                .lock()
                .unwrap()
                .get(&crate::identity::block_volume_export_nqn("pvc-host"))
                .is_none(),
            "and NOT to clients — a leg is not an export of the volume"
        );

        // Idempotent: CreateVolume is retried by the provisioner, and
        // placement is deterministic, so this runs again unchanged.
        tgt.calls.lock().unwrap().clear();
        r.host_leg("pvc-host", 1 << 20, "node-composer").await.expect("again");
        assert!(
            !tgt.methods().iter().any(|m| m == "bdev_lvol_create"),
            "second call minted a second lvol: {:?}",
            tgt.methods()
        );

        // A leg of a volume THIS target composes is a contradiction:
        // the raid claims that lvol exclusively, so a second export of
        // it is EPERM.
        let err = r.host_leg("pvc-host", 1 << 20, &target_id()).await.unwrap_err();
        assert!(err.contains("composed HERE"), "{err}");
    }

    /// PLACEMENT, composer side — and the whole pipeline behind it:
    /// recording where the second copy lives is enough. The frame, the
    /// rebuild and the in-sync mark all follow from the record.
    #[tokio::test]
    async fn a_recorded_placement_frames_the_volume_and_the_rebuild_fills_it() {
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
        r.ensure("pvc-placed", Some(1 << 20)).await.expect("provision");
        r.record_leg("pvc-placed", "node-peer", "10.0.0.2", 4420).await.expect("record");

        // STALE, always: the peer's lvol exists and is empty.
        let legs = backend.block_legs("pvc-placed").await.unwrap().unwrap();
        assert_eq!(
            legs.iter().find(|l| l.target_id == "node-peer").unwrap().sync_state,
            crate::state_backend::extent_alloc::LEG_STALE
        );
        // And the peer is dialable — the registry row is what every
        // dial site resolves through.
        let (traddr, port) = {
            let targets = backend.block_target_list().await.unwrap().unwrap();
            let t = targets.iter().find(|t| t.target_id == "node-peer").expect("registered");
            (t.traddr.clone(), t.trsvcid)
        };
        assert_eq!((traddr.as_str(), port), ("10.0.0.2", 4420));

        // One converge, and the volume is composed with a slot waiting.
        r.reconcile_hosts("pvc-placed").await.expect("frame");
        assert_eq!(
            tgt.members_of("flintraid-pvc-placed"),
            vec![r.bdev_name("pvc-placed")],
            "one member, one empty slot"
        );

        // The rebuild fills it, and only then is the copy real.
        r.observe("node-peer", true, now_unix());
        assert!(matches!(
            r.rebuild_leg("pvc-placed", "node-peer").await,
            RebuildOutcome::Rebuilt { .. }
        ));
        assert_eq!(tgt.members_of("flintraid-pvc-placed").len(), 2);
        let legs = backend.block_legs("pvc-placed").await.unwrap().unwrap();
        assert_eq!(
            legs.iter().find(|l| l.target_id == "node-peer").unwrap().sync_state,
            crate::state_backend::extent_alloc::LEG_INSYNC
        );

        // A placement naming this target is a volume as its own second
        // copy.
        let err = r.record_leg("pvc-placed", &target_id(), "10.0.0.9", 4420).await.unwrap_err();
        assert!(err.contains("its own second copy"), "{err}");
    }

    /// A leg dies with its volume — and only a LEG does. The peer's
    /// lvol is named by nothing once the composer's record is swept, so
    /// the drop has to reach it, and it must never be mistaken for the
    /// volume itself.
    #[tokio::test]
    async fn dropping_a_leg_takes_the_export_the_lvol_and_the_record() {
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
        r.host_leg("pvc-gone", 1 << 20, "node-composer").await.expect("host");
        r.drop_leg("pvc-gone").await.expect("drop");

        assert!(
            tgt.subsystems
                .lock()
                .unwrap()
                .get(&crate::identity::block_leg_export_nqn("pvc-gone"))
                .is_none(),
            "the leg export is gone"
        );
        assert!(
            !tgt.bdevs.lock().unwrap().contains_key(&r.bdev_name("pvc-gone")),
            "and so is the copy"
        );
        assert!(
            backend.block_volume_seat("pvc-gone").await.unwrap().unwrap().is_none(),
            "and the record"
        );
        // Idempotent — the controller fans this at whatever the
        // composer's record named, which a retry may already have swept.
        r.drop_leg("pvc-gone").await.expect("again");

        // A volume this target COMPOSES is not a leg of anything.
        r.ensure("pvc-mine", Some(1 << 20)).await.expect("provision");
        let err = r.drop_leg("pvc-mine").await.unwrap_err();
        assert!(err.contains("COMPOSED here"), "{err}");
        assert!(
            tgt.bdevs.lock().unwrap().contains_key(&r.bdev_name("pvc-mine")),
            "and its bytes are untouched"
        );
    }

    /// THE LEG EXPORT is the door `EvictAtLeg` closes, and who may come
    /// through it is derived from the seat: exactly the current
    /// composer.
    ///
    /// So a deposed peer loses its reach because the record stopped
    /// naming it, not because anyone remembered to revoke it — and a
    /// target that becomes the composer withdraws its own leg export,
    /// because the raid module's exclusive claim on that lvol would
    /// otherwise fail with EPERM.
    #[tokio::test]
    async fn a_leg_is_offered_to_the_composer_the_record_names_and_to_nobody_else() {
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
        let now = now_unix();
        r.self_register().await.expect("this target must be registered to be elected");
        // We hold a copy but do NOT compose it: the seat names a peer.
        backend.block_target_register("node-a", "10.0.0.1", 4420, now).await.unwrap().unwrap();
        backend.block_seat_volume("pvc-leg", "node-a", now, now + 120).await.unwrap().unwrap();
        tgt.bdevs.lock().unwrap().insert("lvs_test/pvc-leg".into(), "uuid-leg".into());

        r.ensure_leg_export("pvc-leg").await.expect("offer the leg");
        let leg_nqn = crate::identity::block_leg_export_nqn("pvc-leg");
        let hosts = tgt.hosts_of(&leg_nqn);
        assert_eq!(
            hosts,
            vec![crate::nvmeof_export::flint_host_nqn("node-a")],
            "exactly the composer, and nobody else: {hosts:?}"
        );
        assert!(
            tgt.subsystems.lock().unwrap().contains_key(&leg_nqn),
            "the leg has its OWN subsystem — a client admitted to the volume has no \
             business reaching the leg"
        );

        // The seat moves to another peer. The ordinary converge evicts
        // the old composer and admits the new one — no separate act.
        backend
            .block_target_register("node-b", "10.0.0.2", 4420, now)
            .await
            .unwrap()
            .unwrap();
        backend
            .block_leg_mark("pvc-leg", "node-b", crate::state_backend::extent_alloc::LEG_INSYNC, now)
            .await
            .unwrap()
            .unwrap();
        let seat = backend.block_volume_seat("pvc-leg").await.unwrap().unwrap().unwrap();
        backend
            .block_promote("pvc-leg", seat.epoch, "node-a", "node-b", now)
            .await
            .unwrap()
            .unwrap();
        r.ensure_leg_export("pvc-leg").await.expect("re-offer");
        let hosts = tgt.hosts_of(&leg_nqn);
        assert!(
            hosts.contains(&crate::nvmeof_export::flint_host_nqn("node-b"))
                && !hosts.contains(&crate::nvmeof_export::flint_host_nqn("node-a")),
            "the deposed composer lost its reach with the record, not with a revocation: \
             {hosts:?}"
        );

        // And when WE become the composer, the leg export is withdrawn —
        // the raid's exclusive claim cannot coexist with an export of
        // the same lvol.
        let seat = backend.block_volume_seat("pvc-leg").await.unwrap().unwrap().unwrap();
        backend
            .block_leg_mark(
                "pvc-leg",
                &target_id(),
                crate::state_backend::extent_alloc::LEG_INSYNC,
                now,
            )
            .await
            .unwrap()
            .unwrap();
        backend
            .block_promote("pvc-leg", seat.epoch, "node-b", &target_id(), now)
            .await
            .unwrap()
            .unwrap();
        r.ensure_leg_export("pvc-leg").await.expect("withdraw");
        assert!(
            !tgt.subsystems.lock().unwrap().contains_key(&leg_nqn),
            "a composer exports no leg of its own"
        );
    }

    /// THE WHOLE FAILOVER ORDER, on one volume: CAS → horizon → evict →
    /// assemble → replay. Driven from the survivor's side, which is the
    /// side that has to be right.
    ///
    /// The horizon is the part with teeth. Assembly must REFUSE while
    /// the deposed composer's lease still runs — it may still be acking
    /// its clients' writes, and taking its fan-in away is what strands
    /// them on a doomed leg. Only once that lease lapses may the
    /// survivor become the serving composition.
    #[tokio::test]
    async fn assembly_waits_out_the_horizon_then_takes_the_composition() {
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
        let me = target_id();
        let now = now_unix();

        // The world just before a failover TO us: the volume is seated
        // at a peer, that peer holds a live epoch-1 lease, and our leg
        // is in sync. (The lvol is ours to converge; provisioning it
        // here stands in for the rebuild that would have filled it.)
        backend.block_target_register("node-peer", "10.0.0.2", 4420, now).await.unwrap().unwrap();
        r.self_register().await.unwrap();
        backend
            .block_seat_volume("pvc-fo", "node-peer", now, now + 60)
            .await
            .unwrap()
            .unwrap();
        backend
            .block_leg_mark(
                "pvc-fo",
                &me,
                crate::state_backend::extent_alloc::LEG_INSYNC,
                now,
            )
            .await
            .unwrap()
            .unwrap();
        // A client admitted, and one FENCED — the fence replay's whole
        // reason for existing, since PTPL never travels to a survivor.
        let live = crate::nvmeof_export::flint_host_nqn("node-live");
        let doomed = crate::nvmeof_export::flint_host_nqn("node-doomed");
        backend.block_node_attach("pvc-fo", &live, "node-live", now).await.unwrap().unwrap();
        backend.block_host_admit("pvc-fo", 9, &doomed, now).await.unwrap().unwrap();
        backend.block_fence_record("pvc-fo", 9, now).await.unwrap().unwrap();
        backend.block_host_evict("pvc-fo", 9).await.unwrap().unwrap();

        // THE CAS: the peer is condemned, we are the in-sync survivor.
        let long_ago = now - 10 * verdict_min_secs();
        for _ in 0..verdict_strikes() + 1 {
            r.observe("node-peer", false, long_ago);
        }
        r.observe(&me, true, now);
        match r.attempt_promotion("pvc-fo").await {
            PromotionOutcome::Promoted { to, epoch, evict_after_unix, .. } => {
                assert_eq!((to.as_str(), epoch), (me.as_str(), 2));
                assert_eq!(evict_after_unix, now + 60, "the horizon is the deposed lease");
            }
            other => panic!("expected a promotion, got {other:?}"),
        }

        // The lvol is already here — the rebuild's output, standing in.
        // It goes in BEFORE the first attempt on purpose: with it
        // present, the ONLY thing between this call and a completed
        // assembly is the horizon, so the refusal below is attributable
        // to the horizon and to nothing else.
        tgt.bdevs.lock().unwrap().insert("lvs_test/pvc-fo".into(), "uuid-fo".into());

        // THE HORIZON: assembly refuses while that lease still runs.
        match r.assemble("pvc-fo").await {
            AssemblyOutcome::AwaitingHorizon { deposed, until_unix } => {
                assert_eq!((deposed.as_str(), until_unix), ("node-peer", now + 60))
            }
            other => panic!("assembly must wait out the deposed lease, got {other:?}"),
        }
        let nqn = crate::identity::block_volume_export_nqn("pvc-fo");
        assert!(
            tgt.subsystems.lock().unwrap().get(&nqn).is_none(),
            "nothing may be built for a composition that has not been assembled"
        );

        // The lease lapses (expressed by re-granting the DEPOSED one in
        // the past — what a lapse looks like from the record's side).
        backend
            .block_lease_grant("pvc-fo", 1, "node-peer", now - 1)
            .await
            .unwrap()
            .unwrap();
        match r.assemble("pvc-fo").await {
            AssemblyOutcome::Assembled { epoch, deposed } => {
                assert_eq!(epoch, 2);
                assert_eq!(deposed.as_deref(), Some("node-peer"));
            }
            other => panic!("expected assembly, got {other:?}"),
        }

        // ASSEMBLY IS THE LEASE GRANT.
        let l = backend.block_lease("pvc-fo").await.unwrap().unwrap().unwrap();
        assert_eq!((l.epoch, l.holder.as_str()), (2, me.as_str()));

        // The deposed leg is STALE, so the election gate cannot hand the
        // composition straight back to it.
        let legs = backend.block_legs("pvc-fo").await.unwrap().unwrap();
        let peer = legs.iter().find(|l| l.target_id == "node-peer").expect("peer leg");
        assert_eq!(peer.sync_state, crate::state_backend::extent_alloc::LEG_STALE);

        // THE FENCE REPLAY: the export is up, the live client is
        // admitted, and the fenced one is NOT — nothing about the fence
        // travelled here except this MDS-side computation.
        let hosts = tgt.hosts_of(&nqn);
        assert!(hosts.contains(&live), "live client admitted: {hosts:?}");
        assert!(!hosts.contains(&doomed), "fenced client must not return: {hosts:?}");
        assert!(hosts.contains(&crate::identity::block_mds_host_nqn()), "fence lane present");
        // And the door STAYS shut, which is the part the allow-list
        // check alone would not prove: the fenced client's node tries to
        // re-attach at the survivor, where no PTPL and no reservation
        // ever arrived, and is refused against the durable record.
        match backend.block_node_attach("pvc-fo", &doomed, "node-doomed", now).await {
            Ok(Err(crate::state_backend::extent_alloc::ExtentAllocError::FencedClient)) => {}
            other => panic!("a fenced node must not re-attach at the survivor: {other:?}"),
        }

        // Idempotent: a second pass is a no-op, not a second assembly.
        assert_eq!(
            r.assemble("pvc-fo").await,
            AssemblyOutcome::AlreadyAssembled { epoch: 2 }
        );
        assert!(r.assembly_pass(&["pvc-fo".to_string()]).await.is_empty());
    }

    /// THE DEAD-MAN, end to end: a target that has lost the record's
    /// vouching keeps serving until its lease actually runs out, and
    /// then tears every client's controller down itself.
    ///
    /// Both conditions matter and are shown separately. Suspending on a
    /// refused renewal ALONE would sever a still-entitled composition
    /// mid-horizon, which is what strands acked writes on a doomed leg;
    /// suspending on expiry alone would take down a healthy volume the
    /// moment a loop ran late, since a renewal that SUCCEEDS is what
    /// repairs the expiry in the first place.
    #[tokio::test]
    async fn the_deadman_suspends_only_after_the_horizon_it_was_granted() {
        let tgt = Arc::new(FakeTgt::new());
        // sqlite, not the memory backend: this test needs a real
        // admission (`block_node_attach`), which the memory backend
        // refuses because a block-class volume cannot live there.
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
        r.ensure("pvc-dm", Some(1 << 20)).await.expect("provision");
        let me = target_id();
        let nqn = crate::identity::block_volume_export_nqn("pvc-dm");
        let client = crate::nvmeof_export::flint_host_nqn("node-w1");
        backend
            .block_node_attach("pvc-dm", &client, "node-w1", now_unix())
            .await
            .unwrap()
            .unwrap();
        r.reconcile_hosts("pvc-dm").await.expect("converge");
        assert!(tgt.hosts_of(&nqn).contains(&client), "client admitted");

        // Nothing wrong: the record vouches, the renewal succeeds, and
        // the dead-man does nothing at all.
        assert!(r.deadman_pass().await.is_empty());
        assert!(tgt.hosts_of(&nqn).contains(&client), "a healthy volume is untouched");

        // Depose this target. The lease it holds is still LIVE, so the
        // composition keeps serving — deliberately.
        backend
            .block_target_register("node-b", "10.0.0.2", 4420, now_unix())
            .await
            .unwrap()
            .unwrap();
        backend
            .block_leg_mark(
                "pvc-dm",
                "node-b",
                crate::state_backend::extent_alloc::LEG_INSYNC,
                now_unix(),
            )
            .await
            .unwrap()
            .unwrap();
        let seat = backend.block_volume_seat("pvc-dm").await.unwrap().unwrap().unwrap();
        backend
            .block_promote("pvc-dm", seat.epoch, &me, "node-b", now_unix())
            .await
            .unwrap()
            .unwrap();

        assert!(
            r.deadman_pass().await.is_empty(),
            "refused renewal inside the horizon must not suspend"
        );
        assert!(
            tgt.hosts_of(&nqn).contains(&client),
            "the deposed composer serves out its granted horizon — cutting clients off early \
             is what strands their acked writes"
        );

        // Now let the horizon pass: re-grant the OLD composition's lease
        // with an expiry in the past, exactly as a lapse looks.
        backend
            .block_lease_grant("pvc-dm", 1, &me, now_unix() - 1)
            .await
            .unwrap()
            .unwrap();
        let suspended = r.deadman_pass().await;
        assert_eq!(suspended.len(), 1, "the horizon passed: {suspended:?}");
        assert_eq!(suspended[0].0, "pvc-dm");

        // The client's controller is gone; the fence lane stays, because
        // the MDS must still be able to preempt at this target.
        let hosts = tgt.hosts_of(&nqn);
        assert!(!hosts.contains(&client), "client evicted at the device: {hosts:?}");
        assert!(hosts.contains(&crate::identity::block_mds_host_nqn()), "fence lane kept");

        // The admission itself is NOT deleted: the client is still
        // legitimately admitted, it is this TARGET that lost the right
        // to serve it.
        let admitted = backend.block_hosts("pvc-dm").await.unwrap().unwrap();
        assert!(admitted.contains(&client), "admission survives a suspension: {admitted:?}");

        // The lease was surrendered, so the dead-man is done with it.
        assert!(backend.block_lease("pvc-dm").await.unwrap().unwrap().is_none());
        assert!(r.deadman_pass().await.is_empty(), "nothing left to suspend");

        // And a converge cannot re-open what the dead-man closed: the
        // record does not seat this volume here any more.
        let e = r.reconcile_hosts("pvc-dm").await.expect_err("must refuse");
        assert!(e.contains("not the composer"), "got: {e}");
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
            .block_seat_volume("pvc-theirs", "node-b", 100, 220)
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
