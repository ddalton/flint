# Volume expansion — capability analysis (RWO / RWX / multi-replica / UI)

**Status:** **SHIPPED in v1.21.0 (2026-07-27) — multi-replica RWO fan-out,
RWX orchestration, and the device-size guard, LIVE-VALIDATED on the runah
cluster (drills 2.10 ×2, the degraded-refusal variant, and 3.11 — see §7).**
The analysis below is kept as the reference; per-section addenda mark what
shipped. Deferred by design: ublk online resize (§2.2 — now refused cleanly
instead of half-applying) and the dashboard UI (§4).
**Originally:** analysis — reflected `main` @ 2026-07-24.
**Revised:** §2/§3/§5 corrected after reading the actual SPDK source at
`/Users/ddalton/github/spdk` (v26.05.1-pre) — the "no SPDK raid-grow
primitive" framing was **wrong** (see §2.1). `raid1_resize` exists and works;
the real blockers are flint orchestration + the kernel-exposure layer.
**Re-verified 2026-07-27** against post-v1.20.0 `main` before implementation:
every mechanical claim held; §6's two prerequisites had LANDED (F43 closed);
three gaps found and fixed in this doc — the `is_nfs_emptydir` no-op arm was
missing from §1, the ublk stale-device half-apply was already live in the
shipped RWO path (§2.2a — the TL;DR's old "works today" verdict was
backend-unqualified), and the kuttl test surface (§7) was undocumented with
a verify hole that masked exactly that half-apply.
**Author:** produced with Claude (Opus 4.8, 1M context) via verified
multi-agent workflows — parallel source readers (controller/node mechanics,
RWX, multi-replica, dashboard UI, and the SPDK raid/nvme-of/ublk source),
each adversarially re-checked against source, then synthesized.
Re-verification + implementation: Claude (Fable 5, 1M context).
**Scope:** Flint CSI `ControllerExpandVolume` / `NodeExpandVolume`, the
`expand_refusal` gate, the SPDK raid1-over-lvols multi-replica stack, the
NFS-over-SPDK RWX stack, and the `spdk-dashboard` React UI.

**Questions answered:**

1. What is needed to support **RWX** volume expansion?
2. Is there a **UI menu item** for volume expansion (RWO and RWX)?
3. Does volume expansion support **replicas** (multi-replica volumes)?

**TL;DR (as implemented, v1.21.0):** Online expansion now works for
**single-replica RWO** (pre-existing), **multi-replica RWO** (new: claim +
sync-belt + per-leg fan-out), and **RWX** (new: controller-driven backing
chain) — on the **nvmeof** kernel-exposure backend (the shipping chart
default). On the **ublk** backend the kernel device cannot grow (no online
resize below kernel 6.16, §2.2) and the driver now REFUSES loudly at
NodeExpand instead of half-applying — before v1.21.0 the shipped RWO path
silently reported success over a never-grown filesystem there. The
dashboard still has no expand control (§4, deferred).

**TL;DR (original analysis, 2026-07-24):** Expansion works only for
**single-replica RWO block** volumes today. Multi-replica expansion fails
outright, RWX is explicitly refused, and the dashboard has no expand control
for either. **Correction to an earlier assumption:** these gaps are *not*
blocked by a missing SPDK primitive — SPDK v24.09+ (including the checked-out
v26.05) already has `raid1_resize`, and the lvol→NVMe-oF→`bdev_nvme`→raid
resize chain propagates automatically. The real work is (a) flint controller
orchestration to fan `resize_lvol` out across all replicas, and (b)
propagating the new size through the **kernel-exposure layer** (ublk today
has no resize path; the nvme-of loopback path can reuse the existing
`kernel_nvme_ns_rescan`). Effort: **moderate, not hard.**

---

## 1. How expansion works today (the RWO baseline)

Standard two-phase CSI resize; fully functional only for single-replica RWO
block volumes:

1. **Controller phase** — `ControllerExpandVolume` (`src/main.rs:2138-2259`)
   resolves the volume's storage node via `get_volume_info_from_pv`
   (`src/driver.rs:1640-1689`), which reads the PV attribute
   `flint.csi.storage.io/node-name`. It calls the node agent's
   `POST /api/volumes/resize_lvol` (`src/main.rs:2247`), which issues the SPDK
   `bdev_lvol_resize` RPC with `size_in_mib` (`src/minimal_disk_service.rs:411-415`).
   Size rounds **up** to MiB: `(new_size_bytes + 1048575) / 1048576`
   (`src/minimal_disk_service.rs:409`). On success returns
   `node_expansion_required: true` (`src/main.rs:2257`).
