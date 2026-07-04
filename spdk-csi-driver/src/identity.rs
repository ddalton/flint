//! identity — the single vocabulary for Flint volume identity.
//!
//! Phase 0 of `docs/plans/identity-unification.md` (audit + contract; NO
//! behavior change). An RWX/ROX volume has three identities that share
//! string shapes today and are disambiguated ad-hoc at ~25 sites; every
//! P1 in the aliasing class (637be1c ×6, d7490de, c879bc3) was one of
//! those sites guessing wrong. This module defines the canonical types,
//! the one handle parser, and the one naming surface. Call sites converge
//! here in Phase 1; until then the historical owners (`orphan_sweep`,
//! `replica_sync`, `hot_rejoin`, `nvmeof_export`, `snapshot_models`) keep
//! their bodies and this module DELEGATES, so there is exactly one
//! implementation of every rule from day one — no drift window.
//!
//! The three identities, precisely:
//!  - **Storage identity** (`storage_id`, shaped `pvc-<uuid>`): what lvols,
//!    NQNs, raids, epoch snapshots, and replica records derive from.
//!  - **User attachment handle**: the user PV's volumeHandle — the SAME
//!    string as the storage id. For RWO it names the block consumer; for
//!    RWX/ROX it names NFS clients that have NO block path. Context-free
//!    RPCs cannot tell these apart from the string alone — that ambiguity
//!    is what [`VolumeRef`] + a role resolver exist to remove.
//!  - **NFS backing handle** (`nfs-server-<storage_id>`): the synthetic
//!    PV's volumeHandle behind an RWX volume's NFS server pod — the
//!    server's own block attachment. Minted once (rwx_nfs.rs), currently
//!    re-parsed ad-hoc in ≥6 files.

use crate::orphan_sweep;
use crate::replica_sync;
use crate::hot_rejoin;
use crate::nvmeof_export;
use crate::snapshot::snapshot_models::SnapshotInfo;

pub use crate::orphan_sweep::Owner;

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// Prefix of the synthetic NFS backing PV's volumeHandle. The ONLY marker
/// distinguishing the backing attachment from user handles on the wire.
pub const NFS_BACKING_PREFIX: &str = "nfs-server-";

/// Mint the backing handle for an RWX volume's NFS server PV
/// (rwx_nfs.rs `create_nfs_server_pod`; the same PV also carries
/// `originalVolumeId=<storage_id>` in volumeAttributes).
pub fn backing_handle(storage_id: &str) -> String {
    format!("{}{}", NFS_BACKING_PREFIX, storage_id)
}

/// `Some(storage_id)` iff `handle` is an NFS backing handle. The one
/// inverse of [`backing_handle`]; Phase 1 retires both the scattered
/// `strip_prefix("nfs-server-")` sites and ControllerPublish/NodeStage's
/// parallel `originalVolumeId` context lookups in favor of this.
pub fn parse_backing_handle(handle: &str) -> Option<&str> {
    handle.strip_prefix(NFS_BACKING_PREFIX)
}

/// Storage identity of any volumeHandle: strips one backing prefix, passes
/// user handles through. Same rule as `replica_sync::record_pv_name` (the
/// storage id is also the user PV's name — PV name == volumeHandle for
/// user PVs, which is why records/annotations resolve through this).
pub fn storage_id_of_handle(handle: &str) -> &str {
    replica_sync::record_pv_name(handle)
}

// ---------------------------------------------------------------------------
// Role + VolumeRef
// ---------------------------------------------------------------------------

/// What kind of consumers a volume's USER handle fronts. Resolved from PV
/// access modes (the d7490de source of truth); `NfsShared` covers both RWX
/// (read-write clients) and ROX (`read_only`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Block,
    NfsShared { read_only: bool },
}

/// A volumeHandle, parsed. Constructed once at each RPC entry (Phase 1),
/// passed by value everywhere after — no site re-derives role or identity
/// from the raw string.
///
/// Resolution order (the contract; Phase 1 wires steps 2–3):
///  1. `nfs-server-` prefix → `NfsBacking` (never consults the resolver);
///  2. volume_context role hint (`flint.csi.storage.io/role`, Phase 2);
///  3. the cached role resolver (PV access modes → per-RPC failure
///     default, documented in the Phase-0 audit's matrix);
///  4. default `Block` — fencing semantics preserved (the c879bc3 choice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeRef {
    /// RWO volume, or any bare handle whose PV says RWO — full block path.
    Block { storage_id: String },
    /// User RWX/ROX handle — attachments are NFS clients; NO block path.
    NfsShared { storage_id: String, read_only: bool },
    /// `nfs-server-<id>` backing handle — the NFS server's own block
    /// attachment (stage/export/raid live under THIS handle on the
    /// server's node).
    NfsBacking { storage_id: String },
}

