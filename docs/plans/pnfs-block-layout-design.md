---
title: pNFS block/SCSI layout over NVMe (RFC 9561) — the fast tier
status: designed
type: design-impl-spec
tags: [pnfs, block-layout, nvme, spdk, mds, tla]
created: 2026-08-09
governs:
  - spdk-csi-driver/src/pnfs/mds/layout.rs
  - spdk-csi-driver/src/nfs/v4/dispatcher.rs
  - spdk-csi-driver/src/nfs/v4/operations/fileops.rs
  - spdk-csi-driver/src/pnfs/mds/operations/mod.rs
  - spdk-csi-driver/src/state_backend/sqlite.rs
  - spdk-csi-driver/src/nvmeof_export.rs
  - formal/FlintExtents.tla (new)
  - formal/FlintExtentsProbe.tla (new)
  - flint-csi-driver-chart/templates/pnfs-block-storageclass.yaml (new)
---

# pNFS block/SCSI layout over NVMe — design

## 1. Summary

flint grows a **second pNFS layout type**: the RFC 9561 block/SCSI layout over NVMe,
surfaced as a per-StorageClass fast tier (`layout: pnfs-block`). The file layout stays
as-is and remains the general-purpose RWX tier; nothing existing is removed, demoted, or
re-plumbed. The two classes coexist under one pNFS machinery, selected per volume.

What the block layout buys: it deletes the DS from the data path entirely. Today a read
is `client → sunrpc → DS nfs server → buffered pread → page cache → ext4 → device`.
Under block layout it is `client → NVMe/TCP → spdk-tgt → io_uring → device`. The MDS
becomes an extent allocator; the client does raw block I/O against extents it holds
layouts for.

**Naming correction, up front**: the outline name "block layout" is how we'll talk about
it, but RFC 9561 is a *mapping* document over RFC 8154 (§1: it "does not amend the
existing SCSI layout document"). The wire layout type is **LAYOUT4_SCSI (= 5)**, not
LAYOUT4_BLOCK_VOLUME. Our `LayoutType` enum currently has **BlockVolume=2 and
Osd2Objects=3 swapped versus RFC 8881 §3.3.13** — the `layouttype4` definition, where
the numeric values are actually assigned (real values: OSD2=2, BLOCK=3;
`layout.rs:570-583`, with the test at `layout.rs:2857-2861` asserting the wrong values).
The enum's own doc comment cites §12.2.3 — that's the "pNFS Client" *definition*, not
the values; fix the citation with the enum.
Latent today because only type 1 is served; wire-visible the moment we advertise anything
else. **Fix the enum first, and serve type 5.**

This unifies flint's two data planes. Both tiers end at spdk-tgt; durability for the
block tier is block-level replication under the namespace instead of the file tier's
DS-on-replicated-PVC layering. **Honest caveat on that claim**: today's raid1 is
assembled on the *consumer* node's tgt (`driver.rs:3161-3236`) — a remote pNFS client
cannot see it. Block-layout volumes are single-replica lvols until replication moves
server-side (storage-node raid or MDS-level mirroring). That is real work, scoped in
§12, not a freebie.

## 2. Motivation — the taxes this deletes

Every number below is measured, with its campaign named. See
`docs/oci-registry-pnfs-architecture.md` §2/§5 for the ceiling table.

- **DS buffered pread / page cache**. The DS-path bracket (local, lima/loop rigs)
  measured the page cache at **~5.9x cpu/byte** versus O_DIRECT on the DS's read path
  (~10-12% of DS CPU at those rates). runbk made it worse than a tax: a **fully-cached
  RAM read ran at 1470 MiB/s, slower than cold and under half of O_DIRECT's 3173** —
  an outright loss, not an accelerator. Block layout removes the DS's read()/page-cache
  path from the data plane entirely.
- **NFS READ encode/copy**. The runaz/runba copy-tax investigation cut 71.3% of DS CPU
  by fixing one allocator/copy path — the encode/copy machinery is a first-order cost.
  Block reads are DMA-shaped NVMe transfers; no XDR on the data path.
- **sunrpc per-connection ceiling: ~700-900 MiB/s/conn** (runbi). nconnect trunking
  claws it back (nconnect=4 saturated a 25Gbps-class link at 2857 MiB/s, runbd) but each
  connection remains a single-threaded RPC pipe. NVMe/TCP queues don't have this shape.
- **The kernel nfs_client wall: ~5-6 GB/s per client node regardless of DS count**
  (runbh established the wall at any DS count; runbi's single-client trunking peak was
  5634 MiB/s). One `nfs_client` instance per (server, node) is the choke. The kernel
  blocklayout client already sidesteps sunrpc for data; the later userspace client (§4b)
  sidesteps the kernel entirely.
- **One data plane**. Today flint operates two: the SPDK/NVMe-oF block plane (RWO) and
  the pNFS DS plane (files over sunrpc). Block layout ends both tiers at spdk-tgt — one
  set of observability (`bdev_get_iostat`, `thread_get_stats` — runbk/runbm precedent),
  one restart-coordination story, one fencing substrate.

What we are *not* claiming: that the file layout is slow in absolute terms. ADR 0004
measured 6.02x/4.00x cross-host scaling and the runbl ladder showed flint at 87-105% of
raw device on its rig. The block tier is for workloads that hit the per-client walls
above or that pay the DS CPU bill at fleet scale.

## 3. Protocol background

pNFS (RFC 8881 §12) makes layout types per-filesystem pluggable under one machinery:
one MDS, one state model (layout stateids, CB_LAYOUTRECALL, LAYOUTCOMMIT), N layout
classes. The MDS stays a full NFSv4.1/4.2 server for both classes — metadata, locking,
ACLs, and fallback I/O all remain NFS. Clients mount the MDS over NFS exactly as today
(`NodePublish` is reused as-is; the new work is device visibility, not the mount).

Layout advertisement is per-filesystem via `FATTR4_FS_LAYOUT_TYPES` (attr 62). Today we
hardcode `[1]` at **two** encoder sites — `fileops.rs:1213-1236` (snapshot encoder) and
`fileops.rs:1394-1406` (the pseudo-root/server-capabilities encoder, the arm a mounting
client's fsinfo GETATTR actually hits). The dispatcher's own comment
(`dispatcher.rs:2300-2304`) already warns these sites must agree — reconcile its "three
sites" count while there (two arms emit the array; the 1130/1376 hits are
supported-attr bitmap words). This becomes per-volume **at every emitting site** —
better, one shared advertisement helper, so the encoders cannot diverge. **Graceful
degradation is native to the protocol**: a client that lacks blocklayout support (no
`CONFIG_PNFS_BLOCK`, no device visibility) either negotiates the file layout — if we
choose to advertise both on block-class volumes — or simply does MDS I/O. RFC 8881
*entitles* layoutless clients to MDS I/O; F66 taught us that refusing it is the bug
class (`docs/plans/mds-fallback-proxy-plan.md` §2 — do not re-litigate the cheap fixes).

The stack of documents: RFC 8154 defines the SCSI layout — extents, volume topology,
LAYOUTCOMMIT semantics, PR-based fencing. RFC 9561 maps it onto NVMe: NGUID/EUI64
device identification and NVMe Reservations in place of SCSI PRs, transport-independent
(PCIe, RDMA, TCP, FC — §1). All XDR is RFC 8154's, unchanged.

## 4. Client story — two steps

### 4a. Stock kernel client first

`fs/nfs/blocklayout` handles LAYOUT4_SCSI with NVMe device matching **mainline since
v6.11** (released Sep 2024; commit `3921ae0850a3`, Hellwig, authored Jul 2024). No
blkmapd needed for SCSI/NVMe volumes — resolution is fully in-kernel. Zero client
software to ship. The client node needs: `CONFIG_PNFS_BLOCK`, kernel nvme-tcp sessions
to the storage nodes, and device visibility. csi-node manages the sessions (§5),
reusing the existing `kernel_nvme_connect` machinery — it is already target-agnostic;
only its caller hardcodes loopback (`node_agent.rs:2050-2094`). And v6.11 is a hard
**kernel floor**, not trivia: current LTS fleets sit below it (Ubuntu 24.04 ships 6.8)
and so do default lima images, and a below-floor client does not error — it silently
degrades to MDS I/O, the §6 worst case, so a too-old rig "passes" while proving
nothing. The floor plus `CONFIG_PNFS_BLOCK` are phase-2 rig prerequisites and a
phase-3 admission check (§11).

**The udev landmine (confirmed, from the v6.11 commit message itself)**: the client
resolves devices by trying `/dev/disk/by-id/nvme-eui.<nguid>`. udev derives that link
from the kernel wwid, whose preference is uuid > nguid > eui64 — and **SPDK always
exposes a UUID descriptor** (`subsystem.c:2608-2616` defaults ns UUID to the bdev UUID).
Net: with SPDK defaults, the by-id link is `nvme-uuid.*` and the client's lookup fails,
silently degrading to MDS I/O. csi-node must ship the udev rule that creates the
`nvme-eui.<nguid>` link (whether newer systemd does this natively is unresolved —
verify per-distro, assume not).

> **RIG-PROVEN 2026-08-09** (`tests/lima/pnfs/block-rig.sh`, Ubuntu 24.04 +
> kernel 7.0, systemd 255): CONFIRMED — udev creates only `nvme-uuid.*`; the
> rule (or a link) is required. Assume not, on every current distro. Four more
> kernel-side facts the rig established, each a server obligation:
> (1) **fs_layout_types is read ONCE per superblock**, at the client's fsinfo
> probe — a scsi volume sharing the export root's fsid inherits the root's
> files-class advertisement and asks type-1 layouts forever. Block volumes are
> therefore advertised as their OWN synthetic filesystem
> (`fsid = (SCSI_FSID_MAJOR, hash(volume))`, fileops.rs) so the mount crossing
> re-probes on the volume dir; a free consequence is client-side EXDEV on
> cross-volume rename/link of scsi files.
> (2) **PTPL is kernel-enforced from the first I/O**, not merely restart
> hygiene: `nvme_pr_register` sets CPTPL=PERSIST unconditionally and SPDK
> answers INVALID_FIELD on a namespace without a `ptpl_file` — the client then
> marks the device unavailable (120 s negative cache) and every I/O degrades to
> the MDS path. `nvmf_subsystem_add_ns` must carry `ptpl_file` (blockExport
> `ptplDir`).
> (3) **Read layouts refuse RW_DATA/INVALID_DATA outright** (`verify_extent`)
> and demand gapless tiling of the layout window: LAYOUTGET(READ) is a
> NON-ALLOCATING query (`extent_alloc::grant_read`) presenting committed
> extents as READ_DATA and every hole as NONE_DATA.
> (4) **The namespace record carries the lvol's CANONICAL (UUID-form) bdev
> name**, not the `lvs/vol` alias — an alias-only match in the reconciler saw
> "wrong bdev" every pass and bounced the namespace under live initiators
> (~0.5 s device-node gap per reconcile).

Second kernel constraint: `bl_set_layoutdriver` rejects a layout blksize of 0 or
> PAGE_SIZE. **We must advertise `FATTR4_LAYOUT_BLKSIZE` ≤ 4 KiB for block-class
volumes** — today **both** encoders hardcode 4 MiB: `fileops.rs:1237-1244` (snapshot)
and `fileops.rs:1407-1420` (pseudo-root — the arm the mounting client's fsinfo GETATTR
actually reads, i.e. the value `bl_set_layoutdriver` checks). Patch only the snapshot
site and the pseudo-root still advertises 4 MiB — the exact rejection this paragraph
warns about. Same rule as §3: every emitting site, or one shared helper. Extents can be arbitrarily large; commit
granularity/alignment is this blksize.

### 4b. Userspace client library (later)

`libflint`: a userspace NFSv4.1 *metadata* client plus SPDK's userspace NVMe-oF TCP
initiator for data. An initiator needs **no hugepages and no VFIO** — those are
userspace-NVMe-*driver* requirements (see the SPDK-hugepages finding: the userspace
PCIe driver has never worked on our clusters; the TCP initiator is plain sockets).
This is the step that breaks the ~5-6 GB/s kernel nfs_client wall (runbh), because the
kernel NFS stack leaves the data path completely.

The design insight that makes libflint tractable: **block layout shrinks the userspace
client to a metadata client.** Under the file layout, a userspace client would have to
reimplement NFS READ/WRITE, striping, and session trunking. Under block layout the data
client is SPDK's initiator off the shelf; we write LAYOUTGET/LAYOUTCOMMIT/GETDEVICEINFO
handling and an extent cache. First surface: an OCI-registry storage driver
(`docs/oci-registry-pnfs-architecture.md` is the target architecture), where the
workload is large sequential reads of content-addressed blobs — the best case for raw
extent reads and the workload already pitched against these ceilings.

## 5. Architecture

Four moving parts. ADR 0001's boundary discipline (SPDK modules don't import pNFS code)
survives with the coupling direction it always permitted: pNFS *consumes* the SPDK
control plane. Restate it in code review; don't let the allocator leak into
`nvmeof_export.rs`.

