# CSI attach/detach chaos campaign — 2026-07

**Status:** ACTIVE 2026-07-17 on `runu` — phase 1 complete through 1.15
(1.14 node-terminate deferred to campaign end). Two new P1s: **F8** (amnesiac
csi-node restart, drills 1.9b + 1.15) and **F9** (stale NodeUnstage kills live
cross-node subsystem); **F8/F9 driver fixes in progress**, phase-1 rerun on
fixed bits planned. **Under test:** shipped **v1.15.0** (chart 1.15.0,
`dilipdalton/flint-driver:1.15.0`) + `spdk-tgt:1.6.0-f5fix.1` (F5 recovery
patches). **Cluster:** trove project 38 `runu` — 5× i4i.xlarge on-demand
(1 CP + 3 workers + cordoned builder `runu-aws-4`), k8s v1.34.9. Earlier
clusters: `runs` (project 36, deleted 2026-07-13), `runt` (deleted
2026-07-16 mid-campaign). **Harness:** `tests/chaos/` (Postgres 16 + pgbench
+ acked-write ledger oracle).

## Goal

Torture the CSI attach/detach path (ControllerPublish/Unpublish +
NodeStage/Unstage + NodePublish/Unpublish) under a real stateful workload,
progressing **RWO r1 → RWO r2(/r3) → RWX**, and prove flint recovers with
**zero lost acknowledged writes** and **no manual VolumeAttachment surgery**.
Closes the node-failure / VA-cleanup / pod-lifecycle gaps flagged
HIGH-PRIORITY in `tests/system/MISSING_CRITICAL_TESTS.md`.

## Method

- **Oracle:** `pg-load` (anti-affinity to the DB) runs pgbench pressure plus a
  ledger loop that appends a seq to `/acked/acked.log` only after the
  `INSERT ... RETURNING` commit is acknowledged (`synchronous_commit=on`).
  Any acked seq later missing from the `ledger` table = **lost acked write**.
- **Per-drill verdict** (`verify-drill.sh`, one `results.csv` row each): pod
  Ready + attribution; ledger reconciliation + `pg_amcheck --heapallindexed` +
  WAL-corruption log grep + writability; VA consistency; NVMe session health
  (live volume `live`, no rise in orphaned sessions); orphaned-mount scan;
  driver-log error scan; timing capture.
- NVMe recovery config under test (v1.15.0 defaults): `ctrl_loss_tmo=1800s`,
  `reconnect_delay=5s`.

## Phase 0 — provisioning + smoke (DONE)

- Cluster provisioned; campaign SCs `flint`/`flint-r2`/`flint-r3` applied (all
  WaitForFirstConsumer).
- **Finding P0-a (trove):** SPDK-mode install did not initialize the
  instance-store disks — controller saw 0 capacity, every CreateVolume failed.
  Fixed manually (`/api/disks/initialize` on `0000:00:1f.0` per worker).
- **Finding P0-b (trove):** spot/instance-type launch spec not honored — all
  nodes came up on-demand i4i.xlarge regardless of request.
- kuttl smoke green after disk-init: rwo-pvc-migration, multi-replica,
  rwx-single-replica, clean-shutdown all PASS.

## Phase 1 — RWO, numReplicas=1

_Results table filled from `tests/chaos/results.csv` as drills complete._

