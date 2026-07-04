# Identity unification — Phase 0 audit & contract

**Status:** Phase 0 deliverable (2026-07-04). Companion to
`identity-unification.md`; line references are as of the Phase-0 commit.
`src/identity.rs` (this phase's code artifact) defines the canonical
vocabulary and pins every shape below in tests; no call site changed.

## 1. Role signals in use today — eleven mechanisms

The core finding: the driver currently answers "what kind of attachment is
this handle?" through **eleven** distinct mechanisms, each introduced by
the bug that needed it. Phase 1 collapses 1–8 into `VolumeRef::from_handle`
+ the cached role resolver; 9–11 are data-flow (not identity) signals and
stay, but get consumed *after* the ref is parsed.

| # | Signal | Sites | Phase-1 disposition |
|---|---|---|---|
| 1 | `nfs-server-` handle prefix | NodeUnstage main.rs:2574; `record_pv_name` (replica_sync:776) + its 9 callers; orphan_sweep:225 | `parse_backing_handle` / `storage_id_of_handle` |
| 2 | volume_context `originalVolumeId` | ControllerPublish main.rs:1443; NodeStage main.rs:2006 (both error InvalidArgument if absent) | retired — same fact as #1, parsed from the handle (attr stays written for debuggability) |
| 3 | volume_context `nfs.flint.io/enabled` | ControllerPublish main.rs:1470 (`is_rwx`) | resolver (access modes) |
| 4 | `nfs.flint.io/backend == emptydir` | ControllerPublish main.rs:1475; ControllerExpand main.rs:1905 | stays — backend variant, not role; consumed after parse |
| 5 | capability access_mode `MultiNodeReaderOnly` | ControllerPublish main.rs:1459 (`is_rox`) | resolver (`NfsShared{read_only}`) |
| 6 | volume_context `type == "nfs"` | NodeStage early-exit (~main.rs:2042) | resolver |
| 7 | `driver.pv_access_modes()` | ControllerUnpublish main.rs:1692 (c879bc3); NodeUnstage main.rs:2605 (d7490de) | THE resolver's source of truth, cached |
| 8 | findmnt fstype on staging path | NodeUnstage main.rs:2617 | survives only inside the resolver as the PV-unreadable fallback |
| 9 | publish_context `nfs.flint.io/server-ip` | NodePublish (~main.rs:2902) | stays — tells the client *where* to mount, not what it is |
| 10 | PV-object classification: `is_rwx_pv` / `nfs_backing_parent` (replica_sync:801/790) | cutover:769/783/805, epoch_scheduler:483, catchup:2285, hot_rejoin:2410, node_agent:1843/2628 | stays as the PV-object form; bodies co-located with identity.rs parsers |
| 11 | pNFS keys `pnfs.flint.io/mds-ip` | DeleteVolume main.rs:1226; NodePublish (~main.rs:2843) | out of scope — pNFS is a disjoint backend, checked before ref parsing |

## 2. The behavior matrix (the contract)

Exact current behavior per RPC × identity. **This table is the contract**:
Phase 1 must reproduce it cell-for-cell (divergence assertions during
transition); any cell change is a deliberate, documented decision.

Roles: **Block** = RWO user handle (or resolver default), **NfsShared** =
RWX/ROX *user* handle (attachments are NFS clients), **NfsBacking** =
`nfs-server-*` handle (the NFS server pod's own block attachment).

| RPC (site) | Block | NfsShared (user RWX/ROX) | NfsBacking (`nfs-server-*`) |
|---|---|---|---|
| CreateVolume (main.rs:892) | mint storage id, create lvols/replicas | same + `nfs.flint.io/*` context; server pod NOT created here | n/a — backing PV minted by rwx_nfs (:219,269) during publish flow, never by the provisioner |
| DeleteVolume (main.rs:1210) | replica/lvol/target teardown | `delete_nfs_server_pod` + bounded 90 s flush wait (567c582, rwx_nfs:647) → backing detach → same teardown (storage id is the same string) | never arrives — backing PV is driver-managed; refuse if ever seen |
| ControllerPublish (main.rs:1432) | export + host-fence to node (publish_context: NQN/addr) | ensure NFS server pod (1498–1544), wait ready, return `server-ip` context; ROX via signal #5 | resolve via `originalVolumeId` (#2) → block export + fence to the *server's* node |
| ControllerUnpublish (main.rs:1659) | if `volume_info.node_name != node_id` → `remove_nvmeof_target` (remote-consumer fencing) | **no-op** on the target (c879bc3 at 1692: departing party is an NFS client) | block-path unpublish bookkeeping; target lifecycle belongs to DeleteVolume |
| ValidateVolumeCapabilities (main.rs:1733) | static capability echo — role-independent | ← | ← |
| ControllerExpand (main.rs:1869) | PV lookup by handle; block expand via `get_volume_info` | emptydir backend → no-op (#4); pvc backend takes the block path — **finding L1** | never arrives today (backing PV has no PVC to resize) |
| CreateSnapshot (snapshot_csi:128) | `multi_replica_snapshot_name` (clamped `snap_…`), source = handle as passed | same; backing-shaped ids clamp correctly (snapshot_csi:727 test) | accepted if dashboard-driven — names embed the raw handle, resolution via `record_pv_name` downstream |
| NodeStage (main.rs:1989) | connect initiators, assemble raid `raid_<handle>`, device id from storage id (2138) | early-exit "NFS volume — mount happens in NodePublish" (#6, ~2042) | resolve via `originalVolumeId` (2006) → block stage; raid = `raid_nfs-server-<id>`; record/epoch keying resolves to user PV (driver.rs:1628) |
| NodePublish (main.rs:2798) | bind-mount staged device; ephemeral branch | NFS mount from `server-ip` publish context (#9); pNFS branch before both (#11) | n/a (server pod mounts via its own pod spec, not CSI publish) |
| NodeUnpublish (main.rs:3272) | lazy-then-bounded umount of target path — path-keyed, role-independent | ← | ← |
| NodeUnstage (main.rs:2564) | disconnect + raid teardown keyed FULL handle (2778), ublk id from storage id (2744) | unmount-only via #7→#8 (d7490de at 2596–2627) | strip prefix (2574) → block unstage; same full-handle/storage-id split — **finding L6** |
| NodeGetVolumeStats (main.rs:3559) | fs stats + `check_local_raid_health(handle)` | raid health on a handle whose raid exists nowhere — **finding L2** | raid health keyed on backing handle (correct — raid lives here) |
| NodeExpand (main.rs:3677) | findmnt → block device → nvme resize + resize2fs | findmnt yields an NFS source — **finding L3** | block path |

**Failure defaults (part of the contract, stated once):** unreadable PV ⇒
`Role::Block` — fencing semantics preserved (the c879bc3 choice) — except
NodeUnstage, which falls through to findmnt fstype before defaulting
(the d7490de choice). These are today's shipped defaults, centralized.

Background subsystems key on **storage identity only** and skip
backing/RWX PVs where consumer semantics would double-run; see §5.

## 3. Decision-site inventory (Phase-1 conversion queue)

Every site that today *decides* based on identity shape. "→" = what it
becomes.

| Site | What it decides today | → Phase 1 |
|---|---|---|
| main.rs:1442–1452 (CtrlPublish) | backing via `originalVolumeId`, errors if attr missing | `VolumeRef::from_handle` |
| main.rs:1459–1475 (CtrlPublish) | is_rox (#5) / is_rwx (#3) / emptydir (#4) | ref match + backend flag |
| main.rs:1692–1712 (CtrlUnpublish) | shared no-op vs remote fencing (#7) | `ref.has_block_path()` |
| main.rs:2005–2016 (NodeStage) | backing via `originalVolumeId` | `VolumeRef::from_handle` |
| main.rs:~2042 (NodeStage) | NFS early-exit (#6) | `NfsShared` arm |
| main.rs:2574–2627 (NodeUnstage) | strip (#1) + shared-consumer (#7→#8) | ref parse + resolver |
| main.rs:2744 / 2778 (NodeUnstage) | ublk from storage id vs raid from full handle | `ref.storage_id()` / staging handle — preserve exactly (L6) |
| main.rs:1226 / 2843 (pNFS detect) | #11 | unchanged, hoisted before ref parse |
| driver.rs:1567/1580/1628 | record/annotation PV via `record_pv_name` | `storage_id_of_handle` |
| node_agent.rs:1851–1860 | spdk_id = volumeHandle vs PV name for k8s lookups | ref: staging handle vs storage id |
| node_agent.rs:1984–1987 | raid strip `raid_` → `record_pv_name` | identity parsers |
| node_agent.rs:1843 / 2628–2639 | is_rwx skip; raid_present on volumeHandle | unchanged semantics, shared helpers |
| replica_sync.rs:857/929 | record resolution | `storage_id_of_handle` |
| orphan_sweep.rs:225 | existence check resolves via `record_pv_name` | `storage_id_of_handle` |
| cutover.rs:769/783/805; epoch_scheduler.rs:483; catchup.rs:2285; hot_rejoin.rs:2410 | backing-PV skip / RWX classification (#10) | unchanged; helpers co-located |
| snapshot paths (snapshot_csi:104/165/694; main.rs:765) | names embed handle-as-passed | mint via identity.rs; resolution documented |

Count: ~25 decision sites, matching the plan's estimate. The other ~700
`volume_id` references are pass-through and don't decide anything.

### Phase-1 status (2026-07-04): CONVERTED

All queue rows above are done. The resolver (`identity::RoleResolver`,
embedded in `SpdkCsiDriver`) now backs ControllerUnpublish and
NodeUnstage; backing-handle parses in ControllerPublish/NodeStage/
NodeUnstage/ControllerUnpublish go through `parse_backing_handle`/
`storage_id_of_handle`; driver/node_agent/orphan_sweep helper callers
re-pointed. Deliberate deltas from shipped behavior (all
impossible-on-real-inputs or contract-mandated):

- **L5 unified**: `originalVolumeId` is no longer load-bearing — the
  handle is parsed directly. The attr survives as a transitional
  `IDENTITY-DIVERGENCE` assertion (grep target for Phase 3) and a
  debugging aid. Degenerate delta: a backing PV *missing* the attr used
  to fail InvalidArgument, now works.
- **DeleteVolume refuses backing handles** (matrix cell was "never
  arrives"; now enforced instead of aliased teardown).
- **DeleteVolume invalidates the role cache** on every success return.
- Signals #3/#5/#6 (publish-side context signals) deliberately NOT
  moved to the resolver in Phase 1 — they are CO-authoritative at
  publish time and behavior-identical conversion is not guaranteed
  (e.g. an RWO PVC under an nfs-enabled SC). Phase 2's CreateVolume
  role hint is the honest unification point for those.
- `replica_sync.rs:857/929` unchanged — that module owns the canonical
  body `storage_id_of_handle` delegates to (bodies migrate in Phase 4).

### Phase-3 status (2026-07-04): LIVE-VALIDATED on cluster `runk`

Fresh all-spot cluster (trove project 28, 4× i4i.large incl. CP —
SPDK-eligibility keys off the CP type). Build `identity-p3.0` = Phases
0–2 @169521b; `p3.1`/`p3.2` add the fix-as-found batch below. Results:

- Upgrade ride-through: a 1.5.0-created, hint-less RWO volume survived
  every roll (1.5.0→p3.0→p3.1→p3.2), old data intact, writes landing;
  final teardown clean through the unified path.
- Full kuttl gate (8 standard + clean-shutdown): PASS. rwx-single-replica
  + rox-multi-pod re-run individually: PASS with zero
  Terminating/lingering flint-nfs pods after each.
- Drill C (server bounce, staged clients): clients resume without
  remount over the stable Service. FINDING (recorded, open): a BARE
  server-pod delete has no re-creator — recreation belongs to cutover or
  the next client publish; pod-only death strands clients until then.
- Drills A/A′ (server killed, then client teardown) found THREE
  pre-existing P1/P2s, all fixed + re-validated live:
  - **F1 (e67563b)** NodeUnpublish read a timed-out `mountpoint -q`
    (exit 124 — the dead-hard-NFS signature) as "not mounted", skipped
    the lazy unmount, returned success; kubelet EBUSY-looped forever and
    the node stopped admitting pods. Fix: only clean exit 1 skips;
    verdict helper + truth-table test in mount_util. Validated: A′
    drained in 39 s unassisted ("Treating target path as mounted: true"
    → "Lazy unmount succeeded").
  - **F2 (a78c79c)** NodeGetVolumeStats ran blocking Path::exists +
    statvfs on the runtime; each kubelet poll against a dead mount
    pinned a worker in D-state until the liveness probe starved —
    crash-looping every node with a dead client mount (audit L2 made
    real) and killing the co-located node agent (remote attaches EIO).
    Fix: spawn_blocking + 5 s timeout, timeout ⇒ condition ABNORMAL.
    Validated: two dead mounts held for 20+ min at zero restarts.
  - **F3 (7e75419)** ControllerPublish raced a Terminating server pod:
    `nfs_pod_exists` counted it present, the ready-wait accepted it
    (draining pods still report phase Running), publish bound the new
    client to a Service whose backend vanished. Fix: tri-state
    `nfs_pod_liveness`; Terminating ⇒ bounded wait-for-gone then create
    fresh; ready-wait rejects deletionTimestamp. Validated: revival
    publish created a fresh server in ~10 s.
- Drill B (PVC delete during client churn, 4 live clients): full drain —
  pods/PVC/server-pod/PV all to zero; log shows the 567c582 ordering
  (server pod flushed and terminated before storage teardown).
- Drill D (RWO consumer beside the NFS server on one node): departure
  from the lvol node classified local/no-op; departure from the server
  node took the FENCING branch and removed exactly the RWO volume's
  target while the server kept serving — Block vs NfsShared branch
  separation proven side by side.
- `IDENTITY-DIVERGENCE` sweep: **zero lines** on the controller and all
  five node plugins across the entire campaign. The transitional
  assertions have earned removal (fold into Phase 4 with the lint).
- Kernel-semantics note (not a bug): a writer caught mid-write on a dead
  hard mount is unkillable (D-state) until the server returns; with F1/F2
  the node now stays healthy and the pod drains the moment I/O resolves
  (or immediately after force-delete, via the orphan-cleanup path).

### Phase-2 status (2026-07-04): SHIPPED

CreateVolume stamps `flint.csi.storage.io/role` =
`block|nfs-shared|nfs-shared-ro` into every returned volume_context
(main path, emptydir path, both clone paths; pNFS excluded per #11).
The value is derived from CSI capabilities via
`role_from_csi_capabilities`, test-pinned to agree with the resolver's
`role_from_modes` under the k8s access-mode translation — so a seeded
cache entry can never differ from what the resolver would have
computed. ControllerPublish and NodeStage seed their process-local role
caches from the hint (NodeStage warms the node cache for the
context-free NodeUnstage — the RPC that matters during teardown
storms). ControllerPublish carries a transitional `IDENTITY-DIVERGENCE`
assertion comparing the hint with the legacy publish signals, scoped to
bare handles and non-emptydir volumes (an emptydir RWO volume serves a
Block role over NFS by design; a backing PV inherits the USER volume's
hint through rwx_nfs's attr copy — both documented in-code). Branch
decisions still ride the legacy signals; retiring them is Phase 3+
work, gated on the assertion staying silent on a live cluster.

## 4. Naming inventory

Mints outside the canonical owners, to be re-pointed in Phase 1 and linted
in Phase 4 (`format!("vol_` / `"nqn.` / `"raid_` / `"epoch-` / `"snap_` /
`"lvs_` outside identity.rs and its co-located owners):

- `vol_<id>`: main.rs:601,791; driver.rs:310,448; controller_operator.rs:412 (**L4 shape**: `vol_<id>_<ts>`); minimal_disk_service.rs:292; node_agent.rs:1463; dashboard 2289,2301.
- `raid_<handle>`: driver.rs:1722,2480,2533; catchup.rs:2157; node_agent.rs:2441,2639,2856; hot_rejoin.rs:324,1482,1561; dashboard 1228,3246.
- `nqn…:volume:<id>`: main.rs:1360,1421,1712,2753,3506; driver.rs:1087,2291,2522,2545; node_agent.rs:1619,2452,2912; catchup.rs:2547; replica_sync.rs:701; hot_rejoin.rs:3035; alias form `…:volume:<id>:replica:<i>` main.rs:1310.
- `lvs_<…>`: minimal_disk_service.rs:178 (node+pci mint); controller_operator.rs:208,411,571.
- Foreign-namespace NQN: controller_operator.rs:462 `nqn.2025-05.io.spdk:lvol-*` (replacement-disk flow; invisible to flint sweepers by construction — note, not a bug).
- Initiator controller names `nvme_<sanitized nqn>`: driver.rs:2334; hot_rejoin.rs:191; prefix-filter minimal_disk_service.rs:1928.
- Canonical owners already in place (identity.rs delegates): replica_sync `epoch_name`/`epoch_seq`/`user_snapshot_ts`/`record_pv_name`/`nfs_backing_parent`/`is_rwx_pv`; hot_rejoin naming block (:180–220); nvmeof_export `flint_host_nqn`; orphan_sweep classifiers; snapshot_models parser + snapshot_csi `multi_replica_snapshot_name`/replica_sync `parse_user_snapshot_id`.

## 5. Background subsystems — which identity `volume_id` means

| Subsystem | Keying | Notes |
|---|---|---|
| epoch_scheduler | storage id (user PV) | skips backing PVs (483) — one epoch stream per volume (637be1c fix) |
| catchup | records: storage id; raid/export: staging handle | skips backing PVs (2285); staged-handle→record via record_pv_name (1788) |
| cutover | storage id; RWX ⇒ NFS-pod bounce | classification at 769–805 |
| hot_rejoin | storage id throughout; raid on staging handle | rwx/nfs_backing captured into its view (2410) |
| replica_sync | storage id (record on user PV) | resolution at entry (857/929) |
| orphan_sweep | embedded id, resolved at existence-check time (225) | classifiers are the canonical parsers |
| controller_reap (7b-0) | `:volume:` NQN namespace | hotrejoin NQNs deliberately outside it |
| node_agent health monitor | k8s: PV name; SPDK: volumeHandle | the 1851 split is the general rule stated in one comment today |
| dashboard backend | storage id for display; parses `vol_`/`raid_` | adopts shared parsers in Phase 4 |
| controller_operator | lvs via disk_ref; replacement lvol L4 | replacement flow predates naming conventions |

## 6. Latent findings (fix-as-found queue, Phase 1)

Recorded, not fixed — Phase 0 is no-behavior-change. Each is the existing
bug class caught at audit time instead of live:

- **L1 — ControllerExpand, pvc-backed RWX**: only the emptydir backend
  short-circuits; a pvc-backed RWX user handle takes the block-expand
  path. Lvol resize by storage id is plausibly correct (same lvols), but
  the raid/fs layer lives under the backing attachment on the server
  node. Verify on a live cluster; likely needs an NfsShared arm that
  resizes via the server.
- **L2 — NodeGetVolumeStats on RWX client**: `check_local_raid_health`
  keyed on the user handle; `raid_pvc-X` exists on no node (the raid is
  `raid_nfs-server-pvc-X` on the server). Verify what "abnormal" it
  reports; NfsShared arm should report fs stats only.
- **L3 — NodeExpand on an NFS mount**: findmnt returns an NFS source, the
  nvme-resize path then operates on a non-device string. Probably fails
  loudly (harmless); should be an explicit NfsShared no-op.
- **L4 — replacement lvol `vol_<id>_<ts>`** (controller_operator:412):
  `classify_lvol` yields owner `<id>_<ts>` — a nonexistent PV. Orphan
  sweep's in-use protections likely shield it, but the name is
  sweep-illegible. Pinned in identity.rs test
  `replacement_lvol_shape_is_misclassified_today` (the tripwire to update
  when fixed).
  **VERIFIED UNREACHABLE (2026-07-04):** no production code path invokes
  `controller_operator` (only comments reference it); its dedicated bin
  target is commented out of Cargo.toml; the deployed
  `spdk-controller-operator` pod ran the standard driver with a
  URL-shaped `SPDK_RPC_URL` handed to a Unix-socket client — every SPDK
  call fails, so even the module's loops could never act (observed live
  on runk: connection errors every monitor tick). The chart now defaults
  `spdkOperator.enabled: false`. The tripwire test stays as the guard if
  the flow is ever revived — revival requires making the mint
  identity-legible first.
- **L5 — two parsers for one fact**: ControllerPublish/NodeStage read
  `originalVolumeId` (error if absent) while NodeUnstage strips the
  prefix. Same information; unify on the prefix parse (context stays as
  a debugging attr).
- **L6 — NodeUnstage split keying**: stage/raid names use the FULL handle
  (2778) while ublk ids use the storage id (2744). Correct today —
  preserve EXACTLY through Phase 1; the matrix cell is the spec.
