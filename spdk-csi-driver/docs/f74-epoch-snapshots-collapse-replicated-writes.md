# F74 — epoch snapshots collapse small writes on every replicated volume (~190× on EC2)

Status: **FOUND 2026-10-07 by the CSI benchmark (Phase 1 on EC2); CAUSE
CONFIRMED 2026-10-08 by A/B. NOT FIXED — DECIDED 2026-10-08: keep the
epochs and make their cost small (§5, verified against SPDK v26.05 and the
Flint tree); the no-copy redesign is rejected unless measurement after §5
says otherwise. Not implemented.**
Affects every volume with `numReplicas >= 2` on a chart with the replication
orchestrators on, which is the default since they shipped. Blocks the
head-to-head benchmark (`docs/plans/flint-csi-benchmark-plan.md`): against
Mayastor, Longhorn and Ceph, Flint would lose every replicated-write test by
about two orders of magnitude.

## 1. Symptom

Flint 1.58.0, 3 × i4i.2xlarge (us-east-2c), fio on ext4, 90 GiB file,
`bench/results/2026-10-07-ec2-phase1/`:

| 4K randwrite | r1 | r3 |
|---|---|---|
| QD32 × 4 jobs | 110,130 IOPS (the disk cap) | **572 IOPS** |
| QD1 p50 / p99 | 48 / 72 µs | **2,671 / 6,128 µs** |
| 1M seq write | 1,061 MiB/s (the disk cap) | 320 MiB/s |

r3 reads are healthy (1M seq read 2,438 MiB/s, above one disk). r3 write
IOPS barely move with queue depth. Ruled out while it happened: the disk
(far from its caps per I/O), the network (ENA `bw_*_allowance_exceeded` on
the fio node rose by 23 out / 229 in over the whole run), the reactor
(`framework_get_reactors`: at most 61% busy on the writer, ~20% on the
replicas).

## 2. Cause

1. **The chart turns the epoch scheduler on for every replicated volume.**
   `templates/controller.yaml` sets `FLINT_EPOCH_SCHEDULER=enabled` and
   `FLINT_EPOCH_INTERVAL_SECS` (default 300) inside the
   `replication.orchestrators.enabled` block, which `values.yaml` defaults to
   true. Every 5 minutes `epoch_scheduler.rs` cuts a `bdev_lvol_snapshot` on
   every in-sync replica of every attached volume with >= 2 replicas, so that
   catch-up can copy only what changed. (Its file header still says
   "Default-disabled via FLINT_EPOCH_SCHEDULER" — stale.) r1 volumes are
   skipped, which is the whole r1/r3 split.
2. **After a snapshot, the next write to each cluster copies it.** The head
   becomes a thin clone; a write to a cluster it does not own allocates a new
   cluster and copies the old one in. Flint's lvstores use **1 MiB clusters**
   (`minimal_disk_service.rs`, `bdev_lvol_create_lvstore` `cluster_sz`), so a
   4 KiB write costs 1 MiB read + 1 MiB write + metadata, on every replica.
   SPDK copies even when the write covers the whole cluster.
3. **SPDK runs those copies one at a time per channel.** SPDK v26.05
   `lib/blob/blobstore.c` `bs_allocate_and_copy_cluster`: "There are already
   operations pending. Queue this user op and return because it will be
   re-executed when the outstanding cluster allocation completes." Flint runs
   one reactor per node, so one copy-on-write at a time per replica, and
   RAID1 waits for the slowest leg.

