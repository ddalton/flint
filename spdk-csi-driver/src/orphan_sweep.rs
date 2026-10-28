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
//!   [`crate::replica_sync::record_pv_name`] before the check. The one
//!   exception is a user snapshot (`snap_<vol>_<u64>`): CSI lets it
//!   outlive its source volume, so it also needs a successful
//!   VolumeSnapshotContent list naming neither it nor its uuid (2026-10-28:
//!   condemning on PV absence alone cost every VolumeSnapshot of a deleted
//!   PVC its data).
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
//! * **An unbound PVC is a create in flight.** Strikes alone are a timer:
//!   a thick replica is zeroed as it is created (~1 s per GiB), and a
//!   create that outlasted three cycles lost a replica (bench8,
//!   2026-10-22). A PV-owned object whose volume id is the name the
//!   external-provisioner gives an unbound PVC (`pvc-<uid>`) is not a
//!   candidate. The PVC list is read only when some owner PV is absent,
//!   and a failed read skips the cycle like a failed PV list.
//!
//! Known non-goals, accepted: a consumer-side loopback subsystem whose
//! namespace still references an assembled `raid_*` bdev survives (raid
//! teardown is NodeUnstage's / §10-8's job); an ephemeral leak with its
//! frontend still intact is indistinguishable from in-use and stays; a
//! `temp_pvc_clone_*` snapshot whose new volume's PV exists is the
//! deletion path's bug, not an orphan.

use std::collections::{HashMap, HashSet};

/// Canonical classification lives in `identity` (Phase 4); re-exported
/// here so sweep call sites and the historical test corpus keep reading
/// naturally.
pub use crate::identity::{classify_lvol, classify_subsystem_nqn, Owner};

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
    /// Per-volume KNOWN replica lvol uuids: identity uuids ∪ the sync
    /// record's live/active/reverted uuids. Feeds the stranded-placeholder
    /// rule (C6): a `vol_<vol>_replica_<i>`-shaped lvol whose PV EXISTS but
    /// whose uuid is unknown to that PV is an abort-stranded replacement
    /// placeholder — PV-absence authority can never condemn it. Volumes
    /// absent from this map are exempt (no record evidence → never guess).
    pub pv_known_replica_uuids: std::collections::HashMap<String, HashSet<String>>,
    /// `pvc-<uid>` of every PVC with no volume bound and no deletion
    /// pending: the PV name the external-provisioner will create for it.
    /// Empty unless [`SweepInput::needs_unbound_pvcs`] asked for it.
    pub unbound_pvc_volumes: HashSet<String>,
    /// Every snapshot handle a VolumeSnapshotContent carries (status and
    /// pre-provisioned spec), from a successful full list. `None` = could
    /// not be read this cycle (or no snapshot CRDs): no user snapshot is
    /// condemned.
    pub snapshot_handles: Option<HashSet<String>>,
}

impl SweepInput {
    /// A user (CSI) snapshot belongs to its VolumeSnapshotContent, not its
    /// source PV: CSI lets it outlive the volume, and DeleteSnapshot is
    /// what deletes it. Condemning it on PV absence lost the data of every
    /// VolumeSnapshot whose PVC was deleted, three cycles later (its
    /// restore then failed NOT_FOUND; 2026-10-28). It is an orphan only
    /// when its PV is gone AND a successful VolumeSnapshotContent list
    /// names neither it nor its uuid (pre-fix single-replica handles).
    fn user_snapshot_unowned(&self, lvol: &LvolEntry, id: &str) -> bool {
        self.owner_gone(id)
            && self
                .snapshot_handles
                .as_ref()
                .is_some_and(|h| !h.contains(&lvol.name) && !h.contains(&lvol.uuid))
    }

    /// The owner of a PV-backed object is gone: no PV, and no unbound PVC
    /// whose volume is still being created.
    fn owner_gone(&self, id: &str) -> bool {
        !pv_exists(&self.existing_pvs, id) && !pv_exists(&self.unbound_pvc_volumes, id)
    }

