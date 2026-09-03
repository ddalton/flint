---
title: OCI image serving on flint — registry, snapshotter, and the honest hybrid with S3 lazy pull
status: designed
type: design-impl-spec
tags: [oci, registry, snapshotter, erofs, pnfs, block-layout, nvme, spdk, s3, soci, distribution]
created: 2026-09-01
governs:
  - flint-oci-snapshotter/ (new — lane-3 containerd proxy snapshotter DaemonSet; no libflint)
  - flint-oci-assembler/ (new — assembler pod pool + tar→EROFS converter Deployment)
  - flint-registry-gc/ (new — Distribution offline-GC wrapper consuming the MDS pin set)
  - spdk-csi-driver/src/pnfs_csi.rs (block_layout_capability_refusal ROX admit; read-shared SC key)
  - spdk-csi-driver/src/pnfs/mds/resv_fence.rs (rtype parameter; standing 3h acquire mode)
  - spdk-csi-driver/src/pnfs/mds/operations/mod.rs (image attach/pin rows; RW-LAYOUTGET bounce)
  - spdk-csi-driver/src/state_backend/extent_alloc.rs (reader-fence quarantine rule)
  - spdk-csi-driver/src/nvmeof_export.rs (assembler raw export; reader allow-list converge)
  - spdk-csi-driver/src/pnfs/mds/witness_kube.rs (read-shared class flag in the witness volume document)
  - formal/FlintExtents.tla + cfgs (read-shared constant split, guard + no-guard tranche)
  - flint-csi-driver-chart/ (read-shared StorageClass surface; lane-3 attach knobs)
---

# OCI image serving on flint

Sister docs, cross-referenced by section throughout and never restated: **snapshotter doc** =
`docs/plans/libflint-and-snapshotter-design.md` (lanes §9–10, trust §12, formal §13, phasing
§14–15, incl. the 2026-09-01 amendments: §15 replication-shipped, §11 S3-bracket).
**registry doc** = `docs/oci-registry-pnfs-architecture.md` (registry on the RWX file layout,
§5 decompression honesty, §6 model-weight ladder). **block doc** =
`docs/plans/pnfs-block-layout-design.md` (the substrate; §12 2026-09-01 amendments:
replication shipped, durability-not-failover boundary).

Number labels, used on every figure: **[M rig]** measured on a named run, **[E]** external
published measurement (foreign rig), **[H]** hypothesis — no rig has run, **[I]** inference
from mechanism. A number without a rig is a hypothesis and says so.

## 1. Summary

The problem decomposes into **two independent pieces, and honesty requires keeping them
apart** (parent-session finding; snapshotter doc §11 bullet):

1. **The lazy format.** Pull wall-clock is usually dominated by client-side gunzip
   (registry doc §5). A lazy/streamable format (EROFS here; SOCI/nydus/stargz elsewhere)
   deletes that term **on any backend**. Nothing in the headline "4.1–4.9× cold-start" [E,
   AWS Fargate/SOCI] belongs to the storage substrate.
2. **The backend behind the format.** Where flint competes; the sharpest differentiator is
   the miss path: **a lane-3 demand fault is a kernel block read on an NVMe-oF/TCP
   namespace — no userspace miss daemon, no HTTP, no TLS**. SOCI's miss: ~4.6 ms warm,
   62–290 ms cold, through a daemon [E, arXiv 2607.06868]; flint's: 0.2–0.6 ms p50 [H —
   §9.1, unmeasured]. The other backend claims, scoped honestly: **stored once, served N
   times from one device** (two at `replicas: 2`, balancing unmeasured) — the storm win is
   per-node cache capacity and origin-request elimination, NOT serving bandwidth (§9.2: N
   faults are N device reads; the striped file-layout CAS out-fans any assembled lvol); and
   in-cluster publish without HTTP. Egress/request cost is immaterial at list price, sign
   reversing multi-AZ (§8.4, §8.1) — cost is never the case for flint.

Everything else follows from refusing to conflate the two. The **serving ladder** (§3) starts
at a registry that deploys today with zero code (rung 0, registry doc §4), climbs through a
zero-flint-code lazy pilot off the RWX mount (rung 1 — the burden-of-proof arm), to per-image
block namespaces via a containerd remote snapshotter (rung 2 — the product of this doc), with
libflint lane 2 gated behind tranche A per snapshotter doc §14. The **hybrid posture** (§8)
keeps the external S3-backed registry as origin-of-record always, with automatic degradation
to classic pull; flint is the in-cluster serving tier, justified by latency and storm shape —
never by cost, which S3 wins by two orders of magnitude [I, §8.4]. Every flint-vs-S3 ratio
in this doc is **[H] until the five-arm A/B (§9.4) runs**.

## 2. Decisions of record