The EC2 numbers follow: 572 copies/s × 1 MiB ≈ 570 MiB/s written plus as much
read per disk (the instance's caps) while the network and reactor have room;
~1.75 ms per serialized copy regardless of queue depth. A random-write
benchmark over a large file pays on nearly every write; a workload with a
small hot set pays once per cluster per epoch — but pays again every 5
minutes.

## 3. Confirmation (A/B, `bench/results/2026-10-08-epoch-cow-ab/`)

Kind on the build box, Flint 1.58.0 with `replication.orchestrators.enabled=false`
(so the only snapshot is the experiment's), one r3 volume,
`bench/experiments/epoch-cow-ab.sh`:

| 4K randwrite QD32×4 | IOPS | reactor busy (max node) |
|---|---|---|
| A fresh | 13,606 [11,952–15,260] | 54% |
| B right after one `bdev_lvol_snapshot` on its 3 replicas | **275 [99–451]** | 88% |
| C after rewriting every cluster | 10,510 [9,927–11,093] | 45% |

One variable per step, same volume: the snapshot alone collapses it ~50×, the
reactors go busy copying, and a full rewrite restores it. (Kind cannot measure
r3 latency — its replica legs share one kernel, so QD1 writes were ~10 ms even
in A; use EC2 for that.)

## 4. What not to do

**Do not ship "epochs off" as the fix.** The chart's own note: running
`numReplicas >= 2` without the orchestrators is "actively hazardous" — without
epoch history the raid reassembly admission is attach-everything, so a leg
that truly diverged would be re-admitted as an equal read source. Equally, the
benchmark must run Flint as shipped; an epochs-off arm is a diagnostic only.

## 5. Fix (DECIDED 2026-10-08; not implemented)

**Decision: keep the epochs and make their cost small.** The periodic cut
is insurance paid in advance — a point known equal on every replica
*before* one fails, so a returning replica reverts to its own copy of it and
receives only the delta, with no SPDK on-disk format change and no tracker
that dies with the roaming raid (`incremental-replica-rebuild.md` §2, §7,
§8). The design priced that insurance in space only; its write-path cost
(this finding) was never measured. The alternative — no snapshots while
healthy, a persisted mark-before-write change map on each replica — is
copy-free and would reach parity on continuous random writes, but is the
format change the design rejected plus a rewrite of catch-up, hot rejoin and
reassembly admission with new proofs. Rejected for now; revisit only if the
measured steady state after this section is unacceptable for a workload
that matters.

Three changes in a fixed order, plus the epoch interval default, which is
part of the decision (raise it; exact value set with the rebuild-delta
trade-off stated). The order is forced by the facts in §5.1: at 1 MiB
clusters, the "parallel copies" and "skip the copy" changes do nothing.

### 5.1 Facts the design rests on (verified in SPDK v26.05 / the Flint tree)

- **No write reaching the blobstore is larger than 128 KiB.** Flint's nvmf
  target uses the TCP transport defaults (`docker/Dockerfile.spdk` sets only
  `trtype`): `max_io_size` = 131072 (`lib/nvmf/tcp.c:45`) → MDTS 128 KiB
  (`lib/nvmf/ctrlr.c:3424`), which the kernel initiator never exceeds. So at
  1 MiB clusters no write ever covers a cluster. RAID1 advertises no optimal
  I/O boundary (`module/bdev/raid/raid1.c` sets none → noiob 0,
  `lib/nvmf/ctrlr_bdev.c:215`), so the kernel's 128 KiB pieces are
  cluster-aligned only where ext4 happened to place the file's blocks; the
  lvol bdev then splits them at cluster boundaries (`vbdev_lvol.c:1196`).
- **One copy at a time per lvstore per thread, not per volume.** The
  `need_cluster_alloc` queue lives on the blobstore channel, and the lvol
  bdev's channel is the lvstore's (`lib/lvol/lvol.c:1642`). With one reactor,
  every volume on a node's disk queues behind every other volume's copies.
- **At 1 MiB the copy is disk-bound even when serialized.** 1.75 ms per copy
  = 1 MiB at the i4i's read cap (0.74 ms) + 1 MiB at its write cap
  (0.94 ms). One copy saturates the device; overlapping copies cannot go
  faster until copies are smaller.
- **Same-extent-page metadata writes are already ordered.** SPDK `453faf015`
  (2025-11-04, in v26.05) queues cluster ops per blob by extent table id
  (`blobstore.c:8915-8954`, covering `blob_insert_cluster_on_md_thread`).
  Parallel copies need no guard of their own for this.
- **The lvstore reserves one 4 KiB md page per cluster**
  (`lib/lvol/lvol.c:621,670`, ratio 100), and an unclean shutdown replays the
  whole region (`blobstore.c:5074` — the scan the carried
  `blob-recovery-batched.patch` speeds up: 893,592 pages in 4.6 s,
  `attach-detach-campaign-2026-07.md:2002`). Anything that multiplies the
  cluster count multiplies crash-recovery time unless the ratio is lowered.
- **Flint has one 1 MiB constant** (`minimal_disk_service.rs:230`). Volume
  sizes round to MiB (`:337,453`, a multiple of any smaller power-of-two
  cluster); hot rejoin reads `cluster_size` from the lvstore at runtime
  (`hot_rejoin.rs:1181`); `clear_head_sb` works at the raid level; the
  shallow-copy stall detector counts clusters (finer clusters make it more
  sensitive, not less).
- Longhorn v2 uses 1 MiB clusters too
  (`longhorn-spdk-engine/pkg/spdk/disk.go:34`): same primitive, same copy
  cost, but a snapshot exists there only during a rebuild or when a user
  takes one.

### 5.2 The changes

| # | Change | Effect | Cost / what to guard |
|---|---|---|---|
| 1 | **128 KiB lvstore clusters** (`minimal_disk_service.rs:230`), **with `num_md_pages_per_cluster_ratio` lowered** on `bdev_lvol_create_lvstore` (`vbdev_lvol_rpc.c:102`) so the md region stays the size it is today (the need is ~1 page per 512 clusters, not 1 per cluster) | copy per 4 KiB write 8× smaller; makes 2 and 3 effective | without the ratio: md region 7 → 57 GiB on a 1.875 TB disk (3%) and ~8× longer crash recovery. Per-blob cluster array 8× (6.5 MB per 100 GiB blob, heap). Existing lvstores keep 1 MiB (nothing is deployed). Then try 64 KiB. |
| 2 | **Parallel copies** in `bs_allocate_and_copy_cluster`: wait per cluster instead of the per-channel FIFO; a copy buffer and md page per in-flight copy (today one `new_cluster_page` per channel, `blobstore.c:3696`); cap in flight (~32) | copies overlap up to the disk's bandwidth | SPDK patch, upstreamable. Metadata ordering is already handled (`453faf015`). Memory: cap × (cluster + 4 KiB). |
| 3 | **Full-cluster writes skip the copy**: write the user data into the new cluster, then insert; on `-EEXIST` re-execute against the winner's cluster as today | aligned 128 KiB writes cost no read | only after `filefrag -v` on a bench file shows the aligned fraction; needs clusters ≤ 128 KiB (§5.1). Alternative: raise `max_io_size` to 1 MiB — the TCP shared-buffer pool scales with it. |
| 4 | **Raise the epoch interval default** (`epochIntervalSecs`, 300 → 30–60 min; value to set) | the only lever on the *average*: the tax per epoch is working set × copy cost, so 30–60 min cuts it 6–12× | rebuild delta up to T_snap of writes (still bounded); K·T_snap retention and the space each epoch pins grow the same way. Part of the decision above. |

### 5.3 What to expect (estimates — to be measured, not promised)

| r3, after an epoch cut | today | after 1 + 2 |
|---|---|---|
| 4K randwrite, copy-bound phase | 572 IOPS | ~5–8k per replica (disk bandwidth ÷ 256 KiB of traffic per write) |
| 4K randwrite, 120 s window on the 90 GiB file, 300 s epochs | 572 | ~10k average, varying with where in the epoch the window lands (the r3 mixed spread 2,278–13,653 in §1 is this effect already) |
| 1M / 128K seq write | 320 MiB/s | ~8× today; near the disk cap only with 3 and aligned extents |

Mayastor and Longhorn do not copy on the normal write path and should land
around 50k+ on that test. These changes take Flint from ~190× to ~5–10×
behind; the epoch interval moves the average further. Parity needs no copy
on the normal path at all — a dirty bitmap kept by the healthy side (mark
before write), snapshots cut only when a rebuild needs them — which
rewrites catch-up, hot rejoin, reassembly admission and the §11 lineage
rules. Not chosen; revisit only against measured numbers.

### 5.4 Validation

1. SPDK unit tests for 2 and 3 (`test/unit/lib/blob`), including a forced
   `-EEXIST` on 3.
2. `bench/experiments/epoch-cow-ab.sh` on kind after each change
   (mechanism only — kind cannot measure r3 latency, §3).
3. The Phase 1 r3 matrix on EC2 (`bench/`, `phase2.sh` with
   `DRIVERS=flint`) as shipped, epochs on, with the harness logging each
   cut so results split into copy phase and steady state.
4. Crash-recovery time of a full lvstore before and after change 1 (the md
   ratio is what keeps it flat).
5. Fix the stale "Default-disabled" header in `epoch_scheduler.rs` on the way.