impl VolumeRef {
    /// Parse a volumeHandle. `role_for` is consulted only for bare (user)
    /// handles — backing handles self-identify by prefix.
    pub fn from_handle(handle: &str, role_for: impl FnOnce(&str) -> Role) -> Self {
        if let Some(storage_id) = parse_backing_handle(handle) {
            return VolumeRef::NfsBacking { storage_id: storage_id.to_string() };
        }
        match role_for(handle) {
            Role::Block => VolumeRef::Block { storage_id: handle.to_string() },
            Role::NfsShared { read_only } => {
                VolumeRef::NfsShared { storage_id: handle.to_string(), read_only }
            }
        }
    }

    /// The one conversion to storage identity (== user PV name).
    pub fn storage_id(&self) -> &str {
        match self {
            VolumeRef::Block { storage_id }
            | VolumeRef::NfsShared { storage_id, .. }
            | VolumeRef::NfsBacking { storage_id } => storage_id,
        }
    }

    /// Whether THIS attachment owns a block data path (connect, fence,
    /// stage, raid, target lifecycle). `false` is exactly the c879bc3 /
    /// d7490de "departing party is an NFS client" case: no target
    /// removal, no disconnect, unmount-only unstage.
    pub fn has_block_path(&self) -> bool {
        !matches!(self, VolumeRef::NfsShared { .. })
    }

    pub fn is_backing(&self) -> bool {
        matches!(self, VolumeRef::NfsBacking { .. })
    }

    /// The backing handle for this volume (whatever identity we hold).
    pub fn backing_handle(&self) -> String {
        backing_handle(self.storage_id())
    }
}

// ---------------------------------------------------------------------------
// Naming — every derived name is minted here (or delegated to its current
// owner until Phase 1 moves the body). Grep-lint target: `format!("vol_`,
// `format!("nqn.`, `format!("raid_`, `format!("epoch-`, `format!("snap_`,
// `format!("lvs_` outside this module (Phase 4 CI lint).
// ---------------------------------------------------------------------------

/// NQN namespace all volume-scoped exports live under (consumer/loopback
/// `:volume:<id>`, replica `:volume:<id>_<i>`, alias `:volume:<id>:replica:<i>`).
/// The dead-controller reaper and orphan sweep key on this prefix.
pub const VOLUME_NQN_PREFIX: &str = "nqn.2024-11.com.flint:volume:";

/// Single/primary lvol for a volume: `vol_<id>` (driver.rs, clone paths,
/// minimal_disk_service). Also applied to replica volume ids — a replica
/// lvol is `lvol_name(replica_volume_id(vol, i))`.
pub fn lvol_name(volume_id: &str) -> String {
    format!("vol_{}", volume_id)
}

/// Per-replica volume id: `<vol>_replica_<i>` — embedded in lvol names,
/// replica export NQNs, and record bookkeeping.
pub fn replica_volume_id(volume_id: &str, replica_index: usize) -> String {
    format!("{}_replica_{}", volume_id, replica_index)
}

/// Replica lvol name: `vol_<vol>_replica_<i>`.
pub fn replica_lvol_name(volume_id: &str, replica_index: usize) -> String {
    lvol_name(&replica_volume_id(volume_id, replica_index))
}

/// Hot-rejoin esnap-clone head lvol: `vol_<vol>_replica_<i>_hr`.
pub fn hr_head_lvol_name(volume_id: &str, replica_index: usize) -> String {
    hot_rejoin::head_lvol_name(volume_id, replica_index)
}

/// Hot-rejoin localization pad export id: `<vol>_hrpad<i>`.
pub fn hrpad_export_id(volume_id: &str, replica_index: usize) -> String {
    hot_rejoin::pad_export_volume_id(volume_id, replica_index)
}

/// Raid bdev name: `raid_<staging_handle>`. NOTE: keyed on the STAGING
/// handle, not the storage id — an RWX volume's raid assembles on the NFS
/// server's node under the BACKING handle (`raid_nfs-server-pvc-…`).
/// Callers reasoning about "the volume's raid" must pick the handle for
/// the attachment that stages it (node_agent health monitor, d7490de).
pub fn raid_name(staging_handle: &str) -> String {
    format!("raid_{}", staging_handle)
}

/// Common-epoch snapshot: `epoch-<vol>-<seq>` (replica_sync §5).
pub fn epoch_snapshot_name(volume_id: &str, seq: u64) -> String {
    replica_sync::epoch_name(volume_id, seq)
}

/// Strict "is it ours" epoch parser — `Some(seq)` only for THIS volume's
/// epochs (alignment/tombstone reaping must never touch foreign names).
pub fn epoch_seq(volume_id: &str, name: &str) -> Option<u64> {
    replica_sync::epoch_seq(volume_id, name)
}

/// User (CSI) snapshot: `snap_<vol>_<suffix>` (snapshot_csi; suffix is the
/// §11-clamped timestamp/id, strictly u64).
pub fn user_snapshot_name(volume_id: &str, suffix: u64) -> String {
    format!("snap_{}_{}", volume_id, suffix)
}

/// Strict "is it ours" user-snapshot parser (twin of [`epoch_seq`]).
pub fn user_snapshot_ts(volume_id: &str, name: &str) -> Option<u64> {
    replica_sync::user_snapshot_ts(volume_id, name)
}