2. **Node phase** — `NodeExpandVolume` (`src/main.rs:4354-4443`) locates the
   mounted block device via `findmnt`, detects the filesystem via `blkid`,
   and runs `resize2fs` (ext2/3/4) or `xfs_growfs` (xfs) **online** on the
   mounted device (`src/main.rs:4416-4427`).
3. **Refusal / guard gates** (line refs @ v1.21.0):
   - `expand_refusal` (`src/identity.rs:186`): allows `VolumeRef::Block` —
     and, since v1.21.0, writable `NfsShared` (routed to the RWX
     orchestration, §3); still refuses read-only `NfsShared` (ROX) and
     `NfsBacking` (now "expand the parent RWX PVC instead" — the backing
     chain is driver-orchestrated).
   - pNFS shard-pinned volumes (`~m` suffix) refused at the top of the
     controller path.
   - **NFS emptyDir-backed volumes** (`nfs.flint.io/backend=emptydir` PV
     attribute) succeed as a controller-side **no-op** — emptyDir enforces
     no size, so there is nothing to grow (this arm was MISSING from the
     original analysis; it is what the `tests-nfs-only` kuttl suite
     exercises, §7).
   - Shrink prevented controller-side: `new_size_bytes <= current` returns
     early with `node_expansion_required: false`; the replicated path
     additionally refuses outright when the PV capacity is unreadable
     (`bdev_lvol_resize` would happily shrink — no guard, no resize).
   - **v1.21.0 claim gate:** every expand that reaches a mutation path runs
     under the volume's claim as `OP_EXPAND` (maintainer class — cutover /
     hot-rejoin preempt it, catch-up excludes it). Denial returns
     `Unavailable`; the resizer retries.
   - **v1.21.0 sync belt:** replicated volumes refuse to grow unless every
     replica is `in_sync` (`replica_sync::replicas_not_in_sync`; event
     `ExpandRefusedReplicasNotInSync`) — the C2 belt from the F43 ordering
     constraint.
   - NFS **client** mounts are a node-side no-op — `NodeExpandVolume` detects
     them via `fstype_is_nfs` and returns success immediately.
   - **v1.21.0 device-size guard (NodeExpand):** before any fs resize, the
     node verifies the kernel block device already reflects the target
     (`blockdev --getsize64`, short settle window for the AEN-driven
     rescan). A stale device — ublk backend, or an nvmeof rescan that never
     landed — now fails `failed_precondition` LOUDLY instead of letting
     `resize2fs` no-op "successfully" over the old size. Fail-open only
     when the probe tool itself is unavailable.
   - Raw block (`volumeMode: Block`, no filesystem): `blkid` returns empty and
     falls through to `Status::unimplemented("Unsupported filesystem type: ")`
     — a known gap, fails loudly rather than crashing.

**Online expansion:** yes — `resize2fs`/`xfs_growfs` run on the live mounted
device. There is still no explicit flint-issued NVMe-oF rescan in the expand
path; the kernel initiator's AEN handling grows the device, and the v1.21.0
device-size guard converts "the rescan never landed" from a silent
half-apply into a loud retryable failure.

---

## 2. Q3 — Does expansion support replicas? **YES since v1.21.0** (was: No)

**Implemented 2026-07-27:** `ControllerExpandVolume` dispatches on the
fetched PV's access modes (not the resolver's fallback — a shared volume
misread as Block must never fan out while skipping the server-side fs grow);
the block path takes the `OP_EXPAND` maintainer claim, resolves the replica
list (override-aware via `get_replicas_from_pv`), refuses unless every leg
is `in_sync` (the C2 belt), and fans `resize_lvol` to every replica node —
addressed by the **live** lvol uuid (`active_lvol_uuid` after a catch-up
revert; resizing the identity uuid would target a dead lvol). Partial
failure emits `ExpandReplicaFanoutIncomplete` and returns `Unavailable` —
same-size resize is a blobstore no-op, so the resizer's retry safely
re-drives it. Raid + namespace growth propagate automatically (§2.1,
unchanged). Legacy single-replica volumes keep the pre-v1.21 path
behavior-identical. Acceptance: chaos drill **2.10** (+ its run-after-2.1
degraded-refusal variant).

The original failure analysis (all confirmed in code before implementing):

Multi-replica expansion fails outright **today** — but not for the reason
originally assumed. It is a *flint orchestration* gap, not a missing SPDK
capability. The failure has two real causes in flint code (metadata lookup and
no replica fan-out), plus a kernel-exposure propagation gap; the SPDK raid
layer itself already supports online grow (see §2.1).