**MDS as extent allocator.** New sqlite state (§8): per-volume extent maps, per-client
grant state, recall-before-reuse discipline. LAYOUTGET returns RFC 8154 extents
(`se_vol_id, se_file_offset, se_length, se_storage_offset, se_state`) instead of stripe
maps; LAYOUTCOMMIT — today a half-stub with a hardcoded `/data` path TODO
(`operations/mod.rs:769-812`) — becomes load-bearing: promotion of INVALID_DATA extents
to READ_WRITE_DATA per the client's commit list. The dispatch seam already exists and is
documented as such: `layout_type_served` (`dispatcher.rs:2297-2330`) is the designed-in
single choke point; it becomes a per-volume dispatch (volume geometry grows a
`layout_class`), and `PnfsOperationHandler::layoutget` grows a second arm calling the
allocator instead of `generate_layout`. The unused `Export.layout_types` default of
`[2, 1]` in `pseudo.rs:42-72` is dead code with the wrong values — and it has a twin:
`PseudoFilesystem::get_layout_types` (`pseudo.rs:241-251`) returns the same wrong
`[2, 1]`, each referenced only by the in-file test (`test_pnfs_support`,
`pseudo.rs:368-376`). Delete both plus their tests when wiring per-volume
advertisement — one source of truth means zero stale ones left behind.

**spdk-tgt as the target fleet.** The cross-node listener pattern (node-IP:4420 per
subsystem, `nvmeof_export.rs:400-429`) already exists for raid legs; block layout
generalizes "exactly one consumer node" to "every granted client node". The gap is
policy, not mechanism: `converge_hosts` fencing (`nvmeof_export.rs:440-509`) currently
removes any flint host NQN not in a single-consumer list, so the grant/recall lifecycle
must drive add_host/remove_host per client. Two viable exposure shapes: subsystem-per-
volume scaled up, or one shared per-node subsystem with `no_auto_visible` namespaces and
per-namespace host masking (`nvmf_ns_add_host`, SPDK `nvmf_rpc.c:1858`). Start with
subsystem-per-volume — it's the shape every existing reconcile loop understands.

**Namespace identity.** RFC 9561 §2.1: GETDEVICEINFO carries the designator as
PS_DESIGNATOR_EUI64 with the **NGUID (16 octets) preferred**; the NVMe UUID descriptor
has no mapping and is unusable. **Set NGUID explicitly and stably** on
`nvmf_subsystem_add_ns` (accepted in v26.05, `nvmf_rpc.c:1311-1321`) — today remote
exports pass `ns_identity: None` and lean on the lvol UUID default. The NGUID is the
client's *only* device identity; it must survive lvol migration, rebuild, and tgt
restart. The `stable_ns_identity` pinning pattern (`nvmeof_export.rs:98-122`) exists for
exactly this reason on the loopback path; block exports adopt it unconditionally.
GETDEVICEINFO gains a `pnfs_scsi_deviceaddr4` encoder sibling (volume topology:
BASE/SLICE/CONCAT/STRIPE trees, not netaddr4 lists) — and note `maxcount` is ignored
today (`dispatcher.rs:2559`); topology bodies make GETDEVICEINFO TOOSMALL handling real.

**csi-node session management.** Phase-split to match the exposure shape above, or the
work items contradict it. Under subsystem-per-volume (the starting shape), sessions
stay 1 NQN = 1 volume = 1 session — no refcounting, no multi-namespace subsystems, and
the `find_nvme_device_by_nqn` `/dev/<ctrl>n1` hardcode (`node_agent.rs:2178`) never
fires. What that shape costs instead, and what must be budgeted before calling it
final: sessions and tgt qpairs scale as **volumes × client-nodes** against one
spdk-tgt, and the grant/recall lifecycle drives per-subsystem add_host/remove_host
churn — put a ceiling on both on the phase-2 rig. The shared refcounted session per
(node, storage-node) with `no_auto_visible` namespaces — and the multi-namespace
`/dev/<ctrl>n1` fix it requires — belongs to that later migration, not day one. The
ctrl_loss_tmo / fast_io_fail / hostnqn plumbing transfers unchanged — and is mandatory
(§6). ControllerPublish's existing pNFS no-op (`main.rs:2159-2166`) is the free
per-node hook for hostnqn registration.

> **SESSION MGMT SHIPPED 2026-08-10** (rig-proven — `block-rig.sh` now STAGES
> through the production code via `pnfs-csi-cli stage`, not a bash replay). The
> lifecycle: **ControllerPublish → `AttachBlockNode`** (per-node hostnqn
> admission BEFORE any NFS traffic exists — the allow-list is default-closed and
> the LAYOUTGET-time admission arrives after the connect it would permit; durable
> `block_node_attach` rows, schema v8, keyed by NQN because the NFS client_id is
> minted at EXCHANGE_ID, later still), answering the session coordinates that
> ride publish_context; **NodeStage → `pnfs_block_session::ensure_session`**
> (connect as the MDS-admitted NQN verbatim, ReconnectPolicy + fast_io_fail
> sysfs backfill, NGUID-matched head-device resolution — never the controller
> dir: under native multipath it holds only hidden `nvmeXcYnZ` path devices —
> and the §4a eui link, idempotently re-linked on every re-stage);
> **NodeUnstage** → level-triggered teardown keyed on the derived `:block:` NQN
> having a live kernel controller (works with the PV already gone); 
> **ControllerUnpublish → `DetachBlockNode`** (idempotent; client-earned
> admissions untouched). The fence interlock is load-bearing in both directions:
> the fence's eviction sweeps the NQN's attach rows in the SAME transaction, and
> attach refuses while any fence record names the NQN (fence-rig F6 asserts
> both). **The rig immediately earned its keep**: the admission fast path
> skipped the per-client `block_hosts` row when a stage-time attach already had
> the NQN desired — leaving the fence NOTHING to capture or evict, so the
> attach row kept the fenced NQN admitted and the fenced client reconnected
> (F5 failed live). Fixed — the row is always written, only the RPC converge is
> skipped — and unit-pinned
> (`a_node_attach_must_not_swallow_the_per_client_admission_row`). OWED from
> here: the packaged udev RULE (chart tranche; the managed link covers staged
> volumes), session re-establishment after ctrl_loss exhaustion (reconcile-loop
> tranche), and the admission-model tranche for the same-node zombie residual.

**Fencing, per the RFC.** RFC 9561 §2.2 maps SCSI PRs to NVMe Reservations: the MDS
registers its own key and holds RTYPE=4h (Exclusive Access – Registrants Only); fencing
a client is Reservation Acquire with Preempt/Preempt-and-Abort naming the client's key;
the client sees RESERVATION CONFLICT (0x83) and must commit, return layouts, and
unregister (§2.2.4). Reservation keys are per Host Identifier — **fencing one client
cuts all its paths at once**, which is what we want. Two consequences: (1) **the MDS
must itself be an NVMe host** on each namespace to hold the reservation — it grows an
initiator; (2) SPDK's target-side reservations are complete including
Persist-Through-Power-Loss via per-namespace `ptpl_file` — **PTPL is mandatory**, or a
tgt restart (the csi-node roll landmine) silently unregisters every key and unfences
everyone. We also keep the out-of-spec belt: yanking the host NQN from the allow-list
and draining qpairs (`nvmeof_export.rs:511-530`) is a functional fence the client sees
as connection loss — use it as the enforcement backstop, not the primary, because
conforming clients recover cleanly only from RESERVATION CONFLICT.