| # | Question | Decision | Resolution of brief disagreements (one line each) |
|---|---|---|---|
| D1 | Decoupling from libflint; minimal shippable slice | **YES — the image-serving slice ships with zero libflint** (tranches A–D untouched). Slice = stock Distribution on one RWX file-layout PVC + assembler/converter + snapshotter DaemonSet + MDS pin/attach rows + registry-GC wrapper. Lane-3 consumer nodes speak **no NFS at all**: control gRPC + nvme-tcp only. | Uncontested across briefs; snapshotter doc §14 ordering (driver = tranche A, phase 4) stands. |
| D2 | The ROX enabling change | **One substrate-level read-shared class** (witness-carried flag, `resv_fence` rtype parameter, standing RTYPE 3h reservation, RW-LAYOUTGET bounce, reader-fence quarantine — §4) with **two front doors**: (a) CSI admits `MultiNodeReaderOnly` iff every listed capability is reader-only (generic ROX PVCs: model stores); (b) lane-3 image namespaces **bypass PVC/VolumeAttachment machinery** and drive the same `AttachBlockNode`-shaped witness rows via the snapshotter control lane. | substrate-rox specs the CSI/ControllerPublish path, snapshotter-mechanics says lane 3 bypasses CSI — both stand: same substrate rows, two admission surfaces. |
| D3 | Where the CAS lives + assembly | **Stock CNCF Distribution (`registry:3`, filesystem driver) on one RWX file-layout PVC, today.** The durable contract is the CAS **path layout** (`blobs/sha256/xx/<digest>/data`), not the driver; EROFS is a **derived sibling artifact** beside the blob, never a replacement. Assembly: CSI controller drives (lvol lifecycle + capacity gate), an assembler pod moves bytes (CAS read over kernel RWX mount, nvme-tcp registered writer under 3h), the MDS keeps the `(digest, platform)` ledger and never copies. | Conversion trigger reconciled: cas-ingest's push-notification converter Deployment is the **steady state** (pod start never gates on mkfs); snapshotter-mechanics' first-pull conversion in the assembler is the **day-one path and the permanent fallback** for unconverted layers. Not a fork — one artifact scheme, two triggers. |
| D4 | GC / lifecycle + budgets | **Cross-domain per-digest pins riding a new lane-3 control-lane lease** (snapshotter heartbeat + TTL, NQN-keyed sweep, live-qpair cross-check — §6.1; the shipped NFS lease sweep never covers D1's no-NFS consumers, so the expiry is designed here, not cited); pin lifecycle starts at assembly-row creation; Distribution offline GC wrapped to add pinned digests to the mark set; **assembled lvols are a cache tier** — evictable at attach-refcount 0, re-assembled from CAS, idempotent under Prepare storms. Named budgets in §6.3. | Panel: no attach-row expiry exists in shipped code (`node_attach` discards its timestamp) — mechanism designed in §6.1. Wrapper-vs-upstream-patch is a spike (§12). |
| D5 | S3 / hybrid posture | **Origin-of-record = external S3-backed registry, always.** Every flint lane degrades **automatically** to the ordinary pull path (Spegel-shape fallback, never a runbook step) — silent to pods, **loud to operators** (per-node, per-cause fallback metric — §3). The A/B gains a **fourth arm** — SOCI parallel pull mode ("baseline pull" is no longer the weakest credible baseline [E, AppsFlyer 82% cold-start cut]) — and a **fifth (A5)**: a stock lazy snapshotter against the in-cluster registry-on-flint, the strongest cheap competitor and the clean backend attribution (§9.4). Hub/flint-lite S3 hydration is scoped to **warm re-serving only** (hydrate 72.5 s/GiB [M runbs] loses to a direct S3 pull cold). | s3-arm's fourth arm supersedes the three-arm phrasing; A5 added on panel finding (A3-vs-A4 moves three variables). Pilot sub-arms (§9.3) unchanged. |
| D6 | Durability / availability posture | **Lane-3 assembled namespaces default `replicas: 1`; recovery is digest-verified re-assembly from the CAS.** The CAS itself stays on the file layout at `numReplicas ≥ 2` — data-leg node loss self-heals [M, r3 drill], but the CAS volume's node-pinned MDS shard remains a stall point for mounted clients [M runbr] (§7.1); origin re-fetch keeps pods starting while the CAS is dark. This **amends the snapshotter doc §15 bullet** ("lane-3 on replicas: 2"). The block doc §12 boundary is carried verbatim: replication is durability, **not serving-through-failover**. Revisit when the control-plane-failover tranche lands. | durability-ops-trust's refinement beats the §15 bullet: the mirror buys only bytes (serving stalls identically), costs 2× arena for rebuildable WORM cache bytes, and — decisive — couples running rootfs to API-server availability via the dead-man *serving* suspension. Panel correction: attach-time witness dependence is universal (`node_attach` is unconditional — §6.3); `replicas: 1` avoids only the serving-lease coupling. |
| D7 | Intra-namespace addressing | **dm-linear**, loop-offset as documented fallback. In-line bio remap, no double page cache, no loop-device churn in boot storms; blobs 4 KiB-aligned with offsets in the MDS table. Pins the snapshotter doc §9 "loop-offset/dm-linear" floor item to dm-mod. | [I from kernel architecture — the A/B carries a loop-vs-dm control leg once.] |
| D8 | Weight media types on lane 3 | **Excluded from lane 3 by default** (opt-in per image). A 141 GB model image would cost a 141 GB assembly copy; the model path stays the registry doc §6 L0 `pvc://` ladder (2,857 MiB/s [M runbd]) until lane 2 ships. | snapshotter-mechanics raised it as a product call; decided here — the assembly bill is disqualifying by default, the ladder already serves weights better. |
| D9 | First-pull-in-cluster behavior | **Prepare blocks on assembly with a configurable deadline, then declines so containerd runs the classic pull** (gunzip returns on that path; D1 stands — consumer nodes never mount the CAS, so "lane-1 materialization" was a misnomer); nodes 2..N always hit the assembled fast path (ErrAlreadyExists). The whole Prepare chain (attach RPC, rv-CAS write, add_host) carries the deadline, not assembly alone (§6.3). Conversion never blocks a pod (D3). CRI progress-timeout interaction and the deadline default are open (§12). | Panel: the prior "lane-1 fallback" contradicted D1's no-NFS consumer — resolved in favor of classic pull. |

## 3. Architecture and the serving ladder

One CAS, one substrate, four rungs. Each rung is independently shippable and independently
killable; a rung's gate is named at the rung.

```
              origin registry (S3-backed, external)  ← origin-of-record, re-fetch lane (D5)
                        │  (classic pull fallback, always live)
   ┌────────────────────┴─────────────────────────────────────────┐
   │ flint cluster                                                │
   │  CAS: Distribution on RWX file-layout PVC  (rung 0)          │
   │   ├─ blobs/sha256/xx/<digest>/data          (the contract)   │
   │   └─ …/<digest>/erofs + verification record (derived, D3)    │
   │        │loop-mount off RWX (rung 1: pilot)                   │
   │        ▼                                                     │
   │  assembler pod ──nvme-tcp──▶ per-image lvol on DS (rung 2)   │
   │        ▲ reads CAS via RWX   dm-linear → EROFS → overlay     │
   │  snapshotter DaemonSet ──control gRPC──▶ MDS (ledger, pins)  │
   │  [rung 3: libflint lane 2 — gated, snapshotter doc §14]      │
   └──────────────────────────────────────────────────────────────┘
```

**Rung 0 — registry on the file layout. Deployable today, zero code.** Stock `registry:3` on
one RWX flint-pnfs PVC; spread replicas across nodes (the NFS client is the node kernel, not
the pod — registry doc §1); single-AZ mandatory (registry doc §3). Wins outright now:
air-gapped registries, in-cluster publish (§5.4), the model-weight L0 ladder (registry doc §6).

> **AMENDMENT 2026-09-01 (runby, field) — registry-side floor, previously
> unstated.** "Zero code" holds for *serving image pulls*, and rung 0 was
> exercised end-to-end here (a `crane copy` push and repeated pulls of a
> ~400 MB image against `registry:3` on an RWX flint-pnfs PVC, digests
> verified per rep). It does **not** hold for anything that discovers
> per-layer artifacts through the registry: Distribution 3.1.1 does not
> serve the **OCI referrers API** (`/v2/<name>/referrers/<digest>` → 404,
> over plain HTTP and trusted TLS alike), which is how SOCI/stargz-class
> snapshotters find their index. So a stock rung-0 registry cannot host the
> A3/A5 baseline arms, and any rung that leans on standard remote-snapshotter
> discovery inherits a **referrers-capable registry** as a floor alongside
> the consumer-node floors below. Detail and evidence in the §9.4 amendment.
> Rung 2 is unaffected — flint's snapshotter resolves through containerd
> metadata and its own CAS.
>
> Also field-verified at rung 0, and worth recording because it was a
> shipped defect rather than a design gap: 1.43.0 served **zeros with
> `NFS4_OK`** to this exact workload — bounded LAYOUTGETs advertised stripe
> width 1, so clients read a stripe file that did not exist. Registry
> `startedat` files came back as 20 NULs and every concurrent push failed.
> Fixed in `7fd7917b`, RED/GREEN verified on runby (98 corruption errors →
> 0; 548/548 grants at the pinned width). **Rung 0 on any build before that
> fix is unsafe for the push path.**

**Rung 1 — lazy EROFS off the RWX mount. The pilot; zero flint code.** A consumer node mounts
the CAS PVC (file layout — the ROX refusal is block-class only), loop-mounts the derived
EROFS blob per layer + overlay. Lazy to the page, no HTTP, no daemon; fault chain: page fault
→ EROFS → loop → one NFS READ RTT. This arm carries the burden of proof for the whole block
lane: **rung 2 must beat it by ≥20% or rung 2 dies** (§9.5); gate to build rung 2 is §9.3.
**Zero-code is true of the pilot rig only:** rung 1 as a *product* shares the snapshotter,
converter, and GC-pin bills with rung 2 (§6.1); only assembly, ROX, witness-attach, and
session bills are rung-2-marginal.

**Rung 2 — lane-3 per-image block namespaces. The product of this doc.** A containerd remote
snapshotter (proxy plugin, snapshots.v1 over unix socket, privileged DaemonSet — snapshotter
doc §10) returns ErrAlreadyExists for servable layers and mounts: overlay upper/work on
node-local disk, lowerdirs = per-layer read-only EROFS mounts carved by dm-linear out of the
per-image namespace — one refcounted mount per layer per node, shared by every pod of that
image on the node: **lane 3 is node-scoped, never pod-scoped**. Commit of an active snapshot
is local; Remove = unmount, dm remove, detach-if-last-ref, drop pin — never delete-blob
(snapshotter doc §10). **Restart
discipline:** the snapshotter restarts under live pods routinely (upgrade, OOM); it rebuilds
refcounts/dm/mount state idempotently from containerd metadata + /proc/mounts + dm
enumeration **before its first RPC**, Remove blocking mid-rebuild — else the first Remove
after a restart detaches under a running pod or leaks every mount forever. Drill §11.
containerd version gates verbatim from snapshotter doc §10: 1.7.x/2.0.x classic annotations with
`disable_snapshot_annotations=false`; 2.1+ needs the `exports` pass-through or
`use_local_image_pull`. Precedent: containerd's core EROFS snapshotter (experimental 2.1,
improved 2.2) already performs the erofs+overlay stack [E, containerd docs].

**Rung 3 — lane 2 / libflint. Gated.** Per-pod fd-based blob reads, CAS-on-block, the
registry StorageDriver, hot-blob LRU, zero-HTTP publish — all strictly after libflint tranche
A / block doc §11 phase 4 (snapshotter doc §14). Nothing in rungs 0–2 depends on it (D1); the
epoch-2 CAS flip (§5.1) is per-deployment, never a fork.