/// Transient clone-source snapshot: `temp_pvc_clone_<new_volume_id>`
/// (volume-from-volume path; named for the NEW volume, deleted after the
/// clone detaches — the dashboard's lone legitimate "unknown" owner).
pub fn temp_clone_snapshot_name(new_volume_id: &str) -> String {
    format!("temp_pvc_clone_{}", new_volume_id)
}

/// Consumer/loopback export subsystem NQN: `…:volume:<export_id>`. The
/// export id is a volume id OR a replica/pad id — same namespace.
pub fn volume_nqn(export_id: &str) -> String {
    format!("{}{}", VOLUME_NQN_PREFIX, export_id)
}

/// Replica export NQN: `…:volume:<vol>_<i>` (the `export_replica`
/// convention; also what hot rejoin swaps namespaces under).
pub fn replica_export_nqn(volume_id: &str, replica_index: usize) -> String {
    hot_rejoin::replica_export_nqn(volume_id, replica_index)
}

/// Alias replica NQN: `…:volume:<vol>:replica:<i>` (multi-replica publish
/// context; classify_subsystem_nqn's third shape).
pub fn replica_alias_nqn(volume_id: &str, replica_index: usize) -> String {
    format!("{}{}:replica:{}", VOLUME_NQN_PREFIX, volume_id, replica_index)
}

/// Hot-rejoin E_f export NQN: `…:hotrejoin:<vol>` — deliberately NOT under
/// `:volume:` (kept out of the dead-controller reaper's namespace).
pub fn hotrejoin_export_nqn(volume_id: &str) -> String {
    hot_rejoin::ef_export_nqn(volume_id)
}

/// Per-node initiator host NQN: `…:node:<node>` (what makes host fencing
/// possible; fencing only ever removes hosts under this prefix).
pub fn node_host_nqn(node_name: &str) -> String {
    nvmeof_export::flint_host_nqn(node_name)
}

/// SPDK initiator controller name for an attached subsystem:
/// `nvme_<nqn with ':' and '.' → '_'>`; its first namespace bdev is
/// `<name>n1` (driver.rs consumer attach, hot_rejoin E_f attach, and the
/// disk service's "not local storage" filter all assume this rule).
pub fn initiator_controller_name(nqn: &str) -> String {
    format!("nvme_{}", nqn.replace(':', "_").replace('.', "_"))
}

/// LVS name minted at disk init: `lvs_<node>_<pci with ':' '.' → '-'>`
/// (minimal_disk_service). The Disk CR name is `<node>_<sanitized pci>`,
/// so this equals [`lvs_name_for_disk`] of the CR name.
pub fn lvs_name(node_name: &str, pci_address: &str) -> String {
    format!("lvs_{}_{}", node_name, pci_address.replace(':', "-").replace('.', "-"))
}

/// LVS name from a Disk CR name / disk_ref: `lvs_<disk_ref>`
/// (controller_operator replacement path, replica records).
pub fn lvs_name_for_disk(disk_ref: &str) -> String {
    format!("lvs_{}", disk_ref)
}

// ---------------------------------------------------------------------------
// Role resolution (Phase 1) — the ONE place a bare handle's role comes from.
// ---------------------------------------------------------------------------

/// Per-volume role cache. Volume ids are UUIDs (never reused) and PV
/// access modes are immutable, so entries are valid for the volume's
/// lifetime — `remove` at DeleteVolume is memory hygiene, not
/// correctness. Only successful resolutions are cached ("never trust a
/// cached zero": an unreadable PV is re-tried on every RPC).
#[derive(Clone, Default)]
pub struct RoleCache(std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Role>>>);

impl RoleCache {
    pub fn get(&self, storage_id: &str) -> Option<Role> {
        self.0.lock().unwrap().get(storage_id).copied()
    }
    pub fn put(&self, storage_id: &str, role: Role) {
        self.0.lock().unwrap().insert(storage_id.to_string(), role);
    }
    pub fn remove(&self, storage_id: &str) {
        self.0.lock().unwrap().remove(storage_id);
    }
}

/// PV unreadable — the caller applies its documented per-RPC default:
/// [`RoleResolveError::default_block`] (RWO fencing semantics, the
/// c879bc3 choice) everywhere except NodeUnstage, which falls back to
/// findmnt mount-state sniffing first (the d7490de choice).
pub struct RoleResolveError {
    pub handle: String,
    pub reason: String,
}

impl RoleResolveError {
    pub fn default_block(&self) -> VolumeRef {
        VolumeRef::Block { storage_id: self.handle.clone() }
    }
}

/// The cached role resolver — replaces every site-local
/// `pv_access_modes`/findmnt/`is_shared_nfs_consumer` heuristic.
#[derive(Clone)]
pub struct RoleResolver {
    kube_client: kube::Client,
    cache: RoleCache,
}

impl RoleResolver {
    pub fn new(kube_client: kube::Client) -> Self {
        Self { kube_client, cache: RoleCache::default() }
    }