> **RIG-PROVEN 2026-08-10** (`tests/lima/pnfs/block-rig.sh FENCE=1`, i.e.
> `make test-pnfs-fence-rig`; kernel 7.0; `resv_fence.rs` is the MDS's own minimal
> NVMe/TCP initiator). A live O_DIRECT writer on the raw path was fenced by the MDS,
> and the volume's `bdev_get_iostat` `bytes_written` **froze at the fence and stayed
> frozen** — FenceReaches, on the device counters, against a client mid-write. Three
> kernel facts the rig established, each a correction to the paragraph above:
>  1. **The kernel client registers NO reservation key.** `nvme resv-report` on the
>     client is empty and SPDK logs `Can't register zeroed new key` — the kernel's own
>     `pr_register` sends a zero key and SPDK rejects it. So "Reservation Acquire with
>     Preempt naming the client's key" is a **no-op in practice**: there is no client
>     key to preempt. The fence is the MDS *acquiring* RTYPE=4h, which fences the client
>     as a **non-registrant** (SPDK's per-command `nvmf_ns_reservation_request_check`
>     refuses non-registrant READ *and* WRITE under 4h). `fence_preempt` keeps the
>     preempt arm for a *foreign/stale* holder, but EA-RO acquisition is the load-bearing
>     step.
>     **PREEMPT-DRILL CORRECTION 2026-08-10 (same kernel, later run): "registers NO
>     key" is only SOMETIMES true.** The preempt drill's plain `resv-register` bounced
>     with SPDK's *"The same host already register a key with 0x2"* — 0x2 being the
>     client id, i.e. exactly the `pr_key` GETDEVICEINFO hands out: the kernel's
>     `pr_register` DOES land its real key on some flows (the zeroed attempts and a
>     successful register coexist in one run's tgt log). Both worlds are handled and
>     now both are exercised: a registered victim is wiped by `fence_preempt`'s
>     preempt-victim arm (`has_key(victim)` — live in production on registering
>     kernels, since pr_key == client_id == the fence's victim key), an unregistered
>     one is excluded as a non-registrant by the EA-RO. Do not build on "the client
>     never has a key."
>  2. **The conforming client returns its layout on the write error** (`_pnfs_return_layout`
>     fires on the RESERVATION CONFLICT), which frees the grant **clean** — the
>     return-after-fence upgrade, observed live, no quarantine needed.
>  3. The functional backstop reaches the client concretely: after host eviction the
>     client's nvme-tcp **reconnect is refused** (`Connect … is not allowed`).
>
> Wire bug the rig caught (invisible to the in-process fake until it was taught to
> reproduce SPDK): the target sets the **C2HData SUCCESS flag** on the last data PDU of
> a read/report and sends **no** separate response capsule, so the initiator must
> complete on the inline flag or it hangs forever on the reservation report.

## 6. What we lose — be honest

- **Per-file trust granularity, permanently.** Extents are raw device ranges. RFC 8154
  §2.1 is blunt: where clients can't be trusted to enforce extent boundaries, "pNFS
  SCSI layouts MUST NOT be used." A layout-holding client has raw write reach over the
  whole namespace; reservations fence *hosts*, they don't authorize *extents*.
  Node-level trust — today a soft boundary (nodeCIDR NetworkPolicies, control token) —
  becomes a hard, permanent property of the block class. Multi-tenant clusters keep the
  file layout.
- **Client prerequisites.** Every client node needs nvme-tcp reachability and sessions
  to every storage node backing its volumes, plus the udev rule (§4a). And the
  **ctrl_loss_tmo D-state landmine class applies to pNFS clients now**: a dead target
  with default 1800s ctrl_loss_tmo parks I/O, wedges umount in `blkdev_issue_flush`,
  and survives only reboot — we've paid this bill once (fixed via fast_io_fail sysfs
  backfill, commit 560c1d1) and the same policy must ship on block-layout sessions
  from day one.
- **The kernel blocklayout client is niche.** I found no production RFC 9561 deployment
  anywhere; the visible ecosystem is the mainline client plus knfsd-over-XFS.
  (Hammerspace "Tier 0" is **not** this — it's flex-files plus LOCALIO.) We are an
  early adopter of `bl_*` over fabrics. Budget for kernel bugs.
- **Many-writer RWX is weaker than the file layout.** Extent grants are exclusive for
  RW; concurrent writers serialize through recall/regrant or fall back to MDS I/O. The
  file layout remains the answer for shared-write workloads.
- **The ext4 freebies come home to the allocator.** Sparse files, thin provisioning,
  and CLONE (refcounted extents) were free because stripe files lived on ext4. Now the
  allocator owns all of them. And the MDS fallback path must do actual block I/O
  (§8) — a new F66-class surface, *stronger* than the file layout's: the kernel client
  routes unaligned/sub-blksize I/O through the MDS **routinely**
  (`nfs_pageio_reset_write_mds` for sub-PAGE_SIZE direct writes), so MDS block I/O is
  steady-state, not a straggler path. Every client failure mode (device unresolvable,
  bio error) also **degrades to MDS I/O silently** — the F68 observability lesson
  applies verbatim; meter the MDS block lane from day one.
- **Observability leaves NFS.** No DS DataPathMeter, no sunrpc counters.
  `bdev_get_iostat` (per-lvol) and `nvmf_subsystem_get_qpairs` / `nvmf_get_stats`
  become the truth — the runbm arbiter pattern, now load-bearing in production.

## 7. StorageClass and chart surface

**Dispatch landmine first**: CreateVolume branches on the exact string
`layout == "pnfs"` (`main.rs:1432`); today `layout: pnfs-block` **falls through and
silently provisions a plain SPDK volume**. The comparison becomes a `match` with an
explicit reject-unknown arm before anything else ships.

Class sketch (`pnfs-block-storageclass.yaml`, cloned from `pnfs-storageclass.yaml`):

```yaml
provisioner: {{ .Values.driver.name }}  # renders flint.csi.storage.io (values.yaml:64)
parameters:
  layout: pnfs-block
  pnfs.flint.io/extentSize: "4Mi"     # allocation granularity, NOT the wire blksize
volumeBindingMode: WaitForFirstConsumer
allowVolumeExpansion: true            # now a REAL allocation op — see below
```

- New `pnfs.flint.io/*` keys must be added to `sc_params::ALL` (`pnfs_csi.rs:63-70`) —
  the namespace is a closed set and unknown keys hard-fail the provision.
- **volume_context needs a discriminator** (`pnfs.flint.io/layout: block`): every
  downstream classifier keys on `mds-ip` presence or the `~m` shard suffix, and a block
  volume reusing both is indistinguishable. NodeUnstage classification order is the
  live landmine: RWX access modes resolve `NfsShared` *before* the pNFS check
  (`main.rs:3703-3729`), which is harmless today (both branches unmount-only) but leaks
  nvme sessions for a block class whose unstage must deref them. pnfs-block checks first.
- **Expand**: the current pNFS expand path is a metadata-only ack keyed on the shard
  suffix alone (`main.rs:2668-2708`). Block expand must raise the allocation ceiling
  for real — the discriminator check goes before that early return.

> **EXPAND — SHIPPED 2026-08-11, and the client-side caveat it exposed.**
> `ExpandVolume`'s scsi arm now moves BOTH halves of a block volume's capacity:
> `BlockExportReconciler::grow` resizes the lvol (rounded up to MiB, idempotent,
> capacity read BACK from the device) and `extent_alloc::expand_volume` raises
> `volume_alloc.size_ceiling` — **device first, always**; a ceiling that outran its
> namespace would hand out extents past the end of the device, and the write would
> fail at the device with the server believing all was well (unit-pinned:
> `a_failed_device_grow_never_raises_the_ceiling`). Both failure paths answer a gRPC
> error so the driver maps them to UNAVAILABLE and external-resizer re-drives —
> FAILED_PRECONDITION is terminal for the resizer, and a half-applied expand that can
> never be re-driven is the wedge this RPC was fixed to stop causing.
>
> Rig-proven end to end (`make test-pnfs-expand-rig`, §E): a 32 MiB volume filled to
> exactly its ceiling reported **ENOSPC to the application** (not EIO — see §12's
> capacity residual, now closed at the wire), the expand grew the lvol to 128 MiB, and
> the client kernel picked the bigger namespace up on its own (65536 → 262144 sectors:
> SPDK turns the bdev resize into `nvmf_ns_resize` + the ns-changed AEN, and the kernel
> rescans — `lib/nvmf/subsystem.c`, growth needs no I/O quiesce).
>
> **THE CAVEAT THE DRILL FOUND — and then closed.** In the first runs the same mount
> could NOT use the new room: the server granted layouts past the old ceiling, the
> client returned each one immediately and wrote through the MDS lane instead, for
> every new file on that mount. The NFS client caches the blocklayout **device** — its
> length included — from GETDEVICEINFO, and extents past that length are unmappable.
> Recycling the mount fixed it at once (A/B'd on the live rig with the nvme session
> untouched, so it was the NFS device cache and not the block device).
>
> **CB_NOTIFY_DEVICEID (RFC 8881 §20.12) now ships, and expansion is online end to
> end.** Every scsi GETDEVICEINFO records which session fetched the device and which
> notifications it accepts (`gdia_notify_types`), the reply advertises the
> intersection of that with what we send (never more — an advertised-but-unsent
> notification would have the client trust a stale cache forever), and ExpandVolume
> fans out one CB_NOTIFY_DEVICEID per subscriber after the ceiling rises. Linux's
> `nfs4_callback_devicenotify` drops the cached deviceid for both CHANGE and DELETE,
> so the next LAYOUTGET re-fetches it. Best-effort by construction: a client with no
> back-channel or a refusal falls back to the old recycle-the-mount behaviour, and the
> expand itself never fails on it.
>
> **Two wire landmines, both rig-found, both now pinned by byte-level tests:**
>
> 1. **The notify bitmap carries the type's BIT, not the RFC enum's ordinal.** RFC 8881
>    §3.3.7 declares `NOTIFY_DEVICEID4_CHANGE = 1`, but every transmission of it is a
>    `bitmap4`, so the wire value is `1 << 1` = 2 (and DELETE is 4) — which is exactly
>    how Linux defines its own constants (`include/linux/nfs4.h`) and how its callback
>    decoder compares them. Sending the ordinal got NFS4ERR_INVAL from a client that
>    was simultaneously asking us for `[6]` — those same two bits.
> 2. **The CB_COMPOUND header must carry the SESSION's minor version.** Linux takes
>    `cps->minorversion` from our header and resolves the client with
>    `nfs4_find_client_sessionid(net, addr, sessionid, cps->minorversion)`, which
>    requires `clp->cl_minorversion == minorversion`. flint mounts are **vers=4.2**
>    (`mount_opts.rs` default), so the hardcoded `minorversion: 1` matched no client
>    and every callback came back NFS4ERR_BADSESSION *before the callback op was even
>    looked at*. This had been true for CB_LAYOUTRECALL too. It went unnoticed because
>    the one drill that exercises recalls mounts `minorversion=1` — A/B'd both ways:
>    with the probe restored, the 4.1 drill is unaffected; on the 4.2 block rig the
>    same probe reproduces BADSESSION.
- Reuse the `check_echo` version-skew gate (`pnfs_csi.rs:196-225`) for the layout-class
  field: an MDS predating block layout must not ack a block-class CreateVolume (proto3
  drops unknown fields silently).
- Values keys: `pnfs.blockStorageClass.{create,name,reclaimPolicy,extentSize,
  mountOptions}`, `pnfs.networkPolicy.nvmeCIDRs`, an NVMe port key.
- **NetworkPolicy**: add 4420 ingress from nvmeCIDRs (kernel initiators connect from
  node IPs, same shape as the 2049 rule). **Hard caveat**: spdk-tgt lives in the
  hostNetwork csi-node DaemonSet — NetworkPolicy cannot select host-network pods, so a
  4420 policy is only enforceable if block-layout targets get a pod-networked fleet
  (the pnfs-ds StatefulSet pattern); today 4420 is unpoliced entirely, including
  existing raid-leg traffic. Until then, 4420 protection is security-groups/host-
  firewall, outside the chart — say so in the runbook trust-model section.
- The **images-lockstep warning** (values.yaml:476) extends verbatim: the allocator is
  MDS-half, the dispatch and csi session code is driver-half; pins move together.
- The control token covers the allocator's MdsControl surface for free; it does **not**
  cover the NVMe data path — there the guards are hostnqn allow-lists, reservations,
  and network isolation (DH-HMAC-CHAP and TLS exist in both SPDK v26.05 and kernels
  ≥6.0/6.7 if we ever need in-band auth; not phase-1).

## 8. MDS allocator design sketch

**Schema** (rides the same CREATE-TABLE-IF-NOT-EXISTS batch, `sqlite.rs:1251-1372`;
SCHEMA_VERSION bump per the no-migrations policy):

```sql
extents(volume TEXT, file_id INTEGER, logical_offset INTEGER, length INTEGER,
        physical_offset INTEGER, gen INTEGER, state TEXT,  -- invalid|rw|read
        PRIMARY KEY (volume, logical_offset))
extent_grants(volume, logical_offset, client_id, mode, gen, PRIMARY KEY (...))
volume_alloc(volume TEXT PK, size_ceiling INTEGER, next_free INTEGER, ...)
```

> **IMPLEMENTED 2026-08-09** (`state_backend/extent_alloc.rs`, tables in
> `SCHEMA_SQL`, SCHEMA_VERSION 2) with three corrections to the sketch above:
> (1) the PKs gain `file_id` — logical offsets are file-relative, so two files
> in one volume collide at offset 0 under the sketched key; (2) two tables the
> sketch omitted: `extent_free` (the free list, carrying `last_gen` so reuse
> mints `last_gen + 1` — without it a bump-only allocator never reuses and the
> gen detector is dead weight) and `extent_quarantine` (the fenced-holder
> quarantine below, as real rows with a meter and an operator release lever);
> (3) per the FlintExtents tranche-1 finding, `reclaim_complete` re-validates
> holders INSIDE the free transaction and refuses (`NotQuiescent`) — the
> recall snapshot is advisory, never load-bearing. `verify_volume_invariants`
> runs at the end of every writing transaction (logical + physical
> disjointness, watermark containment, grant referential/generation
> integrity) and is itself tested against a deliberately corrupted table.

- **The PK does not police overlap.** `(volume, logical_offset)` admits overlapping
  ranges — `(0, len 8)` and `(4, len 4)` are distinct PKs — so every extents-table
  write carries an app-level disjointness assertion, unit-tested; sqlite will not catch
  range aliasing for us, and aliasing (one physical range under two logical extents
  after a split/merge bug) is the allocator's silent-corruption class. §9 models it as
  `Inv_NoPhysicalAliasing`.
- **Allocation is per-volume, inside the volume's own lvol.** This is forced by
  sharding: shards share zero state (`mds-sharding-plan.md`), a volume pins to one
  shard forever, so a per-volume allocator composes trivially — the volume's whole
  extent map lives on its shard. A cluster-wide pool allocator would be the first
  cross-shard shared state; refused.
- **Write discipline**: extents are high-churn (split/merge per LAYOUTCOMMIT), unlike
  every existing table (placements write once per file). The fire-and-forget
  `enqueue_write` coalescing model keys on whole-row PK and its retry path decomposes
  batches per-op (`sqlite.rs:384-411`) — the group-commit txn is an optimization, not
  an atomicity contract. Extent transitions use the **awaited** discipline
  (`put_volume_geometry` precedent) with explicit multi-row transactions via
  `with_conn`. FlintClaims' machine-checked verdict binds here: **safety lives in a
  record-level CAS (a sqlite transaction), never in leadership** — free→provisional is
  a transaction, not an in-memory free-list op guarded by being "the" MDS.
- **Grant path**: LAYOUTGET → allocate INVALID_DATA extents (or return existing
  RW/READ ones), journal the grant durably *before* the reply, return extents. Carry
  the C6 lesson structurally: grant is check-then-insert with a post-insert recheck —
  never modelled or coded as atomic.
- **LAYOUTCOMMIT**: validate, then apply. Each committed range checks `extent_grants` —
  the (client, gen-at-grant) pair must match a live grant — and a mismatch rejects with
  NFS4ERR_BADLAYOUT/EXPIRED. Not optional politeness: reservations fence the **NVMe
  data path only**, the NFS control path stays open, so a fenced or lease-expired
  client can still deliver a LAYOUTCOMMIT — and an unvalidated one promotes INVALID→RW
  on extents that were freed and reused (the new owner's data) or resurrects a stale
  size. Then apply the commit list (blksize-aligned per RFC 8154 §2.4.2), promote
  INVALID→RW, update size. Replaces today's half-stub entirely.
- **GC / recall-before-reuse**: truncate and delete produce to-free extents; an extent
  frees only when every grant on it is returned or acked-recalled. "Holder is fenced"
  is **not** a freeing condition yet: §9 keeps `FenceReaches` FALSE until the phase-2
  rig proves real preempt delivery, and freeing on an unproven fence designs in the
  corruption path (crashed client → fence issued but undelivered → extent reused → the
  un-fenced client writes the new owner's data). **FLIPPED 2026-08-10** (the rig proved
  delivery; the model re-gated): fenced-holder extents now **free cleanly when the
  fence was CONFIRMED at the target** (`fenced_clients.delivered_unix`, set only on a
  verified preempt — post-report MDS-holder, victim absent; also set by the startup
  re-fence, closing the crash window) and **quarantine only when it was not** —
  the code's preempt arm is best-effort and can fail at runtime, so the belt is
  per-fence confirmation, never a global assumption. Model:
  `FreeRequiresDelivered` — the shipped cfg claims both stale theorems in the
  fences-CAN-fail world (`FenceReaches` stays FALSE there, deliberately: "every fence
  lands" would still be a lie about the code); `FlintExtentsLostFence.cfg` is the
  permanent single-flag A/B, and `ProbeDeliveredFreeFires` licenses the green
  (the fenced-free really fires). The metered quarantine bound and operator release
  lever stay, now scoped to unconfirmed fences. Reuse bumps `gen`. **This is F67's lesson generalized**: an extent map lost
  while data survives = silent zeros served from a reused extent — and there is no stub
  to hang an xattr on, so durability is sqlite-only and therefore must be *stronger*
  (awaited writes, `open_durable` FULL sync on tear-away volumes).
- **Crash recovery**: on restart, reload extents+grants; unknown space is
  allocated-not-free (conservative); un-expired grants are honored, expired ones fenced
  before their extents free (fence → quarantine while FenceReaches is unproven, above).

  > **LEASE SWEEP SHIPPED 2026-08-10** — "expired ones fenced" now has a mechanism.
  > Client-lease expiry in this server was LAZY-only (a top-of-COMPOUND check whose
  > cascade never touched layouts): a volume whose ONLY client died was never reaped,
  > its grant rows blocking every successor forever, files-class layout handles leaking
  > the same way. `start_lease_sweep` (default 30s, FLINT_PNFS_LEASE_SWEEP_SECS, 0 =
  > kill switch; boot hold = one lease window past grace) runs
  > `operations::lease_sweep_pass`: candidates from durable grant rows
  > (`block_grant_clients` — survives MDS restarts) ∪ in-memory layout owners; a dead
  > client gets `return_all_for_client` (handles) and, per volume, **fence →
  > `revoke_client` (the bulk return, REFUSED in-transaction unless the fence is
  > CONFIRMED — `UnconfirmedFence` keeps the rows for the next tick, since deleting a
  > fenced row without proven exclusion is LostFence's corruption through a side door)
  > → auto-unfence** (release + record cleared, so a rescheduled RWO pod's replacement
  > client recovers with NO operator step; the sibling-fence check keeps shared volumes
  > fenced while any other fence stands). Revoke mirrors LAYOUTRETURN semantics —
  > extents stay (provisional rows become re-grantable orphans, committed rows are file
  > data), and the dead writer's file gets the windowed merge. **Residual, stated:**
  > post-release the standing zombie barrier is the durable host eviction, which is
  > per-Host-Identifier (RFC 8154's own granularity) — a replacement pod on the SAME
  > node re-admits the NQN a same-node zombie could ride; not closable at this layer.
  >
  > **THE ADMISSION TRANCHE PAID 2026-08-10** (`formal/FlintAdmission.tla`, gate
  > 109→114): the residual is now MACHINE-CHECKED instead of a prose apology. The
  > module models the admission layer around the sweep — per-Host-NQN allow-list,
  > durable fence records, the volume-wide EA-RO, fence → delivered-gated revoke →
  > auto-unfence — and pins exactly which assumption carries safety once the
  > auto-unfence reopens the door: **`ClientHonorsLease`**, the kernel discarding
  > layout state at lease expiry (every userspace write traverses the kernel's
  > blocklayout driver, so a discarded layout is an unreachable device). Shipped cfg
  > (honours-lease, same-host successor, door wide open): `Inv_NoStaleDeviceWrite`
  > HOLDS. `FlintAdmissionZombie.cfg` (a client frozen past its own lease — a
  > live-migrated/SIGSTOP'd VM — plus a same-host successor): TLC FINDS the stale
  > write, the residual as a counterexample. `FlintAdmissionCrossHost.cfg` (same
  > zombie, successor elsewhere): HOLDS — the eviction barrier is real at per-host
  > granularity. Two probes keep the greens honest (the sweep chain fires; the
  > same-host door is really walked). Scope limits argued in the module head: sweep
  > path only (the manual levers carry an operator contract, not a theorem), no
  > zombie self-re-admission (re-handshake discards the old incarnation),
  > always-evict coarsening.
  >
  > **THE PARTITION DRILL** (`make test-pnfs-sweep-rig` = block-rig.sh `SWEEP=1`):
  > the timer path live — DROP the NFS port under a live raw-path writer, lease
  > expiry, sweep fences/revokes/auto-releases on the timer, node reboots,
  > successor stages + mounts + writes with zero levers. **Client-behaviour
  > finding (two drill drafts refuted by the client, in sequence)**: a LIVE
  > partitioned kernel freezes its raw block-path I/O near-INSTANTLY — the
  > O_DIRECT write path is coupled to the metadata lane closely enough (a
  > per-write attribute/commit RPC is suspected: cheap on a healthy loopback
  > lane at 3 GB/s, blocking the moment the lane dies) that a network partition
  > CANNOT manufacture a data-plane zombie at all; the drill measures and logs
  > the window rather than asserting one. Stronger client behaviour than
  > "honours lease expiry" — and exactly why the model's zombie is the
  > frozen-VM shape, which skips every client courtesy.
  >
  > **THE FROZEN-VM ZOMBIE DRILL — RUN AND PASSED 2026-08-10** (`make
  > test-pnfs-zombie-rig` = block-rig.sh `ZOMBIE=1`; needs a second lima VM,
  > see the rig header). The model's only dangerous shape, made flesh: a second
  > VM stages through the production attach verb + a proxied cross-VM session
  > (lima VMs sit on isolated user-nets — `tcp-proxy.py` on the host bridges
  > them), writes raw at full proxy throughput, and is **SIGSTOPped at the
  > hypervisor mid-pwrite**. The sweep fenced the frozen client on the timer,
  > revoked its rows, swept its attach row (the successor's row asserted
  > SURVIVING — the sweep's scope is per-client, proven), auto-released; a
  > successor wrote 8 MiB of sha-stamped pattern over the REUSED extents; the
  > zombie resumed into a jumped clock. Observed wake path: 4 refused nvme
  > reconnects (the eviction barrier, client-visible), then its writer errored
  > out (EXIT 5) — and the successor's bytes re-read sha-intact at the device:
  > **Inv_NoStaleDeviceWrite held on real hardware through freeze → sweep →
  > reuse → resume.** FlintAdmissionCrossHost's green and the eviction barrier,
  > demonstrated end-to-end. Known quirk surfaced: a count-less `dd` probing
  > past EOF falls to the MDS lane where the zeros-belt answers EIO (loud,
  > safe) instead of a clean 0-byte EOF — candidate belt refinement, recorded
  > in the drill's comments.
  The MDS-HA machinery (durable-DS plan milestone B: sqlite server_id, 90s grace,
  Recreate+RWO fence) carries the allocator state unchanged — but **reservation
  holdership is not in sqlite**: it lives target-side, keyed to the MDS's NVMe Host
  Identifier. The MDS ships a stable, durable hostnqn+hostid (`identity.rs`
  `BLOCK_MDS_HOST_ID` / `BLOCK_MDS_PR_KEY`, compile-time constants — the durable part).
  > **RIG-PROVEN 2026-08-10** (`make test-pnfs-fence-restart-rig` = block-rig.sh
  > `FENCE=1 RESTART=1`): fence a live client, then kill+restart the MDS with the fence
  > active — the fence is UNCHANGED. Two corrections to the sketch above:
  >  1. **"re-register and re-acquire RTYPE=4h on every block volume on restart" is
  >     WRONG**, and only because the fence rig taught us the kernel registers no key:
  >     the MDS therefore holds EA-RO **only during a fence**, never continuously
  >     (continuous 4h would fence every non-registrant client, i.e. all of them,
  >     always). So there is no standing per-volume reservation to re-acquire.
  >  2. What must survive is the **per-fence** reservation plus the sqlite eviction, and
  >     both do with **no action on restart**: startup `reconcile_all` re-converges the
  >     allow-list from `block_hosts` (the fenced client's row was deleted by the fence
  >     → it stays OFF the list; verified the list came back as `[:mds:resv-fence]`
  >     only), and the reservation persisted target-side. The post-restart re-fence
  >     reports `registered=false acquired=false`, `gen` unchanged, MDS key still the
  >     4h holder — the restarted MDS reclaimed holdership through its stable identity,
  >     a **no-op re-acquire**, which is the correct behaviour for an MDS-only restart.
  >
  > **PTPL-survives-tgt-restart RIG-PROVEN 2026-08-10** (`make test-pnfs-ptpl-rig` =
  > block-rig.sh `FENCE=1 TGT_RESTART=1`): the together-restart — kill BOTH tgt and MDS,
  > bring the tgt back on the SAME disk image (the lvstore+lvol auto-load from the
  > superblock), MDS reconcile re-adds the ns with its `ptpl_file`, and SPDK's
  > `nvmf_ns_reservation_restore` reloads the EA-RO reservation from disk (bdev-UUID
  > checked against the reloaded lvol). The tgt's memory was wiped, so the post-restart
  > re-fence reporting `registered=false acquired=false rtype=0x4` MDS-holder can ONLY be
  > the ptpl_file — proven. (The generation counter resets to 0 on reload; the
  > reservation *type and holder* restore intact, which is what fences.) Without PTPL
  > this restart is the "silently unfences everyone" landmine — the `ptpl_file` on
  > persistent storage (`blockExport.ptplDir`) is what closes it.
  >
  > **PTPL-LOSS recovery RIG-PROVEN 2026-08-10** (`make test-pnfs-fenced-record-rig` =
  > `FENCE=1 TGT_RESTART=1 PTPL_LOSS=1`): the durable `fenced_clients` sqlite table (the
  > POSITIVE record the block_hosts eviction could not be — an absence, not a record) now
  > closes it. `fence_block_client` writes the record FIRST (capturing the host_nqn before
  > eviction removes it); `admit_block_host` refuses a fenced client's re-admission; and
  > MDS startup, after `reconcile_all`, replays `block_fenced_all()` to re-acquire EA-RO on
  > every still-fenced volume. Proven by deleting the ptpl_file AND restarting the tgt: the
  > startup re-fence reports `registered=true acquired=true` (a fresh reservation, gen=1) —
  > the fence was rebuilt from sqlite alone, surviving TOTAL target-state loss. `SCHEMA_VERSION`
  > 4→5; `drop_volume` sweeps the new table; `block_unfence` is the release/lease-recovery
  > hook (the fence is otherwise permanent — see the volume-wide-EA-RO note in §6/§8).
  >
  > **UNFENCE (reservation release) RIG-PROVEN 2026-08-10** (`make test-pnfs-unfence-rig`
  > = `FENCE=1 UNFENCE=1`): the fence is REVERSIBLE. `UnfenceBlockClient` (the operator
  > lever — the MDS never unfences on its own) clears the durable record FIRST (a crash
  > after leaves the fence standing, the safe direction; a lever retry converges), then
  > releases the EA-RO reservation via NVMe Reservation Release (RRELA=0, RTYPE from the
  > report, CRKEY=MDS key; the MDS *registration* stays) — but ONLY when no other client
  > remains fenced on the volume: the reservation is volume-wide (kernel clients register
  > no key, so EA-RO blocks every non-registrant), so it must outlive any single unfence
  > while a sibling's fence stands. Fenced GRANT rows are deliberately untouched — they
  > clear via the client's own return-after-fence or quarantine on reclaim. The rig runs
  > the REAL operator flow: fence a mid-write client, **reboot the node**, then unfence —
  > because rig-found, a client fenced with an O_DIRECT pwrite in flight can park that
  > pwrite in D-state (the F4 "blocked" branch: a burst of belt-refused fallback WRITEs,
  > then silence), and NO umount — lazy, forced, or both — gets past it; the reboot is the
  > only reliable client recovery, so the runbook step is fence → reboot → unfence. The
  > reboot doubles as a together-restart, so the drill first watches the fence
  > RE-ESTABLISH from the durable record (§T's property as a precondition), then: the
  > lever answered `released=true`, `fenced_clients` drained, the evicted hostnqn
  > reconnected (refused minutes earlier), and a fresh O_DIRECT write put ≥8 MiB through
  > the device counter the fence had frozen, with the durable re-admission row back in
  > `block_hosts`. Re-admission is grant-driven (the client's next LAYOUTGET) — nothing is
  > proactively restored.
  >
  > **Rig-found while proving it: the "durable" record was not power-loss durable.** The
  > MDS opened its state DB with `synchronous=NORMAL` (`config.rs build_state_backend`),
  > whose commits are durable at the *checkpoint*, not at commit — the drill's node
  > power-off moments after a fence came back with `fenced_clients` EMPTY: the fence
  > silently lifted, exactly the class of loss this section's own "durability is
  > sqlite-only and therefore must be stronger" sentence predicts (and, worse, the same
  > window applies to the extent map itself — a power-lost `extents` commit over
  > committed data is F67's silent zeros). Fixed: the pNFS MDS now uses `open_durable`
  > (synchronous=FULL, per-commit WAL fsync, group-commit amortized) — the same trade the
  > standalone server has shipped since v1.7.
  >
  > **FULL re-priced 2026-08-10** (mdsbench wire A/B `FLINT_PNFS_STATE_SYNC=normal`
  > vs default + extent-bench `FLINT_BENCH_SYNC=full`, release aarch64 in lima):
  > **on the files-layout wire path FULL is free** — w1-create 475 vs 465 ops/s,
  > w2-opencl 7,187 vs 7,138, w3-stat 2,709 vs 2,940, w4-mixed 1,333 vs 1,000
  > (all within run-to-run noise; the dispatch/XDR cost dominates and the writer
  > thread's group commit absorbs the fsyncs). The cost is visible only in the
  > SERIAL allocator transactions (no concurrency to amortize — the worst case):
  > grant-open 251→875 µs, grant-4k 224→426, commit-4k 108→258, while batched
  > shapes barely move (commit-1 over 1024 rows 6.3→7.1 ms, return+free +6%).
  > Verdict: the §8 GO stands — a 4 MiB grant at 875 µs is still under half of
  > one files-layout create cycle, and the shapes that would hurt (per-4k
  > commits) amortize whenever more than one txn is in flight.
  >
  > **PERIODIC RECONCILE LOOP SHIPPED 2026-08-10, RIG-PROVEN**
  > (`make test-pnfs-reconcile-rig` = block-rig.sh `RECONCILE=1`): the tgt-ONLY
  > restart — the one restart shape no startup replay can see, because the MDS never
  > restarts — finally repairs itself, retiring the runbook's roll-the-MDS rule. One
  > shared `export_reconcile_pass` (startup replay and the loop call the same body, so
  > they cannot drift) re-converges every scsi volume's export chain from sqlite and
  > re-establishes every active fence from the durable `fenced_clients` record,
  > marking DELIVERED on confirmation — the crash-window closure is now continuous,
  > not boot-only. `FLINT_PNFS_EXPORT_RECONCILE_SECS` (default 30 s; 0 = loud kill
  > switch); level-triggered, so a converged tgt sees probes and zero mutations.
  > Proven: kill the tgt under a live mounted client, restart it empty on the same
  > disk, and within one interval the loop rebuilt subsystem/namespace/allow-list
  > (MDS pid asserted UNCHANGED), the client's surviving kernel controller
  > reconnected on its own, wrote 8 MiB raw through the zeroed device counter, and
  > the run fell through to a clean REMOVE reclaim + unstage/detach on the repaired
  > stack. The client-side residual stands unchanged: a controller whose
  > ctrl_loss_tmo expired during the outage is deleted kernel-side and nothing
  > re-establishes it (csi-node re-stage machinery, still owed) — the production
  > 1800 s default makes that window generous.
- **Fallback lane**: the disposition ladder (`fallback_io_disposition_core`,
  `operations/mod.rs:246-339`) grows a block arm at the same dispatcher fork
  (`dispatcher.rs:2264`): the MDS reads/writes the volume's extents itself via an
  NVMe initiator — the only buildable option: the lvol bdevs live inside spdk-tgt's
  process and an lvolstore has one owner, so "just open the bdev" from the MDS does not
  exist. The initiator makes the MDS a data-path NVMe host doing I/O under the RTYPE=4h
  reservation it itself holds (registrants' — and the holder's — writes pass by
  definition; say so in the code). And the lane is not free-running: **it consults
  `extent_grants` first** — a fallback read/write to a range under an outstanding RW
  grant recalls (or refuses) before touching the device, per RFC 8154's
  recall-conflicting-layouts-before-MDS-I/O requirement. `GrantsExclusive` is
  client-vs-client only; this is the MDS-vs-client arm, and §9 models it with its own
  must-violate run. Load-bearing steady-state (§6). Kill-switch
  idiom per house style: one env var, default ON, checked once. Meter it (F68a
  precedent) from the first commit.
- **Cost budget**: LAYOUTGET-with-allocation **and LAYOUTCOMMIT** get A/B'd on the
  mdsbench harness (`make test-pnfs-mdsbench`) against the Tier-1 table (8.7k
  open/close, 3,531 w3-stat per shard) *before* the client work starts. LAYOUTCOMMIT is
  the declared hot path (split/merge churn under awaited FULL-sync transactions) and
  its worst case is ugly — wire blksize ≤ 4 KiB inside 4Mi extents means one commit
  list can shatter an extent into ~1024 rows; bench that shape, not the happy path. The
  allocator also ships a merge policy and a stated per-volume extent-count bound before
  phase 3; nothing else bounds table growth.

  > **GATE RUN 2026-08-09 — VERDICT: GO, with the merge-policy debt now a
  > measured slope.** Instrument: `state_backend/extent_bench.rs`
  > (`mdsbench_block_allocator_cost`, release build, lima aarch64 ext4,
  > production `StateBackend` path: writer thread + barrier + WAL/NORMAL).
  > The kernel client cannot speak type 5 yet, so the bench prices what is
  > NEW in the scsi path — the allocator transactions — against Tier-1's
  > per-op anchors; dispatch/XDR above it is shared with the files path.
  > Measured (µs/op unless noted):
  >
  > | shape | cost | note |
  > |---|---|---|
  > | admission SELECT | 42 | the per-LAYOUTGET fast path (≈ writer-path floor) |
  > | grant, fresh volume | ~250–310 | one 4Mi extent + hosts SELECT |
  > | grant at 1,000 rows | ~930–1,040 | the O(rows) verify tail |
  > | commit, 4KiB | 109 | no verify in commit; the steady-state shape |
  > | commit, 1×1024 rows | 5.8 ms | the worst-case single LAYOUTCOMMIT |
  > | return+free, 1024 rows | 5.5 ms | the REMOVE shape |
  >
  > Reading against Tier-1 (w2-opencl = 140 µs MDS-CPU per whole files-layout
  > open cycle): a scsi grant costs ~2× a whole files open at LOW row count
  > and serializes through the single writer thread (~1,700 grants/s/shard
  > ceiling at mid-shatter) — **but the scsi client's shape is grants-per-file
  > (1 GiB windows), not grants-per-open**, and commits (the true hot path)
  > are 109 µs unserialized-by-verify. GO for the rig. The debt, made
  > precise: `verify_volume_invariants` costs **~0.9 µs per volume row per
  > writing transaction** (measured slope, 1→1024 rows) — a 1 TiB volume
  > shattered at 4Mi granularity (262k rows) would pay ~230 ms per grant,
  > which is a NO at production scale. The phase-3 merge policy +
  > extent-count bound must therefore ALSO window the verify (check the
  > touched ranges against their neighbours, not the whole volume) — the
  > bound alone does not close a slope this steep.
  >
  > **MERGE POLICY SHIPPED 2026-08-10 (the debt paid; gate 106→109, bench re-run).**
  > Three pieces, all model-gated (FlintExtents merge tranche — MergeHeld's
  > counterexample is why quiescence is mandatory; MergeMin's green is the
  > machine-checked proof the merged-gen choice is safety-irrelevant, MAX kept as
  > free-list monotonicity hygiene):
  > 1. **Windowed verify** — every hot-path transaction now checks its TOUCHED rows
  >    against their immediate neighbours (complete by induction: a txn can only
  >    corrupt what it touches; anchored at the empty arena). The full verifier
  >    survives as a test/debug differential belt behind every windowed check, in
  >    whole-volume ops, and as the row-counter drift check. Plus the bench-found
  >    residual: the per-grant fenced-client check was an unindexed full-table scan —
  >    `idx_grants_client` closes it.
  > 2. **The merge** — at LAYOUTRETURN (the quiescence moment), adjacent rows of the
  >    file that are logically AND physically contiguous, same state, with ZERO grant
  >    rows coalesce to one row at MAX(gen); the free list coalesces on every insert
  >    (`free_insert_coalescing`, MAX(last_gen)). A sequentially-written file's N
  >    extents collapse to 1 the moment its layout returns — the bench's own 1024-row
  >    shatter now un-shatters at return (8.2 ms once) and reclaims as ONE row (0.1 ms,
  >    was 5.5 ms).
  > 3. **The stated bound** — `volume_alloc.extent_rows` (O(1) counter, drift-checked)
  >    against 65,536 rows/volume (FLINT_PNFS_EXTENT_ROW_BUDGET), refused as
  >    `RowBudget` → LAYOUTUNAVAILABLE with a loud fragmentation log. SCHEMA 6→7.
  >
  > Re-measured (same rig, release, lima ext4): grant at 1,000 rows **~930–1,040 →
  > ~250–270 µs** (≈ the fresh-volume shape; the 0.9 µs/row linear term is gone,
  > residual quartile drift ≈0.14 µs/row and btree-shaped); commit-4k 101 µs;
  > commit-1×1024 6.1 ms (unchanged — commit never merges: its rows are still
  > granted). The 262k-row nightmare arithmetic no longer exists: the bound refuses
  > at 65,536 and the hot paths no longer scale with the count anyway.

## 9. FlintExtents — the TLA+ module

First-class deliverable, not documentation garnish. **Sequencing rule: the module and
its drills come before any client work** — FlintTruncate exists because the one
invariant pNFS holds in its own hands deserved a machine check, and the extent
allocator holds a strictly larger one. Model the *implementation* (two-step grants, the
sqlite/volatile split), never the abstraction — this corpus has paid for
THE-ABSTRACTION-WAS-THE-BUG twice (atomic-LAYOUTGET going green on a property the code
doesn't hold, FlintTruncateGrantRace.cfg; scalar `raidHost` unable to represent two tgt
incarnations, FlintReplication.tla:711-730).

**Conventions inherited** (formal/README.md): FlintTruncate is the skeleton — same
layer, same three-party shape (MDS / storage / clients-that-bypass-the-MDS-on-reads),
same headline hazard (a ghost set by a data-path read the control plane never sees).
Boolean fix arms TRUE=belt-exists; `Inv_<Claim>` naming; safety-only spec, no fairness
in the breadth cfgs; every cfg lists the full constant vector; `CHECK_DEADLOCK FALSE`;
strict/mutation verdicts wired into `scripts/check-tla.sh` (substring matching, never
`grep -q`). Borrow FlintSnapshots' version-counter style for reuse detection and
FlintClaims' crash budget + WF bundles for the deep cfg. A `FlintExtentsProbe.tla`
sibling ships from day one — per the A2Probe standing rule, **no strict run is citable
without a paired non-vacuity probe TLC must violate**, and probes name the *action*
(provenance ghost with one writer), never the situation.

**State**: extents are **sets of blocks over a small physical domain**, not atomic
identities — §8's declared hot path is split/merge per LAYOUTCOMMIT, and fixed extent
identities make range aliasing (two logical extents overlapping one physical range
after a split/merge bug) unrepresentable, the scalar-`raidHost` mistake re-armed. So:
`alloc ∈ [Blocks → {free, provisional, committed}]` with extents as block-sets and
Split/Merge actions, `owner`, `gen` (bumped on every free→provisional — the reuse
detector; zeros from a fresh ProvisionalInvisible extent are not stale content, *bytes
from a previous owner are*, which is exactly why `gen` and not content-emptiness
carries the invariant), `grants` (records carrying `g` = gen-at-grant), `granting`
(the unpublished two-step window), `fsize`, `recalls`/`fenced`, `resv` (the
target-side reservation registry: registered keys + holder per namespace — a
**variable**, not an assumption, because §5's PTPL-is-mandatory hazard is
unrepresentable without target-side state a TgtRestart can erase), `durable` (the
sqlite image; volatile state dies with MdsCrash), budgets `ops`/`crashes`. Ghosts,
single-writer each: `staleRead`, `staleWrite`, `zeroRead`, `reuseFired`, `fenceFired`.

**Fix arms**: `GrantsExclusive`, `RecallBeforeReuse`, `FenceReaches` (the
`RecallReaches` analog — **keep FALSE in the shipped cfg until proven against real
spdk-tgt reservation behavior on real hardware, and re-justify it every time the code
moves** — a constant encoding an assumption silently becomes a lie), `ProvisionalInvisible`,
`CommitGatesSize`, `CommitChecksGen`, `PersistGrants`, `PersistReservations` (PTPL),
`RecoverConservative`, `PublishRecheck`, `RecallBlocksGrant` (the
NFS4ERR_RECALLCONFLICT arm), `FallbackChecksGrants`, `SplitKeepsDisjoint`. Every arm
has a run that fails without it — the matrix below; an arm with no failing run is dead
weight by this corpus's own doctrine.

> **TRANCHE-1 AMENDMENT (2026-08-09, FlintExtents.tla now exists — the module
> corrected this section before any allocator code was written).** The arm set
> above is **insufficient as sketched**: `PublishRecheck` and `RecallBlocksGrant`
> cannot close the grant-vs-reclaim races. A grant that passes its gate check
> before a reclaim starts and publishes after the reclaim's holder snapshot is
> invisible to that snapshot, and the reclaim can complete-and-free **between the
> grant's insert and any recheck** (FlintTruncate survives the analogous
> interleave only because its "free" — the set_len fanout — destroys the content;
> an extent free destroys nothing, the harm arrives with the next owner). Nor can
> any grant-time check compensate: the transaction validates against extent/grant
> rows, and a freed block has left the tables — the free is precisely the step
> that destroys the evidence. Safety therefore belongs to the **free side**: the
> free transaction re-validates the grants table and refuses while a live
> unfenced grant covers any block — the **`FreeRevalidates`** arm, sqlite-native
> (the free and the grant insert execute over the same tables).
> `FlintExtentsStaleSnapshotFree.cfg` pins the refuted grant-side-only design
> permanently. `PublishRecheck` and `RecallBlocksGrant` remain owed, demoted to
> **progress** arms (without them the reclaim wedges behind grants it must then
> fence) — their teeth are liveness runs, deferred with that tranche. The
> allocator implementation MUST put the holder re-validation inside the free
> transaction; a bookkeeping-only free is machine-refuted.

> **TRANCHE-2 AMENDMENT (2026-08-09, same day — LAYOUTCOMMIT/size/scrub are
> now modelled, behind `CommitEnabled`).** Three more corrections of this
> section's sketch, recorded in the matrix rows below, plus one
> self-correction: adding the committed state broke tranche 1's own
> `GrantInsert` disjointness predicate ("provisional ∧ held" stopped seeing
> committed-and-held blocks — TLC produced two live grants overlapping a
> committed block in 6 states). The predicate now reads "not free ∧ held";
> the general lesson is that a validation predicate ENUMERATING states goes
> stale the day the state set grows. The implementation
> (`extent_alloc.rs`) had the honest form already — its conflict check joins
> grant rows through overlapping extent rows regardless of state — so this
> was a model-only defect, caught by the model's own growth.

**Actions**: MDS — Allocate, Split/Merge (guarded by `SplitKeepsDisjoint`),
GrantCheck/GrantInsert (two-step; GrantCheck also refuses any range with a recall in
flight when `RecallBlocksGrant` — the NFS4ERR_RECALLCONFLICT obligation; a grant
landing between Recall and RecallAck re-arms the reuse hazard after the recall
"completes"), LayoutCommit (validates (client, gen-at-grant) against the live grant
when `CommitChecksGen` — §8's commit-time check), Recall (with the honest
`∃ lost ⊆ Clients: FenceReaches ⇒ lost = {}` arm), RecallAck, Fence (writes `resv`),
MdsRead/MdsWrite (the §8 fallback lane is steady-state, so the MDS is a data-path
actor in the model, not scenery — guarded by recall-or-refuse on conflicting RW grants
when `FallbackChecksGrants`), Truncate (recall-then-invalidate-then-free — *not*
set_len fanout; FlintTruncate stays the file-layout truncate authority, do not
re-model its gate), Free (guarded by RecallBeforeReuse; fenced-holder ranges
quarantine per §8), Reuse, MdsCrash/MdsRestart (restart re-acquires the reservation
through `resv` — fencing capability is **not** restored for free, §8 crash recovery).
Target — TgtRestart (clears `resv` unless `PersistReservations` — spdk-tgt is the
most-restarted component in the system, FlintReplication proved it; without this
action the model cannot state §5's "PTPL is mandatory" at all). Client — ClientRead
(sets `staleRead` iff grant.g ≠ gen[e]), ClientWrite (sets `staleWrite` iff
grant.g ≠ gen[e] or client ∈ fenced — the write, not the read, is the crown-jewel
hazard at this layer: §6 gives the client raw write reach, and a stale write corrupts
the *new* owner's committed bytes), LayoutReturn, ClientCrash (grants unreturnable;
only lease-expiry+fence clears them). **If two tgt incarnations can ever expose one
extent range, the serving-target state is a SET from day one** — hence `resv` as real
state, never a constant.

**Invariants**: `Inv_NoConflictingGrants`; `Inv_RecallCompletesBeforeReuse`;
`Inv_NoPhysicalAliasing` (no block under two live extents — the split/merge
silent-corruption class); `Inv_NoStaleExtentRead == ~staleRead` **and**
`Inv_NoStaleExtentWrite == ~staleWrite` — **the theorems**, descendants of
`Inv_NoStaleServe`, and the write is first-class, not a corollary: FlintTruncate's
read-only shape was right for its layer (the truncate hazard is stale reads) and would
be a blind spot copied here, because the entire point of RFC 8154/9561 fencing is
stopping *writes*, and a fenced client writing a reused extent is strictly worse than
a stale read. Both expected NOT to hold until fencing is code-real and kept out of the
shipped cfg exactly as FlintTruncate.cfg does (listing them would be the model
asserting a delivery the code does not achieve); `Inv_SizeCommitCoupled` (`~zeroRead` —
no observable size covers a provisional extent; the F67 shape);
`Inv_CrashRecoverySound`; TypeOK + structure checks — `GenMonotone` lives **here**,
not among the invariants: the spec is `gen`'s only writer, so it cannot fail, and an
invariant that cannot fail proves nothing (the dropped-run-5q doctrine). Liveness only in a 2-extent deep cfg
(`RecallResolves`, `FreedEventuallyReusable`) with the BounceStarve caveat: under a
crash budget, "transiently unavailable forever" needs an unbudgeted limbo constant.

**Cfg matrix** (every mutation single-flag; deliberate must-fail runs marked):

| cfg | arms | verdict |
|---|---|---|
| FlintExtents.cfg | shipped world: FenceReaches=FALSE, every belt shipped-TRUE | must HOLD, listing **every invariant except NoStaleExtentRead/Write** — only those two depend on FenceReaches. Excluding CrashRecoverySound / NoConflictingGrants / NoPhysicalAliasing / RecallCompletesBeforeReuse here would leave the shipped belts with zero citable greens, since Target.cfg is uncitable by its own row |
| FlintExtentsReuseUnderGrant.cfg | RecallBeforeReuse=FALSE | **must VIOLATE** NoStaleExtentWrite (and Read) — the F65-of-extents |
| FlintExtentsGrantOverlap.cfg | GrantsExclusive=FALSE | **must VIOLATE** NoConflictingGrants |
| FlintExtentsGrantRace.cfg | PublishRecheck=FALSE | ~~must VIOLATE~~ **superseded by the tranche-1 amendment**: with FreeRevalidates carrying safety, this arm has no safety teeth — returns as a *liveness* mutation (reclaim starvation) in the liveness tranche |
| FlintExtentsGrantDuringRecall.cfg | RecallBlocksGrant=FALSE | ~~must VIOLATE~~ **superseded likewise** — the RECALLCONFLICT obligation is a progress belt; liveness tranche |
| FlintExtentsStaleSnapshotFree.cfg | FreeRevalidates=FALSE | **must VIOLATE** RecallCompletesBeforeReuse — the tranche-1 finding, pinned (SHIPPED 2026-08-09) |
| FlintExtentsLostFence.cfg | all on except FenceReaches | **must VIOLATE** NoStaleExtentWrite — the standing residual (LostRecall analog); the harm is the **write** |
| FlintExtentsBlindProvision.cfg | ProvisionalInvisible=FALSE | **must VIOLATE** NoPriorOwnerDisclosure (SHIPPED 2026-08-09 — the sketch's "BlindCommit → SizeCommitCoupled" row misnamed both the hazard and the harm: the arm is scrub-at-allocation, and the disclosure is deleted-data resurrection, intra-volume by construction) |
| FlintExtentsUngatedSize.cfg | CommitGatesSize=FALSE | **must VIOLATE** SizeCommitCoupled (SHIPPED 2026-08-09 — restated TRANSACTIONALLY: the sketched "no observable size covers a provisional extent" is false on legal hole-filling; the theorem is that a size-advance never applies without its range promotion, and the mutation is the half-stub) |
| FlintExtentsForgedCommit.cfg | CommitChecksGen=FALSE | **must VIOLATE** NoForgedCommit (SHIPPED 2026-08-09 — ~~NoStaleExtentWrite~~ was impossible: a commit writes no bytes; the harm is bookkeeping corruption — the fenced client's control path promoting extents it no longer owns — stated as its own theorem) |
| FlintExtentsBlindFallback.cfg | FallbackChecksGrants=FALSE | **must VIOLATE** NoStaleExtentWrite — the MDS-vs-grant arm (§8 fallback discipline) |
| FlintExtentsTgtAmnesia.cfg | PersistReservations=FALSE | **must VIOLATE** NoStaleExtentWrite — §5's "PTPL is mandatory", with teeth |
| FlintExtentsAliasedSplit.cfg | SplitKeepsDisjoint=FALSE | **must VIOLATE** NoPhysicalAliasing |
| FlintExtentsCrashAmnesia.cfg / RecoverOptimist.cfg | PersistGrants / RecoverConservative =FALSE | **must VIOLATE** CrashRecoverySound |
| FlintExtentsTarget.cfg | all arms TRUE | HOLDS — conditional green, **cite as goal only** |
| FlintExtentsProbe*.cfg (one per ghost, ×5) | — | **must VIOLATE** `Probe*Fires` — non-vacuity, no ghost exempt |

Plus a reserved MarkOverwrite-style slot for the first refuted second-belt hypothesis.
Sizing: 2 clients × 2 files × a 4-block physical domain (extents as block-sets),
MaxReuses≈2, MaxCrashes≤2, a GenBound-style CONSTRAINT — target 10⁴-10⁶ states,
breadth invariants-only (temporal checking is ~94% of a flagship run's cost; a cfg
carries the arms its claim needs and no more). The README's run count and
`check-tla.sh`'s header count both move with this matrix — that header has a recorded
history of miscounting, so the bump is a named deliverable, not a side effect.

Layering: byte durability at an extent is an **axiom with no license — say so, don't
dress it up.** The FlintSnapshots pattern (borrow the substrate's invariants) does not
apply here: FlintReplication models the consumer-node raid1 machine, and §1 concedes
block-class volumes are single-replica lvols precisely because that machine cannot
serve remote pNFS clients — the licensor's guarantees are vacuous for this tier. A
committed extent's bytes survive exactly as well as one lvol does, and no formal
durability story exists until server-side replication (§12) lands, at which point
FlintReplication (or a successor) must be re-scoped to cover it; citing a green
FlintExtents run as a durability claim would be the axiom laundering itself. And
control-plane reachability (recall delivery, reservation delivery) is unmodelled
anywhere in the corpus, so FlintExtents carries it as explicit constants with failing
runs, never delegates it.

## 10. RDMA

**NVMe-oF/RDMA is free at both ends.** SPDK's target compiles RDMA in our image already
(`--with-rdma`, Dockerfile.spdk:109, rdma-core in the runtime stage) — enabling it is
one runtime `nvmf_create_transport {"trtype":"RDMA"}` plus RDMA listeners; the SPDK
initiator (libflint's data plane) implements it equally. RFC 9561 is explicitly
transport-independent (§1 — "independent of the underlying transport used by the NVMe
Controller"). No protocol work on our side at all.

Fabric matrix:

| Fabric | Verdict |
|---|---|
| On-prem RoCE / InfiniBand | Yes — standard NVMe-oF/RDMA territory |
| Azure HBv3 (the planned rig) | Yes — **this is the planned validation site** (Azure South Central plan: Lsv4 DSes + HBv3 RDMA rig) |
| AWS / EFA | **No** — EFA has no RC verbs; SPDK nvmf RDMA needs RC. The AWS answer is NVMe/TCP + ENA Express, full stop |

Strategic note: this *demotes* the RDMA workstream's M2 for the fast tier. M2
(RPC-over-RDMA chunked READ/WRITE, M1 proven on lima Soft-RoCE) becomes the **file-tier
RDMA story** — still worth having for the general-purpose tier, no longer the fast
tier's path to zero-copy. The block tier gets RDMA by flipping a transport flag.

## 11. Phasing

Each phase ships standalone value; none is gated on the next.

1. **MDS allocator + full wire surface behind the per-volume gate, under FlintExtents
   + drills.** Enum fix (§1), FlintExtents module + full cfg matrix green in
   `check-tla.sh`, allocator with sqlite schema, LAYOUTGET/LAYOUTCOMMIT arms, the
   `pnfs_scsi_deviceaddr4` GETDEVICEINFO encoder with real TOOSMALL/maxcount handling
   (§5), per-volume FATTR4_FS_LAYOUT_TYPES + LAYOUT_BLKSIZE at every emitting site
   (§3/§4a), mdsbench A/B. No client; the wire surface is **present but gated
   per-volume, off by default** — not "no wire change", which would contradict the
   next sentence: flipping pynfs's BLOCK tests requires answering block-layout ops on
   the wire. Standalone value: the model and the allocator are testable against pynfs
   `st_getdevicelist` on a block-class test export — the pinned baseline's 3
   expected-fail BLOCK tests (`mds-sharding-plan.md`) flip to in-scope.
2. **Fencing machinery + stock-kernel validation on lima/kind with nvme-tcp.** This
   phase *builds* what it then proves — each item here belongs to no earlier phase and
   must not fall into the gap: the MDS's NVMe initiator with stable hostnqn+hostid and
   reservation acquire/preempt orchestration (§5 consequence 1, §8 crash recovery),
   the grant/recall-driven add_host/remove_host rework of `converge_hosts` (§5), the
   udev rule shipping in csi-node (§4a), per-node hostnqn registration via
   ControllerPublish (§5). Rig prerequisite: client kernel **≥ 6.11 with
   CONFIG_PNFS_BLOCK** — stock lima images and Ubuntu 24.04 (6.8) sit below the floor,
   and a below-floor client silently validates MDS I/O instead of the thing under test
   (§4a). Lima runs a full Linux server+client rig with no cluster (the v4.2
   copy/sparse precedent); kind runs real spdk-tgt. Prove: device resolution (udev
   rule), reads/writes/LAYOUTCOMMIT, fencing via reservations with PTPL across a tgt
   restart, blksize ≤ 4 KiB advertisement.
3. **Chart class + NetworkPolicy + roll-safety hardening.** `pnfs-block` SC, 4420
   policy (with the hostNetwork caveat resolved or documented), discriminator ctx key,
   unstage ordering fix. **Prerequisite, not optional**: today's roll orchestration is
   blind to remote initiators — the consumer model is `volumes[].consumer`, so a "safe"
   roll restarts a tgt under N live clients with zero signal; and the nvmeof preStop
   deletes subsystems out from under them. The MDS layout table feeds the roller
   (or ANA-INACCESSIBLE draining + CB_LAYOUTRECALL before preStop). The drain-roll
   chart flag is already **ON by default since 1.22.0** (drill 3.14 passed on runap ×4
   and again on runar) — what remains from that work is its LOCAL half (A2, rolling a
   node that hosts consumers; design-only,
   `docs/f62-local-half-outage-and-blind-barrier.md`), on top of the remote-initiator
   blindness above. GA also gains a **kernel admission check** — **SHIPPED
   2026-08-10**: `pnfs_block_session::kernel_block_layout_support` (floor 6.11,
   `FLINT_PNFS_BLOCK_KERNEL_OVERRIDE=1` for distro backports, loud) gates every
   session mouth — NodeStage (FailedPrecondition: retries can't change a kernel),
   the CLI `stage` (BEFORE the attach RPC, so an unstageable node never plants a
   durable attach row), and `ensure_session` itself (covers re-establishment onto
   a downgrade-booted node) — plus a startup banner ("pnfs-block DISABLED on this
   node", file layout unaffected). Proven live both directions: a stock Ubuntu
   24.04 VM (6.8.0-106) refused before touching the endpoint and sailed past the
   gate with the override; the 7.0 rig stages through the same code green. The
   MDS block-lane meter (§6, F68 precedent) stays as the backstop degradation
   detector — the check is per-node prevention, the meter is fleet-wide proof.
   **MULTI-HOST PROVEN 2026-08-11** (`make test-pnfs-multi-rig`, §M): a second lima VM
   stages the SAME volume through the production attach verb over a host TCP bridge,
   and the properties that only exist with two hosts are asserted — admission is
   ADDITIVE (both NQNs on the allow-list at once), both clients write raw with a
   PHYSICALLY DISJOINT extent map (GrantsExclusive on real hardware, across hosts),
   same-file contention is REFUSED rather than overlapped (18 conflict refusals, map
   still disjoint, first client's bytes intact), and a fence naming ONE client evicts
   only that client — its raw writer stops at the device (EIO) while the OTHER host
   keeps writing THROUGH the fence. That last result answers the open question the
   drill was built for: the EA-RO reservation admits the surviving registrant, so a
   per-client fence is per-client at the DEVICE too, not just on the allow-list.

   > **ROLL-SAFETY SHIPPED 2026-08-11 — the roller can see block initiators.**
   > The blindness above was worse than "no signal": a block volume is
   > single-replica, so `gather_volume_maint` skipped it at the
   > `replicas_from_pv` guard and it never entered the roller's world at all
   > — no marks, no in-sync map, no consumer. A "safe" roll deleted the
   > csi-node pod whose spdk-tgt hosts the export and every remote kernel
   > initiator lost its namespace mid-write, with the MDS's own fallback
   > lane dying alongside it (same tgt, shared socket). There is no drain
   > that survives that, so the answer is REFUSAL in F62's vocabulary, not
   > a barrier.
   >
   > **The fact**: new `BlockExportStatus` MdsControl RPC — `enabled`, the
   > export's node (from the MDS pod's own downward-API `spec.nodeName`,
   > since the hostPath socket makes MDS and tgt colocated by
   > construction), its listener, and every live initiator across every
   > volume on the shard (`block_node_attach` ∪ `block_hosts`, with
   > provenance). The controller unions it across ALL shards
   > (`block_export_status_all`) — each shard runs its own tgt on its own
   > node, so only the union answers "may I restart node X".
   > `RollStep::Refused` now carries a per-node `RefusalCause`, because the
   > two causes need different operators: a local consumer is visible in
   > `kubectl get pods -o wide`, block clients are on other nodes and hold
   > no object here at all.
   >
   > **Three things that had to be got right, each with a test:**
   > (a) *Idle exports must still roll.* The predicate is "hosts an export
   > AND someone is connected", never "hosts an export" — the F61 livelock
   > is one careless `!initiators.is_empty()` away.
   > (b) *Unreachable ≠ empty.* A shard that cannot answer fails the whole
   > union and PAUSES the campaign; an empty list is permission to restart
   > a target, and the two must never arrive as the same value. The tick
   > still renews marks on the way out, so an MDS outage cannot quietly
   > lapse a mid-roll node's suppression.
   > (c) *Client-earned admissions expire, or the fix ships its own
   > livelock.* `block_hosts` rows are removed by NOTHING in the normal
   > lifecycle — not unmount, not unstage, not detach; only a fence or
   > DeleteVolume — and the lease sweep only visits clients holding GRANT
   > rows, so a clean unmount leaves a row that reports an initiator
   > forever. The report now lease-filters client-earned rows (the layout
   > manager takes the same `alive` oracle the sweep does); node
   > attachments are NOT filtered, because ControllerUnpublish already owns
   > their lifetime.
   >
   > Also handled: initiators arriving DURING a roll (F63's completion-path
   > hole, block edition — ClearMarks, never DeletePod, or a live roller
   > would renew the drained leg's suppression forever), and an MDS too old
   > to name its own node (the listener address resolves against the Node
   > objects; a busy export that still cannot be named is an ERROR, since
   > "somebody loses their device but we can't say whose roll does it" must
   > not degrade to "nobody"). Rig-proven in §R of `block-rig.sh`: a real
   > kernel session shows up in the report while it writes, both hosts are
   > counted under MULTI=1, a FENCED host drops out while its live sibling
   > stays, and the count converges to 0 after unstage+detach.
   >
   > **Still open here**: the preStop hook (`node.yaml`) deletes NVMe-oF
   > subsystems on shutdown, so a MANUAL pod delete, eviction or node drain
   > still cuts block clients with no warning — the roller only governs its
   > own campaign. And a refusal is terminal: nothing yet recalls layouts
   > and drains block clients so the roll can proceed (the
   > ANA-INACCESSIBLE + CB_LAYOUTRECALL half of this item).
4. **Registry storage driver on the userspace library.** libflint (metadata client +
   SPDK TCP initiator), surfaced as the OCI-registry driver per
   `docs/oci-registry-pnfs-architecture.md`.
5. **RDMA on the Azure rig.** HBv3 validation of NVMe-oF/RDMA end-to-end, including
   the kernel pr_ops preempt path over the fabric (individually confirmed both sides,
   never tested in combination — §12).

## 12. Risks and open questions

- **Capacity semantics become real.** The file layout inherited allocation, sparse
  semantics, and ENOSPC from ext4. The allocator now owns: real allocation at LAYOUTGET
  time, real expansion (§7), thin provisioning policy, and an ENOSPC story that isn't
  "fallback FailFasts and the app sees EIO" (the current runbook residual).
  **PARTLY CLOSED 2026-08-11**: real expansion is shipped and rig-proven (§7 box), and
  the ENOSPC story is honest at the wire — the MDS lane now answers NFS4ERR_NOSPC when
  the arena is exhausted instead of the blanket EIO, so an application on a full block
  volume sees ENOSPC (rig: a 32 MiB volume filled to exactly its ceiling, `dd` reported
  "No space left on device"). What REMAINS: thin-provisioning policy (the lvol is thin
  and the ceiling is logical, so a fleet can oversubscribe its lvolstore and discover it
  at write time, where the errno is an lvol-level failure and not this ENOSPC path),
  capacity-aware placement across shards, and the client-side refresh
  (CB_NOTIFY_DEVICEID) that would make expansion online end to end.
  **THIN-PROVISIONING OVERSUBSCRIPTION CLOSED 2026-08-12** — create and expand now
  gate on the lvolstore, and the gate had to be a LOGICAL one. SPDK will not make
  this check itself: `blob_resize` skips its free-cluster test entirely for thin
  blobs (`lib/blob/blobstore.c:2292`, `spdk_blob_is_thin_provisioned(blob) ==
  false`), so an oversubscribed create or grow succeeds at the device, the arena
  ceiling follows it, the PVC reports its full size, and the application meets the
  truth at write time. Nor could `free_clusters` answer it — a thin lvol consumes
  no clusters until written, and the rig shows a volume admitted with **544 MiB
  promised and 956 MiB still physically free**. The gate therefore compares
  PROMISED logical bytes (summed from `bdev_get_bdevs`, filtered to this store by
  alias) plus the new request against `total_data_clusters × cluster_size`, and
  refuses with those numbers in the message. `FLINT_PNFS_BLOCK_OVERCOMMIT=1` opts
  back in, loudly — thin provisioning legitimately means overcommitting, but the
  cost lands on an application as a failed write rather than on the operator as a
  refused PVC, so it is never the default. An unreadable store PROCEEDS (a blipped
  RPC must not block every provision in the fleet); that is the opposite of the
  roller's fail-closed rule and deliberately so, because there the failure mode is
  data loss and here it is a refused provision. Rig-proven: 4 GiB refused on a
  1 GiB store with no lvol created. **It also found a latent bug in the driver's
  own lvstore parsing** — `spdk_native.rs` read `total_clusters`, which SPDK does
  not emit (it writes `total_data_clusters`), so that field had been silently 0 on
  every lvstore; the gate read 0 and disabled itself on its first rig run, which
  is exactly the kind of self-disabling a unit test against a fake cannot catch.
- **An MDS restart made the next expand fail with EIO — MEASURED
  2026-08-12, and FIXED the same day.** The device-notify address book
  (`LayoutManager.device_notify`) is an in-memory map of which sessions
  fetched which volume's device. `EXPAND=1 MDS_BOUNCE=1` bounces the MDS
  between the fetch and the expand, against the identical drill without
  the bounce: control = 1 client took CB_NOTIFY_DEVICEID and the same
  mount wrote past the old ceiling; bounce = **0 notifications and the
  write failed EIO**, with 52 zeros-belt refusals as the client fell back
  to the MDS lane. So the old "costs a missed notification, recycle the
  mount" reading was wrong — the application gets an I/O error on a
  volume that has the space.
  **The same run decided how to fix it.** The MDS log shows startup
  *deliberately* discarding persisted sessions ("dropped them so kernel
  re-CREATE_SESSIONs naturally on BADSESSION"); the client then does
  CREATE_SESSION with a NEW session id under its EXISTING clientid (no
  EXCHANGE_ID) and issues **no fresh GETDEVICEINFO** — its cached
  blocklayout device outlives the session that fetched it. Therefore:
  persisting the book as it stands, keyed on `SessionId`, would restore
  addresses that provably no longer exist; and deriving targets from
  layout holders cannot work either, since the dangerous client holds no
  layout.
  **Shipped exactly that**: `device_notify(volume, client_id, notify_mask,
  fetched_unix)` (schema 10), written at GETDEVICEINFO, read at expand,
  with the session resolved at send time by
  `CallbackManager::send_notify_deviceid_to_client` — which tries every
  session the client holds NOW, because after a restart that is never
  the session that fetched. Rows are dropped when the volume goes away or
  the client's lease expires, and **never because a send failed**: right
  after a restart a live client has no back-channel yet, and pruning
  there would re-create the bug. Same drill, after: 1 notification
  accepted, same mount wrote past the old ceiling sha-intact
  (`make test-pnfs-expand-bounce-rig`).
  **The alternative was investigated and REJECTED on kernel evidence.**
  Generation-stamping the deviceid (`hash(volume, gen)`, bumped on
  expand) would have removed the book, its durability and the
  back-channel from the correctness path — but the SCSI device object
  owns the PR key registration: `bl_register_scsi` registers
  (`pr_register(bdev, 0, dev->pr_key, true)`) on every deviceid
  resolution, and `bl_free_device` **unregisters unconditionally**
  (`fs/nfs/blocklayout/dev.c:23,39`; `blocklayout.c:592`). Two device
  objects for one namespace carrying the same per-client `pr_key` means
  freeing the stale one unregisters the key the live one still needs,
  and the live object keeps its `PNFS_BDEV_REGISTERED` bit so it never
  re-registers — silently turning a registrant into a non-registrant.
  That is precisely the property multi-rig M4c proved (host A wrote
  THROUGH B's fence *because* it was a registrant under EA-RO). Distinct
  keys per generation would fix it only by making a fence preempt every
  key a client holds, which `fenced_clients` and the preempt path are
  not built for. The open half of the *open* was fine — `bl_open_path`
  passes a NULL holder (`dev.c:373`), so two objects over one bdev
  coexist — and `d->len = bdev_nr_bytes(bdev)` (`dev.c:416`) confirms the
  cached length is snapshotted from the kernel's bdev view at parse time,
  which is why only a re-parse fixes it.
- **LAYOUTCOMMIT after LAYOUTRETURN — FIXED 2026-08-11 (commit-grace tranche).**
  Found by the zombie drill 2026-08-11 (pre-existing — the rule dates from the
  allocator's first commit, 4afb9b2). `extent_alloc::commit` validates against a LIVE
  grant row for `(file_id, logical_offset, client_id)`; the Linux client, writing 8 MiB
  through 1 MiB grant windows, repeatedly LAYOUTRETURNs and only then LAYOUTCOMMITs, so
  the commit lands on rows that were just dropped and is rejected with "no grant for
  this client". The extents stay `invalid`, the stub's size never advances, and the
  file is durably SHORT — the drill's successor.bin ends at 4 MiB of 8 MiB written,
  which no amount of re-reading fixes (an earlier session misread exactly this as a
  cold-read/commit race and papered it with a polling read-back; the poll cannot help,
  because the bytes are genuinely uncommitted). Requiring a live grant is stronger than
  the model needs: the invariant `CommitRejected` protects is that a commit must not
  promote an extent whose GENERATION has moved on (freed and re-granted to someone
  else) — a client finishing its own write it just returned is not that. Fix shape:
  validate the commit against the extent's generation and the returning client's
  recent ownership rather than a currently-live row.
  **Shipped exactly that.** `layout_return` now leaves a generation record behind
  (`extent_commit_grace`: volume, file_id, offset, client, gen) before dropping the
  grant rows, and `commit_extents` accepts a live grant OR that record — with the SAME
  generation check on both doors. The record is deliberately not holdership: nothing in
  the conflict, reclaim or free paths reads the table, so a returned client still
  cannot block a reclaim (pinned by a test). Safety rests on the generation, not on the
  row's liveness — after a free+reuse the block's gen has moved and a stale record
  refuses on its own, which is what lets the table be pruned lazily (hygiene on
  reclaim) instead of exactly. A fenced client gets no door: `fence_client` drops its
  grace rows in the same transaction, and a return of already-fenced rows mints none.
  Model tranche: `CommitGraceEnabled` + `graceG` in FlintExtents, with the A/B that
  makes it mean something — with grace OFF the model reproduces the shipped-until-now
  refusal and the `commitAfterReturn` probe HOLDS; with it ON the probe must be
  VIOLATED (the door is reachable), while InvCommit and Inv_NoForgedCommit stay green.
- **Kernel blocklayout maturity.** Mainline since v6.11, near-zero production soak over
  fabrics, no known production RFC 9561 deployment to learn from. Every client bug
  degrades silently to MDS I/O, which both masks it and moves its load to the MDS.
  Mitigation: F68-style metering on the MDS block lane, and the phase-2 lima rig stays
  a standing regression harness.
- **Replication for the block tier** (§1 caveat). Server-side raid on the storage node,
  or MDS-level mirroring across namespaces with the MDS coordinating writes — the
  latter re-inserts the MDS into the data path and is probably disqualified. Undecided;
  phase 1-3 ship single-replica with `reclaimPolicy` and workload guidance saying so.
- **Fencing end-to-end**: ~~confirmed on each side, untested in combination~~ **PROVEN
  in combination 2026-08-10** (§5 RIG-PROVEN box; `make test-pnfs-fence-rig`): a real
  Linux client mid-write was stopped at the device by the MDS's NVMe reservation, with
  the mechanism turning out to be EA-RO acquisition against a non-registrant kernel, not
  key-preempt. **This proves FenceReaches *for this tgt/kernel pairing on the rig*; it
  does NOT flip the shipped cfg.** The `FenceReaches` constant stays FALSE until the
  formal-model gate is re-run with it TRUE (the FlintExtentsLostFence residual re-modeled
  and the 99-run gate green) — a separate, deliberate step, not taken here. Until that
  flip GC still **quarantines** fenced-holder extents rather than reusing them (§8):
  reuse-after-unproven-fence stays designed out. What the rig changes is the *confidence*
  that the flip is safe to pursue, and it becomes the standing regression harness that
  keeps the fence path honest between now and then. Open sub-item: the rig proves reach
  on ONE tgt; multi-namespace / multi-tgt preempt and the MdsRestart re-acquire
  (reservation holdership is target-side, not in sqlite) still need their own drills.
- **CoW / CLONE**: RFC 8154 §2.4.5 gives the extent vocabulary (READ_DATA source +
  INVALID_DATA dest, client merges); the Linux client expects the *server* to
  orchestrate CoW. Refcounted extents in the allocator are sketched but unproven;
  snapshots/clones stay refused for the block class (inherited guard) until designed.
- **udev landmine longevity**: whether newer systemd creates NGUID by-id links natively
  is unresolved. We ship the rule regardless; revisit per-distro.
- **GETDEVICEINFO body size**: volume topology bodies vs the ignored `maxcount` —
  TOOSMALL handling becomes real; small, but wire-visible if skipped.
- **Shared free-space accounting across shards** if we ever want a pooled allocator:
  per-shard sqlite files can't account a shared namespace without static carving.
  Current answer (per-volume allocation) dodges it; written down so nobody
  reintroduces a pool "for efficiency" without meeting the sharding invariant.

Cross-references: `docs/decisions/0001-keep-one-driver-defer-pnfs-split.md` (boundary
discipline — a second layout type is arguably its revisit trigger),
`docs/decisions/0004-pnfs-cross-host-scaling.md` (file-layout baseline + honest
caveats), `docs/decisions/0005-pnfs-durable-ds-replication-cost.md` (lvol substrate
cost; the local-leg read advantage does **not** transfer — block clients are always
remote), `docs/plans/pnfs-durable-ds-plan.md` (Phase 0 pin-at-first-grant precedent;
milestone B MDS-HA), `docs/plans/mds-fallback-proxy-plan.md` (§2 refuted cheap fixes,
§3.6 disposition ladder), `docs/plans/mds-sharding-plan.md` (shard-local allocator
constraint), `docs/plans/mds-performance-plan.md` (mdsbench budget),
`docs/pnfs-operator-runbook.md` (trust model → extend to 4420; known residuals;
truncate machinery the block class retires), `docs/oci-registry-pnfs-architecture.md`
(§2/§5 ceilings; the registry driver target), `docs/cluster-bringup-runbook.md` (read
it first before any live gate). Measurements land as ADR 0006+ in `docs/decisions/`
with raw data in `docs/decisions/data/`, pass criteria declared before the run.
