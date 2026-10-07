# 2026-10-07 kind dry run — HARNESS VALIDATION, NOT RESULTS

Flint 1.58.0 (published `dilipdalton/flint-driver:1.58.0`, chart and
`tests/system/kind-spdk.sh` from tag `v1.58.0`) on a 3-node kind cluster
on the Linux build box: one LVM volume per node on one NVMe, one shared
kernel, loopback networking, SPDK in kind mode (small pools), and TLAPS
proof runs in the background at nice 19. Settings: 20 GiB volume, 16 GiB
file, 3 s ramp, 10 s measured, 2 repetitions.

**Do not quote these numbers.** They show the harness works: fio JSON,
per-node CPU samples, the idle floor and the report all came out for
every test.

Observations to check on real nodes:
- 3-replica writes were far slower than 1-replica (QD1 4K randwrite: 82
  vs 10,885 IOPS; 1M seq write 43 vs 977 MiB/s). Kind routes every
  replica leg through one kernel's loopback, so this may be the rig.
  Phase 1 on EC2 settles it.
- SPDK used ~0.33 cores per node whether idle or loaded (the kind-mode
  target); node busy cores are the whole host (shared /proc/stat).
- A first attempt hung in NodeStage (`blkid` in D state on the new
  NVMe-oF device for 9+ min) on a cluster where interrupted Chainsaw
  suites had left 8 Flint kernel NVMe controllers stuck "connecting".
  After deleting the cluster and those controllers, a fresh cluster
  mounted at once. Not reproduced; noted, not chased.

`dry-summary.md` is `report.py dry-flint-r1 dry-flint-r3`.
