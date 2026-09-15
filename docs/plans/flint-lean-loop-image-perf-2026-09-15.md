# flint-lean — what the loop-mounted tree image costs, measured on EC2

Date: 2026-09-15. Status: **DECIDED: a plain directory by default** (the
user chose option a; chart `workers.quota: false`, plugin default off,
`sizeLimitGib` advisory unless a node turns the quota on). §As deployed has
the check on the built image.

## The question

A lean workspace under the CSI driver keeps its tree on the node's root
filesystem. With the chart's default `workers.quota: true` the tree is a
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
| **a. plain directory** (`workers.quota: false` by default, or `sizeLimitGib: 0`) | native, all shapes | fills the node's root disk; `sizeLimitGib` enforced by nothing | kubelet's ephemeral-storage limit does NOT count the tree (a hostPath; measured 2026-09-13), and an emptyDir tree is deleted at the worker's termination under a live bind |
| **b. plain directory + filesystem project quota** | native, all shapes | `EDQUOT` at the ceiling | needs the node filesystem mounted with project quotas; AL2023's root XFS is not by default (`rootflags=prjquota`), so a node-image change; not measured here |
| **c. keep the image, turn direct I/O on** | ~native buffered and small files; fsync-heavy ~0.45–0.57x | `ENOSPC`, as today | one `losetup --direct-io=on` in quota.rs; preserved undrained trees unchanged |
| d. today | 0.57x buffered (NVMe), 0.58–0.60x fsync-heavy | `ENOSPC` | |

The choice is between the ceiling and the fsync shape; this measurement cannot
make it. If agents mostly build, edit and publish files, c recovers most of
the loss at no risk. If they run git or databases in the tree, only a or b is
at native speed, and a gives up the ceiling.

## As deployed, with the plain-directory default built (image `tree-plain`)

`lean/e2e/perf/tree-layout-drill.sh` installs the chart twice on the same
node: at its new default, and with `workers.quota=true`. It checks the host's
mount table for the tenant's volume, then runs `tree-bench-pod.py` inside a
restricted tenant pod (python:3.12-alpine, uid 1001), through the CSI bind
and the runtime's mount. Output: `results-tree-layout-drill-acc-aws-2.out`.

- **The default is a plain directory:** the host bind's source was
  `xfs /dev/nvme0n1p1` (the root disk); with the quota on it was
  `ext4 /dev/loop0`. The workspace checked out and the pod started in both arms (the bench does not publish; its `floorSecs` is an hour).

| workload (in the pod, root EBS) | plain (default) | loop (`workers.quota=true`) | loop/plain |
|---|---|---|---|
| sequential write, buffered (MiB/s) | 7,732 (7,219–7,898) | 4,148 (3,637–4,265) | **0.54** |
| sequential write + fsync (MiB/s) | 222 (218–237) | 220 (220–220) | 0.99 (EBS-capped) |
| small files, tmp+rename, one sync (files/s) | 8,261 (8,240–8,344) | 8,924 (8,892–9,059) | **1.08** |
| 4 KiB writes, fsync each (ops/s) | 545 (543–548) | 447 (265–474, n=4) | 0.82 |

The buffered path is the double page cache again, now at 256 MiB, which fits
in memory: 0.54x. Three things do not match the node-level run, and are
recorded rather than explained away:

- **Small files were 8% FASTER through the loop in the pod** (ranges do not
  overlap), against 0.84x at node level. The plain arm dropped more between
  node and pod (11,390 → 8,261) than the loop arm (9,560 → 8,924). One
  hypothesis, not tested: the loop device's backing-file writes are done by
  a kernel thread outside the pod's cgroup, so the pod's own writeback
  accounting sees less of them.
- **fsync-each writes through a loop ramp up; a plain directory is flat.** Per
  rep: node loop 267, 266, 329, 328, 329; node loopdio 221, 224, 246, 262,
  269; pod loop 265, 419, 474, 474; plain 557–564 and 543–548 throughout. The
  ratio against plain is ~0.47x on a fresh image and 0.58–0.87x later. It is
  not lazy inode-table init: an idle fresh image on the root XFS added no root-disk
  writes over 5 minutes, and its ext4 wrote nothing. A plausible cause, not
  tested: the first write to each region of the sparse backing file is also
  an XFS allocation. The node-level "settled" check watched allocated size
  and would not have seen either.
- **The loop arm's last rep was evicted:** the node crossed kubelet's
  ephemeral-storage threshold (835 MiB free against 1.28 GiB) and evicted the
  bench pod, then the broker. The image is sparse, but **a block written
  inside it stays allocated in the backing file after the tenant deletes the
  file**: nothing discards it. Ten 256 MiB write-and-delete cycles held
  their high-water mark on an 8 GiB root disk. "The image costs what is
  written" means what was ever written, not what is there now. This is a
  second reason, beside speed, not to default to the image on small root disks.

An NVMe lazy-init probe in between was void: the NVMe's own ext4 had just
been formatted, and its initialization wrote 7 GiB in 75 s under the
measurement.