**Consumer-node floors (rung 2):** nvme-tcp module + EROFS ≥ 5.4 with the pinned mkfs profile
+ dm-mod + overlayfs. NOT 6.11, no CONFIG_PNFS_BLOCK, and the block doc §4a udev-NGUID
landmine does not apply (the snapshotter resolves by NGUID from Identify, like
`ensure_session`). **Floors are verified per node, never asserted per fleet:** a snapshotter
startup probe (nvme-tcp + dm-mod, test-mount of a pinned-profile canary EROFS) gates the
node's lane-3 registration; a failing node falls back **loudly** — event + condition + a
per-node, per-cause fallback-rate metric (floor probe / attach refused / assembly deadline /
origin re-fetch), the observability surface D5 otherwise lacks: silent fallback turns AMI
drift into permanent full pulls indistinguishable from success. **Multi-arch caveat, new:**
EROFS block size ≤ page size below 6.3 — a
4096-block image on 16K/64K-page arm64 needs ≥6.3 uncompressed / ≥6.8 compressed [E, LWN];
pin uncompressed 4096-block if 64K-page arm64 is in scope. Tooling: erofs-utils ≥ 1.8.9
(1.8.8 `--tar` whiteout fixes; 1.8.9 fragment-corruption fix) [E].

## 4. The ROX enabling change

The whole multi-node refusal is one testable pure function —
`block_layout_capability_refusal` (`spdk-csi-driver/src/pnfs_csi.rs:108-137`), refusing
`MultiNodeReaderOnly` at line 116 — while the substrate underneath is already read-shared
(`grant_read`, `state_backend/extent_alloc.rs:638-697`, is non-exclusive and allocates
nothing). **The refusal, not the substrate, is the gating work** (fact 2). The change list:

1. **CSI policy (front door a, D2).** Admit `MultiNodeReaderOnly` iff every listed capability
   is reader-only; keep refusing any mix with a writer mode; keep refusal #2 (volumeMode:
   Block) for now — relaxing it for assembled namespaces that never speak pNFS is open (§12).
   Call site (`main.rs:1584-1588`) runs before the fleet-wide multi-writer advertisement, so
   only the function changes.
2. **A durable read-shared class flag** — new closed-set `pnfs.chert.us/*` SC key (unknown
   keys hard-fail, `pnfs_csi.rs:70-84`), stamped into volume_context **and the witness volume
   document**, not shard-local geometry. The runbq lesson (`operations/mod.rs:1707-1727`)
   applies verbatim: after promotion the survivor must know the namespace is read-shared to
   re-acquire the right reservation type.
3. **Reservation type.** Today RTYPE is a compile-time constant (`RTYPE_EA_REG_ONLY = 0x4`,
   `resv_fence.rs:120`), acquired only at fence time. Read-shared namespaces get RTYPE **3h**
   (Write Exclusive – Registrants Only — the standing snapshotter doc §12 amendment; 4h
   stands for single-writer volumes): (i) `resv_fence` grows an rtype parameter derived from
   the class flag; (ii) a **standing** 3h reservation held by the MDS fence-lane key **from
   lvol creation, before the assembler's first write** (acquired at publication, the fill
   window would run unreserved), PTPL-persisted (plumbing: `nvmeof_export.rs:105`,
   `block_export.rs:3193-3200`). **Residual, stated plainly: 3h refuses writes from
   unregistered hosts only** — under WE-RO every registrant may write, and Reservation
   Register is ungated, so any allow-listed host can register a key and gain write
   permission [E, NVMe base spec]; the block doc §5 preempt-drill correction proves the
   hazard live (kernel clients do land pr_keys). (iii) Mitigation, in this change list: the
   MDS fence lane monitors Reservation Report and **preempts any registrant key that is not
   the fence-lane key or a live assembler**. The residual also strengthens the §12
   alternative — a read-only SPDK snapshot blob, the only true device-level write refusal.
4. **Fencing readers.** Lane-3 consumers are plain nvme-tcp mounts issuing no PR commands;
   under 3h an unregistered reader cannot be fenced by reservation. The reader fence is the
   functional backstop: allow-list removal + qpair drain (`converge_hosts`,
   `nvmeof_export.rs:578,650`; block doc §5). Consequence: a reader fence has no verified
   preempt, so `delivered_unix` is never set and `reclaim_complete`'s clean-free graduation
   (`extent_alloc.rs:803-819`) must be **unreachable for reader fences — freed extents under
   a fenced reader quarantine**. Costless for immutable image namespaces (free-while-alive
   never happens; delete is whole-lvol teardown after the allow-list converges to empty).
   Caveat (block doc §5 preempt-drill correction): kernel pNFS clients DO sometimes register
   pr_key — but preemption under 3h revokes only the registration (the write permission it
   conferred), **never read access: WE-RO permits reads from all hosts**. So a verified
   preempt of a reader must NOT set `delivered_unix`, and the quarantine rule is
   unconditional with respect to registration state — else clean-free graduates under a
   reader the reservation never excluded, the exact staleRead the §11 no-guard arm catches.
5. **MDS grant policy.** LAYOUTGET with iomode RW on a read-shared volume bounces — code
   guard + conformance must-fail leg (§11), not a TLA arm.
6. **Attach fan-out (front door b for lane 3).** The shipped per-node lifecycle is reused
   unchanged in mechanism (`attach_block_node`, `operations/mod.rs:1690-1845`;
   `witness.node_attach` with the NQN-level fence guard; the composer's level-triggered pass
   opens doors for admissions recorded at other shards); lane 3 drives these rows over the
   snapshotter's control gRPC without PVC/VolumeAttachment objects. **Named bill:** hosts +
   attach rows live in ONE ConfigMap per volume advanced by rv-CAS
   (`witness_kube.rs:23-25,117,733`) — an N-node boot storm on one popular image is N
   serialized CAS writes + N add_host RPCs, all API-server-dependent. Measurement owed
   (§11), batching open (§12).

**Interaction with `replicas: 2`** (the non-default case — D6 defaults lane 3 to 1): a
read-shared namespace can be a two-leg composition; nothing in the composition machine
depends on writer count. On promotion the survivor admits all N readers from witness-carried
admissions (`desired_hosts`, `block_export.rs:692-707`; fence replay fail-closed), but each
reader's controller dies and must **redial** the new listener, learned only via re-attach —
kernel nvme-tcp will not re-resolve; the snapshotter owns redial [I]. cntlid bands are
per-member, width 4096 (`nvmeof_export.rs:132,149-163`): up to 4096 reader controllers per
replicated volume per composer — generous for nodes, unproven for per-pod NQNs (§12).

**Explicitly NOT needed:** extent clone/refcounting (fact 3; block doc §12 stands — assembly
is the shape, §5.3); recall fan-out for readers; a new fencing initiator lane; the
shared-subsystem `no_auto_visible` migration on day one (subsystem-per-volume covers ROX;
migration shape reserved, §6.3); changes to `grant_read` or the node session lifecycle;
per-reader NVMe registration or leases; any serving-through-failover claim.

## 5. CAS, ingest, assembly

### 5.1 The CAS-of-record and the two epochs

**Epoch 1 (now):** stock Distribution, filesystem driver, one RWX file-layout PVC (D3;
registry doc §4). The durable invariant is the **path layout**:
`blobs/sha256/xx/<digest>/data` is simultaneously Distribution's store, the snapshotter's
lookup key, and the future StorageDriver's `Move` target (snapshotter doc §8.1). **Epoch 2
(libflint tranche A lands):** the StorageDriver
writes the identical layout onto a block-class `replicas: 2` volume — the flip that opens
lane 2, escapes the per-node kernel `nfs_client` wall (~5–6 GB/s [M runbh]), and brings the
hot-blob LRU without which a libflint registry loses to a kernel-mount registry on hot
traffic (snapshotter doc §11). Content-addressing makes the migration verifiable by
construction; dual-origin window semantics open (§12).

### 5.2 tar→EROFS conversion

EROFS is a **derived sibling artifact** (`…/<digest>/erofs` + verification record) beside the
blob — never a replacement; the compressed-tar digest must keep verifying against `data`.
Steady state: a registry-side **converter Deployment** (long-lived pool, mounts the same RWX
PVC) subscribed to Distribution's push-notification webhook. Day one and permanent fallback:
conversion at first-pull assembly time in the assembler, per-layer EROFS cached by digest so
shared layers convert once per cluster (D3). A pull hitting an unconverted layer declines
Prepare so containerd runs the classic pull (gunzip returns on that path — D1's no-NFS
consumer stands, per D9) while conversion proceeds async — **pod start never gates on mkfs**.

