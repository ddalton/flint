# Volume expansion — capability analysis (RWO / RWX / multi-replica / UI)

**Status:** analysis — reflects `main` @ 2026-07-24. No code changes; this
documents the current expansion surface and what full support would require.
**Revised:** §2/§3/§5 corrected after reading the actual SPDK source at
`/Users/ddalton/github/spdk` (v26.05.1-pre) — the "no SPDK raid-grow
primitive" framing was **wrong** (see §2.1). `raid1_resize` exists and works;
the real blockers are flint orchestration + the kernel-exposure layer.
**Author:** produced with Claude (Opus 4.8, 1M context) via verified
multi-agent workflows — parallel source readers (controller/node mechanics,
RWX, multi-replica, dashboard UI, and the SPDK raid/nvme-of/ublk source),
each adversarially re-checked against source, then synthesized.
**Scope:** Flint CSI `ControllerExpandVolume` / `NodeExpandVolume`, the
`expand_refusal` gate, the SPDK raid1-over-lvols multi-replica stack, the
NFS-over-SPDK RWX stack, and the `spdk-dashboard` React UI.

**Questions answered:**

1. What is needed to support **RWX** volume expansion?
2. Is there a **UI menu item** for volume expansion (RWO and RWX)?
3. Does volume expansion support **replicas** (multi-replica volumes)?

**TL;DR:** Expansion works only for **single-replica RWO block** volumes today.
Multi-replica expansion fails outright, RWX is explicitly refused, and the
dashboard has no expand control for either. **Correction to an earlier
assumption:** these gaps are *not* blocked by a missing SPDK primitive — SPDK
v24.09+ (including the checked-out v26.05) already has `raid1_resize`, and the
lvol→NVMe-oF→`bdev_nvme`→raid resize chain propagates automatically. The real
work is (a) flint controller orchestration to fan `resize_lvol` out across all
replicas, and (b) propagating the new size through the **kernel-exposure
layer** (ublk today has no resize path; the nvme-of loopback path can reuse the
existing `kernel_nvme_ns_rescan`). Effort: **moderate, not hard.**

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
3. **Refusal / guard gates:**
   - `expand_refusal` (`src/identity.rs:186-198`): allows only
     `VolumeRef::Block`; refuses `VolumeRef::NfsShared` (RWX/ROX) and
     `VolumeRef::NfsBacking`.
   - pNFS shard-pinned volumes (`~m` suffix) refused at the top of the
     controller path (`src/main.rs:2155-2161`).
   - Shrink prevented controller-side: `new_size_bytes <= volume_info.size_bytes`
     returns early with `node_expansion_required: false` (`src/main.rs:2230-2237`).
   - NFS **client** mounts are a node-side no-op — `NodeExpandVolume` detects
     them via `fstype_is_nfs` and returns success immediately
     (`src/main.rs:4398-4402`).
   - Raw block (`volumeMode: Block`, no filesystem): `blkid` returns empty and
     falls through to `Status::unimplemented("Unsupported filesystem type: ")`
     (`src/main.rs:4429`) — a known gap, fails loudly rather than crashing.

**Online expansion:** yes for RWO — `resize2fs`/`xfs_growfs` run on the live
mounted device. Note there is **no** explicit NVMe-oF namespace rescan/reconnect
in the expand path; it relies on the kernel observing the new size implicitly.

---

## 2. Q3 — Does expansion support replicas? **No.**

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
  ignores `SPDK_BDEV_EVENT_RESIZE` (`module/ublk/ublk.c:1530` handles only
  REMOVE), so `/dev/ublkbN` stays at the old size. No post-create resize
  mechanism exists — needs a sysfs poke, ublk restart, or an upstream ublk
  patch. **This is the real long pole.**
- **`nvmeof` backend** (shipping chart default, `flint-csi-driver-chart/values.yaml:426`
  — raid re-exported over a kernel nvme-tcp loopback): lighter — the kernel
  initiator needs an NS rescan, and flint already has `kernel_nvme_ns_rescan`
  (`src/node_agent.rs:1772-1787`).

---

## 3. Q1 — What is needed for RWX volume expansion? **Currently refused.**

`expand_refusal` returns (`src/identity.rs:189-191`, enforced at
`src/main.rs:2215-2220` as `failed_precondition`):

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

| Question | Verdict | Key evidence |
|----------|---------|--------------|
| **Q3 — Replica expansion?** | **Not supported today, but not hard to enable.** Fails now because the multi-replica PV lacks `node-name` and there is no replica fan-out — both flint-side. SPDK's `raid1_resize` already exists and auto-propagates. | `src/main.rs:2222`, `src/driver.rs:1682`, `spdk raid1.c:587-630`, `spdk bdev_raid.c:2466-2544` |
| **Q1 — RWX expansion needs?** | **Explicitly refused.** Needs: fan-out lvol resize (partial), raid grow (**auto in SPDK**), NVMe-oF namespace resize (**auto for `bdev_nvme`**), NFS-server-pod FS grow (missing), client statfs (free). | `src/identity.rs:189-191`, `src/main.rs:2215-2220`, `src/nfs_main.rs`, §2.1 |
| **Q2 — UI menu item?** | **No — neither RWO nor RWX.** Dashboard is read-only for volumes; "expand" hits are accordion state only. | `VolumesTable.tsx:417-441`, `schema.d.ts:247-262`, `src/spdk_dashboard_backend_minimal.rs` |

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
`kernel_nvme_ns_rescan`; needs a real fix on the `ublk` backend, which has no
resize path). RWX then adds the NFS-server-pod filesystem-resize step on top.