- **Metadata lookup fails (RWO multi-replica).** `ControllerExpandVolume`
  calls `get_volume_info` (`src/main.rs:2222`) → `get_volume_info_from_pv`
  (`src/driver.rs:1640-1689`), which **requires** the PV attribute
  `flint.csi.storage.io/node-name`. Multi-replica volumes (numReplicas > 1) do
  **not** store that attribute — they store `flint.csi.storage.io/replicas` as
  a JSON array instead (`src/main.rs:1297-1306`). Result: error `"PV found but
  missing flint metadata in volumeAttributes"` (`src/driver.rs:1682`),
  surfaced as `Status::failed_precondition("Volume metadata not found")`.

- **No replica iteration.** Even with the lookup fixed, the expand path resizes
  exactly ONE lvol on ONE node via a single `call_node_agent` (`src/main.rs:2240-2249`).
  There is no loop over legs. `get_replicas_from_pv` exists
  (`src/driver.rs:1537-1596`) and is used by DeleteVolume / NodeStage, but
  `ControllerExpandVolume` never calls it.

- **`raid_service.rs` wraps no resize (flint-side gap, not SPDK).**
  `src/raid/raid_service.rs` exposes only `create_raid1_bdev`,
  `delete_raid_bdev`, `get_raid_status`, and `add_base_bdev_to_raid`. But the
  SPDK raid module **does not need** a flint-issued resize call — it resizes
  itself on a base-bdev resize event (see §2.1). Nothing to add here beyond
  causing the base bdevs to grow.

- **No NVMe-oF namespace resize call in flint.** `src/nvmeof_export.rs` has no
  resize path — this is a flint gap. The SPDK *target* propagates a namespace
  size change to initiators automatically once the backing lvol grows (see
  §2.1); flint just needs to grow each replica's lvol (`resize_lvol` already
  exists) and, for the kernel-exposure layer, poke the consumer-side device.

**Failure-path priority:** a multi-replica **RWX** volume fails *earlier* at
the `expand_refusal` gate (`src/main.rs:2215-2220`) before the metadata lookup.
The metadata-lookup failure applies specifically to the rarer RWO multi-replica
configuration.

### 2.1 Correction: SPDK already has an online raid1-grow primitive

The earlier framing (inherited from `docs/incremental-replica-rebuild.md:760-764`,
citing spdk/spdk#3349) is **stale/incorrect for volume-size growth**. Verified
against the SPDK source at `/Users/ddalton/github/spdk` (v26.05.1-pre):

- **`raid1_resize` exists** (`module/bdev/raid/raid1.c:587-617`), wired as the
  raid1 module's `.resize` handler (`raid1.c:630`). It landed upstream in
  **SPDK v24.09**. spdk/spdk#3349 is about growing **replica count** (adding
  legs), a different feature — the doc conflated the two.
- **Fully online** — no I/O quiesce; a brief spinlock updates `blockcnt` and
  notifies descriptors. Grow-only in practice (`spdk_bdev_notify_blockcnt_change`
  rejects shrink with `-EBUSY` while descriptors are open, `lib/bdev/bdev.c:5737`).