**diffID round-trip** (the snapshotter doc §15 obligation, transferred here): gunzip, tee the
uncompressed stream into sha256, match against the image config's `rootfs.diff_ids`, then
`mkfs.erofs --tar`. The verification record pins {blob digest, diffID, EROFS sha256,
erofs-utils version, exact flag set}; **the snapshotter refuses any EROFS lacking a verified
record** — the guard against a converter silently emitting garbage. The 5.4-readable mkfs
profile is frozen **as an INCOMPAT-bit whitelist** (effectively lz4 + 0padding only — an
*option* list leaves big-pcluster/ztailpacking formally unexcluded, and mkfs defaults drift
across erofs-utils releases), **asserted** against the emitted superblock feature bits after
every conversion (dump.erofs), quarantine on drift. The whitelist→5.4-mountable link is
validated once on an actual 5.4 kernel (§11) — the rig kernels are modern, so without that
leg the floor is a claim that passes broken; failing it, restate the floor as the oldest
kernel actually exercised.

### 5.3 Assembly

No extent clone exists (block doc §12 refuses refcounted extents): a per-image namespace is
**assembled** — a server-side copy per image per cluster, shared layers duplicated across
images (fact 3). Division of labor (D3): the **CSI controller drives** (create lvol sized
from the manifest under the block doc §12 capacity gate, fill, seal); the **assembler pod
moves bytes** — CAS mounted RWX read-only (kernel read path, 2,857 MiB/s/node [M runbd]),
writing the fresh lvol over a plain nvme-tcp raw export as a *registered* writer under 3h,
hence fenceable; no pNFS client, no libflint. The **MDS keeps the ledger** — assembly rows
keyed `(image digest, platform)` in shard sqlite (`device_notify` schema precedent, block
doc §12) plus the GC pins — and never copies.

