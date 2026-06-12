//! §10-14 orphan sweep: reap flint-owned lvols and NVMe-oF export
//! subsystems whose owning PV no longer exists.
//!
//! The deletion paths leak under failure orderings the campaign observed
//! live (e2e-campaign-2026-06-12.md): DeleteVolume with a stale replica
//! left both replica heads, six epochs, a user-snapshot copy and the
//! export subsystems behind; a snapshot copy pinned by a restore clone
//! whose PV is deleted later has no reconciler at all. Per the design doc
//! the answer is a convergent node-local sweep keyed by PV absence, not
//! more ordering cleverness in the deletion paths.
//!
//! Safety model — every rule is load-bearing:
//!
//! * **Strict ownership parsing.** Only names matching a flint-created
//!   shape (`vol_*`, `epoch-<vol>-<seq>`, `snap_<vol>_<u64>`,
//!   `temp_pvc_clone_*`, `eph_*`; NQNs under
//!   `nqn.2024-11.com.flint:volume:`) are ever candidates. Anything else
//!   is invisible to the sweep, mirroring the epoch GC's parser rule.
//! * **PV absence is the only condemnation authority** for PV-backed
//!   objects, and only a successful PV list proves absence — an API error
//!   skips the whole cycle. RWX synthetic ids resolve through
//!   [`crate::replica_sync::record_pv_name`] before the check.
//! * **Ordered candidacy breaks the ephemeral circularity.** Inline
//!   ephemeral volumes (`eph_*`, kubelet ids like `csi-…`) never have a
//!   PV, so PV absence proves nothing for them. Order: (1) PV-owned
//!   lvols condemn on PV absence alone; (2) a subsystem condemns only if
//!   its owner PV is absent AND every namespace bdev is itself absent or
//!   a condemned PV-owned lvol — an active ephemeral's export references
//!   a live eph lvol and therefore survives, whatever its NQN says;
//!   (3) an eph lvol condemns only if no surviving subsystem references
//!   it and it is absent from the ublk frontend list — and only when
//!   that list could actually be fetched (`ublk_bdevs: None` ⇒ every eph
//!   is skipped, because ublk is the default block-device backend and an
//!   unverifiable frontend may be a mounted pod volume).
//! * **Three strikes.** A candidate is reaped only after being condemned
//!   on `strike_threshold` consecutive cycles (60 s apart). CreateVolume
//!   provisions lvols *before* the external-provisioner creates the PV
//!   object, so a young lvol is legitimately PV-less; strikes also ride
//!   out publish/unpublish transients. Leaving the candidate set resets
//!   the count.
//!
//! Known non-goals, accepted: a consumer-side loopback subsystem whose
//! namespace still references an assembled `raid_*` bdev survives (raid
//! teardown is NodeUnstage's / §10-8's job); an ephemeral leak with its
//! frontend still intact is indistinguishable from in-use and stays; a
//! `temp_pvc_clone_*` snapshot whose new volume's PV exists is the
//! deletion path's bug, not an orphan.

use std::collections::{HashMap, HashSet};

/// Who an SPDK object belongs to, per flint naming conventions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// Owned by a PV-backed volume; the String is the volume id as
    /// embedded in the name (NOT yet resolved via `record_pv_name`).
    Pv(String),
    /// Inline ephemeral volume — no PV exists by design.
    Ephemeral,
}