- **Works with `superblock:false`** (flint's mode, `src/driver.rs:2771`):
  `data_offset` is 0, so it computes `min(base blockcnt)` and skips the
  superblock rewrite.
- **Degraded-safe** — absent legs (`desc==NULL`) are skipped (`raid1.c:594`);
  a 1/2 raid still grows on the surviving leg.
- **Event-driven, no RPC** — a base bdev firing `SPDK_BDEV_EVENT_RESIZE`
  auto-triggers `raid_bdev_resize_base_bdev` → `module->resize`
  (`module/bdev/raid/bdev_raid.c:2466-2515,2544`). There is no `bdev_raid_resize`
  RPC and none is needed.

**Automatic propagation chain (zero SPDK patches):** flint's raid1 legs are
SPDK `bdev_nvme` (userspace initiator, `src/driver.rs:2848-2914`), so:

> `bdev_lvol_resize` (`vbdev_lvol.c:1430`) → nvmf target `nvmf_ns_resize` +
> `NS_ATTR_CHANGED` AEN (`lib/nvmf/subsystem.c:2363`, `nvmf.c:1786-1831`) →
> `bdev_nvme` re-identifies NS and calls `spdk_bdev_notify_blockcnt_change`
> (`module/bdev/nvme/bdev_nvme.c:5326-5329`) → raid event handler → `raid1_resize`

fires end-to-end in milliseconds with no C changes.

**The one genuine SPDK-level gap — kernel exposure:**

- **`ublk` backend** (`spdk-csi-driver/helm` default): SPDK's ublk module
  ignores `SPDK_BDEV_EVENT_RESIZE` (`/Users/ddalton/github/spdk/lib/ublk/ublk.c:1530-1538`
  — `ublk_bdev_event_cb` handles only `REMOVE`; every other event, resize
  included, logs "Unsupported bdev event" and returns), so `/dev/ublkbN` stays
  at the old size while the raid bdev underneath has grown. **This is the real
  long pole** — see §2.2.
- **`nvmeof` backend** (shipping chart default, `flint-csi-driver-chart/values.yaml:426`
  — raid re-exported over a kernel nvme-tcp loopback): lighter — the kernel
  initiator needs an NS rescan, and flint already has `kernel_nvme_ns_rescan`
  (`src/node_agent.rs:1772-1787`).

### 2.2 ublk online-resize: the kernel floor and the two-part fix

Verified against the checked-out SPDK (`/Users/ddalton/github/spdk`,
v26.05.1-pre) and the upstream Linux `ublk_cmd.h` (tag bisect):

**The kernel primitive exists, but is new.** Linux added online ublk resize via
`UBLK_U_CMD_UPDATE_SIZE` (`_IOWR('u', 0x15, struct ublksrv_ctrl_cmd)`), gated by
the negotiated feature flag `UBLK_F_UPDATE_SIZE` (`1ULL << 10`); the new size is
passed in `cmd->data[0]` in **sectors**. It is **absent in Linux v6.15 and
present in v6.16** → the hard floor for ublk-backed online expansion is
**kernel ≥ 6.16**. This compounds flint's existing ublk caveat (README: `ublk_drv`
is missing entirely on some kernels, e.g. certain AWS 6.8 builds).

**SPDK needs *two* additions, not one.** The current SPDK ublk client
(`lib/ublk/ublk.c`) implements only eight control commands — `GET_DEV_INFO`,
`ADD_DEV`, `DEL_DEV`, `START_DEV`, `STOP_DEV`, `SET_PARAMS`,
`START_USER_RECOVERY`, `END_USER_RECOVERY` (`lib/ublk/ublk.c:67-74`) — and has
**no** `UPDATE_SIZE` command at all. So a proper fix is:
1. add a `SPDK_BDEV_EVENT_RESIZE` case to `ublk_bdev_event_cb` that reads the new
   `spdk_bdev_get_num_blocks`, and
2. add a new control command wrapping the kernel's `UBLK_U_CMD_UPDATE_SIZE`
   (0x15), submitted like the other `ublk_ctrl_cmd_submit` opcodes, with the
   device having negotiated `UBLK_F_UPDATE_SIZE` at `ADD_DEV` time.

This is an in-band SPDK patch (flint already carries ublk/raid SPDK patches), not
a new maintenance category — but it is a genuine C change plus a kernel-version
dependency.

### 2.2a Found at re-verification (2026-07-27): the half-apply was ALREADY live

The original TL;DR's "expansion works for single-replica RWO today" was
**backend-unqualified** — and on `backend=ublk` it was false in the worst
way: `ControllerExpandVolume` grew the lvol, the ublk device stayed at the
old size (§2.2), `resize2fs` no-opped **successfully**, and the PVC reported
the new capacity over a filesystem that never grew. Silent, shipped, and the
kuttl verify hole (§7) was shaped exactly wrong to catch it. Two compounding
drift hazards: the driver binary's built-in default when `BLOCK_DEVICE_BACKEND`
is absent is `"ublk"` (the chart always sets it — to `nvmeof` — but any
deployment path that drops the env inherits the hazard), and the chart
comment mislabeled ublk as the default (fixed alongside v1.21.0).

**v1.21.0 resolution:** the NodeExpand device-size guard (§1) — the third
"fail cleanly" recommendation below, implemented as a device-size assertion
rather than a feature-flag probe, which also covers "nvmeof rescan never
landed" for free.

**Take / recommendation (opinion; recommendations 2 and 3 SHIPPED v1.21.0):**
- Do **not** use the disruptive fallbacks for a mounted CSI volume: destroy +
  recreate the ublk device interrupts I/O, and a raw `/sys/block/ublkbN/size`
  write is *not* a supported interface (retracted from an earlier draft) — the
  capacity update must go through the kernel control channel (`set_capacity`),
  i.e. `UBLK_U_CMD_UPDATE_SIZE`.
