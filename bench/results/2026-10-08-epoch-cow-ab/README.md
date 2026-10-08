# 2026-10-08 epoch copy-on-write A/B — CONFIRMED

**Question:** is Flint's r3 small-write collapse (Phase 1 on EC2: 572 IOPS vs
110,130 at r1) caused by the epoch snapshots? See
`../2026-10-07-ec2-phase1/README.md`, "Root cause of finding 1".

**Rig:** Linux build box, kind, 3 nodes on one NVMe (LVM), Flint 1.58.0 as
published (chart and `kind-spdk.sh` from the tag) with
`replication.orchestrators.enabled=false` -- no epoch scheduler, so the only
snapshot is the one the experiment cuts. One r3 volume (20 GiB, 16 GiB file),
2 repetitions × 30 s per test. `../../experiments/epoch-cow-ab.sh`.

**Arms, one variable each, same volume:**
- A: preconditioned, never snapshotted
- B: `bdev_lvol_snapshot` on all three replica lvols (the epoch scheduler's
  RPC; `snapshots.log`), then measured WITHOUT rewriting
- C: one full sequential rewrite of the file, then measured

| 4K randwrite QD32×4 | IOPS median [min–max] | reactor busy, busiest node |
|---|---|---|
| A fresh | 13,606 [11,952–15,260] | 54% |
| **B after one snapshot** | **275 [99–451]** | **88%** |
| C after rewriting every cluster | 10,510 [9,927–11,093] | 45% |

**Result: a snapshot alone collapses r3 random writes ~50×, with the reactors
busier (copying), and rewriting every cluster restores them.** B's wide
ranges are the mechanism too: rep 1 runs right after the snapshot, rep 2
after rep 1 already re-owned many clusters (1M seq write: 5 then 151 MiB/s).

**Kind is not a faithful rig for r3 latency.** Even in A, QD1 4K writes
were ~10 ms (99 IOPS) and 1M seq write 48 MiB/s: every replica leg runs
over loopback through one shared kernel. Kind r1 (no network leg) did
10,885 IOPS at QD1. Use EC2 for r3 latency; kind only for A/B of a
mechanism, as here.

`summary.md` is `report.py A-fresh B-after-snapshot C-rewritten`.