    /// Pure mapping from PV access modes to a role. MUST reproduce the
    /// shipped c879bc3/d7490de predicate exactly: shared ⇔ the modes
    /// contain ReadWriteMany or ReadOnlyMany (flint's only shared
    /// implementation is the NFS path).
    pub fn role_from_modes<S: AsRef<str>>(modes: &[S]) -> Role {
        let mut rwm = false;
        let mut rom = false;
        for m in modes {
            match m.as_ref() {
                "ReadWriteMany" => rwm = true,
                "ReadOnlyMany" => rom = true,
                _ => {}
            }
        }
        if rwm {
            Role::NfsShared { read_only: false }
        } else if rom {
            Role::NfsShared { read_only: true }
        } else {
            Role::Block
        }
    }

    /// Resolve a STORAGE ID (== user PV name). Cache → PV access modes.
    /// `Err(reason)` when the PV is unreadable — caller applies its
    /// per-RPC default; the miss is NOT cached.
    pub async fn resolve(&self, storage_id: &str) -> Result<Role, String> {
        if let Some(role) = self.cache.get(storage_id) {
            return Ok(role);
        }
        use k8s_openapi::api::core::v1::PersistentVolume;
        let pvs: kube::Api<PersistentVolume> = kube::Api::all(self.kube_client.clone());
        let pv = pvs.get(storage_id).await.map_err(|e| e.to_string())?;
        let modes = pv
            .spec
            .as_ref()
            .and_then(|s| s.access_modes.clone())
            .unwrap_or_default();
        let role = Self::role_from_modes(&modes);
        self.cache.put(storage_id, role);
        Ok(role)
    }

    /// Resolve a full volumeHandle to a [`VolumeRef`]. Backing handles
    /// self-identify by prefix and never touch the API.
    pub async fn volume_ref(&self, handle: &str) -> Result<VolumeRef, RoleResolveError> {
        if let Some(storage_id) = parse_backing_handle(handle) {
            return Ok(VolumeRef::NfsBacking { storage_id: storage_id.to_string() });
        }
        match self.resolve(handle).await {
            Ok(Role::Block) => Ok(VolumeRef::Block { storage_id: handle.to_string() }),
            Ok(Role::NfsShared { read_only }) => {
                Ok(VolumeRef::NfsShared { storage_id: handle.to_string(), read_only })
            }
            Err(reason) => Err(RoleResolveError { handle: handle.to_string(), reason }),
        }
    }

    /// Deletion-time hygiene (see [`RoleCache`]).
    pub fn invalidate(&self, storage_id: &str) {
        self.cache.remove(storage_id);
    }
}

// ---------------------------------------------------------------------------
// Role hint (Phase 2) — CreateVolume stamps the canonical role into
// volume_context; context-carrying RPCs seed the resolver cache from it so
// the later context-free RPCs (ControllerUnpublish, NodeUnstage) classify
// without an API read. Old volumes without the hint resolve identically
// through the resolver — the hint is an optimization, never a
// compatibility surface.
// ---------------------------------------------------------------------------

/// volume_context / PV volumeAttributes key carrying the canonical role.
pub const ROLE_CONTEXT_KEY: &str = "flint.csi.storage.io/role";

const ROLE_BLOCK_VALUE: &str = "block";
const ROLE_NFS_SHARED_VALUE: &str = "nfs-shared";
const ROLE_NFS_SHARED_RO_VALUE: &str = "nfs-shared-ro";

/// Wire encoding of a role. Total — every role has exactly one encoding.
pub fn role_context_value(role: Role) -> &'static str {
    match role {
        Role::Block => ROLE_BLOCK_VALUE,
        Role::NfsShared { read_only: false } => ROLE_NFS_SHARED_VALUE,
        Role::NfsShared { read_only: true } => ROLE_NFS_SHARED_RO_VALUE,
    }
}

/// Inverse of [`role_context_value`]. Unknown values → `None` (treated
/// as "no hint": a future encoding must degrade to the resolver, never
/// to a guessed role).
pub fn parse_role_hint(value: &str) -> Option<Role> {
    match value {
        ROLE_BLOCK_VALUE => Some(Role::Block),
        ROLE_NFS_SHARED_VALUE => Some(Role::NfsShared { read_only: false }),
        ROLE_NFS_SHARED_RO_VALUE => Some(Role::NfsShared { read_only: true }),
        _ => None,
    }
}

/// The role hint carried in an RPC's volume_context, if any.
pub fn role_hint_from_context(
    ctx: &std::collections::HashMap<String, String>,
) -> Option<Role> {
    ctx.get(ROLE_CONTEXT_KEY).and_then(|v| parse_role_hint(v))
}