- **Sequence the work:** ship multi-replica / RWO online grow on the `nvmeof`
  backend first (easy — reuse `kernel_nvme_ns_rescan`), and treat ublk online
  grow as a follow-up gated on the SPDK patch + kernel ≥ 6.16. *(Done — ublk
  online grow remains the deferred follow-up.)*
- **Fail cleanly** on ublk when the kernel lacks `UBLK_F_UPDATE_SIZE`: refuse
  the expand with a clear message (the same pattern flint already uses for
  absent capabilities) rather than resizing the lvol/raid and leaving the kernel
  device stale — a half-applied expand where the filesystem never sees the new
  space. *(Done, generalized: the device-size guard refuses ANY stale kernel
  device at NodeExpand time.)*

---

## 3. Q1 — What is needed for RWX volume expansion? **IMPLEMENTED v1.21.0** (was: refused)

**Implemented 2026-07-27 — controller-driven end to end (`expand_rwx`):**

1. **Block side:** the same claimed, belted, live-uuid fan-out as §2, run
   against the backing handle with records keyed on the parent user PV
   (the record home — the same key catch-up and cutover claim, so the
   claim actually excludes them).
2. **Backing PV `spec.capacity` patch** — the kubelet fs-grow trigger:
   kubelet's volume-manager populator compares it against the backing
   PVC's `status.capacity` for a mounted volume and drives
   `NodeExpandVolume` on the **server node** (the fs grow under the NFS
   export, protected by the device-size guard).
3. **Completion = backing PVC `status.capacity` reached the target** —
   kubelet stamps it only after the fs actually grew, so it doubles as the
   fs-growth proof. Until then the user-PV expand returns `Unavailable`
   and the resizer retries; on completion the backing PVC's `requests` are
   aligned (cosmetic) and the user expand succeeds with
   `node_expansion_required: false` (clients read statfs from the server).

**Why not delegate the backing chain to external-resizer** (the original
"cleanest fix" sketch): re-verification found the backing PV/PVC pair is
**statically provisioned** with `storageClassName: "flint"` — a match
string for pre-binding that need NOT exist as an SC object (standard chart
installs create `flint-spdk`/`flint-nfs`, not `flint`). The resizer looks
the SC up and would simply never act. Controller-driven avoids the
dependency entirely and keeps every step idempotent: a cutover mid-flow
(resolver — preempts our maintainer claim on the retry) relocates the
server pod and kubelet on the NEW node completes the pending grow; the
flow converges instead of colliding (C4 resolved).

Acceptance: chaos drill **3.11**. ROX stays refused (readers hold no
writable filesystem); direct backing-PVC expands stay refused with the
pointer to the parent (and the controller now resolves backing handles via
`pv_name_of_handle`, so that refusal actually surfaces instead of 404ing).

The original analysis (layer map still the reference):

`expand_refusal` returned (enforced as `failed_precondition`):

> "shared (RWX/ROX) NFS volume -- expansion is not yet supported: the
> filesystem lives under the NFS server's backing attachment and a client-side
> expand cannot apply"

RWX architecture: client pods mount NFS from a per-volume **NFS server pod**,
which holds a backing RWO PVC/PV over an SPDK lvol/raid volume. The NFS server
binary (`src/nfs_main.rs`, ~243 lines) is a pure NFSv4.2 export with no resize
hook, RPC endpoint, or sidecar.

**Ordered layers that must be resized:**

| # | Layer | What must happen | Exists today? |
|---|-------|------------------|---------------|
| 1 | Backing lvol(s) | Resolve user handle → backing handle (`nfs-server-<id>`, `src/identity.rs:46-52`), look up each replica's lvol via `get_replicas_from_pv` (`src/driver.rs:1537-1596`), `resize_lvol` on **each** replica node | Partial — `resize_lvol` RPC exists (`src/minimal_disk_service.rs:405-426`) but is called for one node only; replica lookup not wired into expand |
| 2 | Raid bdev | Grow `raid_nfs-server-<id>` to new member sizes | **Auto** — SPDK `raid1_resize` handles this on base-bdev resize (see §2.1); no flint or SPDK code needed once the legs grow |
| 3 | NVMe-oF namespace | Initiator sees the new block size | **Auto within SPDK** for `bdev_nvme` legs (target AEN → initiator re-identify, §2.1). Flint just calls `resize_lvol` per replica; `src/nvmeof_export.rs` needs no resize path |
| 4 | FS inside NFS server pod | `resize2fs`/`xfs_growfs` on `/mnt/volume` in the server pod | **Missing** — `src/nfs_main.rs` has no resize mechanism. Cleanest fix: patch the backing PVC capacity so kubelet drives `NodeExpandVolume` for the backing PV on the server's node. Alternatives: `kubectl exec` resize into the pod, or add a resize signal/RPC to `nfs_main`. NB: the backing PV is consumed via the kernel-exposure layer, so the §2.1 ublk/nvme-of caveat applies here too |
| 5 | NFS clients observe new size | Clients see updated `statfs` via NFSv4 GETATTR | **Free** — no action; `NodeExpandVolume` already no-ops NFS client mounts (`src/main.rs:4398-4402`) |