| # | Kill vector | Ready | Stall | Verdict | Notes |
|---|---|---|---|---|---|
| 1.1 | in-container postmaster SIGKILL | 22s | 22s | PASS | runs; repro'd on runt — in-place restart, zero CSI calls |
| 1.2 | graceful pod delete | 11s | 9s | PASS | runt, F5-fixed bits; clean shutdown |
| 1.3 | force delete (`--grace-period=0 --force`) | 6s | 3s | CSI PASS / db N/A | DB corruption = expected F1 semantics (bar re-scoped to CSI hygiene); runt |
| 1.4 | cordon + delete, cross-node | 22s | 20s | PASS | runu rerun (runt verdict was contamination-invalidated, see below); clean cross-node migration |
| 1.5 | drain | 32s | 15s | PASS | runu rerun; drain-ordered detach clean |
| 1.6 | controller killed mid-attach | 16s | 13s | PASS | runu; in-flight ControllerPublish survives controller restart |
| 1.7 | controller killed mid-detach | 10s | 5s | PASS | runu |
| 1.8 | controller scaled 0 for 60s mid-migration | 98s | 66s | PASS | runu; attach parks until controller returns, no VA surgery |
| 1.9 | spdk-tgt hard kill (process only) | 49s | 41s | PASS | runu; v1.15.0 graceful recovery, io_resume=49s, restartCount stable |
| 1.9b | csi-node POD delete on pg's node | never | never | **FAIL — F8** | landmine mechanism exposed: amnesiac reconciler, exports never rebuilt, health check lies; recovery = consumer bounce (cross-node) |
| 1.10 | churn ×20 create/delete | 241s | 15s | PASS | runu; tot_gm=1 vas=1, no NVMe session leak |
| 1.11 | kubelet stop, slow path | 989s | 944s | PASS | runu; k8s eviction timing dominates (notready=48s evict=347s) |
| 1.12 | kubelet stop + oos taint | 314s | 262s | PASS | runu clean rerun (first attempt READY-contaminated by F3 disk-pressure taint, env) |
| 1.13 | ☠ EC2 stop of pg's node | — | — | PASS | teardown clean with dead backing volume; node restore needed manual EC2 start (rolesanywhere lacks ec2:StartInstances) + disk re-init |
| 1.14 | ☠ EC2 terminate of pg's node | | | DEFERRED | destroys a worker; run at campaign end with trove replacement queued |
| 1.15 | ☠ full csi-node DS roll | 1756s | 1261s | **FAIL — F8b** | landmine reproduced; **pod-bounce recovery failed same-node** (see F8 addendum); db PASS (zero lost acked writes); manual recovery 44s once dead session force-dropped |

### Verify contamination across drills (2026-07-13 batch — 1.4/1.5 verdicts invalidated)