    /// Some user snapshot has no PV: only then is the VolumeSnapshotContent
    /// list worth reading (until it is, every user snapshot is spared).
    pub fn needs_snapshot_handles(&self) -> bool {
        self.lvols.iter().any(|l| {
            matches!(
                crate::identity::lvol_owner(&l.name),
                Some((id, crate::identity::LvolKind::UserSnapshot)) if self.owner_gone(id)
            )
        })
    }

    /// Some lvol or subsystem has no PV: only then is the PVC list worth
    /// reading (it can only spare candidates).
    pub fn needs_unbound_pvcs(&self) -> bool {
        let absent = |id: &str| !pv_exists(&self.existing_pvs, id);
        self.lvols.iter().any(|l| matches!(classify_lvol(&l.name), Some(Owner::Pv(id)) if absent(&id)))
            || self.subsystems.iter().any(|s| classify_subsystem_nqn(&s.nqn).is_some_and(|id| absent(&id)))
    }
}

/// Every snapshot handle in a VolumeSnapshotContent list (the objects'
/// `data`): the bound handle in status, and spec.source.snapshotHandle for a
/// pre-provisioned content not yet bound.
pub fn snapshot_handles<'a>(contents: impl IntoIterator<Item = &'a serde_json::Value>) -> HashSet<String> {
    contents
        .into_iter()
        .flat_map(|c| [&c["status"]["snapshotHandle"], &c["spec"]["source"]["snapshotHandle"]])
        .filter_map(|h| h.as_str().filter(|h| !h.is_empty()).map(str::to_string))
        .collect()
}

