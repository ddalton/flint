# F74 — epoch snapshots collapse small writes on every replicated volume (~190× on EC2)

Status: **FOUND 2026-10-07 by the CSI benchmark (Phase 1 on EC2); CAUSE
CONFIRMED 2026-10-08 by A/B. NOT FIXED — the fix is a design choice (§5).**
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

## 5. Fix options (design call, not made)

| Option | Effect | Cost / risk |
|---|---|---|
| Smaller lvstore clusters (e.g. 64 KiB) | copy size 16× smaller | still one copy at a time; more metadata; existing lvstores keep 1 MiB (only new ones change) |
| Sparser epochs (`epochIntervalSecs`) | fewer collapses | each one is as bad; widens catch-up deltas |
| Dirty tracking without snapshots (a raid1 write-intent bitmap, md-style) | removes the copy-on-write entirely | the largest change: new SPDK patch or module, and the catch-up/hot-rejoin admission rules re-proved against it (the epoch history is what they rely on today) |
| Parallel cluster allocation in SPDK blobstore | removes the serialization | upstream SPDK change; the copy amplification stays |

Measuring any fix: `bench/experiments/epoch-cow-ab.sh` for the mechanism on
kind, then the Phase 1 r3 matrix on EC2 (`bench/`, `phase2.sh` with
`DRIVERS=flint`) as shipped, with epochs on.