/// Classify a local lvol name. `None` = not flint-shaped, never touched.
pub fn classify_lvol(name: &str) -> Option<Owner> {
    if let Some(rest) = name.strip_prefix("vol_") {
        if rest.is_empty() {
            return None;
        }
        return Some(Owner::Pv(strip_replica_suffix(rest).to_string()));
    }
    if let Some(rest) = name.strip_prefix("epoch-") {
        // epoch-<vol>-<seq>: vol itself contains '-', so split at the
        // LAST '-' and require a strictly numeric, non-empty tail.
        let (vol, seq) = rest.rsplit_once('-')?;
        if vol.is_empty() || seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        return Some(Owner::Pv(vol.to_string()));
    }
    if let Some(rest) = name.strip_prefix("snap_") {
        // snap_<vol>_<suffix>, suffix strictly u64-parseable (the §11
        // clamp guarantees it; anything else is not ours).
        let (vol, suffix) = rest.rsplit_once('_')?;
        if vol.is_empty() || suffix.parse::<u64>().is_err() {
            return None;
        }
        return Some(Owner::Pv(vol.to_string()));
    }
    if let Some(rest) = name.strip_prefix("temp_pvc_clone_") {
        if rest.is_empty() {
            return None;
        }
        return Some(Owner::Pv(rest.to_string()));
    }
    if name.strip_prefix("eph_").is_some_and(|r| !r.is_empty()) {
        return Some(Owner::Ephemeral);
    }
    None
}

const VOLUME_NQN_PREFIX: &str = "nqn.2024-11.com.flint:volume:";

/// Classify a subsystem NQN to its owning volume id (unresolved).
/// Covers all three export shapes: `:volume:<id>` (consumer/loopback),
/// `:volume:<id>_<i>` and `:volume:<id>:replica:<i>` (replica exports).
pub fn classify_subsystem_nqn(nqn: &str) -> Option<String> {
    let rest = nqn.strip_prefix(VOLUME_NQN_PREFIX)?;
    if rest.is_empty() {
        return None;
    }
    let base = match rest.split_once(":replica:") {
        // `:volume:<id>:replica:<i>` — <id> may itself be a replica
        // volume id, so strip that suffix too.
        Some((id, _)) => strip_replica_suffix(id),
        None => {
            // `<vol>_replica_<i>` replica volume ids embed their owner —
            // try that whole suffix before the bare `_<digits>` replica
            // index (`:volume:<id>_<i>`); plain PV-backed ids
            // (`pvc-<uuid>`, `nfs-server-…`, `csi-…`) contain neither.
            let stripped = strip_replica_suffix(rest);
            if stripped != rest {
                stripped
            } else {
                match rest.rsplit_once('_') {
                    Some((id, idx))
                        if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) =>
                    {
                        strip_replica_suffix(id)
                    }
                    _ => rest,
                }
            }
        }
    };
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// `<vol>_replica_<i>` → `<vol>` (replica volume ids embed their owner).
fn strip_replica_suffix(id: &str) -> &str {
    if let Some((base, idx)) = id.rsplit_once("_replica_") {
        if !base.is_empty() && !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) {
            return base;
        }
    }
    id
}

/// One local lvol, as listed per-lvstore via `bdev_lvol_get_lvols`.
#[derive(Debug, Clone)]
pub struct LvolEntry {
    pub lvs: String,
    pub name: String,
    pub uuid: String,
}

impl LvolEntry {
    pub fn alias(&self) -> String {
        format!("{}/{}", self.lvs, self.name)
    }
}

/// One local subsystem, as listed via `nvmf_get_subsystems`.
#[derive(Debug, Clone)]
pub struct SubsystemEntry {
    pub nqn: String,
    /// `namespaces[].bdev_name` — may be lvol aliases, uuids, or raw
    /// bdev names (`raid_*`).
    pub ns_bdevs: Vec<String>,
}

/// Everything one sweep cycle observed.
#[derive(Debug, Clone)]
pub struct SweepInput {
    pub lvols: Vec<LvolEntry>,
    pub subsystems: Vec<SubsystemEntry>,
    /// Bdev names attached to ublk frontends; `None` = could not be
    /// determined this cycle (skip all ephemeral candidates).
    pub ublk_bdevs: Option<Vec<String>>,
    /// Every bdev identifier on the node (names, aliases, uuids from
    /// `bdev_get_bdevs`) — includes non-lvol bdevs such as `raid_*`. A
    /// subsystem namespace referencing any PRESENT bdev that is not a
    /// condemned lvol is a possibly-live data path and survives.
    pub all_bdevs: HashSet<String>,
    /// Names of every PV currently in the cluster (from a successful
    /// full list — the caller must skip the sweep on a list error).
    pub existing_pvs: HashSet<String>,
}

