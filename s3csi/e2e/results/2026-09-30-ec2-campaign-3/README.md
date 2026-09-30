# flint-s3-csi on EC2, campaign 3 — 2026-09-30

The passthrough review's five fixes (`5926334a`) and the read-only mount
sharing (`8e025b46`) had been verified on kind on the box only. This
campaign put the whole single suite, the two measurements the box could
not make, and the fourteen real-node legs on a trove cluster:

- **cluster** `s3a`: 3 × i4i.large spot (control plane + 2 workers),
  us-west-1, Amazon Linux 2023, kernel 6.18, containerd 2.2, k8s 1.34.12,
  8 GiB gp3 root (every emptyDir lives there), no `crictl`, no `tc`.
- **images** `dilipdalton/flint-s3-{csi,worker,worker-lean}:ec2-0930`,
  built on the box from `b859b38d`; the lean worker wraps the PUBLISHED
  `flint-sync:1.56.0` (lean/syncer does not build at HEAD).
- **stores**: RustFS in-cluster for the single suite (MinIO's public
  images are gone since 09-29); the real buckets only for the S3 phase.
- **the second worker was cordoned** for the single suite: every
  node-scoped helper targets `$NODE`, and run 1 died at S9 when the
  reader was recreated on the other node (the 09-04 campaign had one
  worker). Uncordoned before the real-node legs.

## The single suite — five runs to one green drill

| run | drill | tally | what it found |
|---|---|---|---|
| 1 (`single-rustfs-run1-stopped-at-S9.log`) | `b859b38d` | stopped at S9 | the reader landed on the second worker; the kill hit nothing |
| 2 | — | never ran | setup applied into namespaces still Terminating (Forbidden), then hung |
| 3 (`single-rustfs-run3.log`) | `b859b38d` | **223 ok, 17 bad** | S14 7, S20 5, S21 1, S24 2, S25 1, S17 1 — none the product (below) |
| legs (`relegs-S11-S14-S20-S21-S24-S25.log`) | rewritten | 100 ok, 1 bad | the S20 tree.img read (fixed) |
| 4 (`single-rustfs-run4-fixed-drill.log`) | rewritten | **258 ok, 4 bad** | S17 inconclusive, S23 no `tc`, S27 vacuous ×2 — rig, all three (below) |
| legs (`relegs-S23-S27.log`) | `acd23c8c` | **18 ok, 0 bad** | |

### S14 / S20 / S21 were written for a protocol the product no longer has

S14 (2026-09-03) froze the lean holder with SIGSTOP and applied a second
pod, expecting it to wait out the lease and rotate the manifest. That is
the LIFETIME lease. Since the per-barrier publish fence (`b50c2faf`,
2026-09-13) the cell is claimed only inside a commit section — startup
runs `verify_claim` and `release_stale_own` and claims nothing — so the
freeze froze nothing: the cell S14 read (`lean-a5b35562…`, epoch 3,
released) was the S13 drain's leftover, both pods' incarnations
(`incarnation.json`: epoch 0, never claimed) were strangers to it, and
every observation was of an inert cell. S20 rode on S14's fence, S21 on
a renewal between barriers. The 09-17 kind run had already recorded S14
as UNKNOWN. RustFS was not the cause: `conformance.json` read
`conditional_put: true, conditional_delete: true`.