/// `pvc-<uid>` of every PVC in `pvcs` that is unbound and not being deleted.
pub fn unbound_pvc_volumes(pvcs: &[k8s_openapi::api::core::v1::PersistentVolumeClaim]) -> HashSet<String> {
    pvcs.iter()
        .filter(|c| c.metadata.deletion_timestamp.is_none())
        .filter(|c| c.spec.as_ref().and_then(|s| s.volume_name.as_deref()).is_none_or(str::is_empty))
        .filter_map(|c| c.metadata.uid.as_ref().map(|uid| format!("pvc-{uid}")))
        .collect()
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
    existing.contains(id) || existing.contains(crate::identity::storage_id_of_handle(id))
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
        let user_snapshot = matches!(
            crate::identity::lvol_owner(&lvol.name),
            Some((_, crate::identity::LvolKind::UserSnapshot))
        );
        match classify_lvol(&lvol.name) {
            Some(Owner::Pv(id)) if user_snapshot => {
                if input.user_snapshot_unowned(lvol, &id) {
                    condemned_lvols.insert(lvol.name.as_str());
                    condemned_lvols.insert(lvol.uuid.as_str());
                    condemned_aliases.push(lvol.alias());
                }
            }
            Some(Owner::Pv(id)) if input.owner_gone(&id) => {
                condemned_lvols.insert(lvol.name.as_str());
                condemned_lvols.insert(lvol.uuid.as_str());
                condemned_aliases.push(lvol.alias());
            }
            Some(Owner::Ephemeral) => eph_lvols.push(lvol),
            Some(Owner::Pv(id)) => {
                // Stranded-placeholder rule (C6, wave 2): the PV exists, the
                // name is replica-shaped, but no record (identity, active
                // head, revert) knows this uuid — a replacement placeholder
                // whose adopting PV patch never landed. Three strikes like
                // every uncertain condemnation; operation-scoped shapes
                // (_hr scratch heads, _hrpad pads) are lease-owned, exempt.
                let replica_shaped = lvol.name.contains("_replica_")
                    && !crate::guarded_destroy::is_operation_scoped(&lvol.name);
                if replica_shaped {
                    if let Some(known) = input.pv_known_replica_uuids.get(&id) {
                        if !known.contains(&lvol.uuid) {
                            // The shared strike layer below provides the
                            // 3-cycle discipline (same as every uncertain
                            // condemnation) and auto-resets when the uuid
                            // becomes known or the lvol disappears.
                            condemned_lvols.insert(lvol.name.as_str());
                            condemned_lvols.insert(lvol.uuid.as_str());
                            condemned_aliases.push(lvol.alias());
                        }
                    }
                }
            }
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
            .map(|id| input.owner_gone(&id))
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
        SweepInput { lvols, subsystems, ublk_bdevs, all_bdevs, existing_pvs: pvs(existing), pv_known_replica_uuids: Default::default(), unbound_pvc_volumes: Default::default(), snapshot_handles: Some(HashSet::new()) }
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
    fn classifies_hot_rejoin_shapes_to_their_owner() {
        // Tier-2 7b shapes, pinned against the actual name builders. The
        // 2026-07-02 live run: the sweep deleted a SERVING raid leg because
        // the `_hr` head read as an absent-PV orphan. While the PV exists
        // these must classify to it (kept); once the PV is truly gone the
        // same classification makes them reapable — both directions matter.
        let vol = "pvc-c689";
        assert_eq!(
            classify_lvol(&crate::hot_rejoin::head_lvol_name(&crate::identity::StorageId::of_handle(vol), 1)),
            Some(Owner::Pv(vol.into()))
        );
        assert_eq!(
            classify_subsystem_nqn(&format!(
                "nqn.2024-11.com.flint:volume:{}",
                crate::hot_rejoin::pad_export_volume_id(&crate::identity::StorageId::of_handle(vol), 1)
            ))
            .as_deref(),
            Some(vol)
        );
        // The E_f export NQN is outside the `:volume:` prefix on purpose —
        // invisible to the sweep (its lifecycle belongs to the hot-rejoin
        // scrub/localize paths, which own targeted cleanup).
        assert_eq!(classify_subsystem_nqn(&crate::hot_rejoin::ef_export_nqn(&crate::identity::StorageId::of_handle(vol))), None);
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

    /// 2026-10-28, live on the kind tier: a VolumeSnapshot of a deleted
    /// PVC lost its lvol three cycles after the PV went, and its restore
    /// failed NOT_FOUND. The snapshot object, not the PV, owns it.
    #[test]
    fn a_user_snapshot_outlives_its_pv_while_a_snapshot_object_names_it() {
        let snap = lvol("lvs0", "snap_pvc-dead_76878503594463742", "u-snap");
        let legacy = lvol("lvs0", "snap_pvc-dead_1718000001", "u-legacy");
        let head = lvol("lvs0", "vol_pvc-dead", "u-head");
        let base = input(vec![snap.clone(), legacy.clone(), head.clone()], vec![], Some(vec![]), &[]);
        let handles = |h: &[&str]| SweepInput { snapshot_handles: Some(pvs(h)), ..base.clone() };

        // Named by handle (name, or a pre-fix uuid handle): only the head goes.
        let plan = plan_once(&handles(&["snap_pvc-dead_76878503594463742", "u-legacy"]));
        assert_eq!(plan.delete_lvol_aliases, vec!["lvs0/vol_pvc-dead"]);

        // The list could not be read: every user snapshot is spared.
        let plan = plan_once(&SweepInput { snapshot_handles: None, ..base.clone() });
        assert_eq!(plan.delete_lvol_aliases, vec!["lvs0/vol_pvc-dead"]);

        // CONTROL: no snapshot object names them -- the copies a failed
        // DeleteSnapshot left are orphans, and go with the head.
        let plan = plan_once(&handles(&[]));
        assert_eq!(plan.delete_lvol_aliases.len(), 3);

        // A live PV still protects its snapshots whatever the list says.
        let plan = plan_once(&SweepInput { existing_pvs: pvs(&["pvc-dead"]), ..handles(&[]) });
        assert!(plan.delete_lvol_aliases.is_empty());
    }

    #[test]
    fn snapshot_handles_come_from_status_and_preprovisioned_spec() {
        let contents = [
            serde_json::json!({"spec": {"source": {"volumeHandle": "pvc-a"}}, "status": {"snapshotHandle": "snap_pvc-a_1"}}),
            serde_json::json!({"spec": {"source": {"snapshotHandle": "u-legacy"}}}),
            serde_json::json!({"spec": {"source": {"volumeHandle": "pvc-b"}}}),
        ];
        assert_eq!(snapshot_handles(contents.iter()), pvs(&["snap_pvc-a_1", "u-legacy"]));
        let needs = input(vec![lvol("lvs0", "snap_pvc-dead_1", "u")], vec![], Some(vec![]), &[]);
        assert!(needs.needs_snapshot_handles());
        assert!(!SweepInput { existing_pvs: pvs(&["pvc-dead"]), ..needs }.needs_snapshot_handles());
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
        let mk = |raid_present: bool| SweepInput { pv_known_replica_uuids: Default::default(), unbound_pvc_volumes: Default::default(), snapshot_handles: Some(HashSet::new()),
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
    // bench8 (2026-10-22): a thick 3-replica create zeroes its replicas for
    // minutes; replica 0 waits PV-less meanwhile. Its PVC, still unbound,
    // spares it however many cycles pass; once the PVC is gone, the usual
    // three strikes apply.
    #[test]
    fn an_unbound_pvc_spares_a_volume_still_being_created() {
        let young = lvol("lvs0", "vol_pvc-young_replica_0", "u1");
        let export = SubsystemEntry {
            nqn: "nqn.2024-11.com.flint:volume:pvc-young_replica_0".into(),
            ns_bdevs: vec![],
        };
        let creating = SweepInput {
            unbound_pvc_volumes: pvs(&["pvc-young"]),
            ..input(vec![young], vec![export], Some(vec![]), &[])
        };
        assert!(creating.needs_unbound_pvcs());
        let mut strikes = HashMap::new();
        for _ in 0..5 {
            assert_eq!(plan_sweep(&creating, &mut strikes, 3), SweepPlan::default());
        }
        assert!(strikes.is_empty(), "a spared volume collects no strikes");

        // The PVC was deleted before it bound: an orphan like any other.
        let abandoned = SweepInput { unbound_pvc_volumes: HashSet::new(), ..creating.clone() };
        assert!(plan_sweep(&abandoned, &mut strikes, 3).delete_lvol_aliases.is_empty());
        assert!(plan_sweep(&abandoned, &mut strikes, 3).delete_lvol_aliases.is_empty());
        let plan = plan_sweep(&abandoned, &mut strikes, 3);
        assert_eq!(plan.delete_lvol_aliases, vec!["lvs0/vol_pvc-young_replica_0"]);
        assert_eq!(plan.delete_subsystem_nqns, vec!["nqn.2024-11.com.flint:volume:pvc-young_replica_0"]);

        // Another volume's unbound PVC spares nothing here.
        let other = SweepInput { unbound_pvc_volumes: pvs(&["pvc-other"]), ..creating.clone() };
        assert_eq!(plan_once(&other).delete_lvol_aliases, vec!["lvs0/vol_pvc-young_replica_0"]);
    }

    #[test]
    fn the_pvc_list_is_read_only_when_some_owner_pv_is_absent() {
        let vol = |name: &str| lvol("lvs0", name, "u");
        let sub = |nqn: &str| SubsystemEntry { nqn: nqn.into(), ns_bdevs: vec![] };
        assert!(!input(vec![], vec![], None, &[]).needs_unbound_pvcs());
        assert!(!input(vec![vol("vol_pvc-a"), vol("mydata"), vol("eph_csi-1")], vec![], None, &["pvc-a"]).needs_unbound_pvcs());
        assert!(input(vec![vol("vol_pvc-a"), vol("vol_pvc-b_replica_1")], vec![], None, &["pvc-a"]).needs_unbound_pvcs());
        assert!(input(vec![], vec![sub("nqn.2024-11.com.flint:volume:pvc-b")], None, &["pvc-a"]).needs_unbound_pvcs());
        assert!(!input(vec![], vec![sub("nqn.2014-08.org.nvmexpress.discovery")], None, &[]).needs_unbound_pvcs());
    }

    #[test]
    fn unbound_pvc_volumes_are_the_pvcs_waiting_for_a_volume() {
        use k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
        let pvc = |uid: Option<&str>, volume: Option<&str>, deleting: bool| PersistentVolumeClaim {
            metadata: ObjectMeta {
                uid: uid.map(str::to_string),
                deletion_timestamp: deleting.then(|| Time(Default::default())),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec { volume_name: volume.map(str::to_string), ..Default::default() }),
            ..Default::default()
        };
        let got = unbound_pvc_volumes(&[
            pvc(Some("u-pending"), None, false),
            pvc(Some("u-empty"), Some(""), false),
            pvc(Some("u-bound"), Some("pvc-u-bound"), false),
            pvc(Some("u-deleting"), None, true),
            pvc(None, None, false),
        ]);
        assert_eq!(got, pvs(&["pvc-u-pending", "pvc-u-empty"]));
    }

    #[test]
    fn stranded_placeholder_condemned_by_strikes_only_when_uuid_unknown() {
        // C6/wave-2: a replica-shaped lvol whose PV EXISTS but whose uuid no
        // record knows = abort-stranded replacement placeholder.
        let lvol = LvolEntry {
            name: "vol_pvc-x_replica_1".into(),
            uuid: "uuid-stranded".into(),
            lvs: "lvs0".into(),
        };
        let known: std::collections::HashMap<String, HashSet<String>> = [(
            "pvc-x".to_string(),
            ["uuid-legit".to_string()].into_iter().collect::<HashSet<_>>(),
        )]
        .into_iter()
        .collect();
        let input = SweepInput {
            lvols: vec![lvol.clone()],
            subsystems: vec![],
            ublk_bdevs: Some(vec![]),
            all_bdevs: Default::default(),
            existing_pvs: pvs(&["pvc-x"]),
            pv_known_replica_uuids: known.clone(),
            unbound_pvc_volumes: Default::default(),
            snapshot_handles: Some(HashSet::new()),
        };
        let mut strikes = HashMap::new();
        // Strikes 1 and 2: watched, not condemned.
        assert!(plan_sweep(&input, &mut strikes, 3).delete_lvol_aliases.is_empty());
        assert!(plan_sweep(&input, &mut strikes, 3).delete_lvol_aliases.is_empty());
        // Strike 3: condemned.
        let plan = plan_sweep(&input, &mut strikes, 3);
        assert_eq!(plan.delete_lvol_aliases, vec!["lvs0/vol_pvc-x_replica_1".to_string()]);

        // A KNOWN uuid (identity, active head, or revert) is never touched —
        // and clears any accumulated strikes.
        let mut legit = input.clone();
        legit.lvols[0].uuid = "uuid-legit".into();
        let plan = plan_sweep(&legit, &mut strikes, 3);
        assert!(plan.delete_lvol_aliases.is_empty());
        assert!(!strikes.contains_key("lvol:lvs0/vol_pvc-x_replica_1"));

        // No record evidence for the volume → exempt (never guess).
        let mut no_evidence = input.clone();
        no_evidence.pv_known_replica_uuids = Default::default();
        let mut s2 = HashMap::new();
        for _ in 0..4 {
            assert!(plan_sweep(&no_evidence, &mut s2, 3).delete_lvol_aliases.is_empty());
        }

        // Operation-scoped scratch heads are lease-owned — exempt.
        let mut hr = input.clone();
        hr.lvols[0].name = "vol_pvc-x_replica_1_hr".into();
        let mut s3 = HashMap::new();
        for _ in 0..4 {
            assert!(plan_sweep(&hr, &mut s3, 3).delete_lvol_aliases.is_empty());
        }
    }

}