/// What to delete this cycle, in order: subsystems first (their
/// write-opens block lvol deletion), then lvols (the executor retries
/// leaf-first by re-running failed deletes in passes).
#[derive(Debug, Default, PartialEq)]
pub struct SweepPlan {
    pub delete_subsystem_nqns: Vec<String>,
    pub delete_lvol_aliases: Vec<String>,
    /// Ephemeral lvols skipped because the ublk frontend list was
    /// unavailable — logged, never silently dropped.
    pub eph_skipped_unverifiable: usize,
}

fn pv_exists(existing: &HashSet<String>, id: &str) -> bool {
    existing.contains(id) || existing.contains(crate::replica_sync::record_pv_name(id))
}

/// Pure planning step. `strikes` persists across cycles (keyed
/// `lvol:<alias>` / `subsys:<nqn>`); entries that stop being candidates
/// are removed, entries reaching `strike_threshold` go into the plan.
pub fn plan_sweep(
    input: &SweepInput,
    strikes: &mut HashMap<String, u32>,
    strike_threshold: u32,
) -> SweepPlan {
    let mut plan = SweepPlan::default();

    // (1) PV-owned lvols: condemned on PV absence alone.
    let mut condemned_lvols: HashSet<&str> = HashSet::new(); // name AND uuid AND alias
    let mut condemned_aliases: Vec<String> = Vec::new();
    let mut eph_lvols: Vec<&LvolEntry> = Vec::new();
    for lvol in &input.lvols {
        match classify_lvol(&lvol.name) {
            Some(Owner::Pv(id)) if !pv_exists(&input.existing_pvs, &id) => {
                condemned_lvols.insert(lvol.name.as_str());
                condemned_lvols.insert(lvol.uuid.as_str());
                condemned_aliases.push(lvol.alias());
            }
            Some(Owner::Ephemeral) => eph_lvols.push(lvol),
            _ => {}
        }
    }
    let condemned_alias_set: HashSet<&str> =
        condemned_aliases.iter().map(|a| a.as_str()).collect();

    // (2) Subsystems: owner PV absent AND every namespace bdev absent or
    // itself a condemned PV-owned lvol. Presence is judged against ALL
    // bdevs (raids included): a present, non-condemned namespace bdev is
    // a possibly-live data path — tearing raids down is NodeUnstage's
    // job (§10-8), never the sweep's.
    let bdev_present = |bdev: &str| input.all_bdevs.contains(bdev);
    let bdev_condemned = |bdev: &str| {
        condemned_lvols.contains(bdev) || condemned_alias_set.contains(bdev)
    };
    let mut surviving_ns_bdevs: HashSet<&str> = HashSet::new();
    let mut condemned_subsystems: Vec<String> = Vec::new();
    for sub in &input.subsystems {
        let owner_absent = classify_subsystem_nqn(&sub.nqn)
            .map(|id| !pv_exists(&input.existing_pvs, &id))
            .unwrap_or(false); // foreign / non-volume NQN: never ours
        let all_ns_dead = sub
            .ns_bdevs
            .iter()
            .all(|b| !bdev_present(b) || bdev_condemned(b));
        if owner_absent && all_ns_dead {
            condemned_subsystems.push(sub.nqn.clone());
        } else {
            surviving_ns_bdevs.extend(sub.ns_bdevs.iter().map(|b| b.as_str()));
        }
    }

    // (3) Ephemeral lvols: unreferenced by surviving subsystems and
    // verifiably absent from ublk frontends.
    for lvol in eph_lvols {
        match &input.ublk_bdevs {
            None => plan.eph_skipped_unverifiable += 1,
            Some(ublk) => {
                let alias = lvol.alias();
                let referenced = surviving_ns_bdevs.contains(lvol.name.as_str())
                    || surviving_ns_bdevs.contains(lvol.uuid.as_str())
                    || surviving_ns_bdevs.contains(alias.as_str())
                    || ublk
                        .iter()
                        .any(|b| b == &lvol.name || b == &lvol.uuid || b == &alias);
                if !referenced {
                    condemned_aliases.push(alias);
                }
            }
        }
    }

    // Strike bookkeeping: only candidates condemned `strike_threshold`
    // cycles in a row are planned for deletion.
    let mut current: HashSet<String> = HashSet::new();
    for nqn in condemned_subsystems {
        let key = format!("subsys:{}", nqn);
        let n = strikes.entry(key.clone()).or_insert(0);
        *n = n.saturating_add(1);
        if *n >= strike_threshold {
            plan.delete_subsystem_nqns.push(nqn);
        }
        current.insert(key);
    }
    for alias in condemned_aliases {
        let key = format!("lvol:{}", alias);
        let n = strikes.entry(key.clone()).or_insert(0);
        *n = n.saturating_add(1);
        if *n >= strike_threshold {
            plan.delete_lvol_aliases.push(alias);
        }
        current.insert(key);
    }
    strikes.retain(|k, _| current.contains(k));

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lvol(lvs: &str, name: &str, uuid: &str) -> LvolEntry {
        LvolEntry { lvs: lvs.into(), name: name.into(), uuid: uuid.into() }
    }

    fn pvs(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// SweepInput whose `all_bdevs` is derived from the lvol list — the
    /// common case; tests with non-lvol bdevs (raids) build their own.
    fn input(
        lvols: Vec<LvolEntry>,
        subsystems: Vec<SubsystemEntry>,
        ublk_bdevs: Option<Vec<String>>,
        existing: &[&str],
    ) -> SweepInput {
        let all_bdevs = lvols
            .iter()
            .flat_map(|l| [l.name.clone(), l.uuid.clone(), l.alias()])
            .collect();
        SweepInput { lvols, subsystems, ublk_bdevs, all_bdevs, existing_pvs: pvs(existing) }
    }

    // --- classification -------------------------------------------------

    #[test]
    fn classifies_every_flint_lvol_shape() {
        assert_eq!(classify_lvol("vol_pvc-abc"), Some(Owner::Pv("pvc-abc".into())));
        assert_eq!(
            classify_lvol("vol_pvc-abc_replica_0"),
            Some(Owner::Pv("pvc-abc".into()))
        );
        assert_eq!(
            classify_lvol("epoch-pvc-abc-12"),
            Some(Owner::Pv("pvc-abc".into()))
        );
        assert_eq!(
            classify_lvol("snap_pvc-abc_1718000000"),
            Some(Owner::Pv("pvc-abc".into()))
        );
        assert_eq!(
            classify_lvol("temp_pvc_clone_pvc-new"),
            Some(Owner::Pv("pvc-new".into()))
        );
        assert_eq!(classify_lvol("eph_csi-0123abcd"), Some(Owner::Ephemeral));
    }

    #[test]
    fn classify_matches_real_name_builders() {
        // Pin against the actual format!() shapes used at create time.
        let vol = "pvc-59f2";
        assert_eq!(
            classify_lvol(&crate::replica_sync::epoch_name(vol, 7)),
            Some(Owner::Pv(vol.into()))
        );
        assert_eq!(
            classify_lvol(&format!("vol_{}_replica_{}", vol, 2)),
            Some(Owner::Pv(vol.into()))
        );
    }

    #[test]
    fn rejects_foreign_and_malformed_names() {
        for name in [
            "raid_pvc-abc",          // raid bdev, not an lvol we own
            "mydata",                // user-created
            "vol_",                  // empty id
            "epoch-pvc-abc-",        // empty seq
            "epoch-pvc-abc-12a",     // non-numeric seq
            "epoch-12",              // no vol part
            "snap_pvc-abc_notnum",   // §11 clamp violated → not ours
            "snap__123",             // empty vol
            "eph_",                  // empty id
            "Vol_pvc-abc",           // case matters
        ] {
            assert_eq!(classify_lvol(name), None, "{name} must be invisible");
        }
    }

    #[test]
    fn classifies_all_subsystem_nqn_shapes() {
        let p = "nqn.2024-11.com.flint:volume:";
        assert_eq!(classify_subsystem_nqn(&format!("{p}pvc-a")), Some("pvc-a".into()));
        assert_eq!(classify_subsystem_nqn(&format!("{p}pvc-a_1")), Some("pvc-a".into()));
        assert_eq!(
            classify_subsystem_nqn(&format!("{p}pvc-a:replica:2")),
            Some("pvc-a".into())
        );
        assert_eq!(
            classify_subsystem_nqn(&format!("{p}pvc-a_replica_0")),
            Some("pvc-a".into())
        );
        assert_eq!(
            classify_subsystem_nqn(&format!("{p}nfs-server-pvc-a")),
            Some("nfs-server-pvc-a".into())
        );
        // Host NQNs and foreign subsystems are invisible.
        assert_eq!(classify_subsystem_nqn("nqn.2024-11.com.flint:node:w1"), None);
        assert_eq!(classify_subsystem_nqn("nqn.2014-08.org.nvmexpress.discovery"), None);
    }

    // --- planning: PV-owned lvols ----------------------------------------

    fn plan_once(input: &SweepInput) -> SweepPlan {
        // threshold 1 = condemn immediately, for tests not about strikes
        let mut strikes = HashMap::new();
        plan_sweep(input, &mut strikes, 1)
    }

    #[test]
    fn reaps_full_leak_set_of_deleted_pv() {
        // The campaign's observed leak: replica head, epochs, a snapshot
        // copy, and the export subsystems — owning PV gone.
        let input = input(
            vec![
                lvol("lvs0", "vol_pvc-dead_replica_1", "u1"),
                lvol("lvs0", "epoch-pvc-dead-5", "u2"),
                lvol("lvs0", "epoch-pvc-dead-6", "u3"),
                lvol("lvs0", "snap_pvc-dead_1718000001", "u4"),
            ],
            vec![SubsystemEntry {
                nqn: "nqn.2024-11.com.flint:volume:pvc-dead_1".into(),
                ns_bdevs: vec!["u1".into()],
            }],
            Some(vec![]),
            &["pvc-alive"],
        );
        let plan = plan_once(&input);
        assert_eq!(
            plan.delete_subsystem_nqns,
            vec!["nqn.2024-11.com.flint:volume:pvc-dead_1"]
        );
        assert_eq!(plan.delete_lvol_aliases.len(), 4);
    }

    #[test]
    fn existing_pv_protects_everything() {
        let input = input(
            vec![
                lvol("lvs0", "vol_pvc-alive", "u1"),
                lvol("lvs0", "epoch-pvc-alive-3", "u2"),
                lvol("lvs0", "temp_pvc_clone_pvc-alive", "u3"),
            ],
            vec![SubsystemEntry {
                nqn: "nqn.2024-11.com.flint:volume:pvc-alive".into(),
                ns_bdevs: vec!["u1".into()],
            }],
            Some(vec![]),
            &["pvc-alive"],
        );
        assert_eq!(plan_once(&input), SweepPlan::default());
    }

    #[test]
    fn rwx_synthetic_id_resolves_to_user_pv() {
        let input = input(
            vec![lvol("lvs0", "vol_nfs-server-pvc-rwx", "u1")],
            vec![],
            Some(vec![]),
            &["pvc-rwx"], // only the USER PV exists
        );
        assert_eq!(plan_once(&input), SweepPlan::default());
    }

    // --- planning: subsystems --------------------------------------------

    #[test]
    fn subsystem_with_live_raid_namespace_survives() {
        // Consumer loopback subsystem of a deleted PV whose namespace
        // still references an ASSEMBLED raid: a possibly-live data path
        // (the §3/§10-8 zombie-consumer case) — raid teardown is
        // NodeUnstage's job, and yanking the export from under a live
        // mount is exactly the D-state hang bug #4 fixed. Survive.
        let mk = |raid_present: bool| SweepInput {
            lvols: vec![],
            subsystems: vec![SubsystemEntry {
                nqn: "nqn.2024-11.com.flint:volume:pvc-dead".into(),
                ns_bdevs: vec!["raid_pvc-dead".into()],
            }],
            ublk_bdevs: Some(vec![]),
            all_bdevs: if raid_present {
                ["raid_pvc-dead".to_string()].into_iter().collect()
            } else {
                HashSet::new()
            },
            existing_pvs: pvs(&[]),
        };
        assert!(plan_once(&mk(true)).delete_subsystem_nqns.is_empty());
        // Raid gone (teardown ran, subsystem delete failed): reapable.
        assert_eq!(plan_once(&mk(false)).delete_subsystem_nqns.len(), 1);
    }

    #[test]
    fn subsystem_referencing_live_lvol_of_existing_pv_survives() {
        let input = input(
            vec![lvol("lvs0", "vol_pvc-alive", "u1")],
            vec![SubsystemEntry {
                // NQN parses to a dead owner, but the namespace points at
                // a live lvol owned by an existing PV → survive.
                nqn: "nqn.2024-11.com.flint:volume:pvc-dead".into(),
                ns_bdevs: vec!["u1".into()],
            }],
            Some(vec![]),
            &["pvc-alive"],
        );
        assert!(plan_once(&input).delete_subsystem_nqns.is_empty());
    }

    // --- planning: ephemeral ----------------------------------------------

    #[test]
    fn active_ephemeral_protected_by_its_own_export() {
        let input = input(
            vec![lvol("lvs0", "eph_csi-aaa", "u1")],
            vec![SubsystemEntry {
                // kubelet's csi-… id never has a PV, so the owner looks
                // absent — but the namespace references the live eph
                // lvol, which is not PV-owned-condemned → subsystem
                // survives → eph lvol is referenced → both stay.
                nqn: "nqn.2024-11.com.flint:volume:csi-aaa".into(),
                ns_bdevs: vec!["u1".into()],
            }],
            Some(vec![]),
            &[],
        );
        assert_eq!(plan_once(&input), SweepPlan::default());
    }

    #[test]
    fn ublk_attached_ephemeral_survives_and_unreferenced_is_reaped() {
        let mk = |ublk: Vec<String>| {
            input(vec![lvol("lvs0", "eph_csi-bbb", "u1")], vec![], Some(ublk), &[])
        };
        assert_eq!(plan_once(&mk(vec!["eph_csi-bbb".into()])), SweepPlan::default());
        assert_eq!(
            plan_once(&mk(vec![])).delete_lvol_aliases,
            vec!["lvs0/eph_csi-bbb"]
        );
    }

    #[test]
    fn unverifiable_ublk_skips_ephemeral_loudly() {
        let input = input(vec![lvol("lvs0", "eph_csi-ccc", "u1")], vec![], None, &[]);
        let plan = plan_once(&input);
        assert!(plan.delete_lvol_aliases.is_empty());
        assert_eq!(plan.eph_skipped_unverifiable, 1);
    }

    // --- strikes -----------------------------------------------------------

    #[test]
    fn three_strikes_then_reap_and_departure_resets() {
        let input = input(vec![lvol("lvs0", "vol_pvc-young", "u1")], vec![], Some(vec![]), &[]);
        let mut strikes = HashMap::new();
        assert!(plan_sweep(&input, &mut strikes, 3).delete_lvol_aliases.is_empty());
        assert!(plan_sweep(&input, &mut strikes, 3).delete_lvol_aliases.is_empty());
        // PV appears (provisioner caught up) → candidate leaves → reset.
        let healed = SweepInput { existing_pvs: pvs(&["pvc-young"]), ..input.clone() };
        assert_eq!(plan_sweep(&healed, &mut strikes, 3), SweepPlan::default());
        assert!(strikes.is_empty(), "departure must reset the count");
        // Gone again: needs three fresh cycles.
        assert!(plan_sweep(&input, &mut strikes, 3).delete_lvol_aliases.is_empty());
        assert!(plan_sweep(&input, &mut strikes, 3).delete_lvol_aliases.is_empty());
        assert_eq!(
            plan_sweep(&input, &mut strikes, 3).delete_lvol_aliases,
            vec!["lvs0/vol_pvc-young"]
        );
    }
}