The rewrite stages each verdict of the claim step by WRITING the cell
(`lcell_stage`, the S3 backend's own `EpochBody`) and observes it through
one triggered publish (`lpublish`, S12's sentinel). Measured on s3a:

| cell staged | ack latency | epoch | manifest seq | holder after |
|---|---|---|---|---|
| released, reserved for nobody | 3 s | 13 → 14 | 3 → 4 | this pod |
| HELD by a stranger, never renewed | 63 s | 23 → 24 | 4 → 6 (rotation, then install) | this pod |
| released, reserved for a stranger | 22 s | 33 → 34 | 6 → 7 | this pod |
| HELD by this pod's previous container | released at the restart, then 3 s | 43, 44 | 7 → 8 | this pod |

S20 stages the Succeeded worker the way it now happens (the syncer exits
0 on SIGTERM under a mounted tenant), sees it relaunched by the next
republish (60 s, `SyncerRecreated`), then deletes the tenant against a
fence a LIVE stranger holds (renewed every 5 s from the rig): the delete
stood 83 s, kubelet killed the worker at the derived grace (141 s from a
60 s floor), nothing attested, the tree was preserved with the uncited
file inside (`DrainNotAttested`). With `workers.quota=true` the tree is
an ext4 image that preservation unmounts, so the leg reads it through a
loop mount.

### The other rig holes a three-node cluster opened

- **S24, S25, S27 read a plugin log that was not the publisher's.** The
  DaemonSet is rolled by S9, S17, S17f and every `chart_up`, one pod at a
  time across three nodes, and lines written at setup went with the pod.
  Each leg now recreates its pods in-leg.
- **S27 was vacuous in run 4**: run 3 had pulled `pull-test` onto the
  node, `image_absent` asked only Docker Hub, and the worker started from
  containerd's cache at once — the two log assertions caught it. The leg
  drops the node's reference with `ctr images rm` (one name; `crictl
  rmi` took the image with every tag, `b859b38d`).
- **S23 printed nothing in run 3** (its writer never came up; no else
  branch) and in run 4 could not shape egress: AL2023 ships no `tc`. It
  installs iproute-tc where dnf is; a missing writer is a BAD.
- **S17 goes inconclusive on a fast cluster**: the 4000-file checkout
  finishes before the plugin roll lands. It now says so as a NOTE; S17f
  is the deterministic form and passed.
- **S16's node drain recreates the RustFS pod** (emptyDir): the store is
  empty after the suite, so any re-run needs `teardown` + `setup`.
- **`setup` after `teardown`** applied into namespaces still
  Terminating; it now waits them out.

What passed on real nodes and had not before: S25–S29 (the five review
fixes) including the Docker Hub pull-test round trip in S27, S23 (F72's
order), S24 (sharing), S17f (adoption of a frozen checkout), S26
(kubelet's `OutOfmemory` refusal named on the tenant).

## The S3 phase

`aws-measure.sh 3 4` against the real bucket through a VPC gateway
endpoint, from a tenant on s3a-aws-1 (`measure-*.tsv`, `s3-measure.log`).

### M1 — the block cache AS DEPLOYED: a gp3 root disk is the ceiling

Four parallel `cat`s of a set, cold (fresh worker) then warm (same
worker), three reps; the cache arm is the sharing default (768 MiB) on
the node's emptyDir, which on these nodes is the 8 GiB gp3 root
(3000 IOPS, **125 MiB/s**; the 435 GB NVMe instance store is unmounted).

| set | arm | cold (ms, 3 reps) | warm (ms, 3 reps) |
|---|---|---|---|
| mid, 4 × 128 MiB (fits the cache) | no cache | 1139 / 1119 / 1110 | 960 / 940 / 940 |
| mid | cache 768 MiB | 3250 / 1210 / 3270 | **369 / 390 / 389** |
| big, 6 × 1 GiB (8× the cache) | no cache | 10360 / 9490 / 10079 | 10119 / 9059 / 9829 |
| big | cache 768 MiB | **48130 / 48750 / 48739** | **49399 / 49300 / 49789** |

Uncached, the node reads S3 at ~600 MB/s (6 GiB in 10 s). The set that
fits the cache warms 2.5× faster. The set that does not is **5× slower
with the cache than without, cold AND warm**: Mountpoint writes every
fetched block to the cache directory, the disk under it takes 125 MiB/s,
and 6 GiB / 125 MiB/s = 48 s — the measured number to the second. A
warm read of a set larger than the cache misses everything and pays the
disk again. This is the 09-12 door drill's blind spot: it ran on host
NVMe with a 60 GB cache. On a node whose emptyDir is a small gp3 root
the sharing default's cache is a regression for any working set larger
than it; the cache directory belongs on the instance store, or the
cache off for such sets (follow-up in the design doc).

### M2 — one shared mounter under four concurrent readers of 6 GiB

`spec.sharing.readOnly` CR, four reader pods pinned to the node, each
`cat`ting all six 1 GiB objects; the worker's cgroup `memory.peak` and
whether it was OOM-killed, at three mount-s3 memory targets.

| arm | `--memory-target` | worker limit | readers | slowest reader | worker peak | terminated |
|---|---|---|---|---|---|---|
| fix (two thirds of the limit, `5926334a`) | 682 MiB | 1Gi | 4/4 | 98 s | 1030 MiB | no |
| old (mount-s3's own 95% default, every mount before 09-30) | 973 MiB | 1Gi | 4/4 | **185 s** | 1038 MiB | no |
| `workers.sharedResources` 4Gi | 2730 MiB | 4Gi | 4/4 | 97 s | 3204 MiB | no |

No arm was OOM-killed: the peak sits at the cgroup ceiling in both 1Gi
arms because the page cache is charged to it, and reclaim held. What the
two-thirds target buys under this load is speed, not survival: the 95%
target that fix 1 replaced took 1.9× longer for the same reads at the
same limit — the mounter's own working set pressed against the ceiling
and reclaim thrashed. The 4Gi arm is no faster than 1Gi at two thirds,
so the ceiling here is the cache disk again (M1), not memory. The
sharing default's cache was on in every arm.

### The fourteen real-node legs (`aws-passthrough.sh`, `s3-passthrough.log`)

**51 ok, 1 bad, 1 skipped.** The three real buckets: the main one, an
SSE-KMS one, a us-east-1 one.

| leg | verdict |
|---|---|
| P1 throughput: 512 MiB written whole, read back byte-identical, both directions over 20 MiB/s | ok |
| P2 a 5000-object prefix lists complete, a sample reads correct | ok |
| P3 16 tenants on one node, 16 workers, all reclaimed | ok |
| P4 a tenant container restart keeps its mount and worker | ok |
| P5 a kubelet restart mid-read: mounts serve, the driver re-registers | ok |
| P6 a REAL reboot of the node: the tenant comes back mounted with a working worker, nothing orphaned | ok |
| P7 instance TERMINATION of the second worker: NotReady in 45 s, the Deployment tenant rescheduled and mounted on the other node in 79 s | ok |
| P8 rotation soak: 360 reads every 5 s over 1832 s, **zero errors** | ok |
| P8 ≥10 rotations | BAD — the broker's issued counter read 175 → 159 because P14 rolled the broker mid-soak and the counter is per pod; the leg now counts distinct keys in the soak worker's creds.json (not re-run) |
| P9 ambient identity | SKIP — trove pods have no instance/IRSA chain; the leg judges the platform's precondition and says so |
| P10 SSE-KMS bucket: the write lands encrypted with the bucket's key, reads back | ok |
| P11 a us-east-1 bucket mounted from us-west-1 | ok |
| P12 a VPC gateway endpoint for S3: the route is there, mounts keep serving through it (pre-existing, kept) | ok |
| P13 an S3 partition on the node: reads stall while the drop rules hold, resume within a minute | ok |
| P14 broker HA: two replicas rolled mid-read, zero read errors | ok |

The S3 rig template still named `quay.io/minio/mc:latest` (401 since
09-29, like every MinIO image); it now uses the Chainguard client like
the RustFS rig. The first S3 setup sat 30 minutes in ImagePullBackOff
before that was found.

## Ozone

`../2026-09-30-ozone-mount-s3/`: mount-s3 1.24.0 mounts Ozone 2.2.1's
s3g with no special flags; listings, reads, 48 MiB multipart writes with
default CRC32C checksums, deletes and the block cache all work. The CSI
path against Ozone was not run: the rig seeds and checks through `mc`,
which cannot list folders on Ozone.

## Resources

Every AWS resource created for this campaign was deleted at the end and
the deletion verified: trove project s3a deleted (3 instances terminated, no orphans, no volumes, no security groups; the VPC is the account's default), the 3 buckets purged and deleted, the IAM access key, policy and user deleted (NoSuchEntity), the KMS alias deleted and the key PendingDeletion for 2026-10-07, the VPC gateway endpoint gone (NotFound), the Docker Hub pull-test tag absent (404), the local key file removed; on the box the Ozone compose, the worktree, the branch and the ec2-0930 images removed. Verified 2026-09-30 ~20:50Z (`teardown-campaign-3.log` in the session scratchpad; the VERIFY block is reproduced in this commit's message)..