The 1.3→1.4→1.5 batch ran back-to-back with **no harness reset**: the
inter-drill health gate (pg-0 Ready + ledger acking) passed even though 1.3's
by-design two-postmaster overlap had corrupted the DB (amcheck FAIL). Both
1.3's and 1.4's verifies reported the **identical** 93 missing acked seqs
(2477…) — and 1.5's verify then found **all 20,471 acked writes present**,
including those 93, on the same uninterrupted ledger lineage. Genuine storage
loss cannot un-lose writes; delayed visibility of the doomed postmaster's
flushed-but-orphaned commits (shared PGDATA, shutdown checkpoint racing the
replacement's recovery) can. Conclusion: 1.4's "LOST ACKED WRITES" is 1.3
residue, not a cross-node-migration bug — but the only honest verdict is a
rerun. **Rule adopted: any drill whose expected outcome includes DB corruption
(1.3, ☠ drills) is followed by a mandatory `deploy-harness.sh reset` before
the next drill's T0.**

## Phase 2 — RWO, numReplicas=2 (RAID1)

_TBD._

## Phase 3 — RWX (NFS)

_TBD._

## Findings

### F1 — RECLASSIFIED 2026-07-13: concurrent postmasters, not a storage bug

**Verdict: NOT a flint storage defect.** Reproduced first-try on the fresh
`runt` cluster with the replacement pod's attempt-0 log intact, which the
first incident was missing. The log proves temporal overlap: attempt-0 read
`pg_control` at 13:42:01.625 whose "last known up" stamp was **13:42:01** —
written by the *old* postmaster, still alive after the force delete. Then
attempt-0's recovery failed with `xlog flush request 1/12FFF7A0 is not
satisfied --- flushed only to 1/12FFF6F0` — a heap page whose LSN advanced
**past attempt-0's view of end-of-WAL, while it ran**, because the old
postmaster was still writing through the same node-shared mount.

**Mechanism (documented k8s foot-gun, not flint):** `kubectl delete pod
--grace-period=0 --force` removes the API object immediately; the STS
controller creates the replacement at once; the same node satisfies WFFC; and
RWO is **node-scoped**, so kubelet happily NodePublishes the same staged
volume to the new pod while the old containers still run. Two postmasters,
one PGDATA, one shared page cache. `postmaster.pid` cannot protect across
containers (separate PID namespaces). Every anomaly in the original F1
analysis — the "reverted" `pg_control` (8 KB read-modify-write clobber by the
stale-read instance), the missing/recycled WAL segments (the old postmaster's
checkpoint legally recycled them), the zeroed `6A` shell — is explained with
zero storage-layer misbehavior.

On `runt` the picture was further muddied by an independent failure hit 6 s
later: kubelet DiskPressure evicted the csi-node pod (see **F2**), killing
spdk-tgt under the wedged mount and producing the reconnect-loop/EIO tail.

**What this changes:**
- Drill 1.3's original PASS bar ("ledger clean after WAL replay") was
  mis-specified for RWO: Kubernetes explicitly documents that force-deleting
  StatefulSet pods can violate at-most-one semantics. With RWO the DB *can*
  legally corrupt itself. The drill's flint-scoped bar is: CSI hygiene stays
  clean (exactly one VA, healthy session, no orphaned mounts, clean
  detach/reattach) — which it did, in both incidents.
- flint advertises `SINGLE_NODE_SINGLE_WRITER` (RWOP). A **1.3b RWOP drill**
  is added: with `ReadWriteOncePod`, kubelet must refuse the second pod's
  mount until the first is fully unpublished — force-delete must then be
  corruption-free end-to-end.
- The open durability question (does a *hard spdk-tgt kill* lose fsync-acked
  writes?) is exactly drill **1.9**'s ledger check — the durability leg the
  v1.15.0 grace3/grace4 drills (which validated liveness/resumption) never
  exercised.

Original (now superseded) analysis kept below for the record.

#### Original F1 writeup (superseded)

**Drill 1.3** (`kubectl delete pod pg-0 --grace-period=0 --force`, same-node
replacement, RWO r1 on `runs-aws-3`): the replacement pod's postgres went into
permanent CrashLoopBackOff —
`PANIC: could not find redo location 3/655168B8 referenced by checkpoint
record at 3/6E9FEDD8`. Every CSI-level check passed (1 VA on the right node,
NVMe session `live`, no orphaned mounts, no unresolved driver-log errors) —
the volume *attached* fine; its **contents** were inconsistent.

**On-disk state** (forensics preserved in
`tests/chaos/artifacts/1-1.3-1783920814/`):

- `pg_control`: checkpoint `3/6E9FEDD8`, redo `3/655168B8`, cluster state
  `shutting down`, mtime 05:33:35 (kill was T0=05:33:34).
- `pg_wal`: segments `65,66,67,68,69,6B,6C,6D` **missing**; `6A` present but
  zeroed (xlp_pageaddr=0 — a fresh pre-allocated shell, not the original);
  `6E` present with real WAL; ~90 recycled future segments `6F`–`C8`.
- `pg_waldump` of `6E`: after the `CHECKPOINT_ONLINE` record that `pg_control`
  points at, there is a **completed `CHECKPOINT_SHUTDOWN` record at
  `3/6E9FEE50`** (redo `3/6E9FEE50`) — the fast-shutdown checkpoint triggered
  by the pod kill ran to completion.
- dmesg: **clean** ext4 unmount at detach, clean mount at reattach — no fs
  errors, no journal complaints. One `nvme nvme2: Property Set error: 880,
  offset 0x14` (NVMe-oF controller-shutdown register write failed during the
  disconnect).

**Why this indicts the storage path:** postgres orders a shutdown checkpoint
strictly: ① flush WAL through the `CHECKPOINT_SHUTDOWN` record (fsync) →
② rewrite `pg_control` pointing at it, state `shut down` (write+fsync) →
③ unlink/recycle now-obsolete segments (`65`–`6D`). The disk shows ③
persisted, ② **reverted to its previous version** (the state-`shutting down`
write from checkpoint start), and ① partially present. A write that fsync
returned for was lost while *later* writes to other blocks survived — i.e.
lost/reordered acked write on one LBA range, not a torn suffix. The ledger
oracle showed no lost acked *transactions* (stall began at kill), but
`pg_control` is itself an fsynced write that vanished.

**Prime suspect:** the NodeUnstage NVMe disconnect racing in-flight/cached
writes in spdk-tgt — the failed CC-register shutdown write in dmesg says the
disconnect path did not cleanly quiesce the controller. Force delete is the
only drill so far that unmounts within ~1s of heavy dirty-page flushing.

**Repro/next steps (fresh cluster):** rerun 1.3 under load N times (expect
flaky — it's a race); instrument spdk-tgt flush handling (does lvol honor
NVMe FLUSH before disconnect teardown?); check NodeUnstage ordering
(umount → flush → controller shutdown → disconnect). Until root-caused,
treat **force-delete of a busy pod on v1.15.0 as data-loss-capable**.

Evidence files: `pg_controldata.txt`, `pg_wal-forensics.txt`,
`pg_control.bin`, `wal-segment-6E.bin.gz`, `dmesg-runs-aws-3.txt`,
`driver-logs.txt`, `db-verdict.txt`.

### F2 (real flint chart bug, FIXED) — csi-node evictable under DiskPressure

The chart set **no `priorityClassName`** on the csi-node DaemonSet (or the
controller). On `runt`, the 8 GB root EBS crossed the kubelet
ephemeral-storage eviction threshold at 13:42:08 (images + churn) and kubelet
chose the csi-node pod for eviction — **killing spdk-tgt under every mounted
flint PVC on the node** (the csi-node-roll landmine, self-inflicted), then
kept evicting each DS replacement until pressure cleared (6 evictions,
13:42–13:50). NVMe sessions reconnect-looped (`ctrl_loss_tmo=1800`), and the
pre-existing mount was wedged until manual controller delete via sysfs.

**Fix (shipped in-repo, applied live to runt):** chart now sets
`priorityClassName: system-node-critical` on the csi-node DS and
`system-cluster-critical` on the controller (values-overridable:
`node.priorityClassName` / `controller.priorityClassName`). Kubelet never
selects system-node-critical pods for resource eviction.

Unstick recipe recorded: dead controller in reconnect loop blocks unmount →
`echo 1 > /sys/class/nvme/<ctrl>/delete_controller` (host has no nvme CLI),
then pod teardown proceeds and the PV deletes cleanly through the driver.

### F3 (environment/trove) — 8 GB worker root is too small

Base images + flint images + one workload image (~4.8 GB) leave <2.5 GB
headroom; pod churn crosses the 85% eviction threshold within minutes of
harness deploy. Kubelet's reclaim also deletes just-pulled images (the
re-pull hit a Docker Hub 502 mid-recovery). Trove backlog: bigger root
volume (or dedicated imagefs on the instance store). Campaign mitigation:
F2's priority fix + accepting workload-pod evictions as legitimate chaos.

### F5 (**P0 — data loss**) — hard spdk-tgt death loses fsync-acked data; a young lvol vanishes entirely

**Experiment D (drill 1.9p), 2026-07-13 on runt — 100% reproduction, first try:**

1. Fresh volume (thin lvol, flint default `thinProvision=true`), pgbench +
   ledger load for ~7 min (`lvols: 1, free: 884636MB` in the driver's LVS
   view). All postgres commits fsync-acked through NVMe FLUSH.
2. `pkill -9 -f spdk_tgt` on the lvol's node (the exact kill vector the
   v1.15.0 graceful-recovery feature targets; sidecar restarted cleanly,
   consumer pod untouched).
3. spdk-tgt gen-3 startup: `blobstore bs_recover: Performing recovery on
   blobstore` (unclean-shutdown path) → `Lvol store found … examination done`
   → **`lvols: 0, free: 890101MB`** — the lvol is GONE from the recovered
   metadata; its ~5.5 GB returned to free space. Every fsync-acked byte lost.
4. Aftermath (**F6**): reconcile-on-loss (#1) re-creates the subsystem but
   `nvmf_subsystem_add_ns` fails forever (`bdev … cannot be opened,
   error=-19` — the bdev no longer exists) retrying every 10 s; the
   initiator reconnect-loops against a listener that exports nothing; the
   consumer's disk I/O hangs indefinitely (35+ min observed) while the pod
   stays **Ready** (its probe, `pg_isready`, touches no disk).

**Why v1.15.0's grace3/grace4 validation missed it:** those drills used aged
volumes (metadata long since synced by a prior clean unload) and verified
*liveness* — held-open fds, I/O resumption — through the still-warm page
cache. They never verified *durability* of recent writes, and never killed
the target while the blobstore held unsynced metadata.

**Mechanism (two candidates, isolation = follow-up):** flint never issues any
blobstore/blob md sync (`grep -r sync_md` over the driver: zero hits;
`thin_provision` defaults true at `main.rs:1135`). SPDK persists thin-lvol
cluster allocations — and evidently, on this v26.05 build, even blob
creation — only at clean unload / explicit `spdk_blob_sync_md`. Alternative
or compounding: `bdev_uring` buffered-vs-O_DIRECT semantics (gen-N buffered
writes in host page cache never reaching media, gen-N+1 reading the device
directly). Either way the contract is broken: **NVMe FLUSH is acked for data
whose metadata (or content) does not survive target process death.**

This retro-explains **incident 2** (the 14:10 DS roll under load → mixed
old/new on-disk state: a clean SIGTERM unload racing its 30 s grace under
active connections, then partial metadata persistence), and plausibly the
original runs incident (eviction storm = repeated hard kills).

**Experiment T (same kill, thick lvol — `thinProvision: "false"`): NO
mitigation.** Pre-kill `lvols: 1, free: 869621MB` (the full 20 GiB truly
allocated at create); post-kill `lvols: 0, free: 890101MB` — the thick lvol
vanished identically, I/O never resumed (300 s+). So blob *existence* is
only persisted at clean unload, independent of provisioning mode.

**Refined mechanism:** blobstore metadata (blob existence, cluster maps) is
written to media only on clean unload (SIGTERM path) or explicit
`spdk_blob_sync_md` — which flint never issues. Data-cluster writes for
*previously-synced* blobs go to media directly and survive process death —
which is exactly why the aged volumes in grace3/grace4 survived hard kills
(their md was synced by earlier clean restarts) while any volume created
since the last clean shutdown is silently **un-created** by the next hard
death, and thin allocations/resizes on older volumes roll back.

**ROOT CAUSE PINNED (Experiment R, 2026-07-13): flint's own
`blob-recovery-optimized.patch` drops valid on-media blobs during recovery.**

Discriminating experiment on a fresh volume (`pvc-9b07e1d5…`, lvol-local on
runt-aws-1):

1. Pre-kill raw scan of `/dev/nvme1n1`: the lvol's creation md page IS on
   media (name xattr at device offset 356414 → device page 87). Creation
   `blob_persist` works; O_DIRECT confirmed on the spdk_tgt fds
   (`flags=01140002`).
2. `pkill -9 spdk_tgt` → sidecar gen+1 → `bs_recover` runs the **patched**
   path (`Recovery: Using batched reads (64 pages/batch)` NOTICE — patch
   confirmed active in the deployed `spdk-tgt:1.5.0`) → `lvols: 0`.
3. Post-kill scan: **the md page is still on media, byte-identical offset**,
   and decodes perfectly by upstream validity rules — `id=0x1_00000002`
   matches its md-region position (page_index 2 with `md_start=85` per the
   superblock), `sequence_num=0`, `next=0xffffffff` (single page), CRC set.
   Upstream `bs_load_replay_md` would recover this blob; the batched
   replacement skipped it and then durably rewrote the store as empty
   (used-masks flushed by `bs_load_write_used_md`).

Additional defects visible in the patch by inspection, independent of the
exact skip bug: it never follows blob md page chains (`in_page_chain` can
never become true — multi-page blobs lose all pages after the first); it
never calls `bs_load_replay_extent_pages` (extent-table cluster allocations
are never replayed — silent data truncation even where the blob survives);
and at end-of-scan `bs_load_replay_md_chain_cpl` calls `spdk_free(ctx->page)`
on a pointer into the already-freed batch buffer (invalid free / UAF).

**Companion bug — `lvol-flush.patch`:** makes lvol advertise FLUSH and
completes every flush as an immediate no-op success ("blobstore handles
persistence" — it does not; "the underlying base bdev handles actual flush" —
it is never forwarded, and `bdev_uring` supports only READ/WRITE). Every
fsync through the stack is acked without flushing anything. In practice
O_DIRECT completion has been covering data writes, but the FLUSH contract is
void — device volatile-cache loss on power failure is unhandled, and nothing
ever persists blobstore md at runtime.

**Fix plan:**
- **Revert `blob-recovery-optimized.patch`** — take upstream recovery's
  correctness over scan speed (the batched scan of this 893,592-page md
  region took ~4.5 s; upstream's serial scan is slower but this is a
  crash-recovery path). If the optimization is wanted later, it must be
  rebuilt with chain-following, extent-page replay, per-page parity with
  upstream, and an A/B recovery test against a store with multi-page +
  extent-table + freshly-created blobs.
- Rework `lvol-flush.patch` to forward FLUSH to the base bdev (bdev_uring
  needs real flush support) and/or sync blob md on flush; at minimum stop
  acking flushes as no-ops.
- Rebuild spdk-tgt, and gate on Experiments D/T/R as regression drills
  (create → write → `kill -9` → recover → ledger + amcheck + cold-reader
  verify).
- Until fixed: treat any hard spdk-tgt death as data-loss-capable; the only
  safe restart is clean SIGTERM with generous grace (the 30 s DS default
  under load is itself suspect — incident 2).
- F6 independently: reconcile-on-loss must escalate when the bdev is gone
  (surface VolumeCondition, mark the volume failed) instead of silent
  infinite retry under a Ready pod.

Evidence: `tests/chaos/artifacts/expD-1783953943/` (spdk-tgt gen-2/gen-3
logs incl. the `bs_recover` line, csi-driver NVME-RECOVERY loop, LVS views).

### F5 fix + validation (2026-07-13, spdk-tgt:1.6.0-f5fix.0)

Patches reworked (`blob-recovery-batched.patch` — batched reads with
upstream-identical processing; `lvol-flush-sync.patch` — FLUSH →
`spdk_blob_sync_md` on the lvolstore md thread). Unit gate on the builder
node: **blob_ut 500/500** (206,448 asserts; every `blob_dirty_shutdown`
recovery sub-case through the batched path), lvol_ut 37/37, vbdev_lvol_ut
23/23. Image `dilipdalton/spdk-tgt:1.6.0-f5fix.0` (digest 62664caf) deployed
to runt's DS (roll performed with zero PVs — landmine-safe).

First live gate run recovered the blobs (`Recover: blob 0x0 / 0x1` NOTICEs)
— but surfaced **F7** (below). After F7 remediation (clean stores),
**D-redux-2 PASSED the full gate**:

- kill: `pkill -9 spdk_tgt` under pgbench on the lvol-local node →
  `bs_recover` → batched scan of all 893,592 md pages in **4.6 s** →
  `Recover: blob 0x0 / 0x1` → `Lvol store found — begin parsing` →
  reconcile re-export → initiator reconnect → **I/O resumed at +45 s**,
  consumer pod untouched (same UID, restarts 0→0)
- **WARM verify: PASS** — all 687 acked writes present, `pg_amcheck
  --heapallindexed` clean, writable
- **COLD verify: PASS** — cordon + graceful delete → cross-node reschedule
  (fresh session, cold cache) — all 956 acked writes present, amcheck clean
- **kill-2: PASS, and harder than designed** — during the second kill on the
  same (already-recovered) store, kubelet evicted pg-0 off its node
  (XFS-dynamic "inode" pressure = F3 space pressure in disguise; the
  csi-node itself survived — the F2 priority fix held), and the STS
  replacement landed on the lvol host **while its spdk-tgt was
  mid-recovery** — NodeStage retried until the bdev appeared, pod Ready
  ~70 s after the kill. Ledger: **all 1,423 acked writes present**, amcheck
  clean. Recovery idempotency + eviction + cross-node move mid-recovery,
  zero loss.

Compare the identical drill on the broken bits: lvol vanished entirely,
I/O wedged forever. **F5 is fixed.** Follow-ups that remain open on the
flint side: F6 (reconcile escalation + VolumeCondition), F7 fleet
remediation (or tolerant-recovery mode), packaging the fixed spdk-tgt into
the next release (this campaign ran `1.6.0-f5fix.0`).

### F7 — stores that ran the broken recovery are poisoned for strict recovery

The old broken recovery "deleted" lost blobs by rewriting empty used-masks
while leaving their (valid, CRC-intact) md pages on media. Normal blobstore
deletes zero md pages (`blob_persist_zero_pages`), so healthy stores never
contain valid orphan pages — but stores that ever ran the broken recovery
do. The corrected (upstream-semantics) recovery then finds the stale blob,
replays its extent table — whose extent pages have since been reused by
newer blobs — hits an id mismatch (`bs_load_cur_extent_page_valid`) and
fails the whole store load with `-EILSEQ` (identical to what vanilla
upstream recovery would do). Observed live: D-redux-1 recovered stale blob
`0x1` (`lvol_pvc-fa92d8e6…`, deleted in Experiment D) and the LVS load
failed; the consumer stayed wedged.

**Remediation (applied to runt):** wipe super+masks+md region (`dd` first
4.4 GB) + agent re-initialize on all three workers; controller scale-cycle.
**Fleet implication:** any deployed store that experienced a hard spdk-tgt
death on the broken-recovery images carries latent orphan pages; before
relying on the fixed recovery, stores must be rebuilt — or recovery needs an
opt-in tolerant mode (skip-and-WARN on blobs with dangling extent pages
instead of failing the store). Recorded as follow-up work.

### F8 — csi-node pod restart is amnesiac: exports never re-created, health checks lie (drill 1.9b, 2026-07-17)

Drill 1.9b (delete the whole csi-node pod on pg's node, so node-agent +
csi-driver + spdk-tgt all restart) on **v1.15.0 + f5fix.1** reproduced the
documented landmine and exposed its mechanism:

- spdk-tgt restarts and re-loads the LVS from disk, but the **NVMe-oF
  subsystem/listener/namespace exports are runtime state** that only
  reconcile-on-loss re-creates — and the reconciler's staged-volume records
  are **in-memory in the csi-driver container**. After a pod-level restart
  the reconcile loop runs happily with `success=0 skip=0 error=0`: it has
  nothing to reconcile. (This is why a bare spdk-tgt kill recovers in
  ~45–67s — the surviving csi-driver still knows the volume — but a pod
  delete never recovers.)
- The host initiator survives on ctrl-loss-tmo and reconnect-loops against
  the missing export **forever** (ECONNREFUSED ×253 observed, 5s cadence).
- The node's volume health check reported `healthy=true` and the consumer
  pod stayed Ready throughout — 20+ minutes of dead I/O, zero signals
  (same silent-hang family as F6).
- **Recovery validated:** force-delete the consumer pod. The replacement's
  ControllerUnpublish/Publish cycle rebuilt the export (replacement landed
  cross-node and attached remotely); ~7 min total, **zero VA surgery**.
- Evidence: `tests/chaos/artifacts/1-1.9b-landmine-1784250607/`.

Fix directions (flint work, with F6): reconcile from persistent ground truth
(this node's VolumeAttachments / kubelet staging dir), not an in-memory set;
make the DS/volume health probe actually touch the export path; surface
VolumeCondition so consumers aren't silently dead.

**F8b addendum (drill 1.15, full DS roll, 2026-07-17): the validated
"consumer bounce" recovery only works cross-node.** The 1.15 bounce
rescheduled pg-0 onto the **same node** (runu-aws-2) and never recovered:

- Same-node replacement reuses the already-staged volume — kubelet issues no
  NodeStage, so NodeStage self-heal never runs and the amnesiac reconciler
  (F8) never rebuilds the export. Post-roll the tgt is so bare that even the
  discovery listener refuses connections (ECONNREFUSED on 127.0.0.1:4420) —
  there is nothing target-side for the initiator's reconnect loop to find.
- The doomed postmaster sits in D-state on the dead session; kubelet cannot
  complete the kill (`FailedKillPod: KillContainer ... DeadlineExceeded`),
  so the old sandbox pins the mount while the new pod's postgres fails
  readiness against the same dead filesystem. Wedged 20+ min until manual
  intervention (would self-clear only at ctrl_loss_tmo=1800s).
- **Working manual recipe (validated live):** cordon the node → force the
  dead initiator session down (`echo 1 > /sys/class/nvme/<ctrl>/
  delete_controller`, D-state clears instantly) → delete the consumer pod.
  Cross-node republish then rebuilt the export on the bare tgt
  (`volumeType:"remote"`, listener on the node IP) and pg-0 was Ready in
  **44s** — versus ~7 min in 1.9b, where the stuck unstage had to wait out
  the 6-min force-detach window. Ledger reconciliation: **zero lost acked
  writes** (db PASS).
- StatefulSet consumers have no scale-cycle escape hatch equivalent to
  Deployments: a bare pod delete can land same-node any time the node has
  capacity. Until F8 is fixed, treat the landmine recipe for STS as
  cordon-first, then bounce.

Environmental note from the same verify: the orphaned-mounts check flagged
kubelet-leaked tmpfs/hugetlbfs mounts for deleted pods on **all four** nodes
(including ones the drill never touched) — residue of the F9 eviction storm
(~2.2k Evicted dashboard pods, since deleted; leaked mounts unmounted).
Zero flint volume mounts were orphaned; not a flint defect.

Harness hardening from the same incident (both fixed): `wait_acks_fresh`
raced (the final pre-kill ack looks "fresh" — now requires an ack newer than
T0), and verify-db had no timeouts (a dead volume wedged the whole batch
inside pg_amcheck — every check is now timeout-wrapped).

### F9 (**P1 — cross-node data-plane kill**) — stale NodeUnstage deletes a live subsystem (2026-07-17)

Between drills, a revived node's deferred cleanup destroyed the volume it no
longer owned. Timeline (all UTC, volume pvc-c15f47dd, single replica **on
runu-aws-2**, evidence `tests/chaos/artifacts/1-1.13pre-rofs-1784255332/`):

- 02:20 drill 1.12-rerun: kubelet stopped on aws-2 (pg's node) + oos taint.
  Pod force-deleted by GC — but aws-2's kubelet is dead, so its containers
  and mounts survive untouched.
- 02:21:35 ControllerUnpublish(aws-2) — **fencing works**: aws-2's host NQN
  removed from the subsystem. 02:25:14 ControllerPublish(aws-1) repeats the
  defensive `nvmf_subsystem_remove_host` (#3 disconnect-before-reuse). The
  replacement pod on aws-1 attaches cross-node to aws-2's target; Ready
  02:25:33; verify PASSES (writes flowing).
- ~02:26 the drill's cleanup restarts aws-2's kubelet. It finds the stale
  pod dir → NodeUnpublish (02:26:33.4) → **NodeUnstage (02:26:33.5) →
  `delete_nvmeof_block_device()` (driver.rs ~1175) → agent
  `/api/blockdev/delete_nvmeof` → `nvmf_delete_subsystem(<volume NQN>)` on
  aws-2's spdk-tgt — the subsystem actively serving aws-1's live
  attachment.**
- 02:26:37 aws-1 dmesg: `Buffer I/O error on dev nvme2n1 … lost async page
  write` → ext4 remounts RO → postgres FATAL "Read-only file system" →
  error spam fills aws-1's 8 GB root → kubelet evicts pg-0 for
  ephemeral-storage (02:29:44) → F3-style disk-pressure taints on two nodes.
- The harness's post-drill health gate caught it (1.13's preflight refused
  to start), and teardown after the incident was clean (ns + PVs deleted,
  no finalizer hang).

**The bug:** NodeUnstage's contract is initiator-side cleanup (unmount +
disconnect the local session). `nvmf_delete_subsystem` is target-lifecycle
work — correct only while the unstaging node is the sole consumer. After a
force-detach + cross-node re-attach, the stale node's late unstage deletes
the export under the live consumer. Host-level fencing (F/#3) doesn't
protect the subsystem object itself.

**Fix directions (post-campaign, with F8/F6):** NodeUnstage must not delete
a subsystem that (a) it didn't stage, or (b) has any other live host/VA —
guard by checking the subsystem's host list / this node's VA ground truth;
target teardown belongs to ControllerUnpublish-of-last-attachment or volume
deletion. Durability note: acked writes were WAL-fsynced to the (intact)
lvol before the kill — this is an availability P1, not a lost-acked-write
P0; the post-reset ledger reconciliation will confirm.

### Other findings

- **P0-a / P0-b** (trove provisioning) — see Phase 0. Not flint bugs; recorded
  for the trove backlog.
- Pre-existing orphaned NVMe session observed after kuttl create/delete churn
  (a controller stuck `connecting` for a deleted PV, 1800s ctrl-loss-tmo).
  Cleaned before drills; flagged to reproduce deliberately in the churn drill
  (1.10) to determine whether flint leaks NVMe sessions on rapid volume delete.

## Patch hygiene (2026-07-16)

The F5 fix was developed on a throwaway `f5fix` branch in the local spdk
checkout, then exported to the repo's patch files. Per project policy the
patch files are the only artifact: applying `nvmf-hostlog` + `ublk-debug` +
`blob-recovery-batched` + `lvol-flush-sync` onto pristine **v26.05**
(d519b163c, the latest SPDK release; `Dockerfile.spdk` pins `git checkout
v26.05`) was verified byte-identical to the branch on every touched file, and
the branch was deleted. `spdk-tgt` is rebuilt from the committed Dockerfile +
patches alone (tag `1.6.0-f5fix.1`) to prove the patch-only path end-to-end.

## Teardown

2026-07-13, on user request (pause + stop costs): `flint-chaos` ns deleted —
PV released and deleted through the driver in ~30 s **from a CrashLoopBackOff
consumer** with no finalizer hang, zero VAs left (a clean-detach datapoint in
itself). Trove project 36 deleted; all 4 EC2 terminated; spot/EBS/EIP orphan
sweep clean; kubeconfig removed. Campaign resumes on a fresh cluster at
drill 1.3 (repro of F1).
