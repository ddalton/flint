//! Tier-2 phase 7b-1: the hot-rejoin mechanism library.
//!
//! Rejoins one stale replica into a LIVE raid1 without a consumer restart:
//! a leased quiesce bounds a window in which a final epoch `E_f` is cut on
//! the survivors, exported, esnap-cloned into a fresh head on the rejoin
//! target, and admitted with `bdev_raid_add_base_bdev --skip-rebuild`
//! (carried SPDK patch, v3 lease semantics). Afterwards the head's parent
//! is still the REMOTE `E_f` export — the backfill replays the §5 chain
//! onto the old head (the "landing pad"), snapshots it as the local `E_f`,
//! and `bdev_lvol_set_parent` re-roots the esnap head onto it
//! (localization). See `docs/UnansweredOn7b.md` and
//! `docs/incremental-replica-rebuild.md` §7.
//!
//! Crash discipline: a `hot_rejoin` marker (the E_f name) is written on the
//! replica's record as INTENT before the window opens and survives until
//! localization completes (`mark_in_sync` clears it). Every crash point
//! resolves by inspecting reality against the marker:
//!
//! * record `stale` + marker, head leg live in the raid → the window
//!   committed but the flip was lost: adopt (re-flip).
//! * record `stale` + marker, no live leg → the window died uncommitted:
//!   scrub the stranded artifacts (head clone, E_f export subsystem,
//!   unrecorded E_f snapshots) and clear the marker.
//! * record `standby` + marker, leg live → resume localization.
//! * record `standby` + marker, leg gone → the head localized already →
//!   promote to a plain standby (phase-4 admission owns it); otherwise
//!   demote to stale (the esnap parent may be gone — never admit directly).
//!
//! While the marker is set the replica belongs exclusively to this
//! reconciler: the chase and the bulk catch-up skip it (their export-swap /
//! revert choreography would fight the live leg), and reassembly excludes
//! it (revert-first).
//!
//! The E_f export uses `nqn.2024-11.com.flint:hotrejoin:<volume>` —
//! deliberately OUTSIDE the `:volume:` prefix so the node agent's
//! dead-controller reaper (7b-0) can never condemn the esnap parent's
//! controller while the source is merely restarting.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tracing::{info, warn};

use crate::catchup::{
    copy_chain_to, detach_controller, get_bdev, get_raids, list_lvol_names, pick_source,
    revert_head, revert_head_to_empty, CatchupConfig, CatchupRpc, CatchupStore, RpcError,
};
use crate::driver::SpdkCsiDriver;
use crate::epoch_scheduler::{is_already_exists, is_missing};
use crate::minimal_models::ReplicaInfo;
use crate::nvmeof_export::flint_host_nqn;
use crate::replica_sync::{
    epoch_name, epoch_seq, expected_remote_base_bdev, ReplicaSyncRecord, SyncState,
    VolumeSyncRecord,
};

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Everything the window needs beyond `CatchupRpc`: the E_f export's
/// listener is created on a raw NQN (not via `export_replica`), which needs
/// the source node's address.
#[async_trait]
pub trait HotRejoinRpc: CatchupRpc {
    async fn node_ip(&self, node: &str) -> Result<String, RpcError>;
}