/// Canonical role from CSI volume_capabilities at CreateVolume time.
/// MUST agree with [`RoleResolver::role_from_modes`] under the k8s
/// translation (RWX ↔ MULTI_NODE_MULTI_WRITER, ROX ↔
/// MULTI_NODE_READER_ONLY) — the hint and the resolver are two encodings
/// of one fact, and the seed path depends on them never disagreeing.
/// Mirrors CreateVolume's shipped `is_rwx`/`is_rox` predicates exactly;
/// read-write wins over read-only, like `role_from_modes`.
pub fn role_from_csi_capabilities(caps: &[crate::csi::VolumeCapability]) -> Role {
    use crate::csi::volume_capability::access_mode::Mode;
    let mut rwx = false;
    let mut rox = false;
    for cap in caps {
        if let Some(am) = &cap.access_mode {
            if am.mode == Mode::MultiNodeMultiWriter as i32 {
                rwx = true;
            } else if am.mode == Mode::MultiNodeReaderOnly as i32 {
                rox = true;
            }
        }
    }
    if rwx {
        Role::NfsShared { read_only: false }
    } else if rox {
        Role::NfsShared { read_only: true }
    } else {
        Role::Block
    }
}

impl RoleResolver {
    /// Seed the cache from a context-carried role hint (ControllerPublish
    /// / NodeStage fast path). Only ever called with the canonical
    /// CreateVolume-stamped role, which by construction equals what
    /// `resolve` would compute from the PV — so seeding can never change
    /// a classification, only skip the API read that produces it.
    pub fn seed(&self, storage_id: &str, role: Role) {
        self.cache.put(storage_id, role);
    }
}

// ---------------------------------------------------------------------------
// Parsers — ownership classification (delegated to current owners; bodies
// move here in Phase 1 and the owners re-point).
// ---------------------------------------------------------------------------

/// Inverse of [`raid_name`]: the staging handle a raid bdev belongs to
/// (`raid_<handle>`), `None` for foreign bdevs. Mirrors the health
/// monitor's historical `strip_prefix` exactly (no emptiness check).
pub fn parse_raid_name(bdev_name: &str) -> Option<&str> {
    bdev_name.strip_prefix("raid_")
}

/// Classify a local lvol name to its owner (`None` = not flint-shaped,
/// never touched). Covers `vol_*` (+replica/hr suffixes), `epoch-*`,
/// `snap_*`, `temp_pvc_clone_*`, `eph_*`. Returns the id AS EMBEDDED —
/// resolve through [`storage_id_of_handle`] before PV lookups.
pub fn classify_lvol(name: &str) -> Option<Owner> {
    orphan_sweep::classify_lvol(name)
}

/// Classify a subsystem NQN to its owning volume id (unresolved). Covers
/// `:volume:<id>`, `:volume:<id>_<i>`, `:volume:<id>:replica:<i>`, and the
/// replica/hrpad embedded-owner ids.
pub fn classify_subsystem_nqn(nqn: &str) -> Option<String> {
    orphan_sweep::classify_subsystem_nqn(nqn)
}

/// Owner volume id of a snapshot lvol name (`snap_…`/`epoch-…`), `None`
/// for foreign shapes. Canonical Option form of the legacy
/// `SnapshotInfo::volume_id_from_snapshot_name` ("unknown" sentinel).
pub fn snapshot_owner(snapshot_name: &str) -> Option<String> {
    match SnapshotInfo::volume_id_from_snapshot_name(snapshot_name) {
        s if s == "unknown" => None,
        s => Some(s),
    }
}