States `assembling → sealed → serving`; crashed assemblies re-driven from the row; duplicate
Prepare storms coalesce on the row. Invariants, each with a drill or guard (§11): **seal is
digest-verified per layer by read-back through the export path** (hashing bytes as written
hashes the outbound stream and passes a torn write; a byte-count seal passes a truncated
copy — anti-vacuity); **re-drive fences first** — it preempts the prior assembler's PR
registration and asserts sole-registrant before its first write, since under 3h any
registrant writes and a paused-not-dead assembler (cgroup freeze, healed partition) would
resume scribbling after the re-driven copy verifies; **abandoned rows are reaped** — an
`assembling` row with no waiting Prepare leaks a full-span-charged lvol, so a reaper (row TTL
+ zero waiting Prepares) collects it, provably unable to collect a row a live re-drive holds
(this repo's gated-reaper history demands the drill); **the row dies with its shard**
(node-pinned, §7.1) — re-assembly serialization must live above the shard (controller-side
keyed `(digest, platform)`, or witness-carried; open, §12), else N mid-storm Prepares
stampede N full-image copies. Capacity: the writable lvol is charged its full logical span;
whether seal cuts a read-only snapshot (gets `num_allocated_clusters` accounting per the
block doc §12 2026-08-13 amendment, but touches refused snapshot machinery) or exports
read-only at full-span charge is open (§12). **Dedup honesty, stated plainly: the CAS dedups
(one blob per digest per cluster); lane-3 assemblies re-duplicate shared layers per image.**
The duplication factor is a named, unmeasured bill (§12).

### 5.4 In-cluster publish

Today: the producing job mounts the CAS PVC (publish-scoped subpath) and writes each blob
temp + fsync + atomic RENAME into the CAS path (snapshotter doc §3 table); the manifest
(KBs) goes as one registry PUT so Distribution's link tree stays Distribution's. Blob bytes
never traverse HTTP. Push zstd or uncompressed (registry doc §5) so conversion is cheap. The
MDS HTTP file API is a token-gated, mount-free **fallback** lane, not the publish path.
Endgame (zero HTTP): libflint write + manifest registration — tranche A+ (snapshotter §10).

### 5.5 CAS miss at assembly — the origin re-fetch lane

Origin re-fetch is an **availability** lane, not durability (snapshotter doc §15 amendment).
On a miss the **assembler** (never the consumer node, never the MDS) resolves
`cri.image-ref`, fetches from origin, ingests through the normal temp+rename digest-verified
path, triggers conversion, resumes. If the origin fetch fails, Prepare returns the error —
not ErrAlreadyExists — so containerd falls back to the classic pull and pods keep starting.
Drill guard: assert the CAS row + digest appeared, **not** that the pod started — classic
fallback also starts the pod, so a pod-start oracle passes broken. Credential plumbing open
(§12).

## 6. GC, lifecycle, budgets

### 6.1 Cross-domain pins

Every node attach registers a per-digest pin at the MDS. **The expiry the pins ride must be
designed, not cited** — panel-refuted: no attach-row expiry exists in shipped code
(`node_attach` discards its timestamp; the only sweep, `lease_sweep_pass` at
`operations/mod.rs:2335`, is keyed to NFS client leases that D1's no-NFS consumers never
hold; front door (b) bypasses ControllerUnpublish too). Design: a **control-lane lease on
lane-3 attach rows** — snapshotter-renewed heartbeat with TTL, NQN-keyed sweep at the MDS —
with a second-signal guard: eviction and allow-list convergence **cross-check live
controller qpairs at the tgt** (`nvmf_subsystem_get_qpairs`) before tearing anything, so a
crashed snapshotter on a live node never fences a running rootfs (dead-renewer drill, §11);
expiry also clears the phantom-initiator drainRoll wedge (§7.2). **Pin lifecycle starts at
assembly-row creation, not first attach** — in-flight assemblies and conversions appear in
the pin set, else a retag storm plus a scheduled GC collects blobs mid-assembly, papered
over silently by origin re-fetch (drill variant, §11). Registry-side, Distribution's offline
mark-and-sweep gets a wrapper that adds pinned digests to the mark set (wrapper-vs-patch
spike owed). Retag lifecycle: an old digest survives until its last attachment drops
(snapshotter doc §10). The pin is a **hard prerequisite for any rung serving live rootfs
from CAS blobs (rungs 1–3)** — a rung-1 loop-mount off a collected blob EIOs identically.
Design-only today.

### 6.2 Assembled lvols are a cache tier

Evictable at attach-refcount 0 by LRU/age; re-assembly from CAS on the next Prepare is
idempotent. Eviction policy and re-assembly storm behavior when a target dies holding many
images are undesigned (§12). Delete = whole-lvol teardown after the allow-list converges to
empty.

### 6.3 Named budgets (the bills, in one place)

- **Sessions/qpairs:** lane 3 is node-scoped ⇒ sessions ≈ nodes × distinct-images-per-node — 100 × 40 ≈ **4,000** [I] (vs ~40,000 per-layer, the refused shape — snapshotter doc §10). The block doc §5 shared-session migration (`no_auto_visible` + `nvmf_ns_add_host`) collapses this to nodes × targets (~400), churn at pull/GC rate — held in reserve, triggered by the ceiling rig (§11). The pod-multiplied bill (volumes × pods, per-pod NQNs) belongs to lane 2 only — a real decoupling advantage.
- **Bytes:** the gzip CAS blob AND its EROFS sibling ride the file layout at `numReplicas ≥ 2` (D6), so the honest bill is ≈ 2×gzip + 2×EROFS + 1×assembled lvol ≈ **~5× layer bytes** cluster-side [I] — and the basis matters: EROFS and lvol terms are uncompressed-size (the profile pins uncompressed — §3), so the multiplier against *compressed* layer bytes runs higher. The EROFS cache is droppable for re-conversion CPU; the assembly duplication factor across images sharing layers is unmeasured (§12).
- **Assembly:** one full-image copy per image per cluster (CAS read + lvol write, east-west, single-AZ); one transient writer session per in-flight assembly, bounded by pool size; conversion = one gunzip + mkfs per layer per push, doubled for multi-arch only for platforms actually pulled (first-pull conversion — §3 rung 2).
- **Control plane:** an N-node attach storm = N serialized rv-CAS writes on one ConfigMap + N add_host RPCs (§4 item 6). **Attach-path witness/API dependence is universal for lane 3** — `attach_block_node` writes `witness.node_attach` unconditionally (`operations/mod.rs:1700-1795`); what `replicas: 1` avoids is the dead-man *serving* suspension only (D6). Correlated-failure bill, named: boot storms co-occur with API-server stress (mass rescheduling, AZ recovery), and every new attach needs API writes that classic pull and S3-lazy never issue; running rootfs is unaffected. The Prepare-chain deadline (D9) turns an API outage into prompt classic-pull fallback, never a wedge past CRI timeouts.
- **CAS MDS-shard stall:** the CAS volume's metadata shard is node-pinned; its node's death hangs every mounted registry replica until recovery [M runbr] (§7.1) — origin re-fetch (§5.5) keeps pods starting via classic pull while the CAS is dark.
- **cntlid:** 4096 reader controllers per composer per replicated volume — fine for nodes, unproven for pods (§4).

## 7. Durability, availability, trust

### 7.1 What a composer death means, per rung

The boundary (block doc §12, runbo/runbr [M]): seat promotes (~20 s CAS), surviving leg
in-sync, but the volume's MDS shard is node-pinned and does not fail over — **serving resumes
only after recovery**. For image serving:

- **Rung 0/1 (file layout):** serves through **data-leg** node loss (self-heal
  chaos-validated [M, r3 drill]) and registry-pod node loss — but the CAS volume's own MDS
  shard is node-pinned, and its node's death **hangs already-mounted clients until
  recovery** [M runbr: md5sum timeout on a live mount; the pinned shard cannot reschedule].
  Every registry replica mounts the same CAS PVC, so replica spread does not help; sibling
  redirect (39 s [M runbr]) covers only new attach-class resolution. "A different failure
  domain" holds for data legs only — the same node-pinned-metadata boundary class as the
  block plane. Bound: origin re-fetch (§5.5) keeps pods starting while the CAS is dark.
  Still where the CAS lives (D6) — durability and data-leg self-heal, not shard immunity.
- **Rung 2, already-attached namespaces:** page-cache-warm reads keep flowing [I]; cold
  reads stall — the nvme-tcp session targets the dead composer's traddr, the kernel
  reconnects only to the SAME traddr, and pre-attached multipath standby is forbidden by
  design (fencing breaks — block doc §12). Stall duration is ctrl_loss_tmo — an undesigned
  knob with a 30-minute D-state precedent (chart 1.24 note); stall-vs-EIO for a rootfs is a
  policy choice (§12). **ext4 emergency-ro does not apply**: EROFS is journal-less and
  read-only — the failure mode is hung/EIO reads, never forced remount [I; the measured
  emergency-ro finding was ext4-only]. Live-mount recovery at the same traddr should ride
  kernel reconnect transparently [I, unmeasured]; at a new traddr it cannot — pods restart.
- **Rung 2, new attaches / first pulls:** attach-class resolution survives via a sibling
  shard (39 s [M runbr] — one client, not a fleet), but a dead composer's exports stall until
  recovery. Under D6 (`replicas: 1` + cache posture) the recovery path is **re-assembly on
  a surviving blockExport shard from the CAS** (placement is a controller decision) — so
  composer death for lane 3 is re-assemble-elsewhere, not stall-until-recovery, **provided
  re-assembly serialization survives the shard** (the ledger row dies with it — §5.3; drill
  guard: re-assembly count == 1). The storm also **splits the fleet**: already-attached
  nodes stall at the dead traddr while unattached nodes get the fresh assembly; the
  snapshotter owns migrating stalled nodes to the re-assembled copy (it owns redial — §4),
  mechanics open (§12). This softens the snapshotter doc §15 GA gate for this slice. Origin
  re-fetch (§5.5) bounds the worst case; magnitude unmeasured (§12).
- **One open lever:** the witness carries allow-list identities and a read-only namespace
  needs no extent-allocator rows — a promoted composer *might* converge and serve lane-3
  exports without the dead shard's sqlite. Plausible, unproven — drill 1 (§11) decides.

### 7.2 Witness, zones, rolls

`replicas: 2` dead-man suspension converges the export down to the fence lane when an API
outage outlives the serving lease — tearing reader controllers too; fence enforcement stays
local through the outage. D6 keeps running rootfs off that coupling. Zone trap [M runbo]:
unlabelled nodes refuse every `replicas: 2` provision; labels are read once at controller
start — label, then restart flint-csi-controller. `replicaCrossZone` bills inter-zone egress
on every mirrored write. **drainRoll:** the roller refuses any node whose tgt hosts a
pnfs-block export with live remote initiators (`maint_roll.rs:208,256-270`); fleet image
serving makes that refusal the **steady state** — a popular image has initiators everywhere,
so `helm upgrade` legitimately never rolls registry-serving tgt nodes. Stale attach rows
from dead nodes would keep the refusal alive even after a real image-drain — the §6.1 lease
expiry clears the phantom initiators. GA needs an image-drain procedure (quiesce/re-place
namespaces or an accepted outage window) — undesigned (§12). Also load-bearing and unowned:
`release_quarantine`/`quarantine_stats` have no operator surface (block doc §12).

### 7.3 Trust

Whole-namespace visibility is scoped by assembly: a lane-3 namespace exposes only that
image's layers — bytes the pod would see anyway (snapshotter doc §12). The allow-list admits
**node** NQNs; any pod on an admitted node can read the raw device — same trust class as a
node-local containerd content store. Tenants whose images are secrets from co-tenants need
per-tenant node pools or the file layout (block doc §6). Reservations: 3h read-shared / 4h
single-writer (§4). NQN admission is structurally fail-closed (`allow_any_host: false`, host
list converged before namespace+listener — block doc §12). Production checklist:
controlToken on 50051; NetworkPolicy extended to 4420. Lane-2 token posture unchanged
(snapshotter doc §9). The CSI multi-node refusal stays intact for filesystem block volumes;
ROX rides the read-shared class (§4), not a blanket lift.

## 8. The S3 arm and the hybrid

### 8.1 The strongest honest case for S3-only

The headline mechanism is backend-agnostic (§1). The baseline arm got stronger in 2025–26
**without lazy loading**: SOCI parallel pull mode (parallel ranged GETs + unpack, no index,
in EKS AMIs) measured an 82% cold-start cut [E, AppsFlyer]; node disk, not registry
bandwidth, binds [E, AWS] — and fully-local-then-start adds zero runtime failure modes.
Throttling almost never binds: ≥5,500 GET/s per partitioned prefix (a sha256-keyed CAS
spreads prefixes by construction); ECR API quotas cost ~7 s in a 1,000-node × 20-layer storm
[E]. Bills flint pays that S3 doesn't: replication ceiling 2 + witness + shard config vs
eleven-nines zero-ops; **regional serving** (S3 serves every AZ free; flint's DS striping
and raid1 legs are same-AZ by placement — so an AZ-spread fleet needs a fleet per AZ or
eats $0.02/GB); and the **stall bill** (§7.1) that S3-backed lazy pull never presents —
levied by the node-pinned MDS shard on every rung, the file-layout CAS included.

### 8.2 What S3 structurally cannot do

1. **The demand-fault floor.** Every S3-lazy miss traverses a userspace daemon (nydus
   fscache serves only *hits* in-kernel): ~4.6 ms warm, 62–290 ms cold [E]; lane 3's miss is
   a sub-ms kernel block read [H, §9.1]. Dominates for sparse, fault-latency-sensitive
   shapes: serverless/scale-to-zero, agent sandboxes, p99-sensitive GB-image cold starts.
2. **One stored copy, one fleet's capacity.** N nodes share a single stored copy — no N
   private disk-cache copies (spot churn cannot evaporate them), no per-storm origin
   requests, each miss sub-ms [H] and bill-free. NOT read-amplification relief: no DS-side
   cache exists (§9.1) and nothing coalesces concurrent same-LBA reads, so N faults of one
   block are N device reads — the very concentration that is §9.2's single-lvol fan-out
   ceiling. The same shape is the win (capacity, requests) and the risk (serving).
3. **The miss daemon as runtime dependency.** Daemon crash = EIO on running rootfs until
   restart (time-bounded by background full-fetch — honest). Lane 3 has no daemon but swaps
   in a worse dependency until control-plane failover ships: the MDS-shard stall. **Neither
   side wins this line today; said plainly.**
4. **In-cluster publish** without compress→HTTP→S3→index (§5.4).

### 8.3 P2P, named honestly

Spegel/Dragonfly solve boot-storm origin bandwidth with zero shared storage (55% network-
phase cut [E, Crusoe]); they help only when a peer already holds the layer, don't delete
gunzip alone, and die with spot churn. Right answer for large, stable, storm-prone clusters
with no storage appetite.

### 8.4 Cost (list-price arithmetic — marked, not a rig number)

Lazy-on-S3, 10 GiB image at 8 MiB ranges ≈ $0.0005/node-pull; 1,000-node storm ≈ $0.51.
Storage $24/mo/TiB. The reference flint deployment (6 DS + MDS, registry doc §3) is order
$3–4k/mo on-demand [I]. **Request+storage cost never justifies flint; only latency/storm
shape or an already-amortized fleet does.** Even amortized, the marginal cost is small but
named, never ≈0: capacity ~5× layer bytes + unmeasured duplication (§6.3); **storm-time
device contention** with the very data workloads that justified the fleet (§9.2); the
drainRoll/image-drain ops bill (§7.2). The full bill lands on image serving iff the fleet
doesn't already exist.

### 8.5 Posture and decision table (D5)

Origin-of-record = external S3-backed registry, always; flint is the in-cluster serving tier;
every lane degrades automatically to the ordinary pull, verified by the kill-flint-mid-storm
drill (§11). Escalation arms: (0) S3 + SOCI-parallel or lazy-on-S3; (0.5) pull-through cache
— Distribution proxy mode on rung 0, zero flint code; (1) registry-on-flint file layout,
optionally under a **stock lazy snapshotter** (the A5 shape, §9.4); (2) hub/flint-lite S3
hydration — **warm re-serving only** (hydrate 72.5 s/GiB [M runbs] loses cold); (3) lanes 3
then 2 per this doc. Churn is split deliberately: **node churn** favors lane 3 (the assembled
lvol survives node death and re-attaches); **image churn** is a K3 input against it (every
new digest pays a full assembly copy; at reuse < 2 lane 3 is classic pull + a copy bill).

| Cluster | Images | Churn shape | Cold-start SLO | Arm |
|---|---|---|---|---|
| <50 nodes or multi-AZ, no flint fleet | any | any | ≥30 s | 0: S3 + SOCI-parallel |
| any | ≤2 GB app | moderate | ≥10 s | 0 (+ lazy-on-S3 if unpack dominates) |
| large, stable, no storage appetite | any | storm-prone | seconds | P2P + lazy-on-S3 |
| flint fleet already deployed | 1–20 GB | node churn / storms | seconds | hybrid: lane 3 + origin re-fetch — **iff the §9.5 gates passed; rung 1/A5 else** |
| flint fleet, CI-style image churn | any | high image churn, reuse < 2 | any | 0 or 0.5 — K3 territory, lane 3 anti-indicated |
| any with flint | 10–150 GB weights | scale-to-zero | TTFT-bound | file-layout L0/L0.5 now; lane 2 later (D8) |
| air-gapped / in-cluster publish | any | any | any | 1: registry-on-flint |
| small cluster, warm locality | any | re-pull-heavy | ≥10 s | 0.5: pull-through cache; hub shape only if measured warm-serving wins |

## 9. Performance expectations and the measurement plan

### 9.1 (a) Lane-3 fault-latency model

Chain: page fault → EROFS lookup ([I] <10 µs) → dm-linear remap ([I] µs) → nvme-tcp →
spdk-tgt → lvol → device (~80–100 µs device-side [E vendor-class]; ~50–200 µs RTT [I]). No
DS-side RAM cache exists on this path — the block plane removed it; runbk measured cache as
a *loss* (3,173 O_DIRECT vs 1,470 cached MiB/s [M]). **Every flint number we own here is
sequential throughput; small-random-read (4–128 KiB) p50/p99 over the nvme-tcp export has
never been measured.** Total miss: **[H] 0.2–0.6 ms p50, <2 ms p99 warm** — vs SOCI ~4.6 ms
warm / 62–290 ms cold [E]. If [H] holds, ~8–20× lower warm with no daemon; **hypothesis
until the rig runs**, and only the A/B makes the two commensurable (each number bundles its
own stack). Per-fault latency is the term that matters: lazy ready-time ≈ serialized
critical-path faults × per-fault latency + streamed fraction ÷ bandwidth; at ~1–5k
serialized faults [I, unvalidated] that's 5–23 s at SOCI-warm rates vs 0.5–2.5 s at [H]
flint rates.

### 9.2 (b) Boot-storm arithmetic

N nodes, image size S, first-touch working set W (~6–15% of S in external literature [E,
Slacker-order]; **our W unmeasured**). Baseline: N × (S÷wire + S÷gunzip), gunzip dominates.
Lazy-on-S3: ~W of range fetches per node; aggregate scales with N; the tax is per-fault
latency. Lane 3: N×W bytes concentrated on the **one DS hosting the assembled lvol** (no
clone — fact 3): fan-out ceiling ≈ min(device ~2,850 MiB/s [M runbk], DS wire ~3 GB/s
[M-class]) — e.g. N=100, W=300 MiB → ≈10.5 s serving floor if sequentialized [I]; the real
risk is IOPS queueing raising per-fault p99 [H]; on an amortized fleet the storm also
contends with tenant data workloads on the same device (§8.4 — a term of this arithmetic).
`replicas: 2` lifts only the **device** term — ceiling = min(2 × device, composer NIC −
remote-leg ingress): single-seat serving forbids active/active paths (block doc §12) and
remote-leg reads transit the composer's own nvme-tcp initiator, so bandwidth moves ~2,850 →
~3,000 MiB/s (~5%) [I]; the measurable prize is device-IOPS headroom for per-fault p99 —
even that assuming raid1 balances reads, unmeasured (§12). By contrast the file-layout CAS
stripes width-N across the DS fleet: **rung 1's single-image storm ceiling is structurally
higher than any assembled lvol's**, whose fan-out lever is hard-capped at 2 — the
storm-shape advantage belongs to the cheap arm until measured. Multi-image storms spread
across lvols and plausibly inherit near-linear aggregate scaling [I — mechanism transfer:
measured on the striped *file* layout (registry doc §2); aggregate independent-lvol
nvme-tcp scaling has no rig — ceiling-rig leg owed, §11]. Where each wins [I]: flint on
per-fault latency and per-node cache/request elimination within fleet bandwidth × window;
S3 on raw aggregate at very large N against one hot lvol, and on every ops bill.

### 9.3 (c) The pilot — rung 1, and it runs FIRST

EROFS blobs loop-mounted off the RWX file-layout mount: zero flint code, lazy to the page,
no daemon. Rig: lima client+server first, then kind (functional only), then a real cluster
for storm legs; one multi-GB real image, erofs-utils `--tar`, 5.4-readable profile pinned.
Arms per rep, interleaved: **(P1)** baseline pull+start; **(P2)** EROFS blob on local disk,
loop-mounted — the format win; **(P3)** same blob on the flint RWX mount — the remote-fault
tax. P3−P2 is the flint tax; P1−P2 is the format's win. Metrics: time-to-first-exec;
first-fault p50/p99 (bpftrace); NFS READ ops + bytes (server counters); cpu-ms/GiB served —
never MiB/s (house rule). **Anti-vacuity guards:** (G1) per rep, drop_caches + remount; rep
invalid unless server READ-op delta ≥ W÷rsize (faults went remote); (G2) falsifiability leg
— a pre-warmed client cache must show ~zero server READs and collapsed fault latency (the
oracle can detect a broken lazy path); (G3) full-readability sweep (`find | cat`) catching
EROFS profile drift as EIO now; (G4) digest identity across arms. **Pre-declared pass
criteria:** P3 first-fault p99 < 5 ms; P3 ready-time ≤ 1.5× P2; zero EIO in G3; G1/G2 pass.
Fail ⇒ the file-layout tier is not viable; lane 3 inherits the burden of proof alone.

### 9.4 (d) The five-arm cold-start A/B — the gate for any headline ratio

Arms: **(A1)** baseline containerd pull; **(A2)** SOCI parallel pull mode (the strongest
non-lazy baseline — D5); **(A3)** lazy-on-S3 (SOCI or nydus against the same image in an
S3-backed registry); **(A4)** lazy-on-flint (P3 now; lane 3 when it exists); **(A5)** the
same lazy snapshotter as A3 pointed at the in-cluster registry-on-flint (rung 0) — the
strongest cheap competitor: zero flint code, no re-format for SOCI, the striped CAS storm
ceiling (§9.2), no WAN/TLS cold-connection term, no egress. A3-vs-A4 moves format + miss
path + backend at once and attributes nothing; **A3-vs-A5 (registry endpoint swap only) is
the clean backend attribution**, and A5-vs-A4 isolates the block lane's marginal claim.
Same digest, same nodes, same kernel, same containerd version/config except the snapshotter; single-AZ
(budget the bytes). Metrics: time-to-container-ready; first-fault p50/p99; cpu-ms/GiB on the
serving side; per-backend op counts (nfsstat / `bdev_get_iostat` / S3 access logs).
Discipline: interleave arms per rep; paired per-rep ratios; **per-rep backend attribution —
the flint arm must show zero S3 GETs and the S3 arm zero flint reads, else the rep is void**;
idle-CPU check before each rep (saturation compresses ratios toward 1.0 — the runaway-logind
lesson); cold-ness guard per rep (drop+remount / daemon restart / evict content store)
asserting the arm's own counters moved ≥ working set. Storm legs: N ∈ {1, 8, 32} real nodes
on a trove cluster — loopback/kind cannot measure fan-out; kind reps never quoted.

> **AMENDMENT 2026-09-01 (runby, field) — two corrections to §9.4, one of
> which blocks A3/A5 outright.**
>
> **(1) A3 and A5 require a registry that serves the OCI referrers API, and
> stock Distribution does not.** SOCI discovers its index through
> `GET /v2/<name>/referrers/<digest>`. Distribution **3.1.1** (`registry:3`)
> answers that **404 `page not found`** — over plain HTTP *and* over trusted
> TLS, with the subject manifest present (200) and the registry advertising
> only `docker-distribution-api-version: registry/2.0`. The index is pushed
> and well-formed (it is fetchable under SOCI's fallback tag
> `sha256-<digest>`, `artifactType: application/vnd.amazon.soci.index.v1+json`),
> but SOCI's *read* path does not fall back to that tag. Consequence: SOCI
> **silently serves an ordinary eager pull** — no error, no warning, arms
> that look healthy and are not lazy. Measured: A3/A5 `pull_ms` ≈ 20 s
> against A1's ≈ 11.5 s, i.e. eager **plus** snapshotter overhead, with
> `"failed to prepare remote snapshot"` ×42 in the snapshotter journal.
> **This falsifies the "A5 = zero flint code, no re-format for SOCI" premise
> in D5**: A5 additionally requires a referrers-capable registry, which
> rung 0 as specified (stock `registry:3`) is not. Before the A/B is
> re-attempted, settle the registry: a build/config of Distribution that
> serves referrers, or a registry that does (zot), or a lazy path that does
> not depend on referrers discovery. **Note this does not touch rung 2** —
> flint's own snapshotter resolves through containerd metadata and its own
> CAS, never the referrers API. It is A3/A5 — the *baseline* and the *clean
> backend attribution* — that are blocked.
>
> **(2) The coldness guard is one-sided and produced a wrong number.** As
> written it cools the *client* (drop+remount / daemon restart / evict
> content store) and asserts *the arm's own* counters moved. That cannot
> see a warm **backend**: the registry pod holds blobs in its page cache
> after the first pull, so later reps are served from RAM and the storage
> layer under test is never read. Measured on runby: **five flint pulls of
> a ~400 MB image produced TWO `LAYOUTGET granted` lines**; with the
> serving registry restarted per arm the same workload produced **42**. The
> client's own counters move either way — it downloads the bytes regardless
> — so the existing assertion passes in both worlds. A 3.8% "flint beats
> S3" figure was produced off the warm-backend version before this was
> caught; cooled on both sides the paired median was 0.969 (~3%).
> **Required:** cool the serving backend per arm as well, and assert a
> **server-side** counter with a known non-zero expectation
> (`LAYOUTGET granted` for flint, S3 access-log GETs for S3). The rule:
> *the arm that is supposed to be loud must actually be loud* — if the
> control cannot produce signal, the leg is VOID, not PASS.
>
> Both corrections share one shape, and it is the shape to design against:
> every rep individually valid, zero voids, tight variance, and **the thing
> under test not exercised**. No attribution/integrity/coldness/load guard
> can see it. The question that catches it is not "did the reps pass?" but
> **"did the workload reach the thing I am measuring?"** Rig, gate and
> evidence: `tests/k8s/oci-ab/` (`FINDINGS.md`, `drive-ab.sh`,
> `stripe-width-gate.py`).

### 9.5 (e) Pre-registered kill conditions for the block lane

Lane 3 is **dead** if, on a valid rig: **(K1)** it fails to beat the rung-1 arm by ≥20% on
both first-fault p99 and ready-time, at N=1 and at storm N — rung 1 then delivers the win
without the assembly, session, or reservation bills; **(K2)** measured first-fault p99 ≥ 4.6
ms — the differentiator is falsified; **(K3)** lane 3's reuse-weighted per-image cost
(assembly wall-clock + capacity amortized over measured cross-node reuse within the
retention window) fails to beat the strongest surviving cheap arm — A2 or A5, not the
demoted baseline; OR, independently, reuse < 2 (nothing amortizes the copy); **(K4)**
single-lvol storm fan-out at target N loses to lazy-on-S3 — the only lever is `replicas: 2`
device-IOPS headroom (§9.2), a designed ceiling of exactly 2; **(K5)** lane 3 AND rung 1
both fail to beat A5 — the product is then rung 0 plus a stock lazy snapshotter, no
flint-side lazy code at all. Any one triggers retreat to the next-cheapest surviving arm,
S3 remaining origin per D5.

## 10. Phasing and sizing

- **P0 (now, zero code):** rung 0 registry per registry doc §3–4. Exit: in use.
- **P1 (pilot):** rung 1 rig (§9.3) on lima, then storm legs on a real cluster alongside the five-arm A/B at N=1. Exit: pass criteria met (else this doc's block lane stops here — K1/K5).
- **P2 (substrate ROX):** §4 change list; formal arm (§11); conformance must-fail leg; read-shared SC surface for model-store ROX PVCs (front door a). Exit: gate green + drills 1–3 designed.
- **P3 (the slice):** snapshotter DaemonSet + assembler/converter + pins + GC wrapper (D1); storm A/B at N ∈ {8, 32}; ceiling-rig images×nodes leg. Exit: A/B clears K1–K5; GC drill legs pass; drainRoll image-drain procedure exists (§7.2 — GA blocker).
- **P4 (gated):** lane 2 / StorageDriver / CAS epoch 2 — strictly after libflint tranche A (snapshotter doc §14); revisit D6 replicas when the control-plane-failover tranche lands.

**Sizing points** (all [I] until the rigs run): sessions ≈ 4,000 at 100 nodes × 40 images
(migration to ~400 reserved — §6.3); cntlid ceiling 4096 readers/volume/composer;
cluster-side bytes ≈ ~5× layer bytes (§6.3) + unmeasured assembly duplication; one ConfigMap
rv-CAS serialization per volume per attach storm; assembler pool bounds writer sessions.

## 11. Verification

**Formal (no-dead-arm doctrine, formal/README.md):** ONE owed arm — a FlintExtents constant
split. `ClientRead`'s guard `c ∉ resv` encodes 4h; a read-shared variant pins
reads-passing-resv. The new constant defaults FALSE in every existing cfg (TLC binds all
constants, so the cfg *files* gain a line); existing **state graphs** stay bit-identical,
verified by flagship distinct-count match — the module's own tranche requirement. The
**must-violate** no-guard run: reads pass resv AND a reader fence graduates to clean-free
via the delivered bit → free+reuse under the never-excluded reader → `staleRead` fires — a
run LostFence's counterexample never exercises, so the arm has its own teeth. The guarded
run (reader fences never set delivered ⇒ quarantine) must pass. **Not owed:**
immutable-lane-3 fence arms (a mutation that cannot lose proves nothing); new
FlintComposition reader arms (pair exists); any new TLA module (snapshotter doc §13);
`LockHolderFallbackWrite` (write-lane, untouched). The RW-LAYOUTGET bounce is a
**conformance must-fail leg**, not a TLA arm (the corpus carries no reader-consistency
invariant).

**Rigs:** the pilot (§9.3, guards G1–G4); the five-arm A/B (§9.4); the phase-2 ceiling rig
extended with an images×nodes leg that **must drive a target past its actual qpair ceiling**
(a leg showing "N sessions worked" proves nothing), an attach-storm leg on one volume's
ConfigMap, and a multi-image × multi-lvol aggregate-scaling leg (earns the §9.2 label);
ingest fuzz + diffID round-trip on the assembler (transferred snapshotter doc §15
obligation); a loop-vs-dm control leg (D7); a one-time **5.4-kernel readability leg** — a
pinned-profile artifact mounted + G3-swept on a real 5.4 VM (else the §3 floor restates to
the oldest kernel exercised — §5.2).

**GA failure drills, each with its anti-vacuity guard:**
1. **Composer death under live rootfs** (replicas: 2 arm): warm reads continue; cold-read stall measured; tests the §7.1 open lever (promoted composer serving via witness allow-lists). *Guard:* pre-kill cache drop proving cold reads flow via the composer traddr; a control read DURING the window must fail/stall — if it succeeds, a stale path leaked.
2. **Re-assembly recovery** (replicas: 1, the D6 default): destroy the hosting node; re-assemble on a healthy shard; digest-verify every layer. *Guard:* a corrupted-CAS-blob control arm must FAIL the verify; the dead node stays off so old bytes cannot serve. **Mid-storm variant:** kill the composer during an N-node Prepare storm; *guard:* re-assembly count == 1 (a leg that doesn't count assemblies passes with a stampede — §5.3), and record what already-attached nodes experienced (the split-fleet outcome, §7.1).
3. **Witness/API outage** past the serving lease (`iptables -j DROP` — SGs are stateful): the replicas: 2 namespace suspends as documented AND fencing enforces locally mid-outage. *Guard:* a replicas: 1 namespace on the same cluster must serve straight through — else the drill measured kubelet, not the witness. **Scale-up arm:** a node added during the outage must degrade to classic pull *promptly* (within the Prepare deadline, not a wedge) — the attach path is API-dependent at every replica count (§6.3), and a drill without this arm hides it.
4. **drainRoll:** `helm upgrade` with live namespaces — roller refuses tgt nodes with the named event, zero consumer EIO; then the image-drain procedure completes. *Guard:* a no-initiator canary node must actually roll in the same campaign (discriminating refusal vs wedged roller).
5. **Registry GC vs live rootfs** (run against the rung-1 shape too — the pin protects rungs 1–3, §6.1): retag, run GC while pods run the old digest; pinned blobs survive, reads keep working. *Guard:* with pins disabled, GC must collect and reads must fail — the leg can lose. **Mid-assembly variant:** retag + GC during an in-flight assembly; *guard:* assembly completes digest-verified with **zero origin re-fetches** (a leg allowing re-fetch passes with the pin broken).
6. **Kill-flint-mid-storm fallback (D5):** *guard:* origin-registry-side fetch count ≠ 0 — measured at the origin's access logs, since a flint-side counter reads a dead component — AND per-pod time-to-ready ≤ baseline classic-pull time + the configured Prepare deadline (an unbounded "pods still start" passes a 20-minute CRI-backoff wedge); plus the §5.5 re-fetch leg asserting CAS population, never pod start.
7. **GC collection legs:** an unpinned orphan is actually collected; a dead node's control-lane lease (§6.1) lapses, the pin expires, then collects — a GC drill that never deletes anything passes with GC broken. **Dead-renewer leg:** kill the snapshotter process (not the node); the lease lapses but the qpair cross-check sees live controllers — *guard:* the lvol is NOT evicted and running pods are unharmed (the second-signal guard can lose).
8. **Snapshotter restart under live pods:** restart the DaemonSet, then delete one pod — only its refs drop, every other pod's rootfs unharmed. *Guard:* a Remove issued mid-rebuild must not detach a namespace another live pod holds (§3 restart discipline).

## 12. Risks and open questions (carried, none silently dropped)

**Reservation / seal / write-refusal:**
- Standing 3h WE-RO from lvol creation vs a read-only SPDK snapshot blob as the device-level write refusal (§4 — the only true one, given the registrant residual) — the ro-blob route touches the block class's refused snapshot surface and its billing amendment (block doc §12). Undecided.
- Seal mechanism: snapshot (gets `num_allocated_clusters` accounting) vs read-only export at full-span charge — does internal use share the user-snapshot refusal?
- volumeMode: Block for read-shared consumers that never speak pNFS — admit (the LAYOUTGET rationale evaporates for assembled namespaces) or keep one refusal story?
- The §4 registrant-preempt monitor (fence lane sweeps Reservation Report for foreign keys): sweep cadence, and whether the ro-snapshot alternative supersedes it entirely.

**Witness / attach scale:**
- ConfigMap rv-CAS contention under an N-node attach storm on one popular image — batch node_attach, or a different store for hosts rows? The ceiling rig must cover attach storms, not only pod-churn allow-list churn.
- cntlid band (4096/volume) sufficiency once per-pod NQNs arrive with lane 2; per-pod allow-list churn ceiling under pod storms (gates the lane-2 default).
- Actual spdk-tgt qpair ceiling at images×nodes scale (~4,000-order) — decides shared-session migration timing.

**Failover / recovery behavior:**
- Can a promoted composer converge and serve read-only lane-3 exports from witness-carried allow-lists without the dead shard's sqlite? Drill 1 decides.
- Re-assembly serialization above the shard (§5.3): controller-side keyed `(digest, platform)`, or witness-carried assembly state — pick one; drill 2's mid-storm variant gates it.
- Stalled-attach migration after re-assembly (§7.1): the snapshotter owns redial to the new copy — mechanics (detection, dm re-point vs pod restart) undesigned.
- Does a live EROFS-over-nvme-tcp mount transparently survive composer recovery at the same traddr? [I yes; unmeasured.] Fleet-scale redial after promotion (runbr's 39 s is one client, not a fleet).
- Stall policy for a rootfs: ctrl_loss_tmo / fast_io_fail_tmo (30-minute D-state precedent), and what running lane-3 pods actually experience during a composer-death stall (frozen-then-resume vs task crash) — decides whether stall-until-recovery is acceptable for rootfs GA or gates on control-plane failover.
- Boot-storm stall magnitude under composer outage; how well origin re-fetch bounds it.
- Assembled-lvol eviction policy and re-assembly storm behavior when a target dies holding many images.

**Measurement unknowns (the differentiator rests on the first):**
- Small-random-read (4–128 KiB) p50/p99 over flint's nvme-tcp export, warm and under N-node load — never measured on any flint rig.
- Does shipped raid1 balance reads across legs (buying device-IOPS headroom — the only term it can lift, §9.2) or pin to one?
- First-touch working set W and serialized-fault count for representative images on EROFS.
- Rung-1 per-fault latency: loop-over-NFS readahead under EROFS access patterns could amortize or amplify faults.
- Assembly economics: wall-clock + capacity vs measured cross-node reuse (the K3 inputs); break-even vs S3-lazy instant start (image size × churn); the lane-3 duplication factor on a realistic image population — decides whether no-clone stays cheap or forces refcounted extents back open.
- SPDK/lvol IOPS headroom holding per-fault p99 flat at storm N, or a ceiling to state.
- Lazy-on-S3 prefetch bandwidth in storms — could move the crossover against flint; and whether SOCI-parallel beats lazy-on-flint p50 on fast local NVMe, shrinking lane 3's claim to sparse-access/spot-churn shapes.
- Prepare-time attach latency (RPC + connect + dm + mounts) — order-tens-of-ms is [H].
- Hub middle shape: is 72.5 s/GiB hydrate intrinsic or improvable?

**Ingest / conversion / CAS:**
- Origin pull credentials reaching the assembler (CRI label pass-through vs cluster secret vs keychain) — no mechanism decided.
- Distribution integration: webhook reliability as sole conversion trigger vs a CAS-scan reconciler; whether foreign sibling files (EROFS + record) under blobs/ survive its GC untouched (unverified); GC wrapper vs upstream patch (spike owed).
- mkfs.erofs `--tar` determinism + diffID round-trip at fleet scale; conversion throughput (the first-pull tax).
- Multi-arch conversion cost and per-platform derived-artifact naming.
- Crashed-assembly resume granularity (fresh lvol vs per-layer markers) — pick after assembly wall-clock is measured; any resume obeys the §5.3 invariants (re-drive preempts + sole-registrant assert, read-back seal, reaper anti-resurrection).
- Epoch-2 dual-origin migration semantics (authoritative copy per digest; pin-ledger span).

**Product / ops:**
- First-pull Prepare-blocking vs CRI progress-timeout; the deadline default.
- Mount-stack risks: overlayfs lowerdir option-length at ~40+ layers with long dm paths; containerd 2.2 mount-manager rework vs a proxy snapshotter returning dm-device mounts.
- The drainRoll image-drain procedure (GA blocker, §7.2); `release_quarantine` operator surface (unowned, load-bearing).
- Multi-AZ posture: fleet-per-AZ vs single-AZ pinning vs paid cross-AZ egress — no decision-table arm covers an AZ-spread fleet wanting flint serving.
- ROX PVC content population for the non-OCI shape (model stores: dataSource? publish flow?) — assembly is defined only for images at first pull.