**Orchestration required:** for an `NfsShared` volume, `ControllerExpandVolume`
must remove the `expand_refusal` gate, resolve to the backing handle, resize all
replica lvols (the raid + namespace then grow automatically), propagate the size
through the kernel-exposure layer, grow the filesystem on the server's node
(patch the backing PVC/PV capacity), and return `node_expansion_required`
appropriately.

**Dependency:** RWX expansion builds on the same multi-replica orchestration as
Q3 (fan-out `resize_lvol` + kernel-exposure resize), plus the extra
NFS-server-pod filesystem-resize step. It is **not** gated on any missing SPDK
raid primitive.

---

## 4. Q2 — Is there a UI menu item for expansion? **No — neither RWO nor RWX.**

The `spdk-dashboard` (React/TS) has zero volume-capacity-expansion
functionality.

**What the UI exposes for volumes:**

- `VolumesTable` Actions column (`spdk-dashboard/src/components/tables/VolumesTable.tsx:417-441`):
  exactly two controls — a read-only **Details** button (opens `VolumeDetailAPI`)
  for managed volumes, and a trash-icon **Delete orphaned SPDK volume** for
  raw/unmanaged volumes.
- `VolumeDetailAPI` modal tabs (Overview / Replicas / RAID / Events / SPDK
  Details) are display-only — no mutation buttons.
- OpenAPI schema (`spdk-dashboard/src/api/schema.d.ts:247-262`): `/api/volumes`
  is GET-only; put/post/delete/patch are `never`.
- Dashboard backend (`src/spdk_dashboard_backend_minimal.rs`): the volumes
  route is GET-only (`warp::get()`); no resize/expand route in the `.or()` chain.

**All "expand"/"resize" string hits in the dashboard are UI state, not volume
capacity:**

- `expandedVolumes`, `expandedDisks`, `expandedGroups` — `Set<string>`
  accordion open/close state (e.g. `EnhancedSnapshotsTab.tsx`,
  `NodeDetailView.tsx`, `DiskSetupTab.tsx`).
- `BrushMode: 'resize-start' | 'resize-end'` — SVG timeline brush drag handle
  (`snapshots/timelineLayout.ts`).
- `ResizeObserver` — DOM width measurement.

**To add an expand control you would need:** a backend proxy route forwarding to
the node agent's `resize_lvol`; a UI dialog collecting `new_size_bytes` with
validation (must exceed current); access-mode gating per the `expand_refusal`
matrix (RWO-only until RWX lands); an OpenAPI schema update; and admin-role
authorization matching existing destructive routes (e.g. orphan delete).

---

## 5. Summary

| Question | Verdict @ v1.21.0 | Key evidence |
|----------|-------------------|--------------|
| **Q3 — Replica expansion?** | **IMPLEMENTED** — claim + sync belt + live-uuid fan-out; raid/namespace growth auto (`raid1_resize`). Was: failed on the `node-name` lookup with no fan-out. Live validation: drill 2.10. | §2 addendum; `spdk raid1.c:587-630`, `spdk bdev_raid.c:2466-2544` |
| **Q1 — RWX expansion needs?** | **IMPLEMENTED** — controller-driven backing chain (fan-out → backing-PV capacity patch → kubelet fs grow on the server node → completion by backing-PVC status). Was: explicitly refused. ROX + direct backing expands stay refused. Live validation: drill 3.11. | §3 addendum; `src/nfs_main.rs` (still no resize hook — none needed) |
| **Q2 — UI menu item?** | **No — unchanged, deferred.** Dashboard is read-only for volumes; "expand" hits are accordion state only. | `VolumesTable.tsx:417-441`, `schema.d.ts:247-262`, `src/spdk_dashboard_backend_minimal.rs` |
| **ublk backend** | **Refused loudly** (device-size guard) instead of the pre-v1.21 silent half-apply (§2.2a). Online ublk grow deferred: SPDK 2-part patch + kernel ≥ 6.16. | §2.2, §2.2a |