// ---------------------------------------------------------------------------
// Tests — the live-shape corpus (fixtures mirror live RPC/SPDK output; the
// lvol-counter lesson) + agreement with the legacy owners.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Real ids from the runj cluster (v1.5.0 validation window).
    const VOL: &str = "pvc-a6846916-3267-404e-bb22-72d820652299";
    const NODE: &str = "runj-aws-1";
    const PCI: &str = "0000:00:1f.0";

    // -- handles ------------------------------------------------------------

    #[test]
    fn backing_handle_round_trip() {
        let backing = backing_handle(VOL);
        assert_eq!(backing, format!("nfs-server-{}", VOL));
        assert_eq!(parse_backing_handle(&backing), Some(VOL));
        assert_eq!(parse_backing_handle(VOL), None);
        assert_eq!(storage_id_of_handle(&backing), VOL);
        assert_eq!(storage_id_of_handle(VOL), VOL);
    }

    #[test]
    fn storage_id_agrees_with_record_pv_name() {
        for h in [VOL, "nfs-server-pvc-123", "pvc-123", "csi-ephemeral-x"] {
            assert_eq!(
                storage_id_of_handle(h),
                crate::replica_sync::record_pv_name(h),
                "divergence from record_pv_name for {h}"
            );
        }
    }

    /// Pinned: exactly ONE prefix is stripped. A hypothetical doubly
    /// prefixed handle is not a shape we mint; this documents (not
    /// endorses) current behavior.
    #[test]
    fn storage_id_strips_a_single_prefix() {
        assert_eq!(storage_id_of_handle("nfs-server-nfs-server-pvc-x"), "nfs-server-pvc-x");
    }

    // -- VolumeRef ----------------------------------------------------------

    #[test]
    fn backing_handles_never_consult_the_resolver() {
        let r = VolumeRef::from_handle(&backing_handle(VOL), |_| {
            panic!("resolver must not run for backing handles")
        });
        assert_eq!(r, VolumeRef::NfsBacking { storage_id: VOL.into() });
        assert!(r.has_block_path(), "the server's attachment IS the block path");
        assert!(r.is_backing());
        assert_eq!(r.storage_id(), VOL);
    }

    #[test]
    fn bare_handle_role_resolution() {
        let block = VolumeRef::from_handle(VOL, |_| Role::Block);
        assert_eq!(block, VolumeRef::Block { storage_id: VOL.into() });
        assert!(block.has_block_path());

        let rwx = VolumeRef::from_handle(VOL, |_| Role::NfsShared { read_only: false });
        assert_eq!(rwx, VolumeRef::NfsShared { storage_id: VOL.into(), read_only: false });
        assert!(!rwx.has_block_path(), "RWX clients have no block path (c879bc3)");

        let rox = VolumeRef::from_handle(VOL, |_| Role::NfsShared { read_only: true });
        assert!(!rox.has_block_path());
        assert_eq!(rox.backing_handle(), backing_handle(VOL));
    }

    // -- naming: exact live shapes + agreement with legacy owners -----------

    #[test]
    fn mints_match_live_shapes() {
        assert_eq!(lvol_name(VOL), format!("vol_{VOL}"));
        assert_eq!(replica_volume_id(VOL, 0), format!("{VOL}_replica_0"));
        assert_eq!(replica_lvol_name(VOL, 2), format!("vol_{VOL}_replica_2"));
        assert_eq!(raid_name(VOL), format!("raid_{VOL}"));
        assert_eq!(
            raid_name(&backing_handle(VOL)),
            format!("raid_nfs-server-{VOL}"),
            "RWX raid keys on the BACKING handle"
        );
        assert_eq!(epoch_snapshot_name(VOL, 12), format!("epoch-{VOL}-12"));
        assert_eq!(user_snapshot_name(VOL, 1719872000), format!("snap_{VOL}_1719872000"));
        assert_eq!(temp_clone_snapshot_name(VOL), format!("temp_pvc_clone_{VOL}"));
        assert_eq!(volume_nqn(VOL), format!("nqn.2024-11.com.flint:volume:{VOL}"));
        assert_eq!(replica_alias_nqn(VOL, 1), format!("nqn.2024-11.com.flint:volume:{VOL}:replica:1"));
        // Live LVS from runj: lvs_runj-aws-1_0000-00-1f-0
        assert_eq!(lvs_name(NODE, PCI), "lvs_runj-aws-1_0000-00-1f-0");
        assert_eq!(lvs_name_for_disk("runj-aws-1_0000-00-1f-0"), "lvs_runj-aws-1_0000-00-1f-0");
        assert_eq!(
            initiator_controller_name(&volume_nqn(VOL)),
            format!("nvme_nqn_2024-11_com_flint_volume_{VOL}"),
            "disk service filters local-storage scans on this prefix"
        );
    }

    #[test]
    fn mints_agree_with_legacy_owners() {
        assert_eq!(epoch_snapshot_name(VOL, 7), crate::replica_sync::epoch_name(VOL, 7));
        assert_eq!(hr_head_lvol_name(VOL, 1), crate::hot_rejoin::head_lvol_name(VOL, 1));
        assert_eq!(hr_head_lvol_name(VOL, 1), format!("vol_{VOL}_replica_1_hr"));
        assert_eq!(hrpad_export_id(VOL, 1), crate::hot_rejoin::pad_export_volume_id(VOL, 1));
        assert_eq!(hrpad_export_id(VOL, 1), format!("{VOL}_hrpad1"));
        assert_eq!(replica_export_nqn(VOL, 0), crate::hot_rejoin::replica_export_nqn(VOL, 0));
        assert_eq!(replica_export_nqn(VOL, 0), format!("nqn.2024-11.com.flint:volume:{VOL}_0"));
        assert_eq!(hotrejoin_export_nqn(VOL), crate::hot_rejoin::ef_export_nqn(VOL));
        assert_eq!(hotrejoin_export_nqn(VOL), format!("nqn.2024-11.com.flint:hotrejoin:{VOL}"));
        assert_eq!(node_host_nqn(NODE), crate::nvmeof_export::flint_host_nqn(NODE));
        assert_eq!(node_host_nqn(NODE), format!("nqn.2024-11.com.flint:node:{NODE}"));
        assert_eq!(
            initiator_controller_name(&hotrejoin_export_nqn(VOL)),
            crate::hot_rejoin::ef_controller_name(VOL),
            "E_f controller naming is the general initiator rule"
        );
    }

    // -- parsers: classification corpus over every live shape ---------------

    #[test]
    fn lvol_classification_corpus() {
        let cases: &[(&str, Option<Owner>)] = &[
            (&lvol_name(VOL), Some(Owner::Pv(VOL.into()))),
            (&replica_lvol_name(VOL, 2), Some(Owner::Pv(VOL.into()))),
            (&hr_head_lvol_name(VOL, 1), Some(Owner::Pv(VOL.into()))),
            // Backing-handle-derived lvol id stays UNRESOLVED (orphan_sweep
            // resolves via record_pv_name at lookup time, not parse time).
            ("vol_nfs-server-pvc-rwx", Some(Owner::Pv("nfs-server-pvc-rwx".into()))),
            (&epoch_snapshot_name(VOL, 12), Some(Owner::Pv(VOL.into()))),
            (&user_snapshot_name(VOL, 1719872000), Some(Owner::Pv(VOL.into()))),
            (&temp_clone_snapshot_name("pvc-new"), Some(Owner::Pv("pvc-new".into()))),
            ("eph_abc123", Some(Owner::Ephemeral)),
            ("Nvme0n1", None),
            ("vol_", None),
            ("epoch-pvc-x-notdigits", None),
        ];
        for (name, want) in cases {
            assert_eq!(&classify_lvol(name), want, "classify_lvol({name})");
        }
    }

    /// AUDIT FINDING L4 (phase-0 audit doc): the controller_operator
    /// replacement lvol `vol_<vol>_<timestamp>` classifies to a
    /// nonexistent-PV owner. Pinned here as CURRENT behavior — the
    /// orphan-sweep interaction is a Phase-1 fix-as-found item, and this
    /// test is the tripwire that must be UPDATED when it lands.
    #[test]
    fn replacement_lvol_shape_is_misclassified_today() {
        assert_eq!(
            classify_lvol(&format!("vol_{}_1719872000", VOL)),
            Some(Owner::Pv(format!("{}_1719872000", VOL)))
        );
    }

    #[test]
    fn nqn_classification_corpus() {
        let cases: &[(String, Option<&str>)] = &[
            (volume_nqn(VOL), Some(VOL)),
            (replica_export_nqn(VOL, 0), Some(VOL)),
            (replica_alias_nqn(VOL, 1), Some(VOL)),
            // Replica-volume-id and pad export ids embed their owner.
            (volume_nqn(&replica_volume_id(VOL, 1)), Some(VOL)),
            (volume_nqn(&hrpad_export_id(VOL, 1)), Some(VOL)),
            // Backing-handle export id stays unresolved (raw), like lvols.
            (volume_nqn("nfs-server-pvc-a"), Some("nfs-server-pvc-a")),
            // Outside the :volume: namespace ⇒ never touched.
            (hotrejoin_export_nqn(VOL), None),
            (node_host_nqn(NODE), None),
            ("nqn.2025-05.io.spdk:lvol-something".into(), None),
        ];
        for (nqn, want) in cases {
            assert_eq!(
                classify_subsystem_nqn(nqn).as_deref(),
                *want,
                "classify_subsystem_nqn({nqn})"
            );
        }
    }

    #[test]
    fn strict_ours_parsers() {
        assert_eq!(epoch_seq(VOL, &epoch_snapshot_name(VOL, 42)), Some(42));
        assert_eq!(epoch_seq("pvc-other", &epoch_snapshot_name(VOL, 42)), None);
        assert_eq!(epoch_seq(VOL, "epoch-pvc-x-"), None);
        assert_eq!(user_snapshot_ts(VOL, &user_snapshot_name(VOL, 99)), Some(99));
        assert_eq!(user_snapshot_ts("pvc-other", &user_snapshot_name(VOL, 99)), None);
    }

    // -- role resolution ------------------------------------------------

    /// The pure mapping must reproduce the shipped c879bc3/d7490de
    /// predicate cell-for-cell: shared ⇔ RWM || ROM present.
    #[test]
    fn role_from_modes_reproduces_the_shipped_predicate() {
        use RoleResolver as R;
        let cases: &[(&[&str], Role)] = &[
            (&["ReadWriteOnce"], Role::Block),
            (&[], Role::Block),
            (&["ReadWriteMany"], Role::NfsShared { read_only: false }),
            (&["ReadOnlyMany"], Role::NfsShared { read_only: true }),
            // RWM wins over ROM (read-write capability dominates).
            (&["ReadOnlyMany", "ReadWriteMany"], Role::NfsShared { read_only: false }),
            (&["ReadWriteOnce", "ReadWriteMany"], Role::NfsShared { read_only: false }),
            (&["ReadWriteOnce", "ReadOnlyMany"], Role::NfsShared { read_only: true }),
            (&["ReadWriteOncePod"], Role::Block),
        ];
        for (modes, want) in cases {
            assert_eq!(&R::role_from_modes(modes), want, "modes {:?}", modes);
            // Agreement with the literal predicate the sites shipped with.
            let legacy_shared = modes.iter().any(|m| *m == "ReadWriteMany" || *m == "ReadOnlyMany");
            assert_eq!(
                matches!(R::role_from_modes(modes), Role::NfsShared { .. }),
                legacy_shared,
                "divergence from the c879bc3 predicate for {:?}",
                modes
            );
        }
    }

    #[test]
    fn role_cache_semantics() {
        let cache = RoleCache::default();
        assert_eq!(cache.get(VOL), None);
        cache.put(VOL, Role::NfsShared { read_only: false });
        assert_eq!(cache.get(VOL), Some(Role::NfsShared { read_only: false }));
        cache.remove(VOL);
        assert_eq!(cache.get(VOL), None, "invalidation is deletion-only and total");
    }

    #[test]
    fn resolve_error_default_is_block_fencing() {
        let err = RoleResolveError { handle: VOL.into(), reason: "pv unreadable".into() };
        let vref = err.default_block();
        assert_eq!(vref, VolumeRef::Block { storage_id: VOL.into() });
        assert!(vref.has_block_path(), "unreadable PV ⇒ RWO fencing semantics (c879bc3)");
    }

    // -- role hint (Phase 2) ---------------------------------------------

    #[test]
    fn role_hint_round_trips_every_role() {
        for role in [
            Role::Block,
            Role::NfsShared { read_only: false },
            Role::NfsShared { read_only: true },
        ] {
            assert_eq!(parse_role_hint(role_context_value(role)), Some(role));
        }
        assert_eq!(parse_role_hint("nfs"), None, "unknown encodings degrade to no-hint");
        assert_eq!(parse_role_hint(""), None);
    }

    #[test]
    fn role_hint_from_context_reads_the_canonical_key() {
        let mut ctx = std::collections::HashMap::new();
        assert_eq!(role_hint_from_context(&ctx), None);
        ctx.insert(ROLE_CONTEXT_KEY.to_string(), "nfs-shared".to_string());
        assert_eq!(role_hint_from_context(&ctx), Some(Role::NfsShared { read_only: false }));
        ctx.insert(ROLE_CONTEXT_KEY.to_string(), "garbage".to_string());
        assert_eq!(role_hint_from_context(&ctx), None);
    }

    /// The seed-path invariant: capability-derived role (what CreateVolume
    /// stamps) must equal the modes-derived role (what the resolver reads
    /// off the PV) under the k8s access-mode translation.
    #[test]
    fn capability_role_agrees_with_modes_role_under_k8s_translation() {
        use crate::csi::volume_capability::access_mode::Mode;
        use crate::csi::{volume_capability::AccessMode, VolumeCapability};

        fn cap(mode: Mode) -> VolumeCapability {
            VolumeCapability {
                access_mode: Some(AccessMode { mode: mode as i32 }),
                access_type: None,
            }
        }

        // (CSI capabilities at CreateVolume, PV access modes the resolver sees)
        let pairs: &[(&[VolumeCapability], &[&str])] = &[
            (&[cap(Mode::SingleNodeWriter)], &["ReadWriteOnce"]),
            (&[cap(Mode::MultiNodeMultiWriter)], &["ReadWriteMany"]),
            (&[cap(Mode::MultiNodeReaderOnly)], &["ReadOnlyMany"]),
            (
                &[cap(Mode::MultiNodeMultiWriter), cap(Mode::MultiNodeReaderOnly)],
                &["ReadWriteMany", "ReadOnlyMany"],
            ),
            (&[], &[]),
        ];
        for (caps, modes) in pairs {
            assert_eq!(
                role_from_csi_capabilities(caps),
                RoleResolver::role_from_modes(modes),
                "hint/resolver divergence for modes {:?}",
                modes
            );
        }
    }

    #[test]
    fn seed_matches_resolver_cache_semantics() {
        let cache = RoleCache::default();
        cache.put(VOL, Role::NfsShared { read_only: false });
        // seed() is cache.put by definition — the hint can only ever
        // pre-fill what resolve() would compute.
        assert_eq!(cache.get(VOL), Some(Role::NfsShared { read_only: false }));
    }

    #[test]
    fn raid_name_round_trip() {
        assert_eq!(parse_raid_name(&raid_name(VOL)), Some(VOL));
        assert_eq!(
            parse_raid_name(&raid_name(&backing_handle(VOL))),
            Some(backing_handle(VOL).as_str()),
            "RWX raids parse back to the BACKING handle, resolution is the caller's job"
        );
        assert_eq!(parse_raid_name("Nvme0n1"), None);
    }

    #[test]
    fn snapshot_owner_wraps_the_unknown_sentinel() {
        assert_eq!(snapshot_owner(&user_snapshot_name(VOL, 123)).as_deref(), Some(VOL));
        assert_eq!(snapshot_owner(&epoch_snapshot_name(VOL, 3)).as_deref(), Some(VOL));
        assert_eq!(snapshot_owner(&temp_clone_snapshot_name("pvc-x")), None);
        assert_eq!(snapshot_owner("garbage"), None);
        // Agreement with the legacy String form.
        for name in [user_snapshot_name(VOL, 1).as_str(), "garbage"] {
            let legacy = SnapshotInfo::volume_id_from_snapshot_name(name);
            match snapshot_owner(name) {
                Some(id) => assert_eq!(id, legacy),
                None => assert_eq!(legacy, "unknown"),
            }
        }
    }
}
