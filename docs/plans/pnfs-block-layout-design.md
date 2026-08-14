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
§12, not a freebie — reviewed and modeled 2026-08-12 (§12's replication entry;
`formal/FlintComposition.tla`), still unimplemented.

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
  firewall, outside the chart.
  **WRITTEN UP 2026-08-12** — `docs/pnfs-operator-runbook.md`, "Port 4420 (NVMe-oF) —
  the gap, stated plainly": what actually enforces today (default-closed per-subsystem
  allow-list, reservations during a fence), the honest weakness (host NQNs are
  DETERMINISTIC — `nqn.2024-11.com.flint:node:<node>` — so the allow-list authorizes a
  name the client asserts about itself; reaching 4420 and naming an admitted node gets
  raw namespace access, bypassing every NFS-level control), the mandatory out-of-chart
  mitigation, and the two upgrade paths verified present in the SPDK we ship:
  **DH-HMAC-CHAP** via `nvmf_subsystem_add_host`'s `dhchap_key`/`dhchap_ctrlr_key`
  (`lib/nvmf/nvmf_rpc.c:1880`, keys in SPDK's keyring) — the one that turns the NQN
  from a claim into a proof — and listener **TLS** via `secure_channel`
  (`nvmf_rpc.c:637`, refused alongside `allow_any_host` at `:908`), which gives
  confidentiality but not host authentication. Neither is wired; both need key
  distribution in the attach flow.
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
  >     WRONG**: the MDS holds EA-RO **only during a fence**, never continuously, so
  >     there is no standing per-volume reservation to re-acquire. (The original
  >     reasoning — "continuous 4h would fence every client, since none registers" —
  >     was based on the retired no-key claim; the conclusion survives it on the
  >     stronger ground that a client registers only while it holds a resolved
  >     device, so a standing EA-RO would fence every client that is between
  >     devices. See §12.)
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
  > remains fenced on the volume: the reservation is volume-wide (EA-RO blocks every
  > non-registrant, and a client holds a key only while it has a resolved device —
  > §12), so it must outlive any single unfence while a sibling's fence stands. Fenced GRANT rows are deliberately untouched — they
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
| FlintExtentsQuarantineBlindRelease.cfg | QuarantineChecksDelivered=FALSE | **must VIOLATE** NoStaleExtentWrite (SHIPPED 2026-08-12) — the sweep hands a PARKED range back without re-checking that the clients IT was parked with are confirmed excluded. LostFence's corruption through the other door: the free correctly refused at reclaim time, taken later without the check |
| FlintExtentsQuarantineVisible.cfg | QuarantineIsolated=FALSE | **must VIOLATE** RecallCompletesBeforeReuse (SHIPPED 2026-08-12) — the parked range keeps its `extents` row instead of moving to the third table, so it reads as an ORPHAN (allocated, no live holder) and the grant path re-hands it out at its old generation. Nine states. This is the run that closed the "two-step grant window" constraint by showing it was never about the window: `grant` is one immediate transaction, and the model's rendering of quarantine was the bug |
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
FlintExtents run as a durability claim would be the axiom laundering itself.
**The successor now exists (2026-08-12): `FlintComposition.tla` models the
serving-composition/failover half of §12's replication design ahead of its code —
control-plane arbitration only; the byte-durability axiom stands unlicensed until
the data-plane tranche (write-hole belt, rebuild) joins it.** And
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
3. **Chart class + NetworkPolicy + roll-safety hardening.**
   > **KIND CHART PASS DONE 2026-08-12** (`make test-kind-chart-pass`). The docker
   > VM kernel is 5.10-linuxkit, far below the 6.11 client floor, so no kind node
   > can ever stage a block volume — and the kernel-floor REFUSAL is already proven
   > on real hardware (a stock 24.04 VM at 6.8 refused before touching the
   > endpoint). What kind adds instead, and nothing else covers: the chart's
   > pnfs-block surface is accepted by a REAL API server. `helm template` only
   > proves the templates produce text; `kubectl apply --dry-run=server` proves
   > Kubernetes agrees the objects are legal — no images, no flint pods, ~1 GB of
   > Docker. The pass covers four value shapes (pnfs off; pnfs on/block off; block
   > on; SC opt-in), asserts the guards REFUSE (blockExport without `lvstore` or
   > `traddr` must fail to render, naming each), and pins the surface that has
   > drifted before: the `pnfs-block` SC, `FLINT_PNFS_BLOCK_LAYOUT` on the
   > controller, `FLINT_NODE_NAME` wired to the downward API (the roller's
   > export-node join key), the PTPL hostPath being outside `/var/tmp` (where
   > systemd-tmpfiles would age the fence out), the §4a udev surface, and the
   > `nodes` RBAC the roller's fallback resolution needs. A/B'd by deleting
   > `FLINT_NODE_NAME` from the template — the pass goes red on exactly that check.
   > Prerequisite it does NOT paper over: the chart's VolumeSnapshotClass needs the
   > external-snapshotter CRDs, a genuine cluster dependency (values.yaml says so),
   > so the pass disables that one object rather than pretending it validates. `pnfs-block` SC, 4420
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
  **AMENDED 2026-08-13 — A SNAPSHOT IS CHARGED WHAT IT HOLDS, NOT WHAT IT SPANS.**
  The rule above ("promise = logical size") is right for a writable blob and wrong
  for a read-only one, and the replication work made that reachable. A snapshot
  blob cannot allocate another cluster (`spdk_blob_is_read_only` — it is why a
  shallow copy demands one as its source), so its footprint is fixed at
  `num_allocated_clusters` and its logical size is a promise nobody can redeem.
  Charging it the span DOUBLE-COUNTS the volume: at the instant of a cut the
  snapshot owns all the head's clusters and the head owns none, yet both are
  billed in full. That is precisely the rebuild ladder's shape (§13) — one cut per
  round, every one alive until the window closes — so billing spans made a rebuild
  read as a full store and refused every unrelated create, expand or leg-host on
  that target, with a message blaming the operator ("delete a volume, grow the
  lvolstore") for flint's own transient state. The sum remains a true UPPER BOUND
  on future physical use: every writable blob is charged everything it may yet
  allocate, every read-only one exactly what it already did, so the gate still
  refuses in the direction it exists to refuse in — an unwritten cut costs
  nothing, a cut holding 8 MiB costs 8 MiB. Rig §4c asserts the two field names
  (`snapshot`, `num_allocated_clusters`) against a REAL SPDK snapshot rather than
  the fake, because this gate's safe branch is PROCEED: a mis-parse reads as "not
  a snapshot" and silently restores the double-count, which is the same shape as
  the `total_clusters` bug above.
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
- **Replication for the block tier** (§1 caveat) — **REVIEWED 2026-08-12 and MODELED
  the same day; still unimplemented, no longer undecided.** A 17-agent adversarial
  review (5 mappers / 4 dimension reviewers / 8 independent refutation passes, every
  SPDK claim re-verified against the v26.05 checkout flint builds) answered the
  standing question: **SPDK raid1 under the exported namespace is the substrate, and
  it is the disk arm ONLY.** MDS-level mirroring stays disqualified (re-inserts the
  MDS into the data path); raid1 keeps the write path client → composing tgt → legs,
  with a remote leg attached over nvme-tcp via `bdev_nvme`. What raid1 verifiably
  provides: mirrored writes with FLUSH/UNMAP fan-out, degraded serving, a
  quiesced-window rebuild engine with a bandwidth cap, superblock auto-assembly
  guarded by `-EBUSY` for an online array, online grow to min-of-legs, deterministic
  thin-zero reads, and identity that already survives a node swap (NGUID/UUID/subnqn
  are pure functions of the volume id — the survivor reproduces them from zero
  migrated state). What it cannot do, each verified in source:
  (1) **ack semantics too weak for LAYOUTCOMMIT alone** — a write acks when all legs
  *respond* but ANY one succeeded, and the record that a leg missed it is written
  async after the ack behind a quiesce (`bdev_raid.c:705-718`, `2440-2444`); crash in
  the window ⇒ equal-seq legs reassemble clean, **no scrub or resync exists** (the
  only process type is REBUILD), reads flap between divergent legs;
  (2) **a survivor cannot self-promote** — its superblock still lists the dead peer
  CONFIGURED, so auto-assembly parks in CONFIGURING forever
  (`bdev_raid.c:3384-3396`, `3730-3737`); any force-degraded lever added naively
  mints mutual solo-online split-brain that seq numbers cannot arbitrate, because
  neither process ever sees the other's leg;
  (3) **rebuild is a full-arena copy** — zero delta tracking, zero zero-detection ⇒
  densifies the thin target leg, hours at multi-TB, ~$20/TiB cross-AZ (legs are
  **same-AZ by placement policy**; AZ-loss durability is a priced tier, not a default).
  Note the asymmetry with the file tier, whose catch-up is blobstore-level and
  allocation-aware — for a thin volume, stock raid1 rebuild moves 5-10× the bytes the
  file tier would for the same volume, and leaves the target leg fully densified.
  **DECIDED 2026-08-12: the first implementation does NOT use raid1's own rebuild
  process for the copy.** flint drives the rebuild itself, sparse-aware —
  `SEEK_DATA`/`SEEK_HOLE` against the source lvol (lvol bdevs support it), copying
  allocated clusters only — so rebuild cost is proportional to allocated bytes, the
  thin target stays thin, and the cross-AZ worst case scales with data, not logical
  size. (SHIPPED as `bdev_lvol_start_shallow_copy` instead, which reads the blob's
  cluster map directly rather than seeking over it — same property, no round trips,
  and the same primitive the file tier already uses. See the rebuild entry below.) The incremental ladder above that is the "rebuilds at scale" tranche, in
  cost order: degraded-window dirty tracking (falls out of the DegradeBarrier
  interposition the model already requires — delta rejoin for the
  brief-absence case), lvol snapshot-diff at the stale-mark cut (the file tier's
  esnap shape one layer down), and a persistent md-style crash-window bitmap (the
  only one that makes the write-hole forced resync incremental). Safety
  precondition for ANY delta path, RejoinGuard transferred: a leg may rejoin
  incrementally only if its content is provably the cut state — which the block
  tier can prove cheaply, because the leg-export admission gate means nothing else
  could have written the absent leg and the record's generation pins which cut it
  left at. **Tranche 2 SHIPPED same day**: record-driven rebuild/rejoin modeled with
  the ancestry rule as a belt (`AncestryGuard` — its A/B shows the delta door opened
  to a leg with bytes of its own leaves live divergence in the composition), the
  write-hole belt (`UncleanResync`: an unclean composer death comes back solo,
  peer-stale, rebuild-only — stock raid1 reassembles equal-seq divergent legs as
  clean equals), the auto-examine self-rejoin refuted with teeth
  (`RecordRejoinOnly`), and the full fail-back round trip probed at MaxEpoch=3;
  `Inv_NoSplitRead` is the new theorem. **Tranche 3 (liveness) SHIPPED same day**,
  and its two required-to-fail runs are build requirements stated as lassos: the
  shipped world's missing redirect actor parks a client at the dead traddr forever
  (`ClientEventuallyRedirected` is the actor's acceptance test), and the shipped
  constructor-traddr preempt shape livelocks every post-failover fence confirmation
  (the target-registry requirement with teeth). The tranche's own finding — both
  halves forced by counterexamples — is a hard implementation commitment: **the
  serving lease names (volume, epoch, composer), not a node.** Renewal is
  record-conditioned (the MDS refuses a deposed node's renewal even when healthy;
  its lapsed horizon must STAY passed), and **assembly is the lease grant** —
  activate-the-composition and grant-the-epoch's-lease are one act, else a composer
  serves leaseless on an earlier epoch's lapse and its eventual deposition reads
  that ancient lapse as an already-passed dead-man horizon, assembling over a
  still-serving zombie. A third run prices `ElectInSync` honestly: a degraded
  volume whose composer then partitions is DOWN until that composer recovers —
  availability spent on durability, with the operator override (LastResortServe
  analog) undesigned and owed alongside the `release_quarantine` surface;
  (4) **PR state never travels** — reservations live at lib/nvmf per node, PTPL to a
  local file; an empty PTPL dir on the survivor loads zero state *silently*
  (`subsystem.c:3154-3158`);
  (5) **the superblock costs ≥1 MiB of data_offset** — in-place conversion of an
  existing volume would shift bytes under the pinned NGUID; conversion is a data
  migration, never a wrap.
  **The missing piece is one mechanism, and this doc pre-committed to it** (§9's
  abstraction note: "if two tgt incarnations can ever expose one extent range, the
  serving-target state is a SET from day one"): a durable serving-target record
  `[epoch, composer]` in MDS sqlite, advanced by one CAS on an unreachability verdict
  that deliberately cannot tell dead from partitioned, enforced at the survivor's
  leg-export (evict the old composer's inter-tgt hostnqn), at the composer itself (a
  serving lease with a self-suspending dead-man — the only exclusion the LOCAL leg
  has), and at the export mouth. **`formal/FlintComposition.tla` now carries that
  machine** (13 gate runs: strict + a redundancy A/B green, 7 single-flag mutations,
  3 non-vacuity probes), modeled before any code exists, FlintExtents-style — with
  the pNFS-specific victim class FlintReplication lacks: clients holding direct
  nvme-tcp sessions to a partitioned composer. **TLC corrected the review's design
  four times before implementation:**
  (a) *eviction must not precede the lease horizon* — severing a still-acking
  zombie's fan-in strands its clients' acked writes on the doomed leg; the failover
  order is CAS → horizon → evict → assemble → replay → redirect;
  (b) *the arbiter needs FlintReplication's election machinery, not just fencing* — a
  degraded-window failover (elect the leg that missed acked solo writes) discards
  data with every fence belt green; promotion requires `ElectInSync` (the record
  refuses a stale leg) and `DegradeBarrier` (the raid acks a solo-landing write only
  after the record carries the peer's stale mark — stock raid1 does the opposite, so
  flint interposes on leg failure; mark-then-degrade, the RecordBarrier transfer);
  (c) *an epoch-valid fence confirmation is not yet a free license* — until assembly,
  the deposed composer's fan-in still reaches the surviving leg **under the
  composer's own inter-tgt hostnqn**, indistinguishable to any per-client preempt;
  the free waits for the confirming epoch's composition to be ACTIVE;
  (d) *the review's "key `delivered_unix` by epoch" is the wrong enforcement point,
  refuted as redundant* — its scenario (fence confirmed at A, range legally freed at
  epoch 1, victim re-attaches to the survivor unfenced because PTPL never travels)
  cannot be closed by keying a free that was legal when it happened; the belt is the
  **export mouth**: the new composer's export opens only after standing fences are
  converged into its allow-list (admissions minus fenced, computed MDS-side,
  fail-closed — converge failure means no listener). With that replay in place, the
  epoch-keyed schema change is machine-checked redundant
  (`FlintCompositionEpochKeyedToo.cfg` explores the identical distinct-state graph — 128,939 after tranche 3's lease correction, 102,962 when first proven),
  so `fenced_clients` keeps its schema.
  **Priced residuals, named:** the dead-man rests on a bounded-skew axiom
  (`DeadmanCertain`; the Skew run prices its absence at a window of stale READS —
  writes stay contained by eviction + the barrier); electing only in-sync legs means
  a degraded volume whose good leg's node dies WAITS rather than serves (availability
  spent on durability; the operator override is FlintReplication's LastResortServe
  analog, undesigned); and **do not pre-attach a standby via multipath** — PR
  registration and fence preemption land on one tgt, so a fenced client would fail
  over to the standby unfenced, breaking the property multi-rig M4c proved.
  **Still to build or design** (beyond the epoch machine itself): the redirect actor
  (csi-node re-attach lane + "session up" ack + per-client notify re-fire — the
  session record today replays the recorded traddr with a deliberate "No MDS call",
  and the only production CB_NOTIFY_DEVICEID sender is expand), a dead-vs-partitioned
  detection verdict (the reconcile pass only logs per-volume failures), un-pinning
  the MDS from the tgt node it must survive (chart `nodeSelector` + static traddr —
  that traddr is now the SEED its target self-registers with, not what any dial site
  reads; see the registry note below),
  `grow()` for composed volumes (the read-back belt validates ONE LVOL, not the
  array — a one-leg ENOSPC mid-sequence could raise the ceiling past the raid), the
  write-hole divergence belt of limit (1) in code (the model's `UncleanResync` names
  its shape: forced rebuild of one leg on any unclean assembly), and the long-owed
  operator surface for
  `release_quarantine`/`quarantine_stats`, which failover work makes load-bearing.
  Phases 1-3 continue to ship single-replica with `reclaimPolicy` and workload
  guidance saying so.
  **THE TARGET REGISTRY IS SHIPPED (schema 11, 2026-08-12)** — the first piece of
  the epoch machine in code, and the one `StaticTraddr` demanded. Two tables:
  `block_targets` (a target self-registers its dial coordinates every reconcile
  pass, so a listener change converges with no operator and a target returning on a
  new address updates its own row) and `block_volume_target`, the volume's SEAT —
  `[epoch, composer]`, written once at provision, `INSERT`-if-absent so seating can
  never be an adoption. They are two tables on purpose: coordinates change without
  identity changing, while the composer changes only by promotion, which bumps the
  epoch — conflated, a re-addressed node would be indistinguishable from a failover.
  Every site that dials or advertises a volume's target now resolves through the
  record — the fence preempt, the fence release, and the `AttachBlockNode` answer —
  and there is NO fallback: an unseated volume and a seat naming an unregistered
  composer are two different refusals, neither of which reaches for the configured
  address, because the moment one does, StaticTraddr's lasso is back. The unit test
  `the_fence_dials_the_record_not_the_constructor` is that run's acceptance test
  (constructor address deliberately dead, registry naming the live one; reverting
  the fence to `self.traddr` fails it). Converge picked up `RecordAssemblyOnly`'s
  door for free while the seat was being read: a reconciler refuses a volume whose
  seat names another composer, which is precisely `FlintCompositionAssembly.cfg`'s
  healed composer re-converging the same subnqn and NGUID over its stale leg — it
  cannot fire today (every volume is seated where it was provisioned), which is the
  argument for having it in place BEFORE promotion can move a seat.
  **THE CAS AND THE UNREACHABILITY VERDICT FOLLOWED (schema 12, same day).** The
  verdict is a per-TARGET probe (`spdk_get_version` — reachability is all it asks;
  a tgt that answers but has lost its bdevs is REACHABLE and broken, which is
  `reconcile_all`'s problem) whose strikes must clear BOTH a count and a wall-clock
  floor. Both, because a count alone is a statement about loop cadence rather than
  about the target — F60's lesson that a pass's real period is the whole loop's
  duration, not its `interval` — and a window alone condemns on one blip. Each
  condition is A/B'd: deleting the floor fails
  `the_verdict_needs_both_the_strikes_and_the_window`. **What the verdict cannot
  say is the point**: `Unreachable` names reach, never liveness, and the type says
  so, because the whole composition machine exists to be correct when that verdict
  is WRONG about death. The other half of the exclusion — the composer's own
  dead-man, the only thing that can reach a partitioned composer's LOCAL leg — is
  still owed and is named at the type so the asymmetry is not mistaken for
  completeness.
  The CAS is `PromoteCAS` verbatim: compare `(epoch, composer)`, refuse if the seat
  moved (`PromotionRaced`), refuse the sitting composer, refuse an unregistered
  candidate (an elected composer nobody can dial is a promotion into a black hole),
  refuse a candidate whose leg is not in sync — `ElectInSync`, whose A/B is that
  deleting it makes the guard test fail — then advance the epoch by exactly one. It
  deliberately does NOT mark the deposed leg stale: that is assembly's, and between
  the CAS and assembly the deposed composer may still be acking. A third table
  `block_volume_legs` supplies the gate's input; today a volume has one leg, its
  composer's, marked in sync at seating, so **promotion refuses every time — which
  is the correct answer for a single-copy volume, not a gap**, and it is
  `WaitsPrice`'s bill arriving in the log rather than in this document. Seating
  marks that leg insert-if-absent, never upsert: re-marking on a converge would let
  an ordinary pass clear a stale mark with no copy behind it, which is
  `FlintCompositionSelfRejoin.cfg` exactly, and the A/B for that is a third
  mutation. A candidate must also be a target this MDS has AFFIRMATIVELY heard
  from — never-observed is not "fine".
  **THE REMOTE PROBER FOLLOWED (same day), and it made the verdict uniform.** The
  probe is `resv_fence::probe_nvme_tcp` — TCP connect plus the NVMe/TCP
  initialize-connection exchange, then hang up — run against every REGISTERED
  target at the coordinates the registry holds, concurrently (a partitioned target
  costs a full timeout, and probing serially would make the pass's duration
  proportional to how many targets are down, which is backwards). It deliberately
  stops before the fabrics Connect: naming a subsystem would make it a question
  about one volume's EXPORT, and the target this most needs to ask about is a
  promotion candidate, which by definition does not export the volume yet.
  ICReq/ICResp is the strongest subsystem-agnostic statement available — not "a
  port is open" but "an NVMe/TCP target is speaking the protocol here".
  It probes this MDS's OWN target the same way, which is the point: "reachable"
  must mean one thing, and what a verdict licenses is a decision about who SERVES.
  A control-plane probe over the local RPC socket answers whether a tgt can still
  be ADMINISTERED, and a target whose process is fine while its nvmf listener is
  wedged passes that while serving nobody. The RPC socket keeps one job, as a
  diagnostic: our own listener silent while our own process answers means the
  configured `traddr` — the address every csi-node dials — is broken, which is a
  configuration fault worth naming rather than a mysterious verdict. That claim is
  a pinned predicate (`listener_is_misconfigured`), never asserted about a remote
  target, because there is no second opinion to be had about someone else's
  process. **Note what the two socket outcomes ARE:** a refusal (process gone) and
  a timeout (packets going nowhere) are precisely "dead" and "partitioned", and the
  verdict folds them together on purpose — distinguishing them is the thing the
  composition machine assumes nobody can do. The timeout is mandatory and bounded
  (`FLINT_PNFS_BLOCK_PROBE_TIMEOUT_SECS`, default 5): a partitioned node
  black-holes, and the pass must not wait out the kernel's SYN retries behind the
  very node that stopped answering.
  **It also discharges the obligation the CAS commit wrote down**: the reconcile
  pass now partitions volumes by the composer their RECORD names, so one target's
  outage is no longer another volume's outage — the volumes seated here converge
  and re-fence while the ones seated at a condemned peer go to the promotion path.
  The A/B is `a_condemned_peer_does_not_strand_the_volumes_seated_here`, which
  returns (0, 0) under the old whole-pass skip.
  **THE LEASE AND THE DEAD-MAN FOLLOWED (schema 13, same day)** — tranche 3's
  finding, in code, both halves. `block_leases(volume, epoch, holder, expires_unix)`
  is its OWN table and the reason is the finding: the CAS moves the seat while the
  lease stays with the OLD epoch, expiring, **and that gap IS the eviction
  horizon** — one row would collapse it. Renewal is record-conditioned, so a
  deposed composer is refused however healthy it is (let it re-arm and the horizon
  never comes: promotion wedges with every process alive), and an ELECTED composer
  is refused too, because assembly grants a lease and a holder never takes one (let
  it self-grant and it serves on an earlier epoch's lapse, which the promoter then
  reads as an already-passed horizon and assembles over a still-serving zombie).
  Each half is A/B'd by deleting exactly its guard. Seating grants the epoch-1
  lease in the same transaction — the first composition is an assembly.
  **The dead-man** (`DeadmanGate`) is the only exclusion a composer's LOCAL leg
  has: eviction at a survivor's leg-export cannot reach a partitioned composer
  serving its own disk to its own clients, so it must stop itself. Each pass it
  renews every lease this target holds; a renewal the record REFUSES, on a lease
  that has ALREADY EXPIRED, suspends the export — converging the allow-list down to
  the fence lane, which tears every client's controller down at the device. Both
  conditions are load-bearing and are tested separately: suspending on refusal
  alone severs a still-entitled composition mid-horizon (the acked-writes-stranded
  shape the CAS→horizon→evict order exists to prevent), and suspending on expiry
  alone would take down a healthy volume whenever a loop ran late — since a renewal
  that SUCCEEDS is what repairs the expiry, a stalled loop simply does not run, and
  suspension can only ever fire where the record has stopped vouching.
  Suspension is expressed as DESIRED STATE (a converge mode), not as a one-off
  teardown, because an imperative one would be undone by the next converge; the
  admissions in sqlite are deliberately kept, since the clients are still
  legitimately admitted and it is this TARGET that lost the right to serve them.
  What it cannot promise is timeliness, which is `DeadmanCertain` priced: a late
  loop leaves a window of stale READS, writes staying contained by eviction and the
  degrade barrier.
  The pass now also skips volumes seated at a target that is neither this one nor
  condemned — not ours to converge — and the fence path's converge-local/dial-remote
  asymmetry is noted at the site: a deposed target's converge is refused there
  (its lease will not renew) rather than fencing the wrong target.
  **EVICTION AND ASSEMBLY FOLLOWED (same day), and the failover order is now whole
  in code**: CAS → horizon → evict → assemble → replay. Assembly refuses while the
  deposed composer's lease still runs (`AwaitingHorizon`), because that composer may
  still be acking its clients' writes and taking its fan-in away is what strands
  them on a doomed leg; then it evicts the deposed at this target's leg-export,
  marks the deposed leg STALE so the election gate cannot hand the composition
  straight back to it (`RecordRejoinOnly` — only a completed rebuild clears it),
  grants the epoch's lease, and converges the export. **Assembly IS the lease
  grant**, which is finding (b) in its enforcement position rather than its
  statement position.
  Two details are worth keeping. **The standing lease is what names the DEPOSED
  target** — there is no separate "who was serving" record, because holder-plus-epoch
  already is one. And the one place code cannot be as atomic as the model is the
  lease grant versus the export build: they are ordered grant-then-build, so a crash
  between them leaves a lease with no export (harmless; the next converge builds it)
  rather than an export SERVING with no lease, which the dead-man's work list would
  never look at again and converge would refuse forever. Exercise must not outlive
  entitlement.
  The **fence replay** rides the ordinary converge path rather than a second
  implementation of it, and its fail-closed property is structural rather than
  hoped-for: `ensure_export` creates the subsystem `allow_any_host: false`,
  converges the host list, and only then adds the namespace and listener — verified
  by reading it, so there is no instant at which the volume is reachable by a client
  the MDS-side computation excluded. The round-trip test asserts the survivor
  REFUSES a fenced node's re-attach, not merely that its NQN is absent from the
  allow-list: the weaker check passes vacuously (the fence deleted the row), while
  the refusal is the belt that actually holds when PTPL never travelled.
  **Eviction is, today, a verification with teeth.** A target's allow-list is
  derived level-triggered from the admission tables and no inter-target host NQN is
  ever in them, so a deposed peer is already excluded by construction and the call
  normally finds nothing to remove. The day replication puts a peer's NQN on a leg's
  allow-list so it can mirror, that changes — and this is the step that removes it,
  in the right place in the order.
  **THE REDIRECT ACTOR FOLLOWED (same day)** — all three pieces the review named.
  (1) **The re-attach lane**: `reestablish_sessions` no longer replays the recorded
  traddr blind; it asks the MDS where the volume lives NOW via `AttachBlockNode`,
  which is deliberately not a new RPC — it is idempotent, it resolves through the
  serving-target record, and it refuses a node fenced meanwhile, which is exactly
  what a re-attach needs. The call is BEST-EFFORT and that is load-bearing: the old
  "No MDS call" behaviour is what makes this pass work through an MDS outage, so an
  unreachable MDS falls back to the record exactly as before and only a successful
  answer overrides it (A/B'd — making the failure fatal fails the test). A moved
  address is persisted BEFORE connecting, so a reboot mid-repair starts from the
  current target rather than walking back to the dead one.
  (2) **The "session up" ack**, a new RPC whose entire content is ORDER: the MDS
  cannot observe a node's reconnect, and a notification sent before the replacement
  device exists is accepted and useless — measured in the unfence drill, not
  reasoned. So the node says when its device exists. The MDS checks the acking node
  actually holds an admission on that volume before fanning callbacks out (A/B'd);
  an unadmitted or detached node is refused by name.
  (3) **The per-client notify re-fire** is the ack's effect: `notify_device_changed`
  over every client that cached the device.
  The MDS control endpoint reaches the node in the publish context
  (`pnfs.flint.io/mds-control`) and is persisted in the session record, stamped by
  the controller with the endpoint it JUST USED — which is by construction the shard
  that owns that volume, so sharding needs no node-side re-derivation. Records
  written before this parse fine and keep the old behaviour, which is precisely the
  world `FlintCompositionNoActor.cfg` parks a client in; the volume and node names
  are derived by inverting `block_volume_export_nqn` and `node_host_nqn`, so no
  record field was needed and the actor works on any record that carries an
  endpoint. **`ClientEventuallyRedirected` now has its subject.**
  **NOT PROVEN, and it needs a rig**: that a live MOUNT recovers end to end across a
  redirect. The unfence drill measured that after a fence + re-stage the mount kept
  issuing I/O to the OLD controller path and every write failed, and that a
  notification landing before the replacement device existed did nothing. This
  ordering — device first, then notify — is the fix that measurement points at, but
  it is a hypothesis until a failover drill runs on hardware.
  **THE COMPOSITION SUBSTRATE SHIPPED (same day) — and two of the review's five
  raid1 limits DISSOLVE under a choice the file tier already made.** flint builds
  raid1 with `superblock: false`, and that single flag answers limits (2) and (5).
  (5) said the superblock's ≥1 MiB `data_offset` makes composing an existing volume
  a data migration — with no superblock each base carries the volume's bytes at LBA
  0, identical to the bare lvol, so a solo volume composes IN PLACE and a
  composition falls back to solo without moving a byte. (2) said a survivor cannot
  self-promote because the superblock lists the peer CONFIGURED — with no superblock
  there is no examine-based auto-assembly at all, so a composition exists exactly
  when flint builds one from the record. That is `RecordAssemblyOnly` satisfied by
  construction rather than defended, and `RecordRejoinOnly`'s "never let
  auto-examine arbitrate" satisfied because auto-examine does not run. The file tier
  reached `superblock: false` for its own reasons (snapshots and clones of
  superblocked bases were unmountable raw, 2026-06-12); this tier depends on it
  harder and for different reasons, so the flag is now load-bearing in two places
  and must not be flipped without reading both.
  What ships: **leg exports** (`…:leg:<vol>`, a separate NQN from the client-facing
  `…:block:<vol>` because a client admitted to read a volume has no business
  reaching a leg, and a composer mirroring onto a leg is not a client), whose
  allow-list is derived from the seat as **exactly the current composer** — so
  `EvictAtLeg` finally has a subject, and a deposed peer loses its reach because the
  record stopped naming it rather than because someone remembered to revoke it. A
  target exports its leg only while it is NOT the composer: the composer's copy is
  claimed by the raid module, and an export of the same lvol fails that claim with
  EPERM (the collision the file tier's `drop_stale_local_exports` exists for). And
  **the composition itself**: the client-facing namespace serves the RAID when the
  record gives the volume more than one IN-SYNC leg and the bare lvol when it is
  solo, with peer legs attached at their REGISTRY addresses under this target's
  inter-target host NQN and F42's `LegTransportPolicy` inherited so a leg on a dead
  node faults out instead of stalling every write.
  A bug worth recording, caught by its own test: the export spec's bdev ALIASES must
  belong to the bdev being served. `ns_matches` accepts a namespace pointing at any
  alias of the spec's bdev, so handing it the LVOL's aliases while asking for the
  RAID makes a stale lvol-backed namespace look correct — the volume would keep
  serving one leg while the record claimed two, every write landing on a single
  copy. Silent divergence, from three characters.
  **THE DEGRADE BARRIER SHIPPED (same day), and the open design question had an
  answer that needed no quiesce RPC at all.** flint is not on the data path and
  cannot gate an ack — so it gates the ABILITY to ack. A composed leg attaches with
  `fast_io_fail_timeout_sec` UNSET, so a missing peer's I/O QUEUES rather than
  failing, and raid1 (which acks when all legs have responded and any one succeeded)
  cannot complete a write only one leg took. Writes stall. Then, in this order:
  the peer's stale mark lands DURABLY, and only then is the leg removed from the
  raid, which drains the queue and serves degraded. After the mark, no ack can be a
  lie — any write the raid completes is one the record already knows the peer
  missed. Both halves are A/B'd: reverse the order and the record no longer knows;
  restore the transport bound and the ack window re-opens.
  **This is not F42 returning**, and the distinction is the design. F42's stall was
  UNBOUNDED because nothing ever removed the leg — the raid reported online 2/2
  while consumers sat in D-state. Here the bound is flint's own mark-then-degrade
  loop rather than a transport timeout: the unreachability verdict fires, the mark
  lands, the leg goes, I/O drains. A merely SUSPECT peer keeps its place — the
  transport is queueing, so nothing can be acked behind its back, and degrading on a
  blip spends a rebuild on a target that never left. If flint dies mid-window,
  writes stall until it returns: an availability event, not a correctness one, and
  the honest cost of not being on the data path. `FLINT_PNFS_BLOCK_LEG_FAST_IO_FAIL_SECS`
  puts the bound back and says in its own warning that it switches the barrier off.
  **THE REBUILD SHIPPED (same wave) — and the copy engine was already in the tree,
  twice over.** The design said `SEEK_DATA`/`SEEK_HOLE` against the source lvol; what
  shipped is `bdev_lvol_start_shallow_copy`, which is strictly better and needs no
  round trips: it walks the source blob's own cluster map and skips every cluster the
  blob does not own (`blobstore.c`, `bs_shallow_copy_cluster_find_next`), writing the
  rest at identical offsets on the destination. Sparse by construction, entirely
  inside the target, and the same primitive the FILE tier's catch-up has used since
  Tier-2. Its precondition — the source must be READ ONLY, or -EPERM — turns out to
  be the mechanism rather than an obstacle: each round snapshots the live head, and
  the snapshot's OWN clusters are exactly the bytes written since the previous round,
  so **the blobstore's copy-on-write IS the dirty-region tracking** and flint keeps
  none of its own. round 1 carries the volume; round n carries what the writer
  dirtied while round n-1 ran; the window carries the last delta. Bounded at
  `FLINT_PNFS_BLOCK_REBUILD_MAX_ROUNDS` because the ladder converges only when the
  copy outruns the writer.
  **The design note about "copy while the target is not a member, then re-create the
  composition with both legs" was WRONG in its second half, and the correction is the
  most load-bearing thing here.** A raid bdev's SLOT COUNT is fixed at creation —
  `bdev_raid_create` refuses an empty base name, and `raid_bdev_add_base_bdev` only
  ever fills a slot some removal emptied — so re-creating the composition to admit a
  leg re-points the namespace under a live client. Instead **the frame is a function
  of the RECORD's leg count**: a volume the record gives two legs is served through a
  two-slot raid whether the peer is present, stale or missing, each absent leg's slot
  made by a null-bdev stand-in that is removed the instant the frame exists (and if a
  stand-in cannot be removed, the whole frame is deleted rather than left serving a
  leg of zeros). A leg then leaves by emptying its slot and rejoins into the same
  slot, and the client sees neither. That also means the composition SURVIVES a
  degrade — it no longer falls back to the bare lvol, because falling back is what
  would cost a re-frame later.
  The window is flint's **carried SPDK patch, whose contract the target enforces**:
  `bdev_raid_add_base_bdev --skip-rebuild` is refused unless a `bdev_raid_quiesce`
  lease is HELD, because the cut that produced the base and the add that admits it
  must sit inside ONE quiesce or the writes between them exist nowhere on the new
  leg. The lease auto-expires, so an orchestrator that dies mid-window cannot leave
  guest I/O gated. flint adds the check the target CANNOT make: a lapsed lease
  auto-unquiesces and a later renewal is indistinguishable from a fresh quiesce at
  the RPC, so the window compares its OWN clock against the lease before admitting
  and abandons rather than admit a leg whose cut may predate a lapse.
  **The order is the degrade barrier's, mirrored**: there the record went stale
  BEFORE the composition degraded; here the leg becomes a member BEFORE the record
  calls it in sync. One rule stated twice — THE RECORD'S OPTIMISM TRAILS REALITY. A
  mark placed in anticipation would leave an electable leg missing the final delta,
  and `ElectInSync` would hand it the composition in good faith; A/B'd by failing the
  admission and asserting the leg is still stale.
  **`UncleanResync` shipped with it, as the frame's own rule**: a composition built
  from nothing DEMOTES every peer it cannot prove, because raid1 acks on any-one-leg
  and records the failure asynchronously, so "the record said in sync" is not
  evidence that two legs hold the same bytes and there is no scrub to ask. The price
  is a copy after an unclean restart; the alternative is reads flapping between
  divergent legs on LAYOUTCOMMIT-confirmed data. A cheaper answer exists — a clean
  marker written when a composition is torn down deliberately, which is what raid1's
  superblock would have carried — but it must be written on a path a crash cannot
  take, and that proof is not free, so it is owed rather than assumed.
  Note WHICH of the model's two rejoin doors this is: always `RebuildComplete`, the
  full copy of the source's allocated set, and never `DeltaRejoin` — so
  `AncestryGuard`'s proof is not needed, because flint does not take the door it
  guards. What it needs instead is the ANCESTOR guard: a shallow copy carries only
  the head's own clusters, so a head that still has a parent snapshot would produce a
  leg reading zeros wherever the ancestor holds data. That is refused, loudly, rather
  than walked (the file tier's `copy_chain_to` is the extension).
  No new schema: the rebuild's durable state is the leg's sync mark, and every
  intermediate — cuts named by volume and round, an attached destination, a quiesce
  lease — is reconstructible or self-expiring on the ONE target that owns them.
  What remains: same-AZ leg PLACEMENT, expand-under-composition (`grow`'s read-back
  belt still validates one lvol, not the frame), and the StorageClass surface.
  **There is still no `replicas: 2` parameter** — the mechanism is complete but
  nothing has yet run it on hardware, and the surface should not ship ahead of a rig.
- **PLACEMENT SHIPPED (same wave) — and it had to be the CONTROLLER'S decision,
  because MDS shards share nothing.** The chart is explicit about it
  (`pnfs-mds.yaml`: "Shards share nothing with each other" — N independent
  Deployments, each with its own sqlite on its own RWO PVC, the attach being the
  single-writer fence). So a target's `block_targets` registry can only ever hold
  rows it wrote itself, which means **a target cannot discover a peer**: the whole
  composition machine was structurally single-target until something told one target
  about another. The CSI controller is the only component that sees the fleet — it
  already enumerates every shard (`FLINT_PNFS_MDS_SHARD_ENDPOINTS`) and already asks
  each for its posture (`BlockExportStatus`) — so placement is decided there, once,
  at CreateVolume, and travels to both sides as a fact.
  **Same-zone by default, and that default is a COST decision before it is a
  durability one.** A cross-zone leg pays inter-zone egress on every mirrored write
  for the life of the volume, and again on the whole allocated set every time the leg
  is rebuilt — which on this tier happens after any unclean restart of the composer.
  So `choose_leg_host` keeps only candidates in the composer's zone unless
  `pnfs.flint.io/replicaCrossZone` says otherwise, and REFUSES rather than quietly
  spending the money; the refusal names the zones that do have targets and the
  parameter that would allow them. An unlabelled node is UNKNOWN, never "the same
  place as every other unlabelled node" — two empty strings are not a shared failure
  domain, and that refusal is its own A/B. The pick is deterministic (FNV-1a of the
  volume name over the sorted eligible set) because the provisioner retries
  CreateVolume by name: a placement that varied between attempts would host a leg on
  a different peer each time and strand every loser as an orphan lvol.
  **Zones come from the Node objects, read by the CONTROLLER, never by the MDS** —
  the block record must not depend on the API server, since fencing and failover run
  exactly when it is down. Topology is read once, at the only moment a placement
  decision exists.
  Two RPCs carry it: `HostBlockLeg` to the peer (seat the volume at its composer,
  mint an EMPTY thin lvol, converge the leg export that offers it to that composer
  alone) and three new `CreateVolume` fields to the composer (registry row + leg row,
  **STALE** — the peer's copy holds none of the volume's bytes, and a leg that
  arrived claiming to be in sync would be electable, with `ElectInSync` handing the
  volume to a copy of nothing). Everything after that follows from the record: the
  next converge frames two slots, the rebuild fills one. **Ordering: host the leg
  BEFORE creating the volume.** The reverse returns a healthy-looking volume whose
  second copy has nowhere to live, and nothing afterwards would ever go back and make
  one — a silently single-copy replicated volume, which is the exact failure this
  tier exists to refuse. The accepted cost of that order is an orphan lvol on the
  peer if the create is then abandoned entirely; retries are idempotent and
  deterministic, so it converges in every normal case.
  **A gap the wiring exposed, and a test caught: `ensure_leg_export` had never been
  called from anywhere but its own tests.** The pass's subject was `scsi_volumes()` —
  the geometry cache — and geometry is recorded at CreateVolume ON THE COMPOSER, so a
  target that merely hosts a copy has none and dropped out of the pass entirely. It
  also has no *serviceable* volumes, which is a SECOND early return the leg lane has
  to sit in front of. Fixed by making the subject the union of geometry and seats,
  and by putting the leg lane before both returns.
  DeleteVolume closes the loop: the composer reports the leg targets it is about to
  sweep (`DeleteVolumeResponse.leg_targets`) and the controller drops each one. That
  reply is the last thing in the system that knows where the copies are, so a failure
  there FAILS the delete rather than leaking an lvol no record will ever name again.
  **Still not proven on hardware.** The chart ships `replicas` (default 1) and the
  values file says experimental in as many words.
- **EXPAND-UNDER-COMPOSITION SHIPPED (same wave) — the review's finding, and its
  mirror.** The finding: `grow`'s read-back belt validated ONE LVOL, not the array, so
  a one-leg ENOSPC could raise the ceiling past what the composition can serve. raid1
  serves `min(legs)` — `raid1_resize` recomputes exactly that on every base resize
  (raid1.c:587-617) — so the lvol is the one number guaranteed to look right. `grow`
  now reads back the SERVED bdev (the raid when there is one), and when the array is
  short it returns the LEGS rather than an error: only the controller can reach a copy
  on another target, and only if the reply names it. The ceiling stays exactly where
  it was, which turns a one-leg ENOSPC into a refused expand instead of EIO at the
  tail of a volume the PVC claims is bigger.
  The peer's half is the SAME idempotent call that placed the leg: `HostBlockLeg`
  carries the leg's DESIRED size, so hosting one that already exists at a smaller size
  grows it, with its own capacity gate running on the target that would run out of
  room. ControllerExpandVolume therefore does: expand → if legs are short, grow each
  named leg → expand again.
  **The mirror is worse than the finding, and it comes from the file tier's
  `leg_size_guard`:** a leg SHORTER than the array is not merely wasteful, it is
  corruption waiting for a reassembly. Adding an undersized base to a live raid is
  refused (-EINVAL at `raid_bdev_configure_base_bdev`, because `data_size` is already
  set), but a fresh CREATE has NO error path — `raid1_start` assigns `min(legs)` before
  registration, so the bdev layer's shrink guard is structurally unreachable and the
  volume silently SHRINKS under a filesystem that already grew. flint's frame never
  creates over a peer leg (stand-ins are sized from the local lvol, and legs enter only
  through the quiesced add), so the create path is safe by construction — but the
  REBUILD now refuses a destination smaller than the volume before copying a byte,
  rather than spending a full copy to fail at the admission.
  The settle wait is bounded and it waits for the right thing: growth reaches the array
  by AEN (peer lvol → peer namespace → composer's nvme bdev → `raid1_resize`), so
  `grow` polls only while the array disagrees with the legs THIS target can already
  see. If the smallest visible leg is itself short there is nothing in flight to wait
  for, and saying so immediately is both faster and more honest.
  Known gap, named rather than papered over: a leg that missed an expand while it was
  down needs ANOTHER expand to become rejoinable — there is no other lane that grows a
  peer's copy today, so a volume can sit with a stale leg that refuses to rebuild until
  someone grows the PVC again.
- **THE RIG RAN, AND IT FOUND TWO BUGS NO UNIT TEST COULD (`tests/lima/pnfs/
  replica-rig.sh`, green ×3, zero cluster spend).** Two spdk-tgt processes and two MDS
  shards with separate sqlite inside one lima VM — the production shape, because shards
  share nothing and every fact that crosses between them has to cross the wire. Six
  proofs: placement, the two-slot frame, the rebuild, THE MIRROR BYTE FOR BYTE (32 MiB
  sha-identical, read back over the peer's own leg export), the degrade barrier
  (mark-then-degrade in that order, writes continue), and the rejoin (the sparse copy
  carried the 8 clusters written while the leg was away).
  **BUG 1 — the composition could never be built at all.** `bdev_raid_create` returned
  EPERM on the first two-target run: an nvmf namespace CLAIMS its bdev
  (`spdk_bdev_module_claim_bdev`, subsystem.c:2592), and CreateVolume builds the export
  before it records the placed leg, so by the first converge the volume's own namespace
  held the lvol and no raid could take it. Every unit test passed because the FakeTgt
  had no claim model. The fix is the file tier's F49 answer from the other side —
  release the namespace's claim, then compose, in the same pass that rebuilds the
  export onto the composition — and the fake now models claims, so the A/B fails eight
  tests instead of none.
  **BUG 2 — the leg export bounced its composer every 5 seconds.** `ensure_leg_export`
  passed NO bdev aliases, and a namespace record carries the CANONICAL bdev name, never
  the `lvs/vol` alias — so `ns_matches` saw a namespace pointing elsewhere on every
  pass and took the remove-and-re-add repair arm, resetting the composer's session
  ~200 ms after each rebuild and dropping the freshly admitted base straight back out
  of the array. The volume export learned this on the rig long ago; the leg export
  shipped with the same three-character gap. Now A/B'd by a test that asserts a second
  converge mutates NOTHING.
  Rig lessons worth keeping: an spdk_tgt renames its process to `reactor_<core>`, so a
  second target (`-m 0x2`) survives `pkill -x spdk_tgt`; and `pkill -f <path>` matches
  the command line of the shell running it, so it kills itself and silently skips every
  cleanup line after it. Reading a peer's copy needs its own host NQN **and its own
  host ID** — the kernel binds one to the other system-wide, and SPDK already holds the
  composer's NQN — which turned the read into a proof that the leg export refuses a
  host its record never named.
- **THE REGISTRATION QUESTION IS SETTLED: the client registers, and the TRIGGER is
  device resolution (measured 2026-08-12, `nvme resv-report`, base rig §9b).** §5's
  preempt-drill correction already retired "the kernel registers NO key" and left
  it at *"only SOMETIMES true"*; this pins down the "sometimes", and it is not
  flaky — it is deterministic. Two samples in ONE run: with the nvme session up and
  the volume mounted but no pNFS I/O yet, `regctl: 0`; after real pNFS block I/O,
  `regctl: 1, rkey: 2` — where 2 is exactly the client id GETDEVICEINFO handed out
  as `sbv_pr_key`. That matches the source: `bl_register_scsi` issues
  `pr_register(bdev, 0, dev->pr_key, true)` (`fs/nfs/blocklayout/dev.c:39`) when a
  deviceid is RESOLVED (`blocklayout.c:592`), guarded by a per-device-object
  `PNFS_BDEV_REGISTERED` bit, and `bl_free_device` unregisters unconditionally. So
  a client registers iff it has a live device object — never at `nvme connect` —
  and the key-distribution channel is now proven at the device rather than
  inferred. Anywhere below that still reads "kernel clients register no key",
  read it as "a client with no resolved device holds no key".
  **Two consequences, both about what we have actually proven:**
  (a) EA-RO is Exclusive Access *Registrants Only*, so it does **not** exclude a
  client that is doing pNFS I/O — for a registrant the fence rests entirely on the
  PREEMPT arm removing the victim's key. **The `FENCE=1` drill now asserts exactly
  that, so the two shapes can no longer blur** (F1b/F3b, 2026-08-12): before the
  lever it requires the victim to BE a registrant whose `rkey` equals the client id
  about to be fenced — the production shape, and a drill whose victim happened to
  be unregistered would otherwise prove the other mechanism while reading the same
  in the log — and after it, requires that key to be gone. Measured en route: the
  post-fence `nvme resv-report` is itself **refused** (rc=1) where the pre-fence one
  succeeded, so a fenced client cannot even read the reservation table; the
  assertion falls back to the MDS's own key list (`keys=[0x666c696e745f6d64(holder)]`
  — the MDS alone) and says which arm answered.
  (b) **"Unfence does not restore the victim's registration" — DRILLED AND
  REFUTED** (`make test-pnfs-unfence-noreboot-rig`, 2026-08-12). The worry was that
  a preempted client keeps its `PNFS_BDEV_REGISTERED` bit and so never
  re-registers. It cannot arise through this path: the fence's host eviction tears
  the client's nvme controller down, so recovery necessarily re-stages onto a NEW
  controller and namespace, which means a fresh `pnfs_block_dev` and a fresh
  registration. Measured: `regctl: 2` afterwards — the MDS's own
  `0x666c696e745f6d64` plus the client — and the client's key is **not the one that
  was fenced**. The fence destroys the client's NFS state too, so it returns under a
  NEW client id (2 → 4) and GETDEVICEINFO hands out that new id as `sbv_pr_key`.
  The drill asserts both halves: the new identity is a registrant, and the fenced
  key never comes back.
  **What that drill found instead, and it is worse: a live mount does NOT survive
  its client being fenced.** After unfence + re-stage the mount keeps issuing I/O
  to the OLD controller path (`dev nvme0c0n1` in dmesg) even though the by-id link
  now points at the new namespace, and every write fails. Recovery needs a remount
  — which is why the standard arm of the drill reboots the node, and why the
  operator runbook's recipe is a reboot. **Sending CB_NOTIFY_DEVICEID from the
  unfence was tried and does not fix it**: the unfence necessarily precedes the
  re-stage (attach is refused while the fence stands), so the notification lands
  before the replacement device exists — accepted 1/1, write still failed. Timing
  it correctly needs a signal the MDS does not have today, "this node's session is
  up", which is a csi-node event after `ensure_session` rather than anything
  visible MDS-side. Left unbuilt deliberately; the negative result is recorded in
  `unfence_block_client` so nobody re-derives it.
- **Fencing end-to-end**: ~~confirmed on each side, untested in combination~~ **PROVEN
  in combination 2026-08-10** (§5 RIG-PROVEN box; `make test-pnfs-fence-rig`): a real
  Linux client mid-write was stopped at the device by the MDS's NVMe reservation, with
  the mechanism turning out to be EA-RO acquisition against a non-registrant kernel, not
  key-preempt. **This proves FenceReaches *for this tgt/kernel pairing on the rig*; it
  does NOT flip the shipped cfg** — and **the flip is RETIRED, not pending
  (2026-08-12)**. It was superseded four days after it was written, by a better route:
  the `FreeRequiresDelivered` graduation. What the flip was FOR — GC reusing
  fenced-holder extents instead of quarantining them — **already ships**, gated
  per-fence on `fenced_clients.delivered_unix` (`extent_alloc.rs`: all fenced holders
  delivered ⇒ clean free; any unconfirmed ⇒ quarantine). The shipped cfg already LISTS
  `Inv_NoStaleExtentRead/Write` with `FenceReaches = FALSE`, and
  `FlintExtentsLostFence.cfg` is a single-flag A/B on `FreeRequiresDelivered`, not on
  `FenceReaches` (both arms hold it FALSE). Setting it TRUE would buy nothing and cost
  something: it asserts *every* fence lands, which is false of the code — the preempt
  arm is best-effort and fails when the tgt is unreachable — so it would trade a claim
  proven in the harsher fences-CAN-fail world for a weaker one proven in an ideal
  world. That is the F65-audit mistake in new clothes. **Do not flip it.**
  **The honest successor SHIPPED (2026-08-12)**: the delivery retry already existed
  (`export_reconcile_pass` re-runs `fence_preempt` and marks `delivered_unix` on
  confirmation); what was missing was anything that revisited a PARKED range
  afterwards, so a range parked by a fence that landed *late* leaked forever.
  `sweep_quarantine_delivered` closes it — it runs at the end of the same reconcile
  pass (after the retry, deliberately: sweeping first re-checks the delivered bits the
  previous pass already found missing and frees nothing) and releases a parked range
  once every client in **that range's own** `fenced_clients` CSV is confirmed excluded.

  **It took five TLC refutations to write, and the argument for skipping the model
  entirely — "the same predicate evaluated later, needing no model change" — was the
  first thing refuted.** The module rendered quarantine as *never freed*, so it had
  never occupied a state where a parked range IS freed. What the counterexamples
  forced, each now a permanent run or a test:
  (1) **Provenance is load-bearing.** A release gated on "all current holders are
  fenced" frees ranges that were never quarantined, skipping the recall — the sweep
  acts on what was PARKED, never on circumstance
  (`the_sweep_checks_the_range_provenance_not_whoever_is_fenced_now`).
  (2) **The delivery retry had to be modelled or the sweep is unreachable**: a client
  is fenced at most once (the waiting set excludes the already-fenced), so an unlanded
  exclusion could never become landed. The *probe* caught that vacuity — a green that
  meant "the sweep never fired". `FenceRetry` is the code's reconcile pass, and
  `FlintExtentsProbeQuarantineRelease.cfg` is the standing non-vacuity licence.
  (3) **Freeing by any other path must un-park**, or the sweep frees the range again
  under its next owner's live grant.
  (4) **Only a real extent can be parked.**
  (5) **The one filed as an open hole in the two-step grant window was the model's
  own abstraction.** Rendering a parked range as still-provisional makes it an ORPHAN —
  allocated, no live holder, which is the module's definition of re-grantable — so TLC
  duly re-granted it and then swept it out from under the new owner. The code never
  had that shape: `reclaim_complete` DELETEs the `extents` row and INSERTs into
  `extent_quarantine`, a third home whose disjointness from the other two
  `verify_volume_invariants` enforces, and `grant` is one immediate transaction that
  allocates from `extent_free`/the watermark and re-grants from `extents`. `alloc` now
  carries a `"quarantined"` state, and `QuarantineIsolated` is the A/B that keeps the
  structure honest — remove the third home and the corruption returns in nine states
  (`FlintExtentsQuarantineVisible.cfg`).
  Three new gate runs (shipped-strict is unchanged and green; blind-release and
  isolation A/Bs; the release probe), five new allocator tests, and one wiring test
  (`the_reconcile_pass_sweeps_quarantine_after_confirming_the_fence`) that pins the
  ORDERING — swap the two loops in `export_reconcile_pass` and it fails with the range
  still parked, verified by running it that way.

  **A rig drill was attempted and withdrawn, and what it taught is worth more than the
  drill would have been.** Staged single-host — writer, tgt killed, fence, `rm` — it
  found `extents=0, grants=0 rows, free_ranges=1`: the reclaim FREED the range. That is
  correct behaviour, and the reason is the return-after-fence upgrade. **A conforming,
  REACHABLE client always returns its layout, and a return is quiescence, so it always
  frees cleanly. Quarantine is only ever reachable for a holder that cannot be reached
  to return** — which is precisely why §10's `REMOVE quarantined 0 range(s)` assertion
  has always passed. Staging a real parked range therefore needs an *unreachable* holder
  plus a reclaim triggered from somewhere else: the second VM issuing the unlink while
  host A is partitioned (the MULTI/SWEEP machinery already exists). **OWED**, and named
  rather than approximated — a single-host drill can only ever prove the clean-free path
  while claiming to prove the other one.
  **Still manual, by design**: `release_quarantine` — the whole-volume lever that frees
  parked ranges *without* the delivered check — remains the only way out for a range
  whose client was UNFENCED (no fence record ⇒ `COALESCE 0` ⇒ undelivered ⇒ the sweep
  correctly refuses it forever). **Gap**: that lever and `quarantine_stats` have no
  gRPC or `StateBackend` surface at all — they are reachable only from unit tests, so
  the "operator lever" this doc describes is not today operable. The rig remains the
  standing regression harness that keeps the fence path
  honest. Open sub-item: the rig proves reach
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