**Corrected verdict on the "keystone blocker":** there is **no** missing SPDK
raid-grow primitive — `raid1_resize` shipped in SPDK v24.09 and the
lvol→NVMe-oF→`bdev_nvme`→raid resize chain fires automatically (§2.1). The prior
`incremental-replica-rebuild.md:760-764` framing conflated volume-size growth
with spdk/spdk#3349 (replica-*count* growth), which is a different, still-open
feature. The genuine work for multi-replica online grow is **moderate**:
~100 lines of controller orchestration to fan `resize_lvol` out across replicas
(trivial — `get_replicas_from_pv` already exists), partial-failure retry
(moderate), and propagating the new size through the **kernel-exposure layer**
(the true long pole: trivial on the `nvmeof` backend via the existing
`kernel_nvme_ns_rescan`; on the `ublk` backend it needs a two-part SPDK patch
plus **kernel ≥ 6.16** for `UBLK_U_CMD_UPDATE_SIZE` — see §2.2). RWX then adds
the NFS-server-pod filesystem-resize step on top.

---

## 6. Prerequisite — this work is ordered behind F43 / v1.20.0 item #1

**SATISFIED AND HONORED (v1.21.0).** Both blockers landed in v1.20.0 — F43
closed with the leased+arbitrated claim registry (whose module header had
already reserved `OP_EXPAND` as the fourth claimant, maintainer class), and
item #8's leg-size guard shipped. How each constraint was honored:

- **C1** → every mutating expand runs under `try_claim(storage_id,
  OP_EXPAND)`; denial is `Unavailable` (resizer retries). Maintainer class:
  cutover/hot-rejoin preempt via reservation, catch-up excludes.
- **C2** → the `replicas_not_in_sync` belt refuses grow-while-degraded up
  front; downstream, a synergy the original analysis predates: the leg-size
  guard floors on **PV capacity**, which an expand raises — a leg that
  somehow missed the fan-out is refused at its next stage LOUDLY instead of
  silently shrinking the raid.
- **C3** → partial fan-out emits `ExpandReplicaFanoutIncomplete` and
  retries; same-size lvol resize is a blobstore no-op, so the re-drive is
  safe.
- **C4** → the backing-PVC-patch collision with `BounceNfsPod` dissolved
  into the claim system: cutover (resolver) preempts expand (maintainer),
  and the kubelet-driven fs grow re-converges on whichever node the server
  pod lands on.

The original constraint analysis (kept for the record; evidence lives in
the F43 doc under **"Ordering constraint — volume expansion must land after
#1"**):

Expansion and F43 (`docs/f43-rwx-replacement-admission.md`) are
**architecturally independent** — disjoint code paths, neither reads the
other's state — but the landing order is **not** free. In brief:

- **C1** — `ControllerExpandVolume` is today the only controller mutation path
  taking **no** per-volume claim (`main.rs:2240-2249`). A multi-replica fan-out
  expand must claim: `spdk_blob_resize` returns `-EBUSY` while a shallow copy
  holds the blob (`lib/blob/blobstore.c:8040-8051`), so an unclaimed expand
  mid-catch-up partially fails. But it cannot join the *current* expiry-less,
  priority-less `volume_claims.rs` without worsening F43's starvation → **F43's
  R2 arbitration first**, then register expand as a *maintainer*.
- **C2 — the data-integrity one.** Expanding during a degraded window makes the
  raid grow on the surviving legs only (`raid1_resize` skips `desc == NULL`,
  `raid1.c:594`). The stale leg's return then fails **two different ways**: a
  hot-add to a live raid is refused `-EINVAL` (`bdev_raid.c:3570-3573`) — a
  parked standby, loud, no data risk; but a NodeStage **reassembly**
  (`admit_standbys_at_stage`, `catchup.rs:1973`) rebuilds the raid at
  `min(leg blockcnt)` with **no error at all** (`raid1_start` — the `-EBUSY`
  shrink guard can't fire on a fresh bdev with no open descriptors), i.e. a
  **silent shrink under an already-grown filesystem**. This is now **v1.20.0
  item #8** (admission-side leg-size guard), which must land *before* this
  work; the expansion side owes the cheap belt — refuse the expand unless every
  replica is `in_sync`.
- **C3** — an undersized leg makes the next resize attempt a shrink, rejected
  `-EBUSY` (`bdev.c:5737-5741`) and logged only. Worth an emit.
- **C4** — RWX layer-4 (patch the backing PVC so kubelet drives the FS grow)
  collides with cutover's `BounceNfsPod`; reachable only once F43 lets cutover
  run for RWX-r2. Also opens a **second** `expand_refusal` arm
  (`VolumeRef::NfsBacking`, `identity.rs:194-196`).

**Attach/detach regression risk:** none material for the recommended first
slice (multi-replica RWO on `nvmeof` — `NodeExpandVolume` runs post-stage on an
already-mounted device and touches no staging/publish/export logic). The **ublk
patch is the real vector**: it edits `ublk_bdev_event_cb`
(`lib/ublk/ublk.c:1530-1538`), the same callback the DEL_DEV / F8 / F9 detach
work depends on — another reason to keep §2.2's sequencing (nvmeof first, ublk
last). *(v1.21.0 held to this: no staging/publish/export path was touched,
no SPDK patch shipped, the legacy single-replica expand path is
behavior-identical, and every refusal class — pNFS, emptydir, ROX,
NfsBacking — kept its semantics.)*

---

## 7. Test surface (added 2026-07-27 — undocumented in the original)

**Pre-existing kuttl suites** (`tests/system/`):
- `tests-standard/volume-expansion` — the RWO baseline regression (1Gi→2Gi
  under a writer, data-preservation check). **Verify hole, fixed in
  v1.21.0:** the old step dd'd 500M `|| true` — 500M fits in the ORIGINAL
  1Gi and the failure was swallowed, so a stale-device half-apply (§2.2a)
  passed the suite. It now asserts `df` grew past 1.5G and writes 1200M
  with failures fatal.
- `tests-nfs-only/volume-expansion` — RWO on the `flint-nfs` SC = the
  **emptydir no-op arm's** test (data preservation through the metadata
  no-op). Left as-is deliberately: emptyDir enforces no size, and dd'ing
  past "capacity" there just pressures the node root fs.