#[async_trait]
impl HotRejoinRpc for SpdkCsiDriver {
    async fn node_ip(&self, node: &str) -> Result<String, RpcError> {
        self.get_node_ip(node).await.map_err(|e| -> RpcError {
            format!("failed to resolve IP of node {}: {}", node, e).into()
        })
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HotRejoinConfig {
    /// Quiesce lease per acquisition (renewed immediately before the add —
    /// hard invariant). FLINT_HOT_REJOIN_LEASE_MS, default 10s.
    pub lease_ms: u64,
    /// Budget for an AER-driven namespace change to surface as a bdev on
    /// the initiator side. FLINT_HOT_REJOIN_AER_WAIT_MS, default 3s.
    pub aer_wait: Duration,
    /// Poll cadence inside `aer_wait`.
    pub aer_poll: Duration,
    /// Retries for `-EBUSY` from the pinned add (lease expiry racing the
    /// add defers to it; the RPC surfaces EBUSY on the release path).
    pub add_retries: u32,
    /// Shallow-copy poll cadence for the localization backfill.
    pub poll_interval: Duration,
    /// The window-duration target (§7 / the eval's ~2 s bar). A committed
    /// window that took longer emits a Warning event — the eval's fallback
    /// trigger (reconsider the atomic-add variant if p99 cannot hold it).
    /// FLINT_HOT_REJOIN_WINDOW_TARGET_MS, default 2s.
    pub window_target: Duration,
}

impl Default for HotRejoinConfig {
    fn default() -> Self {
        HotRejoinConfig {
            lease_ms: 10_000,
            aer_wait: Duration::from_secs(3),
            aer_poll: Duration::from_millis(25),
            add_retries: 3,
            poll_interval: Duration::from_millis(500),
            window_target: Duration::from_secs(2),
        }
    }
}

impl HotRejoinConfig {
    pub fn from_env() -> Self {
        let d = HotRejoinConfig::default();
        let ms = |k: &str, dv: Duration| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or(dv)
        };
        HotRejoinConfig {
            lease_ms: std::env::var("FLINT_HOT_REJOIN_LEASE_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.lease_ms),
            aer_wait: ms("FLINT_HOT_REJOIN_AER_WAIT_MS", d.aer_wait),
            aer_poll: d.aer_poll,
            add_retries: d.add_retries,
            poll_interval: d.poll_interval,
            window_target: ms("FLINT_HOT_REJOIN_WINDOW_TARGET_MS", d.window_target),
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic names — crash recovery and the scrub find artifacts by shape
// ---------------------------------------------------------------------------

/// The E_f export subsystem on the source survivor. NOT under `:volume:`
/// (see module note on the dead-controller reaper).
pub fn ef_export_nqn(volume_id: &str) -> String {
    format!("nqn.2024-11.com.flint:hotrejoin:{}", volume_id)
}

/// The controller name the rejoin target attaches the E_f export under
/// (`bdev_nvme_attach_controller` name → bdevs `{name}n<nsid>`).
pub fn ef_controller_name(volume_id: &str) -> String {
    format!("nvme_{}", ef_export_nqn(volume_id).replace(':', "_").replace('.', "_"))
}

/// The E_f bdev as seen on the rejoin target (the esnap clone's source).
pub fn ef_bdev_on_dst(volume_id: &str) -> String {
    format!("{}n1", ef_controller_name(volume_id))
}

/// The esnap-clone head's lvol name on the rejoin target. Distinct from the
/// replica lvol name — the old head stays behind as the backfill landing
/// pad until localization disposes of it.
pub fn head_lvol_name(volume_id: &str, replica_index: usize) -> String {
    format!("vol_{}_replica_{}_hr", volume_id, replica_index)
}

/// Export id for the landing pad during the backfill (the pad is attached
/// on the SOURCE node as the shallow-copy destination). Distinct from the
/// replica export `{volume}_{index}`, which the live head leg owns.
pub fn pad_export_volume_id(volume_id: &str, replica_index: usize) -> String {
    format!("{}_hrpad{}", volume_id, replica_index)
}

/// The replica export NQN (`export_replica` convention) — the subsystem
/// whose namespace the window swaps from the pad to the esnap head.
pub fn replica_export_nqn(volume_id: &str, replica_index: usize) -> String {
    format!("nqn.2024-11.com.flint:volume:{}_{}", volume_id, replica_index)
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum HotRejoinOutcome {
    /// The window committed and the record flipped. `localized` reports
    /// whether the backfill also completed in this call (a failure there is
    /// not fatal — the marker keeps the reconciler resuming it).
    Rejoined { window_ms: u128, localized: bool },
    /// Nothing to do / preconditions unmet (reason is operator-facing).
    NotEligible(&'static str),
}

// ---------------------------------------------------------------------------
// Topology resolution
// ---------------------------------------------------------------------------

struct Topology<'a> {
    volume_id: &'a str,
    raid_name: String,
    consumer: &'a str,
    /// The stale replica being rejoined.
    rec: &'a ReplicaSyncRecord,
    idx: usize,
    identity: &'a ReplicaInfo,
    /// The E_f export host and backfill source.
    src: &'a ReplicaInfo,
    /// Every in-sync replica (E_f must be cut on all of them to be a
    /// common epoch).
    survivors: Vec<&'a ReplicaInfo>,
}

fn resolve<'a>(
    volume_id: &'a str,
    record: &'a VolumeSyncRecord,
    replicas: &'a [ReplicaInfo],
    consumer: &'a str,
) -> Result<Topology<'a>, &'static str> {
    // One rejoin per volume at a time: the E_f export NQN is per-VOLUME, so
    // a second concurrent window would collide with the first's transport
    // (and its reconciler owns the marked replica anyway).
    if record.replicas.iter().any(|r| r.hot_rejoin.is_some()) {
        return Err("a hot rejoin is already in progress on this volume");
    }
    // Target preference: the most-converged standby (the 7b-2 trigger's
    // class — Tier-1's chase already did the bulk copy, so localization
    // replays the least), else the first stale replica (manual/drill path;
    // its cold chain makes localization long but never incorrect).
    let rec = record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::Standby)
        .max_by_key(|r| {
            r.last_epoch
                .as_deref()
                .and_then(|e| epoch_seq(volume_id, e))
                .unwrap_or(0)
        })
        .or_else(|| record.replicas.iter().find(|r| r.sync_state == SyncState::Stale))
        .ok_or("no stale or standby replica to rejoin")?;
    let (idx, identity) = replicas
        .iter()
        .enumerate()
        .find(|(_, ri)| ri.lvol_uuid == rec.lvol_uuid)
        .ok_or("stale replica's identity is not in the replica list")?;
    let survivors: Vec<&ReplicaInfo> = record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::InSync)
        .filter_map(|r| replicas.iter().find(|ri| ri.lvol_uuid == r.lvol_uuid))
        .collect();
    if survivors.is_empty() {
        return Err("no in-sync survivor to cut E_f on");
    }
    let src = pick_source(record, replicas, Some(consumer)).ok_or("no in-sync source")?;
    Ok(Topology {
        volume_id,
        raid_name: format!("raid_{}", volume_id),
        consumer,
        rec,
        idx,
        identity,
        src,
        survivors,
    })
}

// ---------------------------------------------------------------------------
// The mechanism: pre-stage → window → flip → localize
// ---------------------------------------------------------------------------

/// Hot-rejoin the volume's (first) stale replica into its live raid.
/// `consumer` must be the node currently holding the raid. The record flip
/// is the commit point: after it, a localization failure leaves the marker
/// for the reconciler and is NOT an error of this call.
pub async fn hot_rejoin_volume(
    rpc: &dyn HotRejoinRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    replicas: &[ReplicaInfo],
    consumer: &str,
    cfg: &HotRejoinConfig,
) -> Result<HotRejoinOutcome, RpcError> {
    let Some(record) = store.load(volume_id).await? else {
        return Ok(HotRejoinOutcome::NotEligible("single-replica volume"));
    };
    if record.epochs.is_empty() {
        // E_f seeds the head by itself, but localization needs a §5 base
        // lineage — and a volume with no epoch history yet has bigger
        // problems than a fast rejoin.
        return Ok(HotRejoinOutcome::NotEligible("no epoch history"));
    }
    let topo = match resolve(volume_id, &record, replicas, consumer) {
        Ok(t) => t,
        Err(why) => return Ok(HotRejoinOutcome::NotEligible(why)),
    };

    // The raid must be online on the consumer for a live add.
    let raid_online = get_raids(rpc, consumer).await?.iter().any(|r| {
        r.get("name").and_then(|n| n.as_str()) == Some(topo.raid_name.as_str())
            && r.get("state").and_then(|s| s.as_str()) == Some("online")
    });
    if !raid_online {
        return Ok(HotRejoinOutcome::NotEligible("raid not online on consumer"));
    }

    // Choose E_f: next epoch seq, stepping over any name already present on
    // a survivor (a stranded earlier E_f, or the scheduler winning a race
    // to this seq). The strict in-window cut still refuses EEXIST — this
    // pre-pick just makes collisions rare instead of fatal.
    let mut ef_seq = record.latest_epoch_seq(volume_id) + 1;
    let src_names = list_lvol_names(rpc, &topo.src.node_name, &topo.src.lvs_name).await?;
    while src_names.contains(&epoch_name(volume_id, ef_seq)) {
        ef_seq += 1;
    }
    let ef = epoch_name(volume_id, ef_seq);

    // INTENT before any node mutation: from here every crash point is
    // marker-recoverable. A standby target is demoted to stale in the same
    // write (see mark_hot_rejoin_intent). The CAS closure no-ops if the
    // target changed state under us (e.g. a restage admitted it in_sync
    // between our load and the write) — verify the marker actually landed
    // before opening a window on a stranger.
    store.record_hot_rejoin_intent(volume_id, &topo.rec.lvol_uuid, &ef).await?;
    let intent_landed = store.load(volume_id).await?.is_some_and(|r| {
        r.replicas
            .iter()
            .any(|rec| rec.lvol_uuid == topo.rec.lvol_uuid && rec.hot_rejoin.as_deref() == Some(ef.as_str()))
    });
    if !intent_landed {
        return Ok(HotRejoinOutcome::NotEligible(
            "target replica changed state before the window opened",
        ));
    }

    prestage(rpc, &topo).await.inspect_err(|_| {
        // Nothing consumer-visible was touched; the marker-driven scrub
        // owns whatever skeleton half-landed.
    })?;

    let head_name = head_lvol_name(volume_id, topo.idx);
    match window(rpc, &topo, &ef, &head_name, cfg).await {
        Ok((timings, head_uuid)) => {
            let window_ms: u128 = timings.iter().map(|(_, ms)| ms).sum();
            let cut_uuids: Vec<String> =
                topo.survivors.iter().map(|s| s.lvol_uuid.clone()).collect();
            store
                .record_hot_rejoin_flip(volume_id, &topo.rec.lvol_uuid, &ef, &cut_uuids, &head_uuid)
                .await?;
            let detail = timings
                .iter()
                .map(|(step, ms)| format!("{}={}ms", step, ms))
                .collect::<Vec<_>>()
                .join(" ");
            store
                .emit(
                    volume_id,
                    "Normal",
                    "HotRejoinSucceeded",
                    &format!(
                        "Replica on {} hot-rejoined raid {} at {} in {}ms ({}); localizing the esnap chain",
                        topo.identity.node_name, topo.raid_name, ef, window_ms, detail
                    ),
                )
                .await;
            info!(volume_id, node = %topo.identity.node_name, window_ms, "[HOT_REJOIN] Window committed");
            if window_ms > cfg.window_target.as_millis() {
                store
                    .emit(
                        volume_id,
                        "Warning",
                        "HotRejoinWindowSlow",
                        &format!(
                            "Hot-rejoin quiesce window took {}ms against the {}ms target ({}) — \
                             if this persists, reconsider the atomic-add variant (§7 option b)",
                            window_ms,
                            cfg.window_target.as_millis(),
                            detail
                        ),
                    )
                    .await;
            }

            // Post-commit: reload the record (the flip changed it) and run
            // localization inline. Failure is retried by the reconciler.
            let localized = match store.load(volume_id).await? {
                Some(rec2) => {
                    let marked = rec2
                        .replicas
                        .iter()
                        .find(|r| r.lvol_uuid == topo.rec.lvol_uuid)
                        .cloned();
                    match marked {
                        Some(m) if m.hot_rejoin.is_some() => localize(
                            rpc, store, volume_id, &rec2, &m, replicas, Some(consumer), cfg,
                        )
                        .await
                        .map_err(|e| {
                            warn!(volume_id, error = %e, "[HOT_REJOIN] Localization deferred (reconciler resumes it)");
                            e
                        })
                        .is_ok(),
                        _ => true,
                    }
                }
                None => false,
            };
            Ok(HotRejoinOutcome::Rejoined { window_ms, localized })
        }
        Err(e) => {
            store
                .record_hot_rejoin_cleared(
                    volume_id,
                    &topo.rec.lvol_uuid,
                    &format!("hot-rejoin window unwound: {}", e),
                    false,
                )
                .await?;
            store
                .emit(
                    volume_id,
                    "Warning",
                    "HotRejoinUnwound",
                    &format!(
                        "Hot rejoin of replica on {} unwound: {}",
                        topo.identity.node_name, e
                    ),
                )
                .await;
            Err(e)
        }
    }
}

/// Everything that can happen OUTSIDE the quiesce window: the E_f export
/// skeleton (subsystem + host fence + listener, NO namespace), the
/// namespace-less controller pre-connect on the rejoin target, and the
/// consumer-side controller to the replica export. In-window the only
/// transport work left is `add_ns` / the ns swap, surfaced by AER.
async fn prestage(rpc: &dyn HotRejoinRpc, topo: &Topology<'_>) -> Result<(), RpcError> {
    let vol = topo.volume_id;
    let nqn_ef = ef_export_nqn(vol);
    let src_node = &topo.src.node_name;
    let dst_node = &topo.identity.node_name;

    // E_f export skeleton on the source survivor.
    let create = json!({
        "method": "nvmf_create_subsystem",
        "params": { "nqn": nqn_ef, "allow_any_host": false, "model_number": "FLINT hot-rejoin E_f" }
    });
    match rpc.spdk_rpc(src_node, &create).await {
        Ok(_) => {}
        Err(e) if is_already_exists(&e.to_string()) => {}
        Err(e) => return Err(format!("E_f subsystem on {}: {}", src_node, e).into()),
    }
    let host = json!({
        "method": "nvmf_subsystem_add_host",
        "params": { "nqn": nqn_ef, "host": flint_host_nqn(dst_node) }
    });
    match rpc.spdk_rpc(src_node, &host).await {
        Ok(_) => {}
        Err(e) if is_already_exists(&e.to_string()) => {}
        Err(e) => return Err(format!("E_f host fence on {}: {}", src_node, e).into()),
    }
    let src_ip = rpc.node_ip(src_node).await?;
    let listener = json!({
        "method": "nvmf_subsystem_add_listener",
        "params": {
            "nqn": nqn_ef,
            "listen_address": { "trtype": "TCP", "traddr": src_ip, "trsvcid": "4420", "adrfam": "ipv4" }
        }
    });
    match rpc.spdk_rpc(src_node, &listener).await {
        Ok(_) => {}
        Err(e) if is_already_exists(&e.to_string()) => {}
        Err(e) => return Err(format!("E_f listener on {}: {}", src_node, e).into()),
    }

    // Pre-connect the rejoin target to the (still namespace-less) E_f
    // export — the in-window add_ns surfaces as an AER namespace hot-add.
    let ef_ctrl = ef_controller_name(vol);
    let attach = json!({
        "method": "bdev_nvme_attach_controller",
        "params": {
            "name": ef_ctrl, "trtype": "TCP", "traddr": src_ip, "trsvcid": "4420",
            "subnqn": nqn_ef, "adrfam": "IPv4", "hostnqn": flint_host_nqn(dst_node)
        }
    });
    match rpc.spdk_rpc(dst_node, &attach).await {
        Ok(_) => {}
        Err(e) if is_already_exists(&e.to_string()) => {}
        Err(e) => return Err(format!("E_f pre-connect on {}: {}", dst_node, e).into()),
    }

    // Converge the replica export (subsystem/listener/fence; namespace =
    // the pad, its current state) and pre-connect the consumer to it. The
    // window swaps only the namespace.
    let pad_alias = format!("{}/{}", topo.identity.lvs_name, topo.identity.lvol_name);
    let conn = rpc
        .export_replica(
            dst_node,
            &pad_alias,
            &format!("{}_{}", vol, topo.idx),
            topo.consumer,
        )
        .await?;
    let expected = expected_remote_base_bdev(vol, topo.idx);
    let ctrl = expected.strip_suffix("n1").unwrap_or(&expected).to_string();
    if get_bdev(rpc, topo.consumer, &expected).await?.is_none() {
        // Controller may exist but serve nothing usable (dead reconnect
        // loop after the replica's spdk-tgt restart) — replace it.
        detach_controller(rpc, topo.consumer, &ctrl).await;
        let attach = json!({
            "method": "bdev_nvme_attach_controller",
            "params": {
                "name": ctrl, "trtype": conn.transport.to_uppercase(),
                "traddr": conn.target_ip, "trsvcid": conn.target_port.to_string(),
                "subnqn": conn.nqn, "adrfam": "IPv4", "hostnqn": flint_host_nqn(topo.consumer)
            }
        });
        rpc.spdk_rpc(topo.consumer, &attach)
            .await
            .map_err(|e| format!("consumer pre-connect of {}: {}", conn.nqn, e))?;
    }
    Ok(())
}

/// The quiesce window (§7): every step timed, any failure unwound in
/// reverse dependency order. Returns per-step timings and the head's uuid.
async fn window(
    rpc: &dyn HotRejoinRpc,
    topo: &Topology<'_>,
    ef: &str,
    head_name: &str,
    cfg: &HotRejoinConfig,
) -> Result<(Vec<(&'static str, u128)>, String), RpcError> {
    let vol = topo.volume_id;
    let dst_node = &topo.identity.node_name;
    let src_node = &topo.src.node_name;
    let dst_lvs = &topo.identity.lvs_name;
    let nqn_ef = ef_export_nqn(vol);
    let nqn_replica = replica_export_nqn(vol, topo.idx);
    let expected = expected_remote_base_bdev(vol, topo.idx);
    let pad_alias = format!("{}/{}", dst_lvs, topo.identity.lvol_name);
    let head_alias = format!("{}/{}", dst_lvs, head_name);

    let mut timings: Vec<(&'static str, u128)> = Vec::new();
    let mut t = Instant::now();
    let mut lap = |name: &'static str, t: &mut Instant| {
        timings.push((name, t.elapsed().as_millis()));
        *t = Instant::now();
    };

    // Unwind bookkeeping.
    let mut cut_done = false;
    let mut ef_ns_added = false;
    let mut head_created = false;
    let mut ns_swapped = false;

    // Find the pad's nsid before touching anything (used by the swap).
    let pad_nsid = subsystem_nsid(rpc, dst_node, &nqn_replica).await?.unwrap_or(1);

    let result: Result<String, RpcError> = async {
        // W1: leased quiesce.
        rpc.spdk_rpc(
            topo.consumer,
            &json!({ "method": "bdev_raid_quiesce",
                     "params": { "name": topo.raid_name, "lease_ms": cfg.lease_ms } }),
        )
        .await
        .map_err(|e| format!("quiesce: {}", e))?;
        lap("quiesce", &mut t);

        // W2: E_f on every survivor — strict, all-or-abort, EEXIST refused
        // (an EEXIST snapshot was cut at some OTHER instant and is not the
        // quiesced image; adopting it silently is the §7 divergence bug).
        cut_ef_strict(rpc, &topo.survivors, ef).await?;
        cut_done = true;
        lap("cut E_f", &mut t);

        // W3: publish E_f under the pre-staged export; AER surfaces it on
        // the pre-connected rejoin target.
        rpc.spdk_rpc(
            src_node,
            &json!({ "method": "nvmf_subsystem_add_ns",
                     "params": { "nqn": nqn_ef,
                                  "namespace": { "bdev_name": format!("{}/{}", topo.src.lvs_name, ef) } } }),
        )
        .await
        .map_err(|e| format!("E_f add_ns: {}", e))?;
        ef_ns_added = true;
        let ef_bdev = ef_bdev_on_dst(vol);
        wait_bdev(rpc, dst_node, &ef_bdev, true, None, cfg)
            .await
            .map_err(|e| format!("E_f bdev on {}: {}", dst_node, e))?;
        lap("export+aer E_f", &mut t);

        // W4: esnap-clone the head from the E_f bdev.
        let resp = rpc
            .spdk_rpc(
                dst_node,
                &json!({ "method": "bdev_lvol_clone_bdev",
                         "params": { "bdev": ef_bdev, "lvs_name": dst_lvs, "clone_name": head_name } }),
            )
            .await
            .map_err(|e| format!("esnap clone: {}", e))?;
        let head_uuid = resp
            .get("result")
            .and_then(|r| r.as_str())
            .map(String::from)
            .ok_or("bdev_lvol_clone_bdev returned no uuid")?;
        head_created = true;
        lap("esnap clone head", &mut t);

        // W5: swap the replica export's namespace pad → head. Two AER
        // round-trips (gone, then present with the head's uuid) — the
        // initiator must not serve the pad's cached namespace.
        rpc.spdk_rpc(
            dst_node,
            &json!({ "method": "nvmf_subsystem_remove_ns",
                     "params": { "nqn": nqn_replica, "nsid": pad_nsid } }),
        )
        .await
        .map_err(|e| format!("ns swap (remove): {}", e))?;
        ns_swapped = true;
        wait_bdev(rpc, topo.consumer, &expected, false, None, cfg)
            .await
            .map_err(|e| format!("ns swap (old ns still visible on consumer): {}", e))?;
        rpc.spdk_rpc(
            dst_node,
            &json!({ "method": "nvmf_subsystem_add_ns",
                     "params": { "nqn": nqn_replica,
                                  "namespace": { "bdev_name": head_alias, "nsid": pad_nsid } } }),
        )
        .await
        .map_err(|e| format!("ns swap (add): {}", e))?;
        wait_bdev(rpc, topo.consumer, &expected, true, Some(&head_uuid), cfg)
            .await
            .map_err(|e| format!("head bdev on consumer: {}", e))?;
        lap("export+aer head", &mut t);

        // W6: renew immediately before the add — hard invariant.
        rpc.spdk_rpc(
            topo.consumer,
            &json!({ "method": "bdev_raid_quiesce",
                     "params": { "name": topo.raid_name, "lease_ms": cfg.lease_ms } }),
        )
        .await
        .map_err(|e| format!("lease renew (window breached — never add): {}", e))?;
        lap("lease renew", &mut t);

        // W7: the patched add. EBUSY = a just-released lease's unquiesce in
        // flight — bounded retry.
        let add = json!({ "method": "bdev_raid_add_base_bdev",
                          "params": { "raid_bdev": topo.raid_name, "base_bdev": expected,
                                       "skip_rebuild": true } });
        let mut attempt = 0;
        loop {
            match rpc.spdk_rpc(topo.consumer, &add).await {
                Ok(_) => break,
                Err(e) if is_busy(&e.to_string()) && attempt < cfg.add_retries => {
                    attempt += 1;
                    if !cfg.aer_poll.is_zero() {
                        tokio::time::sleep(cfg.aer_poll).await;
                    }
                }
                Err(e) => return Err(format!("skip_rebuild add: {}", e).into()),
            }
        }
        lap("add --skip-rebuild", &mut t);

        // W8: release. -ENOENT = the lease auto-released already (expiry
        // deferred past the pinned add, then fired) — the release happened,
        // treat as success. Any other failure: v3 keeps the lease armed and
        // its expiry poller retries the release — commit anyway.
        match rpc
            .spdk_rpc(
                topo.consumer,
                &json!({ "method": "bdev_raid_unquiesce", "params": { "name": topo.raid_name } }),
            )
            .await
        {
            Ok(_) => {}
            Err(e) if is_missing(&e.to_string()) || e.to_string().contains("no quiesce lease") => {}
            Err(e) => {
                warn!(volume_id = vol, error = %e, "[HOT_REJOIN] Unquiesce failed post-add — v3 expiry poller owns the release");
            }
        }
        lap("unquiesce", &mut t);
        Ok(head_uuid)
    }
    .await;

    match result {
        Ok(head_uuid) => Ok((timings, head_uuid)),
        Err(e) => {
            // Unwind in reverse dependency order, best-effort.
            if ns_swapped {
                let _ = rpc
                    .spdk_rpc(
                        dst_node,
                        &json!({ "method": "nvmf_subsystem_remove_ns",
                                 "params": { "nqn": nqn_replica, "nsid": pad_nsid } }),
                    )
                    .await;
                let _ = rpc
                    .spdk_rpc(
                        dst_node,
                        &json!({ "method": "nvmf_subsystem_add_ns",
                                 "params": { "nqn": nqn_replica,
                                              "namespace": { "bdev_name": pad_alias, "nsid": pad_nsid } } }),
                    )
                    .await;
            }
            if head_created {
                let _ = rpc
                    .spdk_rpc(
                        dst_node,
                        &json!({ "method": "bdev_lvol_delete", "params": { "name": head_alias } }),
                    )
                    .await;
            }
            if ef_ns_added {
                let _ = rpc
                    .spdk_rpc(
                        src_node,
                        &json!({ "method": "nvmf_subsystem_remove_ns",
                                 "params": { "nqn": nqn_ef, "nsid": 1 } }),
                    )
                    .await;
            }
            if cut_done {
                // E_f never became a recorded epoch — reap it now rather
                // than leaving EEXIST-convergence litter.
                for s in &topo.survivors {
                    let alias = format!("{}/{}", s.lvs_name, ef);
                    let _ = rpc
                        .spdk_rpc(
                            &s.node_name,
                            &json!({ "method": "bdev_lvol_delete", "params": { "name": alias } }),
                        )
                        .await;
                }
            }
            match rpc
                .spdk_rpc(
                    topo.consumer,
                    &json!({ "method": "bdev_raid_unquiesce", "params": { "name": topo.raid_name } }),
                )
                .await
            {
                Ok(_) => {}
                Err(e2) if is_missing(&e2.to_string()) || e2.to_string().contains("no quiesce lease") => {}
                Err(e2) => {
                    warn!(volume_id = vol, error = %e2, "[HOT_REJOIN] Unwind unquiesce failed — lease expiry will release");
                }
            }
            Err(e)
        }
    }
}

/// Cut E_f on every survivor, strictly fresh: EEXIST aborts (see W2 note),
/// any failure rolls back the snapshots already cut.
async fn cut_ef_strict(
    rpc: &dyn CatchupRpc,
    survivors: &[&ReplicaInfo],
    ef: &str,
) -> Result<(), RpcError> {
    let mut cut: Vec<(&str, String)> = Vec::new();
    for s in survivors {
        let alias = format!("{}/{}", s.lvs_name, s.lvol_name);
        let payload = json!({ "method": "bdev_lvol_snapshot",
                              "params": { "lvol_name": alias, "snapshot_name": ef } });
        match rpc.spdk_rpc(&s.node_name, &payload).await {
            Ok(_) => cut.push((&s.node_name, format!("{}/{}", s.lvs_name, ef))),
            Err(e) => {
                for (node, alias) in &cut {
                    let _ = rpc
                        .spdk_rpc(
                            node,
                            &json!({ "method": "bdev_lvol_delete", "params": { "name": alias } }),
                        )
                        .await;
                }
                let kind = if is_already_exists(&e.to_string()) {
                    "E_f name collision (concurrent epoch cut?)"
                } else {
                    "E_f cut failed"
                };
                return Err(format!("{} on {}: {}", kind, s.node_name, e).into());
            }
        }
    }
    Ok(())
}

/// Poll for a bdev to appear (optionally with an exact uuid) or disappear,
/// within the AER budget.
async fn wait_bdev(
    rpc: &dyn CatchupRpc,
    node: &str,
    name: &str,
    want_present: bool,
    want_uuid: Option<&str>,
    cfg: &HotRejoinConfig,
) -> Result<(), RpcError> {
    let deadline = Instant::now() + cfg.aer_wait;
    loop {
        let bdev = get_bdev(rpc, node, name).await?;
        let ok = match (&bdev, want_present) {
            (Some(b), true) => match want_uuid {
                Some(u) => b.get("uuid").and_then(|x| x.as_str()) == Some(u),
                None => true,
            },
            (None, false) => true,
            _ => false,
        };
        if ok {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "bdev {} did not become {} on {} within the AER budget",
                name,
                if want_present { "present" } else { "absent" },
                node
            )
            .into());
        }
        if !cfg.aer_poll.is_zero() {
            tokio::time::sleep(cfg.aer_poll).await;
        }
    }
}

fn is_busy(msg: &str) -> bool {
    msg.contains("EBUSY") || msg.contains("Code=-16") || msg.to_lowercase().contains("busy")
}

async fn subsystem_nsid(
    rpc: &dyn CatchupRpc,
    node: &str,
    nqn: &str,
) -> Result<Option<u64>, RpcError> {
    let resp = rpc
        .spdk_rpc(node, &json!({ "method": "nvmf_get_subsystems" }))
        .await?;
    Ok(resp
        .get("result")
        .and_then(|r| r.as_array())
        .and_then(|subs| {
            subs.iter().find(|s| s.get("nqn").and_then(|n| n.as_str()) == Some(nqn))
        })
        .and_then(|s| s.get("namespaces"))
        .and_then(|n| n.as_array())
        .and_then(|nss| nss.first())
        .and_then(|ns| ns.get("nsid"))
        .and_then(|n| n.as_u64()))
}

// ---------------------------------------------------------------------------
// The marker-driven reconciler (called from the catch-up orchestrator)
// ---------------------------------------------------------------------------

/// Resolve every marker-claimed replica against reality — see the module
/// note's case table. Failures are contained per replica (warned + evented,
/// retried next tick).
pub async fn reconcile_marked(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    record: &VolumeSyncRecord,
    replicas: &[ReplicaInfo],
    consumer_node: Option<&str>,
    cfg: &CatchupConfig,
) {
    for rec in record.replicas.iter().filter(|r| r.hot_rejoin.is_some()) {
        let outcome = match rec.sync_state {
            SyncState::Stale => {
                adopt_or_scrub(rpc, store, volume_id, record, rec, replicas, consumer_node).await
            }
            SyncState::Standby => {
                resume_standby(rpc, store, volume_id, record, rec, replicas, consumer_node, cfg)
                    .await
            }
            // mark_in_sync clears the marker — an in_sync marked replica is
            // a record written by a newer build or a partial CAS; clear it.
            SyncState::InSync => {
                store
                    .record_hot_rejoin_cleared(
                        volume_id,
                        &rec.lvol_uuid,
                        "marker on an in_sync replica (defensive clear)",
                        false,
                    )
                    .await
            }
        };
        if let Err(e) = outcome {
            warn!(volume_id, replica = %rec.lvol_uuid, error = %e, "[HOT_REJOIN] Reconcile failed — retrying next cycle");
            store
                .emit(
                    volume_id,
                    "Warning",
                    "HotRejoinReconcileFailed",
                    &format!(
                        "Hot-rejoin reconcile of replica on {} failed: {}",
                        rec.node_name, e
                    ),
                )
                .await;
        }
    }
}

/// Is the rejoined head configured in the consumer's raid, and does it
/// belong to this replica's `_hr` head lvol?
async fn live_head_leg(
    rpc: &dyn CatchupRpc,
    volume_id: &str,
    idx: usize,
    identity: &ReplicaInfo,
    consumer: Option<&str>,
) -> Result<Option<String>, RpcError> {
    let Some(consumer) = consumer else { return Ok(None) };
    let expected = expected_remote_base_bdev(volume_id, idx);
    let raid_name = format!("raid_{}", volume_id);
    let leg_configured = get_raids(rpc, consumer).await?.iter().any(|r| {
        r.get("name").and_then(|n| n.as_str()) == Some(raid_name.as_str())
            && r.get("base_bdevs_list")
                .and_then(|b| b.as_array())
                .map(|bases| {
                    bases.iter().any(|b| {
                        b.get("name").and_then(|n| n.as_str()) == Some(expected.as_str())
                            && b.get("is_configured").and_then(|c| c.as_bool()) == Some(true)
                    })
                })
                .unwrap_or(false)
    });
    if !leg_configured {
        return Ok(None);
    }
    // The leg is only THIS rejoin's if the consumer-side bdev carries the
    // head lvol's uuid (the namespace inherits the backing bdev's uuid).
    let head_alias = format!(
        "{}/{}",
        identity.lvs_name,
        head_lvol_name(volume_id, idx)
    );
    let head_uuid = get_bdev(rpc, &identity.node_name, &head_alias)
        .await?
        .and_then(|b| b.get("uuid").and_then(|u| u.as_str()).map(String::from));
    let consumer_uuid = get_bdev(rpc, consumer, &expected)
        .await?
        .and_then(|b| b.get("uuid").and_then(|u| u.as_str()).map(String::from));
    match (head_uuid, consumer_uuid) {
        (Some(h), Some(c)) if h == c => Ok(Some(h)),
        _ => Ok(None),
    }
}

/// stale + marker: the window opened but the flip never landed. If the head
/// leg is live in the raid the add committed — adopt it (re-flip). If not,
/// the window died: scrub the strandings and release the claim.
async fn adopt_or_scrub(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    record: &VolumeSyncRecord,
    rec: &ReplicaSyncRecord,
    replicas: &[ReplicaInfo],
    consumer_node: Option<&str>,
) -> Result<(), RpcError> {
    let ef = rec.hot_rejoin.clone().expect("caller filters on marker");
    let Some((idx, identity)) = replicas
        .iter()
        .enumerate()
        .find(|(_, ri)| ri.lvol_uuid == rec.lvol_uuid)
    else {
        return store
            .record_hot_rejoin_cleared(volume_id, &rec.lvol_uuid, "identity replaced", false)
            .await;
    };

    if let Some(head_uuid) =
        live_head_leg(rpc, volume_id, idx, identity, consumer_node).await?
    {
        let cut_uuids: Vec<String> = record
            .replicas
            .iter()
            .filter(|r| r.sync_state == SyncState::InSync)
            .map(|r| r.lvol_uuid.clone())
            .collect();
        store
            .record_hot_rejoin_flip(volume_id, &rec.lvol_uuid, &ef, &cut_uuids, &head_uuid)
            .await?;
        store
            .emit(
                volume_id,
                "Normal",
                "HotRejoinAdopted",
                &format!(
                    "Adopted a committed hot rejoin on {} (flip was lost to a crash); localizing",
                    rec.node_name
                ),
            )
            .await;
        info!(volume_id, node = %rec.node_name, "[HOT_REJOIN] Adopted committed window");
        return Ok(());
    }

    scrub_uncommitted(rpc, volume_id, record, rec, replicas, idx, identity, &ef).await;
    store
        .record_hot_rejoin_cleared(
            volume_id,
            &rec.lvol_uuid,
            "hot-rejoin window died before commit; artifacts scrubbed",
            false,
        )
        .await?;
    store
        .emit(
            volume_id,
            "Normal",
            "HotRejoinScrubbed",
            &format!(
                "Scrubbed the stranded artifacts of an uncommitted hot rejoin on {}",
                rec.node_name
            ),
        )
        .await;
    Ok(())
}

/// Remove everything an uncommitted window may have stranded. Every delete
/// is missing-tolerant; the E_f snapshots are reaped only when E_f never
/// became a recorded epoch.
#[allow(clippy::too_many_arguments)]
async fn scrub_uncommitted(
    rpc: &dyn CatchupRpc,
    volume_id: &str,
    record: &VolumeSyncRecord,
    _rec: &ReplicaSyncRecord,
    replicas: &[ReplicaInfo],
    idx: usize,
    identity: &ReplicaInfo,
    ef: &str,
) {
    let dst_node = &identity.node_name;
    let head_alias = format!("{}/{}", identity.lvs_name, head_lvol_name(volume_id, idx));
    let nqn_replica = replica_export_nqn(volume_id, idx);

    // The head may still be the replica export's namespace — release that
    // claim first or the lvol delete returns EBUSY. The next chase's
    // `ensure_dst_attached` converges the export back to the pad.
    if let Ok(Some(nsid)) = subsystem_nsid(rpc, dst_node, &nqn_replica).await {
        let _ = rpc
            .spdk_rpc(
                dst_node,
                &json!({ "method": "nvmf_subsystem_remove_ns",
                         "params": { "nqn": nqn_replica, "nsid": nsid } }),
            )
            .await;
    }
    let _ = rpc
        .spdk_rpc(
            dst_node,
            &json!({ "method": "bdev_lvol_delete", "params": { "name": head_alias } }),
        )
        .await;
    detach_controller(rpc, dst_node, &ef_controller_name(volume_id)).await;
    let nqn_ef = ef_export_nqn(volume_id);
    for r in record.replicas.iter().filter(|r| r.sync_state == SyncState::InSync) {
        if let Some(ri) = replicas.iter().find(|ri| ri.lvol_uuid == r.lvol_uuid) {
            let _ = rpc
                .spdk_rpc(
                    &ri.node_name,
                    &json!({ "method": "nvmf_delete_subsystem", "params": { "nqn": nqn_ef } }),
                )
                .await;
            if !record.epochs.iter().any(|e| e.name == ef) {
                let alias = format!("{}/{}", ri.lvs_name, ef);
                let _ = rpc
                    .spdk_rpc(
                        &ri.node_name,
                        &json!({ "method": "bdev_lvol_delete", "params": { "name": alias } }),
                    )
                    .await;
            }
        }
    }
}

/// standby + marker: resume localization while the leg lives; when the leg
/// is gone (restage excluded it, or the esnap chain broke) either promote a
/// head that already localized, or demote to stale.
#[allow(clippy::too_many_arguments)]
async fn resume_standby(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    record: &VolumeSyncRecord,
    rec: &ReplicaSyncRecord,
    replicas: &[ReplicaInfo],
    consumer_node: Option<&str>,
    cfg: &CatchupConfig,
) -> Result<(), RpcError> {
    let ef = rec.hot_rejoin.clone().expect("caller filters on marker");
    let Some((idx, identity)) = replicas
        .iter()
        .enumerate()
        .find(|(_, ri)| ri.lvol_uuid == rec.lvol_uuid)
    else {
        return store
            .record_hot_rejoin_cleared(volume_id, &rec.lvol_uuid, "identity replaced", true)
            .await;
    };

    if live_head_leg(rpc, volume_id, idx, identity, consumer_node)
        .await?
        .is_some()
    {
        let hr_cfg = HotRejoinConfig {
            poll_interval: cfg.poll_interval,
            ..HotRejoinConfig::default()
        };
        return localize(
            rpc, store, volume_id, record, rec, replicas, consumer_node, &hr_cfg,
        )
        .await;
    }

    // Leg gone. Localized already? Then it is an ordinary standby at E_f —
    // release the claim and let phase-4 admission own it.
    let head_alias = format!("{}/{}", identity.lvs_name, head_lvol_name(volume_id, idx));
    let head = get_bdev(rpc, &identity.node_name, &head_alias).await?;
    let localized = head
        .as_ref()
        .and_then(|b| b.get("driver_specific"))
        .and_then(|d| d.get("lvol"))
        .and_then(|l| l.get("base_snapshot"))
        .and_then(|s| s.as_str())
        == Some(ef.as_str());
    if localized {
        store
            .record_hot_rejoin_cleared(
                volume_id,
                &rec.lvol_uuid,
                "localized standby; leg not in the current raid — phase-4 admission owns it",
                false,
            )
            .await?;
        info!(volume_id, node = %rec.node_name, "[HOT_REJOIN] Promoted localized head to plain standby");
        return Ok(());
    }

    // Not localized and not serving: the esnap head is unusable without its
    // remote parent — demote and let the ordinary catch-up rebuild.
    if head.is_some() {
        let _ = rpc
            .spdk_rpc(
                &identity.node_name,
                &json!({ "method": "bdev_lvol_delete", "params": { "name": head_alias } }),
            )
            .await;
    }
    detach_controller(rpc, &identity.node_name, &ef_controller_name(volume_id)).await;
    store
        .record_hot_rejoin_cleared(
            volume_id,
            &rec.lvol_uuid,
            "hot-rejoined leg lost before localization; demoted for ordinary catch-up",
            true,
        )
        .await?;
    store
        .emit(
            volume_id,
            "Warning",
            "HotRejoinDemoted",
            &format!(
                "Hot-rejoined replica on {} lost its leg before localization — demoted to stale",
                rec.node_name
            ),
        )
        .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Localization: pad backfill → local E_f → set_parent → dispose pad
// ---------------------------------------------------------------------------

/// Design item 1's choreography. Idempotent at every rung: a crash resumes
/// here via the marker, and a head already re-rooted onto the local E_f
/// short-circuits straight to cleanup.
#[allow(clippy::too_many_arguments)]
async fn localize(
    rpc: &dyn CatchupRpc,
    store: &dyn CatchupStore,
    volume_id: &str,
    record: &VolumeSyncRecord,
    rec: &ReplicaSyncRecord,
    replicas: &[ReplicaInfo],
    consumer_node: Option<&str>,
    cfg: &HotRejoinConfig,
) -> Result<(), RpcError> {
    let ef = rec.hot_rejoin.clone().expect("caller filters on marker");
    let Some((idx, identity)) = replicas
        .iter()
        .enumerate()
        .find(|(_, ri)| ri.lvol_uuid == rec.lvol_uuid)
    else {
        return Ok(());
    };
    let dst_node = &identity.node_name;
    let dst_lvs = &identity.lvs_name;
    let head_name = head_lvol_name(volume_id, idx);
    let head_alias = format!("{}/{}", dst_lvs, head_name);
    let pad_alias = format!("{}/{}", dst_lvs, identity.lvol_name);
    let ef_local_alias = format!("{}/{}", dst_lvs, ef);

    let head = get_bdev(rpc, dst_node, &head_alias)
        .await?
        .ok_or_else(|| format!("esnap head {} missing on {}", head_alias, dst_node))?;
    let already_localized = head
        .get("driver_specific")
        .and_then(|d| d.get("lvol"))
        .and_then(|l| l.get("base_snapshot"))
        .and_then(|s| s.as_str())
        == Some(ef.as_str());

    if !already_localized {
        let Some(src) = pick_source(record, replicas, consumer_node) else {
            return Err("no in-sync source for the backfill".into());
        };

        // §5 base for the pad: the OLDEST recorded epoch (≤ E_f) still
        // present on the destination — conservative by construction (the
        // stale-time back-off was lost at the flip; oldest can only
        // over-copy, never under-copy). None → thin-aware full build.
        let ef_seq = epoch_seq(volume_id, &ef)
            .ok_or_else(|| format!("marker {} is not an epoch of {}", ef, volume_id))?;
        let present = list_lvol_names(rpc, dst_node, dst_lvs).await?;
        let base: Option<String> = record
            .epochs
            .iter()
            .map(|e| e.name.clone())
            .filter(|n| epoch_seq(volume_id, n).map(|s| s < ef_seq).unwrap_or(false))
            .find(|n| present.contains(n));

        // Revert the pad onto the base (or empty) — the landing pad must be
        // write-virgin for the replay. Its lvol name is the stable replica
        // alias, so this is crash-idempotent exactly like the §5 revert.
        let pad_uuid = match &base {
            Some(b) => {
                let base_alias = format!("{}/{}", dst_lvs, b);
                revert_head(rpc, dst_node, &pad_alias, &identity.lvol_name, &base_alias).await?
            }
            None => {
                let src_ef_alias = format!("{}/{}", src.lvs_name, ef);
                let src_ef = get_bdev(rpc, &src.node_name, &src_ef_alias)
                    .await?
                    .ok_or_else(|| {
                        format!("E_f {} missing on source {}", src_ef_alias, src.node_name)
                    })?;
                let bytes = src_ef.get("num_blocks").and_then(|v| v.as_u64()).unwrap_or(0)
                    * src_ef.get("block_size").and_then(|v| v.as_u64()).unwrap_or(0);
                if bytes == 0 {
                    return Err("cannot size the landing pad: E_f reports no size".into());
                }
                revert_head_to_empty(
                    rpc,
                    dst_node,
                    &pad_alias,
                    &identity.lvol_name,
                    dst_lvs,
                    bytes.div_ceil(1024 * 1024),
                )
                .await?
            }
        };

        // Attach the pad on the source as the shallow-copy destination —
        // under its OWN export id: the replica export belongs to the live
        // head leg now.
        let conn = rpc
            .export_replica(
                dst_node,
                &pad_uuid,
                &pad_export_volume_id(volume_id, idx),
                &src.node_name,
            )
            .await?;
        let pad_ctrl = format!("nvme_{}", conn.nqn.replace(':', "_").replace('.', "_"));
        let pad_bdev = format!("{}n1", pad_ctrl);
        if let Some(b) = get_bdev(rpc, &src.node_name, &pad_bdev).await? {
            if b.get("uuid").and_then(|u| u.as_str()) != Some(pad_uuid.as_str()) {
                detach_controller(rpc, &src.node_name, &pad_ctrl).await;
            }
        }
        if get_bdev(rpc, &src.node_name, &pad_bdev).await?.is_none() {
            let attach = json!({
                "method": "bdev_nvme_attach_controller",
                "params": {
                    "name": pad_ctrl, "trtype": conn.transport.to_uppercase(),
                    "traddr": conn.target_ip, "trsvcid": conn.target_port.to_string(),
                    "subnqn": conn.nqn, "adrfam": "IPv4",
                    "hostnqn": flint_host_nqn(&src.node_name)
                }
            });
            rpc.spdk_rpc(&src.node_name, &attach)
                .await
                .map_err(|e| format!("pad attach on {}: {}", src.node_name, e))?;
        }

        // Base-inclusive replay to exactly E_f; the final align snapshots
        // the pad as the LOCAL E_f.
        copy_chain_to(
            rpc,
            volume_id,
            record,
            src,
            identity,
            &pad_alias,
            &pad_bdev,
            base.as_deref(),
            &ef,
            cfg.poll_interval,
            None,
        )
        .await?;

        // Re-root the esnap head onto the local E_f (strips the external
        // parent). "already parent" = a crash between set_parent and the
        // record write — converged.
        let set_parent = json!({ "method": "bdev_lvol_set_parent",
                                 "params": { "lvol_name": head_alias, "parent_name": ef_local_alias } });
        match rpc.spdk_rpc(dst_node, &set_parent).await {
            Ok(_) => {}
            Err(e) if is_already_exists(&e.to_string()) => {}
            Err(e) => return Err(format!("set_parent: {}", e).into()),
        }
    }

    // Cleanup, all missing-tolerant: the pad's copy transport, the pad
    // itself (now a redundant clone of the local E_f), and the E_f export
    // chain the esnap no longer needs.
    if let Some(src) = pick_source(record, replicas, consumer_node) {
        let pad_nqn = format!(
            "nqn.2024-11.com.flint:volume:{}",
            pad_export_volume_id(volume_id, idx)
        );
        let pad_ctrl = format!("nvme_{}", pad_nqn.replace(':', "_").replace('.', "_"));
        detach_controller(rpc, &src.node_name, &pad_ctrl).await;
        let _ = rpc
            .spdk_rpc(
                dst_node,
                &json!({ "method": "nvmf_delete_subsystem", "params": { "nqn": pad_nqn } }),
            )
            .await;
    }
    let _ = rpc
        .spdk_rpc(
            dst_node,
            &json!({ "method": "bdev_lvol_delete", "params": { "name": pad_alias } }),
        )
        .await;
    detach_controller(rpc, dst_node, &ef_controller_name(volume_id)).await;
    let nqn_ef = ef_export_nqn(volume_id);
    for r in record.replicas.iter().filter(|r| r.sync_state == SyncState::InSync) {
        if let Some(ri) = replicas.iter().find(|ri| ri.lvol_uuid == r.lvol_uuid) {
            let _ = rpc
                .spdk_rpc(
                    &ri.node_name,
                    &json!({ "method": "nvmf_delete_subsystem", "params": { "nqn": nqn_ef } }),
                )
                .await;
        }
    }

    // The head's chain reaches the local E_f and the leg has taken every
    // raid write since the add: fully in sync. mark_in_sync clears the
    // marker atomically with the state change.
    store.record_in_sync(volume_id, &rec.lvol_uuid, &ef).await?;
    // Localization lag (design item 5): how long the leg depended on the
    // remote E_f export — the new-in-kind SPOF exposure the eval flags.
    // `since` was stamped at the record flip.
    let exposure = rec
        .since
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        .filter(|secs| *secs >= 0)
        .map(|secs| format!(" after {}s of esnap exposure", secs))
        .unwrap_or_default();
    store
        .emit(
            volume_id,
            "Normal",
            "HotRejoinLocalized",
            &format!(
                "Hot-rejoined replica on {} localized its chain at {} — fully redundant{}",
                rec.node_name, ef, exposure
            ),
        )
        .await;
    info!(volume_id, node = %rec.node_name, ef = %ef, "[HOT_REJOIN] Localization complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// 7b-2: the trigger loop — plan_hot_rejoin + orchestrator (controller role)
// ---------------------------------------------------------------------------

/// PV annotation opting a volume OUT of automatic hot rejoin (Decision 1:
/// policy (B) auto-triggers on the no-opt-in class; this is the surgical
/// per-volume lever). Only the literal "disabled" (any case) opts out.
pub const HOT_REJOIN_ANNOTATION: &str = "flint.csi.storage.io/hot-rejoin";

#[derive(Debug, Clone)]
pub struct HotRejoinTriggerConfig {
    /// FLINT_HOT_REJOIN=enabled — default off. Turning this on is the
    /// operator's deliberate acceptance of the carried skip_rebuild patch;
    /// it is the blast-radius control Decision 1 leans on.
    pub enabled: bool,
    /// A standby may trail by at most this many epochs to be rejoined —
    /// same readiness bar as the cutover planner (the chase has converged;
    /// localization replays the least). FLINT_HOT_REJOIN_MAX_LAG, default 1.
    pub max_lag: u64,
    /// Wall-clock back-off after a FAILED (unwound) window before the
    /// volume is retried — every attempt costs the consumer a quiesce.
    /// FLINT_HOT_REJOIN_RETRY_SECS, default 300.
    pub retry_backoff: Duration,
}

impl Default for HotRejoinTriggerConfig {
    fn default() -> Self {
        HotRejoinTriggerConfig {
            enabled: false,
            max_lag: 1,
            retry_backoff: Duration::from_secs(300),
        }
    }
}

impl HotRejoinTriggerConfig {
    pub fn from_env() -> Self {
        let d = HotRejoinTriggerConfig::default();
        HotRejoinTriggerConfig {
            enabled: std::env::var("FLINT_HOT_REJOIN")
                .map(|v| {
                    v.eq_ignore_ascii_case("enabled")
                        || v.eq_ignore_ascii_case("true")
                        || v == "1"
                })
                .unwrap_or(d.enabled),
            max_lag: std::env::var("FLINT_HOT_REJOIN_MAX_LAG")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.max_lag),
            retry_backoff: std::env::var("FLINT_HOT_REJOIN_RETRY_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.retry_backoff),
        }
    }
}

/// Everything the planner needs about one volume, gathered by the tick.
#[derive(Debug, Clone)]
pub struct VolumeHotRejoinView {
    pub volume_id: String,
    pub record: VolumeSyncRecord,
    /// Node consuming the volume per its VolumeAttachment.
    pub consumer: Option<String>,
    /// The PV is RWX — Tier-1's NFS-pod bounce owns its reassembly.
    pub rwx: bool,
    /// The PV is the synthetic backing PV of an RWX volume. It is itself an
    /// attached multi-replica RWO PV that never opts into rejoin-bounce, so
    /// a literal policy (B) would hot-rejoin it — Decision 1's explicit
    /// exclusion: `plan_cutover` owns those bounces.
    pub nfs_backing: bool,
    /// `flint.csi.storage.io/rejoin-bounce` == enabled: the volume opted
    /// into the disruptive Tier-1 path — the two planners stay disjoint.
    pub rwo_bounce_enabled: bool,
    /// `flint.csi.storage.io/hot-rejoin` == "disabled" (the opt-out).
    pub hot_rejoin_disabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HotRejoinDecision {
    /// Open a window for this volume's rejoin target (the mechanism picks
    /// the same target `resolve` does: most-converged standby).
    Rejoin,
    /// Nothing to do; the reason is for operator-facing logs.
    Wait(&'static str),
}

/// Decide whether this volume gets a hot rejoin now — Decision 1, policy
/// (B): automatic for attached multi-replica RWO volumes that did NOT opt
/// into `rejoin-bounce`, with a per-PV `hot-rejoin: "disabled"` opt-out and
/// the synthetic RWX backing PV excluded. Pure; the tick owns the shared
/// per-volume claim and the retry back-off.
///
/// The trigger fires on a READY STANDBY (lag ≤ max_lag), not on a raw stale:
/// Tier-1's fenced chase stays the bulk-copy engine (no data-path impact
/// while it runs), and hot rejoin is the admission step that replaces the
/// reassembly the (B) class never gets. A cold stale rejoined directly would
/// serve most reads through the remote E_f export for the whole backfill —
/// a live read-latency regression the chase avoids for free.
pub fn plan_hot_rejoin(
    view: &VolumeHotRejoinView,
    cfg: &HotRejoinTriggerConfig,
) -> HotRejoinDecision {
    let vol = &view.volume_id;
    if view.nfs_backing {
        return HotRejoinDecision::Wait(
            "synthetic RWX backing PV — the cutover planner owns its bounce",
        );
    }
    if view.rwx {
        return HotRejoinDecision::Wait("RWX volume — the Tier-1 NFS bounce owns reassembly");
    }
    if view.hot_rejoin_disabled {
        return HotRejoinDecision::Wait("hot-rejoin disabled by PV annotation");
    }
    if view.rwo_bounce_enabled {
        return HotRejoinDecision::Wait(
            "volume opted into rejoin-bounce — the cutover planner owns it",
        );
    }
    if view.record.replicas.iter().any(|r| r.hot_rejoin.is_some()) {
        return HotRejoinDecision::Wait("hot rejoin in progress — the reconciler owns it");
    }
    if view.consumer.is_none() {
        return HotRejoinDecision::Wait(
            "volume not attached — the next stage admits standbys naturally",
        );
    }
    let latest = view.record.latest_epoch_seq(vol);
    if latest == 0 {
        return HotRejoinDecision::Wait("no epoch history");
    }
    // Mirror resolve()'s target choice so the decision and the mechanism
    // agree on which replica a Rejoin means.
    let standby = view
        .record
        .replicas
        .iter()
        .filter(|r| r.sync_state == SyncState::Standby)
        .max_by_key(|r| {
            r.last_epoch
                .as_deref()
                .and_then(|e| epoch_seq(vol, e))
                .unwrap_or(0)
        });
    match standby {
        Some(rec) => {
            let Some(seq) = rec.last_epoch.as_deref().and_then(|e| epoch_seq(vol, e)) else {
                return HotRejoinDecision::Wait("standby mark unreadable — not ready");
            };
            if latest.saturating_sub(seq) > cfg.max_lag {
                return HotRejoinDecision::Wait(
                    "standby lag above threshold — the chase has not converged",
                );
            }
            HotRejoinDecision::Rejoin
        }
        None => {
            if view.record.replicas.iter().any(|r| r.sync_state == SyncState::Stale) {
                return HotRejoinDecision::Wait(
                    "stale replica awaits the Tier-1 catch-up to standby (FLINT_CATCHUP)",
                );
            }
            HotRejoinDecision::Wait("no standby replica to rejoin")
        }
    }
}

/// Background trigger loop (controller role, default-disabled). Each
/// volume's rejoin (or marker reconcile) runs as its own task under the
/// shared per-volume claim — a long localization on one volume must not
/// stall another's two-second window.
pub async fn run_hot_rejoin_orchestrator(
    driver: std::sync::Arc<SpdkCsiDriver>,
    cfg: HotRejoinTriggerConfig,
) {
    info!(
        max_lag = cfg.max_lag,
        retry_backoff_secs = cfg.retry_backoff.as_secs(),
        "[HOT_REJOIN] Hot-rejoin orchestrator started"
    );
    let backoff: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>> =
        Default::default();
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Err(e) = hot_rejoin_tick(&driver, &cfg, &backoff).await {
            warn!(error = %e, "[HOT_REJOIN] Orchestrator tick failed (non-fatal)");
        }
    }
}

async fn hot_rejoin_tick(
    driver: &std::sync::Arc<SpdkCsiDriver>,
    cfg: &HotRejoinTriggerConfig,
    backoff: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>>,
) -> Result<(), RpcError> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    use k8s_openapi::api::storage::v1::VolumeAttachment;
    use kube::api::ListParams;
    use kube::Api;
    use std::collections::HashMap;

    let pvs: Api<PersistentVolume> = Api::all(driver.kube_client.clone());
    let vas: Api<VolumeAttachment> = Api::all(driver.kube_client.clone());

    let consumers: HashMap<String, String> = vas
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|va| va.status.as_ref().map(|s| s.attached).unwrap_or(false))
        .filter_map(|va| {
            va.spec
                .source
                .persistent_volume_name
                .map(|pv| (pv, va.spec.node_name))
        })
        .collect();

    for pv in pvs.list(&ListParams::default()).await?.items {
        let Some(volume_id) = pv.metadata.name.clone() else { continue };
        let is_flint = pv
            .spec
            .as_ref()
            .and_then(|s| s.csi.as_ref())
            .map(|c| c.driver == "flint.csi.storage.io")
            .unwrap_or(false);
        if !is_flint {
            continue;
        }
        let replicas = match crate::replica_sync::replicas_from_pv(&pv) {
            Ok(Some(r)) => r,
            Ok(None) => continue, // single replica
            Err(e) => {
                tracing::debug!(volume_id, error = %e, "[HOT_REJOIN] Skipping PV with unreadable replica info");
                continue;
            }
        };
        let Some(record) = pv
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crate::replica_sync::SYNC_STATE_ANNOTATION))
            .and_then(|s| VolumeSyncRecord::from_annotation(s).ok())
        else {
            continue;
        };

        let annotations = pv.metadata.annotations.as_ref();
        let view = VolumeHotRejoinView {
            volume_id: volume_id.clone(),
            consumer: consumers.get(&volume_id).cloned(),
            rwx: crate::replica_sync::is_rwx_pv(&pv),
            nfs_backing: crate::replica_sync::nfs_backing_parent(&pv).is_some(),
            rwo_bounce_enabled: annotations
                .and_then(|a| a.get(crate::cutover::REJOIN_BOUNCE_ANNOTATION))
                .map(|v| v.eq_ignore_ascii_case("enabled") || v == "true" || v == "1")
                .unwrap_or(false),
            hot_rejoin_disabled: annotations
                .and_then(|a| a.get(HOT_REJOIN_ANNOTATION))
                .map(|v| v.eq_ignore_ascii_case("disabled"))
                .unwrap_or(false),
            record,
        };

        // Marker present: dispatch the reconciler (resume localization,
        // adopt, scrub…) — this is what keeps a crashed rejoin converging
        // even when FLINT_CATCHUP is off. The catch-up orchestrator makes
        // the same dispatch; the shared claim keeps them from overlapping.
        // Skip the backing-PV/RWX classes: their records belong to Tier-1
        // streams that never set markers.
        if !view.nfs_backing
            && view.record.replicas.iter().any(|r| r.hot_rejoin.is_some())
        {
            let Some(claim) = crate::volume_claims::global()
                .try_claim(&volume_id, crate::volume_claims::OP_HOT_REJOIN)
            else {
                continue;
            };
            let driver = driver.clone();
            let view = view.clone();
            tokio::spawn(async move {
                let _claim = claim;
                let store = crate::catchup::KubeStore { client: driver.kube_client.clone() };
                reconcile_marked(
                    driver.as_ref(),
                    &store,
                    &view.volume_id,
                    &view.record,
                    &replicas,
                    view.consumer.as_deref(),
                    &CatchupConfig::from_env(),
                )
                .await;
            });
            continue;
        }

        match plan_hot_rejoin(&view, cfg) {
            HotRejoinDecision::Wait(reason) => {
                tracing::debug!(volume_id, reason, "[HOT_REJOIN] Waiting");
            }
            HotRejoinDecision::Rejoin => {
                // Back off after a failed window — every attempt costs the
                // consumer a quiesce.
                let recently_failed = backoff
                    .lock()
                    .expect("hot-rejoin backoff lock poisoned")
                    .get(&volume_id)
                    .map(|at| at.elapsed() < cfg.retry_backoff)
                    .unwrap_or(false);
                if recently_failed {
                    tracing::debug!(volume_id, "[HOT_REJOIN] In retry back-off — skipping");
                    continue;
                }
                let Some(claim) = crate::volume_claims::global()
                    .try_claim(&volume_id, crate::volume_claims::OP_HOT_REJOIN)
                else {
                    tracing::debug!(volume_id, "[HOT_REJOIN] Volume claimed by another operation — deferring");
                    continue;
                };
                let consumer = view
                    .consumer
                    .clone()
                    .expect("planner only says Rejoin for attached volumes");
                let driver = driver.clone();
                let backoff = backoff.clone();
                let mech_cfg = HotRejoinConfig::from_env();
                tokio::spawn(async move {
                    let _claim = claim;
                    let store = crate::catchup::KubeStore { client: driver.kube_client.clone() };
                    match hot_rejoin_volume(
                        driver.as_ref(),
                        &store,
                        &volume_id,
                        &replicas,
                        &consumer,
                        &mech_cfg,
                    )
                    .await
                    {
                        Ok(HotRejoinOutcome::Rejoined { window_ms, localized }) => {
                            info!(volume_id, window_ms, localized, "[HOT_REJOIN] Rejoin complete");
                        }
                        Ok(HotRejoinOutcome::NotEligible(reason)) => {
                            tracing::debug!(volume_id, reason, "[HOT_REJOIN] Not eligible after claim");
                        }
                        Err(e) => {
                            warn!(volume_id, error = %e, "[HOT_REJOIN] Rejoin failed (unwound) — backing off");
                            backoff
                                .lock()
                                .expect("hot-rejoin backoff lock poisoned")
                                .insert(volume_id.clone(), Instant::now());
                        }
                    }
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catchup::CatchupRpc;
    use crate::driver::NvmeofConnectionInfo;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // -- Fake world ---------------------------------------------------------

    #[derive(Default)]
    struct Sub {
        namespaces: Vec<(u64, String)>, // (nsid, bdev alias/name)
        hosts: Vec<String>,
        listener: bool,
    }

    #[derive(Default)]
    struct World {
        /// (node, bdev name) → bdev JSON. Lvol bdevs are stored under their
        /// alias `lvs/name`; nvme namespace bdevs under `{ctrl}n{nsid}`.
        bdevs: HashMap<(String, String), Value>,
        subsystems: HashMap<(String, String), Sub>,
        /// (node, controller name) → (target node, nqn).
        controllers: HashMap<(String, String), (String, String)>,
        raids: HashMap<String, Vec<Value>>,
        copy_states: Vec<String>,
        uuid_seq: u64,
    }

    impl World {
        fn next_uuid(&mut self) -> String {
            self.uuid_seq += 1;
            format!("uuid-{:04}", self.uuid_seq)
        }

        /// Re-derive every attached controller's namespace bdevs from the
        /// subsystem state — the fake's stand-in for AER-driven hot add.
        fn propagate_namespaces(&mut self) {
            let subs: Vec<((String, String), Vec<(u64, String)>)> = self
                .subsystems
                .iter()
                .map(|(k, s)| (k.clone(), s.namespaces.clone()))
                .collect();
            let ctrls: Vec<((String, String), (String, String))> = self
                .controllers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            // Drop stale namespace bdevs of every controller, then re-add.
            for ((node, ctrl), _) in &ctrls {
                let prefix = format!("{}n", ctrl);
                self.bdevs.retain(|(n, name), _| {
                    !(n == node
                        && name.starts_with(&prefix)
                        && name[prefix.len()..].bytes().all(|b| b.is_ascii_digit()))
                });
            }
            for ((node, ctrl), (target, nqn)) in &ctrls {
                for ((sub_node, sub_nqn), namespaces) in &subs {
                    if sub_node == target && sub_nqn == nqn {
                        for (nsid, backing) in namespaces {
                            let uuid = self
                                .bdevs
                                .get(&(sub_node.clone(), backing.clone()))
                                .and_then(|b| b.get("uuid").and_then(|u| u.as_str()))
                                .unwrap_or("uuid-unknown")
                                .to_string();
                            self.bdevs.insert(
                                (node.clone(), format!("{}n{}", ctrl, nsid)),
                                json!({ "name": format!("{}n{}", ctrl, nsid), "uuid": uuid,
                                        "num_blocks": 2048, "block_size": 4096 }),
                            );
                        }
                    }
                }
            }
        }
    }

    struct FakeRpc {
        world: Mutex<World>,
        calls: Mutex<Vec<(String, String, Value)>>,
        /// (node, method) → queue of injected results; None = pass through.
        fail_seq: Mutex<HashMap<(String, String), Vec<Option<String>>>>,
    }

    impl FakeRpc {
        fn new() -> Self {
            FakeRpc {
                world: Mutex::new(World::default()),
                calls: Mutex::new(Vec::new()),
                fail_seq: Mutex::new(HashMap::new()),
            }
        }

        fn fail(&self, node: &str, method: &str, msg: &str) {
            self.fail_seq
                .lock()
                .unwrap()
                .entry((node.into(), method.into()))
                .or_default()
                .push(Some(msg.into()));
        }

        fn fail_then_ok(&self, node: &str, method: &str, msg: &str) {
            let mut m = self.fail_seq.lock().unwrap();
            let q = m.entry((node.into(), method.into())).or_default();
            q.push(Some(msg.into()));
            q.push(None);
        }

        fn seed_lvol(&self, node: &str, lvs: &str, name: &str, uuid: &str) {
            self.seed_lvol_with_parent(node, lvs, name, uuid, None);
        }

        fn seed_lvol_with_parent(
            &self,
            node: &str,
            lvs: &str,
            name: &str,
            uuid: &str,
            parent: Option<&str>,
        ) {
            let mut w = self.world.lock().unwrap();
            let mut lvol = json!({ "snapshot": false });
            if let Some(p) = parent {
                lvol["base_snapshot"] = json!(p);
            }
            w.bdevs.insert(
                (node.into(), format!("{}/{}", lvs, name)),
                json!({ "name": format!("{}/{}", lvs, name), "uuid": uuid,
                        "num_blocks": 2048, "block_size": 4096,
                        "driver_specific": { "lvol": lvol } }),
            );
        }

        fn seed_raid(&self, node: &str, name: &str, state: &str, bases: &[(&str, bool)]) {
            let mut w = self.world.lock().unwrap();
            let list: Vec<Value> = bases
                .iter()
                .map(|(n, c)| json!({ "name": n, "is_configured": c }))
                .collect();
            w.raids.entry(node.into()).or_default().push(json!({
                "name": name, "state": state, "base_bdevs_list": list
            }));
        }

        fn calls_of(&self, method: &str) -> Vec<(String, Value)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, m, _)| m == method)
                .map(|(n, _, p)| (n.clone(), p.clone()))
                .collect()
        }

        fn methods_in_order(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|(_, m, _)| m.clone()).collect()
        }

        fn has_bdev(&self, node: &str, name: &str) -> bool {
            self.world
                .lock()
                .unwrap()
                .bdevs
                .contains_key(&(node.into(), name.into()))
        }
    }

    #[async_trait]
    impl CatchupRpc for FakeRpc {
        async fn spdk_rpc(&self, node: &str, payload: &Value) -> Result<Value, RpcError> {
            let method = payload.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
            let params = payload.get("params").cloned().unwrap_or(json!({}));
            self.calls
                .lock()
                .unwrap()
                .push((node.to_string(), method.clone(), params.clone()));

            if let Some(q) = self
                .fail_seq
                .lock()
                .unwrap()
                .get_mut(&(node.to_string(), method.clone()))
            {
                if !q.is_empty() {
                    if let Some(msg) = q.remove(0) {
                        return Err(msg.into());
                    }
                }
            }

            let mut w = self.world.lock().unwrap();
            let node_s = node.to_string();
            let resp = match method.as_str() {
                "bdev_raid_quiesce" | "bdev_raid_unquiesce" | "bdev_wait_for_examine"
                | "bdev_examine" => json!({ "result": true }),
                "bdev_raid_get_bdevs" => {
                    json!({ "result": w.raids.get(&node_s).cloned().unwrap_or_default() })
                }
                "bdev_raid_add_base_bdev" => {
                    let raid = params["raid_bdev"].as_str().unwrap().to_string();
                    let base = params["base_bdev"].as_str().unwrap().to_string();
                    if let Some(raids) = w.raids.get_mut(&node_s) {
                        for r in raids.iter_mut() {
                            if r["name"].as_str() == Some(raid.as_str()) {
                                r["base_bdevs_list"]
                                    .as_array_mut()
                                    .unwrap()
                                    .push(json!({ "name": base, "is_configured": true }));
                            }
                        }
                    }
                    json!({ "result": true })
                }
                "bdev_get_bdevs" => {
                    let name = params["name"].as_str().unwrap_or("");
                    match w.bdevs.get(&(node_s.clone(), name.to_string())) {
                        Some(b) => json!({ "result": [b] }),
                        None => return Err(format!("bdev {} not found: No such device", name).into()),
                    }
                }
                "bdev_lvol_get_lvols" => {
                    let lvs = params["lvs_name"].as_str().unwrap_or("");
                    let prefix = format!("{}/", lvs);
                    let names: Vec<Value> = w
                        .bdevs
                        .iter()
                        .filter(|((n, name), _)| *n == node_s && name.starts_with(&prefix))
                        .map(|((_, name), b)| {
                            json!({ "name": name[prefix.len()..],
                                    "uuid": b.get("uuid").cloned().unwrap_or(json!("")),
                                    "alias": name })
                        })
                        .collect();
                    json!({ "result": names })
                }
                "bdev_lvol_snapshot" => {
                    let lvol = params["lvol_name"].as_str().unwrap().to_string();
                    let snap = params["snapshot_name"].as_str().unwrap().to_string();
                    let lvs = lvol.split('/').next().unwrap().to_string();
                    let alias = format!("{}/{}", lvs, snap);
                    if w.bdevs.contains_key(&(node_s.clone(), alias.clone())) {
                        return Err(format!("snapshot {} already exists", snap).into());
                    }
                    // Blobstore chain insertion: the snapshot takes the
                    // head's old parent; the head re-roots onto the
                    // snapshot (what lineage_chain walks).
                    let old_parent = w
                        .bdevs
                        .get_mut(&(node_s.clone(), lvol.clone()))
                        .and_then(|b| {
                            let l = &mut b["driver_specific"]["lvol"];
                            let old = l
                                .get("base_snapshot")
                                .and_then(|s| s.as_str())
                                .map(String::from);
                            l["base_snapshot"] = json!(snap);
                            old
                        });
                    let uuid = w.next_uuid();
                    let mut snap_lvol = json!({ "snapshot": true });
                    if let Some(p) = old_parent {
                        snap_lvol["base_snapshot"] = json!(p);
                    }
                    w.bdevs.insert(
                        (node_s.clone(), alias.clone()),
                        json!({ "name": alias, "uuid": uuid, "num_blocks": 2048, "block_size": 4096,
                                "driver_specific": { "lvol": snap_lvol } }),
                    );
                    json!({ "result": uuid })
                }
                "bdev_lvol_clone_bdev" => {
                    let esnap = params["bdev"].as_str().unwrap().to_string();
                    let lvs = params["lvs_name"].as_str().unwrap().to_string();
                    let name = params["clone_name"].as_str().unwrap().to_string();
                    let uuid = w.next_uuid();
                    let alias = format!("{}/{}", lvs, name);
                    w.bdevs.insert(
                        (node_s.clone(), alias.clone()),
                        json!({ "name": alias, "uuid": uuid, "num_blocks": 2048, "block_size": 4096,
                                "driver_specific": { "lvol": { "esnap_clone": true,
                                                                "external_snapshot_name": esnap } } }),
                    );
                    json!({ "result": uuid })
                }
                "bdev_lvol_clone" => {
                    let snap = params["snapshot_name"].as_str().unwrap().to_string();
                    let name = params["clone_name"].as_str().unwrap().to_string();
                    let lvs = snap.split('/').next().unwrap().to_string();
                    let uuid = w.next_uuid();
                    let alias = format!("{}/{}", lvs, name);
                    let parent_short = snap.split('/').nth(1).unwrap_or(&snap).to_string();
                    w.bdevs.insert(
                        (node_s.clone(), alias.clone()),
                        json!({ "name": alias, "uuid": uuid, "num_blocks": 2048, "block_size": 4096,
                                "driver_specific": { "lvol": { "base_snapshot": parent_short } } }),
                    );
                    json!({ "result": uuid })
                }
                "bdev_lvol_delete" => {
                    let name = params["name"].as_str().unwrap().to_string();
                    if w.bdevs.remove(&(node_s.clone(), name.clone())).is_none() {
                        return Err(format!("lvol {} not found: No such device", name).into());
                    }
                    json!({ "result": true })
                }
                "bdev_lvol_set_parent" => {
                    let lvol = params["lvol_name"].as_str().unwrap().to_string();
                    let parent = params["parent_name"].as_str().unwrap().to_string();
                    let parent_short = parent.split('/').nth(1).unwrap_or(&parent).to_string();
                    match w.bdevs.get_mut(&(node_s.clone(), lvol.clone())) {
                        Some(b) => {
                            let l = &mut b["driver_specific"]["lvol"];
                            l.as_object_mut().unwrap().remove("esnap_clone");
                            l.as_object_mut().unwrap().remove("external_snapshot_name");
                            l["base_snapshot"] = json!(parent_short);
                            json!({ "result": true })
                        }
                        None => return Err(format!("lvol {} not found", lvol).into()),
                    }
                }
                "bdev_lvol_start_shallow_copy" => json!({ "result": { "operation_id": 1 } }),
                "bdev_lvol_check_shallow_copy" => {
                    let state = if w.copy_states.is_empty() {
                        "complete".to_string()
                    } else {
                        w.copy_states.remove(0)
                    };
                    json!({ "result": { "state": state } })
                }
                "nvmf_create_subsystem" => {
                    let nqn = params["nqn"].as_str().unwrap().to_string();
                    w.subsystems.entry((node_s.clone(), nqn)).or_default();
                    json!({ "result": true })
                }
                "nvmf_delete_subsystem" => {
                    let nqn = params["nqn"].as_str().unwrap().to_string();
                    if w.subsystems.remove(&(node_s.clone(), nqn.clone())).is_none() {
                        return Err(format!("subsystem {} not found: No such device", nqn).into());
                    }
                    w.propagate_namespaces();
                    json!({ "result": true })
                }
                "nvmf_subsystem_add_host" => {
                    let nqn = params["nqn"].as_str().unwrap().to_string();
                    let host = params["host"].as_str().unwrap().to_string();
                    w.subsystems
                        .entry((node_s.clone(), nqn))
                        .or_default()
                        .hosts
                        .push(host);
                    json!({ "result": true })
                }
                "nvmf_subsystem_add_listener" => {
                    let nqn = params["nqn"].as_str().unwrap().to_string();
                    w.subsystems.entry((node_s.clone(), nqn)).or_default().listener = true;
                    json!({ "result": true })
                }
                "nvmf_subsystem_add_ns" => {
                    let nqn = params["nqn"].as_str().unwrap().to_string();
                    let bdev = params["namespace"]["bdev_name"].as_str().unwrap().to_string();
                    let nsid = params["namespace"]["nsid"].as_u64().unwrap_or(1);
                    w.subsystems
                        .entry((node_s.clone(), nqn))
                        .or_default()
                        .namespaces
                        .push((nsid, bdev));
                    w.propagate_namespaces();
                    json!({ "result": nsid })
                }
                "nvmf_subsystem_remove_ns" => {
                    let nqn = params["nqn"].as_str().unwrap().to_string();
                    let nsid = params["nsid"].as_u64().unwrap_or(1);
                    if let Some(s) = w.subsystems.get_mut(&(node_s.clone(), nqn)) {
                        s.namespaces.retain(|(id, _)| *id != nsid);
                    }
                    w.propagate_namespaces();
                    json!({ "result": true })
                }
                "nvmf_get_subsystems" => {
                    let subs: Vec<Value> = w
                        .subsystems
                        .iter()
                        .filter(|((n, _), _)| *n == node_s)
                        .map(|((_, nqn), s)| {
                            let nss: Vec<Value> = s
                                .namespaces
                                .iter()
                                .map(|(id, b)| json!({ "nsid": id, "bdev_name": b }))
                                .collect();
                            json!({ "nqn": nqn, "namespaces": nss })
                        })
                        .collect();
                    json!({ "result": subs })
                }
                "bdev_nvme_attach_controller" => {
                    let name = params["name"].as_str().unwrap().to_string();
                    let nqn = params["subnqn"].as_str().unwrap().to_string();
                    // The fake routes by nqn: find the node hosting it.
                    let target = w
                        .subsystems
                        .keys()
                        .find(|(_, n)| *n == nqn)
                        .map(|(host, _)| host.clone())
                        .unwrap_or_else(|| "nowhere".to_string());
                    w.controllers.insert((node_s.clone(), name), (target, nqn));
                    w.propagate_namespaces();
                    json!({ "result": ["ok"] })
                }
                "bdev_nvme_detach_controller" => {
                    let name = params["name"].as_str().unwrap().to_string();
                    w.controllers.remove(&(node_s.clone(), name));
                    w.propagate_namespaces();
                    json!({ "result": true })
                }
                "bdev_nvme_get_controllers" => {
                    let name = params["name"].as_str().unwrap_or("").to_string();
                    let found = w.controllers.contains_key(&(node_s.clone(), name.clone()));
                    if found {
                        json!({ "result": [{ "name": name }] })
                    } else {
                        json!({ "result": [] })
                    }
                }
                other => return Err(format!("fake: unhandled method {}", other).into()),
            };
            Ok(resp)
        }

        async fn export_replica(
            &self,
            node: &str,
            bdev_name: &str,
            export_volume_id: &str,
            consumer_node: &str,
        ) -> Result<NvmeofConnectionInfo, RpcError> {
            let nqn = format!("nqn.2024-11.com.flint:volume:{}", export_volume_id);
            self.calls.lock().unwrap().push((
                node.to_string(),
                "export_replica".to_string(),
                json!({ "bdev": bdev_name, "id": export_volume_id, "consumer": consumer_node }),
            ));
            let mut w = self.world.lock().unwrap();
            // Convergent: the namespace is (re)pointed at bdev_name. The
            // fake resolves a uuid to its alias so propagation can find it.
            let alias = if bdev_name.contains('/') {
                bdev_name.to_string()
            } else {
                w.bdevs
                    .iter()
                    .find(|((n, _), b)| {
                        *n == node && b.get("uuid").and_then(|u| u.as_str()) == Some(bdev_name)
                    })
                    .map(|((_, name), _)| name.clone())
                    .unwrap_or_else(|| bdev_name.to_string())
            };
            let sub = w.subsystems.entry((node.to_string(), nqn.clone())).or_default();
            sub.listener = true;
            sub.namespaces = vec![(1, alias)];
            w.propagate_namespaces();
            Ok(NvmeofConnectionInfo {
                nqn,
                target_ip: format!("10.0.0.{}", node.len()),
                target_port: 4420,
                transport: "tcp".to_string(),
            })
        }
    }

    #[async_trait]
    impl HotRejoinRpc for FakeRpc {
        async fn node_ip(&self, node: &str) -> Result<String, RpcError> {
            Ok(format!("10.0.0.{}", node.len()))
        }
    }

    // -- Fake store ----------------------------------------------------------

    struct FakeStore {
        record: Mutex<VolumeSyncRecord>,
        ops: Mutex<Vec<String>>,
        events: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn new(record: VolumeSyncRecord) -> Self {
            FakeStore {
                record: Mutex::new(record),
                ops: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
            }
        }
        fn record(&self) -> VolumeSyncRecord {
            self.record.lock().unwrap().clone()
        }
        fn ops(&self) -> Vec<String> {
            self.ops.lock().unwrap().clone()
        }
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    const NOW: &str = "2026-07-01T00:00:00Z";

    #[async_trait]
    impl CatchupStore for FakeStore {
        async fn load(&self, _volume_id: &str) -> Result<Option<VolumeSyncRecord>, RpcError> {
            Ok(Some(self.record()))
        }
        async fn pin_retention(&self, _v: &str, epoch: &str) -> Result<(), RpcError> {
            self.ops.lock().unwrap().push(format!("pin:{}", epoch));
            Ok(())
        }
        async fn record_revert(
            &self,
            _v: &str,
            replica_uuid: &str,
            base_epoch: &str,
            new_head_uuid: &str,
        ) -> Result<(), RpcError> {
            self.ops
                .lock()
                .unwrap()
                .push(format!("revert:{}:{}", base_epoch, new_head_uuid));
            let mut r = self.record.lock().unwrap();
            if let Some(rec) = r.replicas.iter_mut().find(|rec| rec.lvol_uuid == replica_uuid) {
                rec.active_lvol_uuid = Some(new_head_uuid.to_string());
                rec.reverted_to = Some(base_epoch.to_string());
                rec.hot_rejoin = None;
            }
            Ok(())
        }
        async fn record_standby(
            &self,
            _v: &str,
            replica_uuid: &str,
            caught_up_through: &str,
        ) -> Result<(), RpcError> {
            self.ops.lock().unwrap().push(format!("standby:{}", caught_up_through));
            self.record
                .lock()
                .unwrap()
                .mark_standby(replica_uuid, caught_up_through, "test", NOW);
            Ok(())
        }
        async fn record_reason(&self, _v: &str, _u: &str, reason: &str) -> Result<(), RpcError> {
            self.ops.lock().unwrap().push(format!("reason:{}", reason));
            Ok(())
        }
        async fn record_epoch_cut(
            &self,
            _v: &str,
            epoch: &str,
            cut_uuids: &[String],
        ) -> Result<(), RpcError> {
            self.ops.lock().unwrap().push(format!("cut:{}", epoch));
            self.record.lock().unwrap().apply_epoch_cut(epoch, cut_uuids, NOW);
            Ok(())
        }
        async fn record_in_sync(
            &self,
            _v: &str,
            replica_uuid: &str,
            last_epoch: &str,
        ) -> Result<(), RpcError> {
            self.ops.lock().unwrap().push(format!("in_sync:{}", last_epoch));
            self.record
                .lock()
                .unwrap()
                .mark_in_sync(replica_uuid, last_epoch, "test", NOW);
            Ok(())
        }
        async fn clear_snapshot_tombstone(&self, _v: &str, name: &str) -> Result<(), RpcError> {
            self.ops.lock().unwrap().push(format!("untomb:{}", name));
            Ok(())
        }
        async fn emit(&self, _v: &str, _t: &str, reason: &str, _m: &str) {
            self.events.lock().unwrap().push(reason.to_string());
        }
        async fn record_hot_rejoin_intent(
            &self,
            _v: &str,
            replica_uuid: &str,
            ef_epoch: &str,
        ) -> Result<(), RpcError> {
            self.ops.lock().unwrap().push(format!("hr_intent:{}", ef_epoch));
            self.record
                .lock()
                .unwrap()
                .mark_hot_rejoin_intent(replica_uuid, ef_epoch, NOW);
            Ok(())
        }
        async fn record_hot_rejoin_flip(
            &self,
            _v: &str,
            replica_uuid: &str,
            ef_epoch: &str,
            cut_uuids: &[String],
            head_uuid: &str,
        ) -> Result<(), RpcError> {
            self.ops
                .lock()
                .unwrap()
                .push(format!("hr_flip:{}:{}", ef_epoch, head_uuid));
            self.record.lock().unwrap().mark_hot_rejoined(
                replica_uuid,
                ef_epoch,
                cut_uuids,
                head_uuid,
                NOW,
            );
            Ok(())
        }
        async fn record_hot_rejoin_cleared(
            &self,
            _v: &str,
            replica_uuid: &str,
            reason: &str,
            demote_to_stale: bool,
        ) -> Result<(), RpcError> {
            self.ops
                .lock()
                .unwrap()
                .push(format!("hr_clear:{}", demote_to_stale));
            self.record
                .lock()
                .unwrap()
                .clear_hot_rejoin(replica_uuid, reason, demote_to_stale, NOW);
            Ok(())
        }
    }

    // -- Fixtures ------------------------------------------------------------

    const VOL: &str = "vol1";

    fn replica(node: &str, uuid: &str) -> ReplicaInfo {
        ReplicaInfo {
            node_name: node.to_string(),
            node_uid: format!("uid-{}", node),
            disk_pci_address: "0000:00:04.0".to_string(),
            lvol_uuid: uuid.to_string(),
            lvol_name: format!("vol_{}_replica_{}", VOL, if uuid.ends_with('a') { 0 } else { 1 }),
            lvs_name: format!("lvs_{}", node),
            nqn: None,
            target_ip: None,
            target_port: None,
            health: "online".to_string(),
        }
    }

    fn replicas2() -> Vec<ReplicaInfo> {
        vec![replica("node-a", "uuid-a"), replica("node-b", "uuid-b")]
    }

    /// Record: replica a in_sync, replica b stale; epochs 1..=2 recorded.
    fn stale_b_record() -> VolumeSyncRecord {
        let mut r = VolumeSyncRecord::initial(&replicas2());
        r.apply_epoch_cut(&epoch_name(VOL, 1), &["uuid-a".into(), "uuid-b".into()], NOW);
        r.apply_epoch_cut(&epoch_name(VOL, 2), &["uuid-a".into(), "uuid-b".into()], NOW);
        r.mark_stale("uuid-b", "leg failed", NOW);
        r
    }

    fn cfg() -> HotRejoinConfig {
        HotRejoinConfig {
            lease_ms: 5000,
            aer_wait: Duration::from_millis(100),
            aer_poll: Duration::ZERO,
            add_retries: 2,
            poll_interval: Duration::ZERO,
            window_target: Duration::from_secs(2),
        }
    }

    fn catchup_cfg() -> CatchupConfig {
        let mut c = CatchupConfig::default();
        c.poll_interval = Duration::ZERO;
        c
    }

    /// A fully staged world: survivor head + epochs on node-a, pad + epoch-1
    /// on node-b, online raid on the consumer with the survivor leg.
    fn staged_world(rpc: &FakeRpc) {
        // Survivor head + its epoch snapshots.
        rpc.seed_lvol("node-a", "lvs_node-a", &format!("vol_{}_replica_0", VOL), "uuid-a");
        rpc.seed_lvol_with_parent(
            "node-a", "lvs_node-a", &epoch_name(VOL, 1), "uuid-ep1", None,
        );
        rpc.seed_lvol_with_parent(
            "node-a", "lvs_node-a", &epoch_name(VOL, 2), "uuid-ep2", None,
        );
        // Chain: head → ep2 → ep1 (parent links walked by lineage_chain).
        {
            let mut w = rpc.world.lock().unwrap();
            let head = w
                .bdevs
                .get_mut(&("node-a".into(), format!("lvs_node-a/vol_{}_replica_0", VOL)))
                .unwrap();
            head["driver_specific"]["lvol"]["base_snapshot"] = json!(epoch_name(VOL, 2));
            let ep2 = w
                .bdevs
                .get_mut(&("node-a".into(), format!("lvs_node-a/{}", epoch_name(VOL, 2))))
                .unwrap();
            ep2["driver_specific"]["lvol"]["base_snapshot"] = json!(epoch_name(VOL, 1));
        }
        // The stale pad + its copy of epoch-1 on node-b.
        rpc.seed_lvol("node-b", "lvs_node-b", &format!("vol_{}_replica_1", VOL), "uuid-b");
        rpc.seed_lvol("node-b", "lvs_node-b", &epoch_name(VOL, 1), "uuid-ep1b");
        // Online raid on the consumer, survivor leg configured.
        rpc.seed_raid(
            "consumer",
            &format!("raid_{}", VOL),
            "online",
            &[(&expected_remote_base_bdev(VOL, 0), true)],
        );
    }

    // -- Window tests ---------------------------------------------------------

    #[tokio::test]
    async fn full_window_flips_and_localizes() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        let store = FakeStore::new(stale_b_record());

        let out = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap();
        match out {
            HotRejoinOutcome::Rejoined { localized, .. } => assert!(localized),
            other => panic!("expected Rejoined, got {:?}", other),
        }

        // Window RPC order on the load-bearing steps.
        let methods = rpc.methods_in_order();
        let idx = |m: &str| methods.iter().position(|x| x == m).unwrap_or(usize::MAX);
        assert!(idx("bdev_raid_quiesce") < idx("bdev_lvol_snapshot"), "quiesce before E_f cut");
        assert!(idx("bdev_lvol_snapshot") < idx("bdev_lvol_clone_bdev"), "cut before clone");
        assert!(idx("bdev_lvol_clone_bdev") < idx("bdev_raid_add_base_bdev"), "clone before add");
        assert!(idx("bdev_raid_add_base_bdev") < idx("bdev_raid_unquiesce"), "add before release");

        // Renew-before-add invariant: two quiesce calls, the second after
        // the ns swap and before the add.
        let quiesces: Vec<usize> = methods
            .iter()
            .enumerate()
            .filter(|(_, m)| *m == "bdev_raid_quiesce")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(quiesces.len(), 2);
        assert!(quiesces[1] < idx("bdev_raid_add_base_bdev"));

        // The add used the standard replica bdev name with skip_rebuild.
        let adds = rpc.calls_of("bdev_raid_add_base_bdev");
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].1["base_bdev"].as_str().unwrap(), expected_remote_base_bdev(VOL, 1));
        assert_eq!(adds[0].1["skip_rebuild"].as_bool(), Some(true));

        // End state: in_sync, marker cleared, E_f recorded, head is live.
        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::InSync);
        assert!(b.hot_rejoin.is_none());
        assert_eq!(b.last_epoch.as_deref(), Some(epoch_name(VOL, 3).as_str()));
        assert!(rec.epochs.iter().any(|e| e.name == epoch_name(VOL, 3)));
        assert!(b.active_lvol_uuid.is_some());

        // Localization disposed of the pad and re-rooted the head locally.
        assert!(!rpc.has_bdev("node-b", &format!("lvs_node-b/vol_{}_replica_1", VOL)));
        let w = rpc.world.lock().unwrap();
        let head = w
            .bdevs
            .get(&("node-b".into(), format!("lvs_node-b/vol_{}_replica_1_hr", VOL)))
            .expect("head kept");
        assert_eq!(
            head["driver_specific"]["lvol"]["base_snapshot"].as_str(),
            Some(epoch_name(VOL, 3).as_str())
        );
        drop(w);

        assert_eq!(store.events(), vec!["HotRejoinSucceeded", "HotRejoinLocalized"]);
    }

