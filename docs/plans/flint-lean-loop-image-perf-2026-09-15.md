# flint-lean — what the loop-mounted tree image costs, measured on EC2

Date: 2026-09-15. Status: **MEASURED, decision open.** Nothing in the
product is changed by this document.

## The question

A lean workspace under the CSI driver keeps its tree on the node's root
filesystem. With the chart's default `node.quota: true` the tree is a
sparse ext4 image loop-mounted at that path (`s3csi/quota.rs`), so a
workspace's `sizeLimitGib` (default 20) is an `ENOSPC` in the tenant's own
write. The loop device is set up by `mount -o loop,noatime`, which leaves
**direct I/O off**: every byte is page-cached in the image's filesystem and
again in the backing file. A 2026-09-13 investigation on a Lima VM found
buffered sequential writes 0.15-0.33x of plain ext4 and asked whether to
drop the image. Its result files were lost; this is the measurement on a
real node.

## Rig and method

- **Node:** trove cluster `acc`, worker `acc-aws-2` (i4i.large spot, 2 vCPU,
  15 GiB, kernel 6.18, AL2023).
- **Tool:** `lean/e2e/perf/loop-tree-bench.sh`, run as root on the node;
  output in `lean/e2e/perf/results-loop-tree-bench-acc-aws-2.out`, summarized
  by `loop_tree_summary.py`.
- **Two backings:**
  - `root` — the node's root filesystem, where the plugin keeps trees as
    deployed: XFS on an 8 GiB EBS volume.
  - `nvme` — a scratch ext4 on the instance's local NVMe, so a disk cap cannot
    flatten the comparison.
- **Three layouts on each backing:**
  - `plain`: a directory, what `sizeLimitGib: 0` gives.
  - `loop`: today's image, mounted as quota.rs mounts it.
  - `loopdio`: the same image on a loop device made with `losetup --direct-io=on`.
- **Controls, asserted before each layout measured:** plain is not a mount
  point; loop's device reports `dio=0`; loopdio's reports `dio=1`. Before
  measuring, each image was left until its allocated size stopped changing.
- **Workloads:** 5 repetitions each, caches dropped before every run, wall
  clock around the whole operation. File sizes: 256 MiB / 5,000 files on root
  (disk pressure), 2 GiB / 20,000 files on NVMe.

## Results

Median (min–max), higher is better; the ratio is the median against plain's.

**Root filesystem (EBS, as deployed):**

| workload | plain | loop, dio off (today) | loop, dio on |
|---|---|---|---|
| sequential write, buffered (MiB/s) | 798 (783–810) | 715 (655–753) · **0.90** | 753 (731–769) · 0.94 |
| sequential write + fsync (MiB/s) | 186 (138–186) | 129 (99–176) · 0.70 | 179 (143–184) · 0.96 |
| cold sequential read (MiB/s) | 131 (123–132) | 131 (95–133) · 1.00 | 130 (125–132) · 0.99 |
| small files, tmp+rename, one sync (files/s) | 11,390 (9,980–11,601) | 9,560 (7,669–10,183) · 0.84 | 10,246 (6,720–11,038) · 0.90 |
| 4 KiB random writes, fsync each (IOPS) | 563 (557–564) | **328 (266–329) · 0.58** | **246 (221–269) · 0.44** |

**Local NVMe (no disk cap):**

| workload | plain | loop, dio off (today) | loop, dio on |
|---|---|---|---|
| sequential write, buffered (MiB/s) | 2,713 (2,684–2,794) | **1,533 (881–1,614) · 0.57** | 2,695 (2,450–2,817) · 0.99 |
| sequential write + fsync (MiB/s) | 283 (255–304) | 273 (266–298) · 0.97 | 297 (288–306) · 1.05 |
| cold sequential read (MiB/s) | 390 (387–439) | 388 (375–430) · 0.99 | 388 (365–389) · 0.99 |
| small files, tmp+rename, one sync (files/s) | 14,749 (12,092–14,959) | 12,461 (12,173–13,063) · 0.84 | 14,482 (13,918–14,848) · 0.98 |
| 4 KiB random writes, fsync each (IOPS) | 25,067 (25,009–25,256) | **14,941 (13,813–16,129) · 0.60** | **14,390 (13,599–16,522) · 0.57** |

A fresh 20 GiB image wrote nothing to its backing file in its first 60 s
(allocated size unchanged at 4,428 KiB on both backings): ext4's lazy inode
table init did not show up as a cost here.

## What it shows

1. **Direct I/O off costs the buffered write path, and direct I/O on
   recovers it.** On NVMe a buffered sequential write runs at 0.57x of plain
   through today's loop and 0.99x with direct I/O; small files 0.84x → 0.98x.
   On the EBS root the disk hides most of it (0.90x, 0.84x), and direct I/O
   brings it to 0.94x and 0.90x.
2. **Every loop layout loses about 40% of fsync-heavy throughput, and direct
   I/O does not help.** 4 KiB writes with an fsync each: 0.58–0.60x with
   direct I/O off, 0.44–0.57x with it on, on both backings, with ranges that
   do not overlap plain's. Each fsync commits the image's ext4 journal and
   then flushes the backing file on the node's filesystem: two journals per
   fsync. Git, package managers and databases are this shape.
3. **Reads are not affected**, and fsync'd sequential writes are within noise
   once the device is not the limit.

So the fix the 2026-09-13 note proposed, direct I/O on, is half a fix: it
gives back the buffered and small-file cost and keeps the fsync cost. Only a
plain directory is at native speed for every shape.

## Options

| | speed | a runaway tree | notes |
|---|---|---|---|
| **a. plain directory** (`node.quota: false` by default, or `sizeLimitGib: 0`) | native, all shapes | fills the node's root disk; `sizeLimitGib` enforced by nothing | kubelet's ephemeral-storage limit does NOT count the tree (a hostPath; measured 2026-09-13), and an emptyDir tree is deleted at the worker's termination under a live bind |
| **b. plain directory + filesystem project quota** | native, all shapes | `EDQUOT` at the ceiling | needs the node filesystem mounted with project quotas; AL2023's root XFS is not by default (`rootflags=prjquota`), so a node-image change; not measured here |
| **c. keep the image, turn direct I/O on** | ~native buffered and small files; fsync-heavy ~0.45–0.57x | `ENOSPC`, as today | one `losetup --direct-io=on` in quota.rs; preserved undrained trees unchanged |
| d. today | 0.57x buffered (NVMe), 0.58–0.60x fsync-heavy | `ENOSPC` | |

The choice is between the ceiling and the fsync shape; this measurement cannot
make it. If agents mostly build, edit and publish files, c recovers most of
the loss at no risk. If they run git or databases in the tree, only a or b is
at native speed, and a gives up the ceiling.
