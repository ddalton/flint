# CSI attach/detach chaos campaign — 2026-07

**Status:** PAUSED 2026-07-13 at drill 1.3 — **probable P0 found** (lost
fsync-acked write under force-delete detach/reattach, see Findings); cluster
torn down on request, campaign resumes on a fresh cluster. **Under test:**
shipped **v1.15.0** (chart 1.15.0, `dilipdalton/flint-driver:1.15.0`), no code
changes. **Cluster:** trove project 36 `runs` — 4× i4i.xlarge (1 CP + 3
workers), k8s v1.34.9, 937 GB instance-store NVMe per worker (DELETED
2026-07-13). **Harness:** `tests/chaos/` (Postgres 16 + pgbench + acked-write
ledger oracle).

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
| 1.1 | in-container postmaster SIGKILL | 22s | 22s | PASS | in-place restart, zero CSI calls, DB recovered |
| 1.2 | graceful pod delete | 6s | 6s | PASS | same-node replace, clean shutdown |
| 1.3 | force delete (`--grace-period=0 --force`) | — | — | **FAIL (P0)** | postgres unrecoverable: WAL redo segments missing; see Finding F1 |
| 1.4–1.15 | — | | | NOT RUN | campaign paused after F1 |

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

**Fix directions to evaluate:**
- Sync blobstore md after `bdev_lvol_create` and periodically / after FLUSH
  (correctness first, then measure).
- Verify `bdev_uring` flush semantics (does an NVMe FLUSH reach `fdatasync`/
  media?); consider O_DIRECT or bdev_aio comparison.
- Mitigation candidate to test: `thinProvision: "false"` in the SC (thick
  lvols allocate at create) — **Experiment T** below.
- F6 independently: reconcile-on-loss must escalate when the bdev is gone
  (surface VolumeCondition, mark the volume failed) instead of silent
  infinite retry under a Ready pod.

Evidence: `tests/chaos/artifacts/expD-1783953943/` (spdk-tgt gen-2/gen-3
logs incl. the `bs_recover` line, csi-driver NVME-RECOVERY loop, LVS views).

### Other findings

- **P0-a / P0-b** (trove provisioning) — see Phase 0. Not flint bugs; recorded
  for the trove backlog.
- Pre-existing orphaned NVMe session observed after kuttl create/delete churn
  (a controller stuck `connecting` for a deleted PV, 1800s ctrl-loss-tmo).
  Cleaned before drills; flagged to reproduce deliberately in the churn drill
  (1.10) to determine whether flint leaks NVMe sessions on rapid volume delete.

## Teardown

2026-07-13, on user request (pause + stop costs): `flint-chaos` ns deleted —
PV released and deleted through the driver in ~30 s **from a CrashLoopBackOff
consumer** with no finalizer hang, zero VAs left (a clean-detach datapoint in
itself). Trove project 36 deleted; all 4 EC2 terminated; spot/EBS/EIP orphan
sweep clean; kubeconfig removed. Campaign resumes on a fresh cluster at
drill 1.3 (repro of F1).