**Unit tests (v1.21.0):** the `expand_refusal` matrix update (writable RWX
passes, ROX/backing refuse); `replicas_not_in_sync` (in_sync passes,
stale/standby/unrecorded refuse — `replica_sync.rs`); `class_of(OP_EXPAND)
== Maintainer` was already pinned in `volume_claims.rs`. Suite: 864 green.

**Chaos drills — LIVE-VALIDATED 2026-07-27 on runah (4× i4i.xlarge spot,
us-west-1, k8s 1.34.10, driver `1.21.0-rc1`, backend nvmeof):**

- **2.10** (phase 2, RWO r2) — **PASS ×2**, 7/7 checks both times.
  20Gi→21Gi in 33s and 21Gi→22Gi in 49s, both under live pgbench writes.
  Consumer fs grew 19987M→20995M→22003M; raid stayed `online 2/2`; max
  ledger stall **1s** (the expand is genuinely online). On the wire after
  the second run, *both* nvme legs and the raid bdev read exactly
  23622320128 bytes = 22Gi — the lvol→nvmf→`bdev_nvme` re-identify→
  `raid1_resize` chain propagates with **no SPDK patch**, as §2.1 predicted.
- **2.10 degraded-refusal variant** — **PASS.** With one leg `stale`
  (induced by drill 2.9), the expand refused on the belt: `Warning
  ExpandRefusedReplicasNotInSync` on the PV naming the lagging leg,
  `VolumeResizeFailed` on the PVC carrying the `Unavailable` status and the
  "retries automatically" wording, PVC `status.capacity` unmoved at 22Gi,
  and **no partial fan-out** (the belt runs before the first `resize_lvol`).
  I/O was unaffected throughout. The claim registry behaved as F43 intends:
  `expand` (maintainer) logged `yielding to a reserved resolver operation`
  rather than ever holding the claim against a resolver.
- **3.11** (phase 3, RWX) — **PASS**, 7/7 checks. User PVC 21Gi→22Gi in 88s
  under writes; backing PVC `status.capacity` reached 23622320128 (the
  kubelet-stamped fs-growth proof); server-node fs 22003M; witness clean
  (0 mismatches); db PASS; max ledger stall 1s.
  Note the client mount is **not** a growth signal — statfs on the export
  root returns zeros before and after, which is exactly why the gate is the
  backing PVC status.

Two drill-script bugs were found and fixed by these runs: `"$CUR→$NEW"` in
both `NOTES=` strings (bash swallows the UTF-8 arrow into the variable name
and dies under `set -u` — now `${CUR}->${NEW}`), and 2.10's per-leg size
probe matching only bdev *aliases* (on the raid host the pv is in the bdev
*name* and the alias is a uuid, so the raid host silently reported "n/a").

**Not covered anywhere yet:** raw-block expand (`blkid`-empty gap, §1 —
pre-existing, unchanged), ublk-backend refusal end-to-end (needs a
`backend=ublk` cluster; the guard's unit surface is the size comparison
itself), UI (none exists).