    #[tokio::test]
    async fn ef_collision_unwinds_without_adding() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // The scheduler wins the race to epoch-3 on the survivor mid-window:
        // seed the name AFTER the pre-pick would have seen it clean — the
        // fake's snapshot handler refuses duplicates, so pre-seed it and
        // fail the pre-pick's step-over by seeding only after resolve.
        // Simplest deterministic injection: fail the snapshot RPC itself.
        rpc.fail(
            "node-a",
            "bdev_lvol_snapshot",
            "snapshot epoch-vol1-3 already exists",
        );
        let store = FakeStore::new(stale_b_record());

        let err = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("collision"), "unexpected: {}", err);

        // Never added, always released, marker cleared, state still stale.
        assert!(rpc.calls_of("bdev_raid_add_base_bdev").is_empty());
        assert!(!rpc.calls_of("bdev_raid_unquiesce").is_empty());
        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Stale);
        assert!(b.hot_rejoin.is_none());
        assert_eq!(store.events(), vec!["HotRejoinUnwound"]);
    }

    #[tokio::test]
    async fn add_failure_unwinds_ladder() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        rpc.fail("consumer", "bdev_raid_add_base_bdev", "Code=-1 add refused");
        let store = FakeStore::new(stale_b_record());

        let err = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("skip_rebuild add"), "unexpected: {}", err);

        // Head deleted, E_f snapshots reaped, pad namespace restored,
        // released.
        assert!(!rpc.has_bdev("node-b", &format!("lvs_node-b/vol_{}_replica_1_hr", VOL)));
        assert!(!rpc.has_bdev("node-a", &format!("lvs_node-a/{}", epoch_name(VOL, 3))));
        let w = rpc.world.lock().unwrap();
        let sub = w
            .subsystems
            .get(&("node-b".into(), replica_export_nqn(VOL, 1)))
            .expect("replica export kept");
        assert_eq!(sub.namespaces.len(), 1);
        assert!(sub.namespaces[0].1.contains("vol_vol1_replica_1"), "pad ns restored");
        drop(w);
        assert!(!rpc.calls_of("bdev_raid_unquiesce").is_empty());
        let b = store.record();
        assert_eq!(b.get("uuid-b").unwrap().sync_state, SyncState::Stale);
        assert!(b.get("uuid-b").unwrap().hot_rejoin.is_none());
    }

    #[tokio::test]
    async fn renew_failure_never_adds() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // First quiesce passes, the renew fails (lease lost mid-window).
        rpc.fail_seq
            .lock()
            .unwrap()
            .insert(
                ("consumer".into(), "bdev_raid_quiesce".into()),
                vec![None, Some("lease expired".into())],
            );
        let store = FakeStore::new(stale_b_record());

        let err = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("never add"), "unexpected: {}", err);
        assert!(rpc.calls_of("bdev_raid_add_base_bdev").is_empty());
    }

    #[tokio::test]
    async fn add_ebusy_retries_then_succeeds() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        rpc.fail_then_ok("consumer", "bdev_raid_add_base_bdev", "Code=-16 EBUSY release in flight");
        let store = FakeStore::new(stale_b_record());

        let out = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap();
        assert!(matches!(out, HotRejoinOutcome::Rejoined { .. }));
        assert_eq!(rpc.calls_of("bdev_raid_add_base_bdev").len(), 2);
    }

    #[tokio::test]
    async fn unquiesce_enoent_is_commit() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        rpc.fail(
            "consumer",
            "bdev_raid_unquiesce",
            "Code=-2 no quiesce lease held on raid bdev raid_vol1",
        );
        let store = FakeStore::new(stale_b_record());

        let out = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap();
        assert!(matches!(out, HotRejoinOutcome::Rejoined { .. }));
        assert!(store.ops().iter().any(|o| o.starts_with("hr_flip:")));
    }

    // -- Reconciler tests ------------------------------------------------------

    #[tokio::test]
    async fn reconcile_adopts_committed_window() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // Reality: the head exists AND its leg is configured in the raid
        // (the add committed), but the record still says stale + marker
        // (flip lost to a crash).
        rpc.seed_lvol("node-b", "lvs_node-b", &format!("vol_{}_replica_1_hr", VOL), "uuid-head");
        {
            let mut w = rpc.world.lock().unwrap();
            let raids = w.raids.get_mut("consumer").unwrap();
            raids[0]["base_bdevs_list"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "name": expected_remote_base_bdev(VOL, 1), "is_configured": true }));
            w.bdevs.insert(
                ("consumer".into(), expected_remote_base_bdev(VOL, 1)),
                json!({ "name": expected_remote_base_bdev(VOL, 1), "uuid": "uuid-head" }),
            );
        }
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        let store = FakeStore::new(record.clone());

        reconcile_marked(
            &rpc, &store, VOL, &record, &replicas2(), Some("consumer"), &catchup_cfg(),
        )
        .await;

        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Standby);
        assert_eq!(b.hot_rejoin.as_deref(), Some(epoch_name(VOL, 3).as_str()));
        assert_eq!(b.active_lvol_uuid.as_deref(), Some("uuid-head"));
        assert!(rec.epochs.iter().any(|e| e.name == epoch_name(VOL, 3)));
        assert_eq!(store.events(), vec!["HotRejoinAdopted"]);
    }

    #[tokio::test]
    async fn reconcile_scrubs_uncommitted_window() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // Strandings: head clone, E_f export subsystem, unrecorded E_f cut.
        rpc.seed_lvol("node-b", "lvs_node-b", &format!("vol_{}_replica_1_hr", VOL), "uuid-head");
        rpc.seed_lvol("node-a", "lvs_node-a", &epoch_name(VOL, 3), "uuid-ef");
        {
            let mut w = rpc.world.lock().unwrap();
            w.subsystems
                .entry(("node-a".into(), ef_export_nqn(VOL)))
                .or_default();
        }
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        let store = FakeStore::new(record.clone());

        reconcile_marked(
            &rpc, &store, VOL, &record, &replicas2(), Some("consumer"), &catchup_cfg(),
        )
        .await;

        assert!(!rpc.has_bdev("node-b", &format!("lvs_node-b/vol_{}_replica_1_hr", VOL)));
        assert!(!rpc.has_bdev("node-a", &format!("lvs_node-a/{}", epoch_name(VOL, 3))));
        assert!(!rpc
            .world
            .lock()
            .unwrap()
            .subsystems
            .contains_key(&("node-a".into(), ef_export_nqn(VOL))));
        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Stale);
        assert!(b.hot_rejoin.is_none());
        assert_eq!(store.events(), vec!["HotRejoinScrubbed"]);
    }

    #[tokio::test]
    async fn reconcile_localizes_marked_standby() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // Post-flip world: E_f cut on the survivor, head esnap-cloned and
        // serving as the configured leg.
        rpc.seed_lvol("node-a", "lvs_node-a", &epoch_name(VOL, 3), "uuid-ef");
        rpc.seed_lvol_with_parent(
            "node-b", "lvs_node-b", &format!("vol_{}_replica_1_hr", VOL), "uuid-head", None,
        );
        {
            let mut w = rpc.world.lock().unwrap();
            // Head chain on survivor now reaches E_f: head → ef → ep2 → ep1.
            let ef_alias = format!("lvs_node-a/{}", epoch_name(VOL, 3));
            let ef = w.bdevs.get_mut(&("node-a".into(), ef_alias)).unwrap();
            ef["driver_specific"]["lvol"]["base_snapshot"] = json!(epoch_name(VOL, 2));
            let head_alias = format!("lvs_node-a/vol_{}_replica_0", VOL);
            let head = w.bdevs.get_mut(&("node-a".into(), head_alias)).unwrap();
            head["driver_specific"]["lvol"]["base_snapshot"] = json!(epoch_name(VOL, 3));
            let raids = w.raids.get_mut("consumer").unwrap();
            raids[0]["base_bdevs_list"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "name": expected_remote_base_bdev(VOL, 1), "is_configured": true }));
            w.bdevs.insert(
                ("consumer".into(), expected_remote_base_bdev(VOL, 1)),
                json!({ "name": expected_remote_base_bdev(VOL, 1), "uuid": "uuid-head" }),
            );
        }
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        record.mark_hot_rejoined(
            "uuid-b",
            &epoch_name(VOL, 3),
            &["uuid-a".into()],
            "uuid-head",
            NOW,
        );
        let store = FakeStore::new(record.clone());

        reconcile_marked(
            &rpc, &store, VOL, &record, &replicas2(), Some("consumer"), &catchup_cfg(),
        )
        .await;

        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::InSync);
        assert!(b.hot_rejoin.is_none());
        // Pad disposed; head re-rooted onto the local E_f.
        assert!(!rpc.has_bdev("node-b", &format!("lvs_node-b/vol_{}_replica_1", VOL)));
        let w = rpc.world.lock().unwrap();
        let head = w
            .bdevs
            .get(&("node-b".into(), format!("lvs_node-b/vol_{}_replica_1_hr", VOL)))
            .unwrap();
        assert_eq!(
            head["driver_specific"]["lvol"]["base_snapshot"].as_str(),
            Some(epoch_name(VOL, 3).as_str())
        );
        drop(w);
        assert_eq!(store.events(), vec!["HotRejoinLocalized"]);
    }

    #[tokio::test]
    async fn localize_short_circuits_after_set_parent_crash() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // Crash landed after set_parent: head already rooted at local E_f,
        // pad already gone. Only cleanup + the record write remain.
        {
            let mut w = rpc.world.lock().unwrap();
            w.bdevs
                .remove(&("node-b".into(), format!("lvs_node-b/vol_{}_replica_1", VOL)));
        }
        rpc.seed_lvol_with_parent(
            "node-b",
            "lvs_node-b",
            &format!("vol_{}_replica_1_hr", VOL),
            "uuid-head",
            Some(&epoch_name(VOL, 3)),
        );
        {
            let mut w = rpc.world.lock().unwrap();
            let raids = w.raids.get_mut("consumer").unwrap();
            raids[0]["base_bdevs_list"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "name": expected_remote_base_bdev(VOL, 1), "is_configured": true }));
            w.bdevs.insert(
                ("consumer".into(), expected_remote_base_bdev(VOL, 1)),
                json!({ "name": expected_remote_base_bdev(VOL, 1), "uuid": "uuid-head" }),
            );
        }
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        record.mark_hot_rejoined("uuid-b", &epoch_name(VOL, 3), &["uuid-a".into()], "uuid-head", NOW);
        let store = FakeStore::new(record.clone());

        reconcile_marked(
            &rpc, &store, VOL, &record, &replicas2(), Some("consumer"), &catchup_cfg(),
        )
        .await;

        // No copy ran; the record still converged to in_sync.
        assert!(rpc.calls_of("bdev_lvol_start_shallow_copy").is_empty());
        assert_eq!(store.record().get("uuid-b").unwrap().sync_state, SyncState::InSync);
    }

    #[tokio::test]
    async fn reconcile_promotes_localized_but_legless() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // Localized head (parent = E_f), but no leg (restage excluded it).
        rpc.seed_lvol_with_parent(
            "node-b",
            "lvs_node-b",
            &format!("vol_{}_replica_1_hr", VOL),
            "uuid-head",
            Some(&epoch_name(VOL, 3)),
        );
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        record.mark_hot_rejoined("uuid-b", &epoch_name(VOL, 3), &["uuid-a".into()], "uuid-head", NOW);
        let store = FakeStore::new(record.clone());

        reconcile_marked(
            &rpc, &store, VOL, &record, &replicas2(), Some("consumer"), &catchup_cfg(),
        )
        .await;

        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Standby, "stays a plain standby");
        assert!(b.hot_rejoin.is_none(), "claim released");
        assert!(
            rpc.has_bdev("node-b", &format!("lvs_node-b/vol_{}_replica_1_hr", VOL)),
            "localized head kept"
        );
    }

    #[tokio::test]
    async fn reconcile_demotes_unlocalized_legless() {
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        // Head still esnap (no local parent), no leg: unusable — demote.
        rpc.seed_lvol("node-b", "lvs_node-b", &format!("vol_{}_replica_1_hr", VOL), "uuid-head");
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        record.mark_hot_rejoined("uuid-b", &epoch_name(VOL, 3), &["uuid-a".into()], "uuid-head", NOW);
        let store = FakeStore::new(record.clone());

        reconcile_marked(
            &rpc, &store, VOL, &record, &replicas2(), Some("consumer"), &catchup_cfg(),
        )
        .await;

        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::Stale);
        assert!(b.hot_rejoin.is_none());
        assert!(!rpc.has_bdev("node-b", &format!("lvs_node-b/vol_{}_replica_1_hr", VOL)));
        assert_eq!(store.events(), vec!["HotRejoinDemoted"]);
    }

    // -- Name/shape tests -------------------------------------------------------

    #[test]
    fn ef_export_is_outside_the_reaper_prefix() {
        // 7b-0's dead-controller reaper condemns only
        // `nvme_nqn_2024-11_com_flint_volume_` controllers — the esnap
        // parent's controller must never match while its source restarts.
        let prefix = crate::controller_reap::flint_controller_prefix();
        assert!(!ef_controller_name("pvc-x").starts_with(&prefix));
    }

    #[test]
    fn head_name_within_lvol_limit() {
        // 36-char uuid volume names + "_hr" must stay under SPDK's 64-char
        // lvol name cap (the 1.2.0-rc2 clamp lesson).
        let vol = "pvc-0123456789abcdef0123456789abcdef0123";
        assert!(head_lvol_name(vol, 2).len() < 64);
    }

    // -- 7b-2: standby targets + the trigger planner ---------------------------

    /// stale_b_record chased to convergence: replica b standby at epoch 2
    /// (the record's latest) — the trigger's class.
    fn standby_b_record() -> VolumeSyncRecord {
        let mut r = stale_b_record();
        r.mark_standby("uuid-b", &epoch_name(VOL, 2), "chased", NOW);
        r
    }

    fn trigger_cfg() -> HotRejoinTriggerConfig {
        HotRejoinTriggerConfig {
            enabled: true,
            max_lag: 1,
            retry_backoff: Duration::from_secs(300),
        }
    }

    fn hr_view(record: VolumeSyncRecord) -> VolumeHotRejoinView {
        VolumeHotRejoinView {
            volume_id: VOL.to_string(),
            record,
            consumer: Some("consumer".to_string()),
            rwx: false,
            nfs_backing: false,
            rwo_bounce_enabled: false,
            hot_rejoin_disabled: false,
        }
    }

    #[tokio::test]
    async fn standby_target_demotes_at_intent_then_flips_and_localizes() {
        // The 7b-2 production path: a converged standby (stuck waiting for
        // a reassembly that never comes) goes through the same window and
        // ends in_sync — with the intent CAS demoting it to stale+marker so
        // the crash decode table stays exact.
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        let store = FakeStore::new(standby_b_record());

        let out = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap();
        match out {
            HotRejoinOutcome::Rejoined { localized, .. } => assert!(localized),
            other => panic!("expected Rejoined, got {:?}", other),
        }

        // Intent landed before the flip (the demote+claim single write).
        let ops = store.ops();
        let pos = |needle: &str| {
            ops.iter()
                .position(|o| o.starts_with(needle))
                .unwrap_or(usize::MAX)
        };
        assert!(
            pos(&format!("hr_intent:{}", epoch_name(VOL, 3)))
                < pos(&format!("hr_flip:{}", epoch_name(VOL, 3))),
            "intent must precede the flip: {:?}",
            ops
        );

        let rec = store.record();
        let b = rec.get("uuid-b").unwrap();
        assert_eq!(b.sync_state, SyncState::InSync);
        assert!(b.hot_rejoin.is_none());
        assert_eq!(b.last_epoch.as_deref(), Some(epoch_name(VOL, 3).as_str()));
        assert_eq!(store.events(), vec!["HotRejoinSucceeded", "HotRejoinLocalized"]);
    }

    #[tokio::test]
    async fn second_rejoin_refused_while_any_marker_set() {
        // The E_f export NQN is per-volume: a second concurrent window
        // would collide with the first's transport. resolve() refuses the
        // whole volume while any replica carries a marker.
        let rpc = FakeRpc::new();
        staged_world(&rpc);
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        let store = FakeStore::new(record);

        let out = hot_rejoin_volume(&rpc, &store, VOL, &replicas2(), "consumer", &cfg())
            .await
            .unwrap();
        match out {
            HotRejoinOutcome::NotEligible(reason) => {
                assert!(reason.contains("already in progress"), "unexpected: {}", reason)
            }
            other => panic!("expected NotEligible, got {:?}", other),
        }
        assert!(rpc.calls_of("bdev_raid_quiesce").is_empty(), "no window opened");
    }

    #[test]
    fn resolve_prefers_the_most_converged_standby() {
        let replicas = vec![
            replica("node-a", "uuid-a"),
            replica("node-b", "uuid-b"),
            {
                let mut c = replica("node-c", "uuid-c");
                c.lvol_name = format!("vol_{}_replica_2", VOL);
                c
            },
        ];
        let mut record = VolumeSyncRecord::initial(&replicas);
        let all: Vec<String> =
            vec!["uuid-a".into(), "uuid-b".into(), "uuid-c".into()];
        for seq in 1..=3 {
            record.apply_epoch_cut(&epoch_name(VOL, seq), &all, NOW);
        }
        // b: stale. c: standby at epoch 3. The standby wins even though the
        // stale comes first in the record.
        record.mark_stale("uuid-b", "leg failed", NOW);
        record.mark_stale("uuid-c", "leg failed", NOW);
        record.mark_standby("uuid-c", &epoch_name(VOL, 3), "chased", NOW);

        let topo = resolve(VOL, &record, &replicas, "consumer").expect("resolves");
        assert_eq!(topo.rec.lvol_uuid, "uuid-c");
        assert_eq!(topo.idx, 2);
    }

    #[test]
    fn plan_rejoins_the_target_class_and_only_it() {
        let cfg = trigger_cfg();

        // The (B) class: attached multi-replica RWO, no opt-in/out, ready
        // standby.
        assert_eq!(plan_hot_rejoin(&hr_view(standby_b_record()), &cfg), HotRejoinDecision::Rejoin);

        // Synthetic RWX backing PV: cutover owns its bounce.
        let mut v = hr_view(standby_b_record());
        v.nfs_backing = true;
        assert!(matches!(plan_hot_rejoin(&v, &cfg), HotRejoinDecision::Wait(r) if r.contains("backing")));

        // RWX parent: Tier-1 NFS bounce.
        let mut v = hr_view(standby_b_record());
        v.rwx = true;
        assert!(matches!(plan_hot_rejoin(&v, &cfg), HotRejoinDecision::Wait(r) if r.contains("RWX")));

        // The surgical opt-out.
        let mut v = hr_view(standby_b_record());
        v.hot_rejoin_disabled = true;
        assert!(matches!(plan_hot_rejoin(&v, &cfg), HotRejoinDecision::Wait(r) if r.contains("disabled")));

        // Bounce-opted volumes stay on the disjoint Tier-1 path.
        let mut v = hr_view(standby_b_record());
        v.rwo_bounce_enabled = true;
        assert!(matches!(plan_hot_rejoin(&v, &cfg), HotRejoinDecision::Wait(r) if r.contains("cutover")));

        // Detached: the next stage admits the standby for free.
        let mut v = hr_view(standby_b_record());
        v.consumer = None;
        assert!(matches!(plan_hot_rejoin(&v, &cfg), HotRejoinDecision::Wait(r) if r.contains("not attached")));
    }

    #[test]
    fn plan_requires_a_ready_standby() {
        let cfg = trigger_cfg();

        // A raw stale belongs to the Tier-1 chase first (read-latency: a
        // cold esnap leg would forward reads to the source for the whole
        // backfill).
        assert!(matches!(
            plan_hot_rejoin(&hr_view(stale_b_record()), &cfg),
            HotRejoinDecision::Wait(r) if r.contains("catch-up")
        ));

        // Lagging standby: the chase has not converged.
        let mut record = standby_b_record();
        record.apply_epoch_cut(&epoch_name(VOL, 3), &["uuid-a".into()], NOW);
        record.apply_epoch_cut(&epoch_name(VOL, 4), &["uuid-a".into()], NOW);
        assert!(matches!(
            plan_hot_rejoin(&hr_view(record), &cfg),
            HotRejoinDecision::Wait(r) if r.contains("lag")
        ));

        // Marker set: the reconciler owns the volume.
        let mut record = stale_b_record();
        record.mark_hot_rejoin_intent("uuid-b", &epoch_name(VOL, 3), "t");
        assert!(matches!(
            plan_hot_rejoin(&hr_view(record), &cfg),
            HotRejoinDecision::Wait(r) if r.contains("in progress")
        ));

        // Fully redundant: nothing to do.
        let mut record = stale_b_record();
        record.mark_in_sync("uuid-b", &epoch_name(VOL, 2), "healed", NOW);
        assert!(matches!(
            plan_hot_rejoin(&hr_view(record), &cfg),
            HotRejoinDecision::Wait(r) if r.contains("no standby")
        ));

        // Unreadable standby mark: never ready.
        let mut record = standby_b_record();
        record.replicas[1].last_epoch = Some("garbage".to_string());
        assert!(matches!(
            plan_hot_rejoin(&hr_view(record), &cfg),
            HotRejoinDecision::Wait(r) if r.contains("unreadable")
        ));
    }
}
