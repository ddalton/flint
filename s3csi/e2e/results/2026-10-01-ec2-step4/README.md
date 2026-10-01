# flint-s3-csi on EC2 — §11 step 4: the placed block cache measured on the instance store (2026-10-01)

Steps 1–3 of the sharing design's §11 (`docs/plans/passthrough-read-only-mount-sharing.md`)
had run on kind only. This put the placement on a real node whose instance
store the cache could sit on, re-ran the two legs that only kind had seen,
and folded in the one campaign-3 leg that had never run green on real nodes.

- **cluster** `s3a` (trove project 167): 3 × i4i.large spot (control plane
  + 2 workers), us-west-1, Amazon Linux 2023, k8s 1.34.12; 8 GiB gp3 root
  (`nvme0n1`, every emptyDir lives there) and a 435.9 GiB instance store
  (`nvme1n1`, "Amazon EC2 NVMe Instance Storage") that nothing mounts.
  The drill mounted it (xfs) at `/mnt/nvme` on every node, finding it by
  model, not by device name, and made `/mnt/nvme/flint-s3-cache` there —
  standing in for the platform, which is whose job that is.
- **images** `dilipdalton/flint-s3-{csi,worker,worker-lean}:ec2-1001`,
  built on the box from `7ebde1a1` (= origin/main; the lean worker wraps
  the PUBLISHED `flint-sync:1.56.0` — lean/syncer still does not build at
  HEAD, `UNTRACKED_GRACE_SECS`).
- **store**: one real bucket in us-west-1 through the public endpoint — no
  VPC gateway endpoint this time, which is why the uncached path reads
  ~420 MiB/s here against campaign 3's ~600.
- **drill**: `aws-measure.sh` with `CACHE_HOST_PATH=/mnt/nvme/flint-s3-cache`
  (3 reps, 4 readers), then `run-legs.sh S24 S30`, with P8 from
  `aws-passthrough.sh` soaking on the second worker throughout
  (`SUITE=aws-passthrough.sh run-legs.sh P8`, new). The second worker was
  cordoned once the soak was placed: the legs' fixtures carry no nodeName
  and every node-scoped helper reads `$NODE`.

## M1 — the cache placed on the instance store: half the penalty, not none

Four parallel `cat`s of a set, cold (fresh worker) then warm (same
worker), three reps. Three arms: no cache; the 768 MiB cache on the
worker's scratch emptyDir (the campaign-3 control, which must reproduce);
the SAME CR with the chart's `workers.cacheHostPath` pointing at the
instance store. Beside the times: whether the tenant got the
`CacheOnRootDisk` note, and how much the cold read left in the worker's
directory on the device (`measure-20261001-152026.tsv`, `measure.log`).

| set | arm | cold (ms, 3 reps) | warm (ms, 3 reps) | note | on the device after the cold read |
|---|---|---|---|---|---|
| mid, 4 × 128 MiB (fits) | no cache | 1649 / 1659 / 1540 | 1409 / 1410 / 1259 | 0 | – |
| mid | cache 768 MiB, emptyDir (gp3 root) | 2939 / 3690 / 2029 | **689 / 950 / 639** | **1** | – |
| mid | cache 768 MiB, **placed** (instance store) | 1850 / 1879 / 1879 | **689 / 719 / 690** | **0** | 515 MiB |
| big, 6 × 1 GiB (8× the cache) | no cache | 15240 / 12939 / 13119 | 14560 / 13860 / 13230 | 0 | – |
| big | cache 768 MiB, emptyDir (gp3 root) | **49899 / 48809 / 49330** | **49149 / 49250 / 49170** | **1** | – |
| big | cache 768 MiB, **placed** (instance store) | **24629 / 23349 / 23140** | **23269 / 23160 / 23000** | **0** | 772 MiB |

The control reproduces campaign 3 to the second: 6 GiB through the
emptyDir cache is 49 s cold and warm, 6 GiB at the gp3 root's 125 MiB/s,
against 13–15 s uncached, and every such pod got the note. Placed, the
same set takes **23 s — half the penalty, still 1.7× slower than no
cache**, and the design's bar for this step ("within 10% of the no-cache
arm cold") is NOT met. The reason is the device, measured on the node
right after (`devprobe-s3a-aws-1.log`, `dd` direct I/O, 4 GiB):

| disk | write, 1 stream | write, 4 streams | read, 1 stream | read, 4 streams |
|---|---|---|---|---|
| instance store `nvme1n1` (i4i.large's one 435.9 GiB slice) | 299 MiB/s | 263 MiB/s | 390 MiB/s | 335 MiB/s |
| gp3 root `nvme0n1` (1 GiB) | 139 MiB/s | – | 125 MiB/s | – |
| S3 through the mounter, uncached (6 GiB, from the no-cache rows) | – | – | 410–470 MiB/s | – |

**An i4i.large's instance store is slower than S3 in both directions.**
6 GiB at its ~270 MiB/s write rate is 23 s, the measured number; a warm
read of a set larger than the cache misses everything and pays it again.
The 512 MiB set's placed warm read at 0.69 s is 740 MiB/s — faster than
the device reads, so that is the page cache (the blocks were just
written), the same win the emptyDir arm shows. §11's "with the placement
knob set that rule relaxes to 'the device is at least as fast as the
node's S3 path' — on NVMe, always" was wrong for the smallest i4i: the
instance-store slice scales with the instance size, and this one does
not out-write S3. The sizing rule — a cache only for a working set that
fits in it — stands placed or not; the placement makes a cache CHEAPER
to miss (2× instead of 5×), not free, and how much cheaper is the
device's write bandwidth, which the operator has to measure.

What the placement mechanics did on real nodes, as the rows record:
every placed pod's directory was `<root>/<worker-name>` on the instance
store (the plugin logged `cache_device=259:0 nodefs_device=259:2`; the
emptyDir arm logged `259:2` twice), no placed pod got the note, the
emptyDir arm's every pod did, and the root was empty after each arm —
every directory went with its worker. The chart rolled onto all three
nodes with the placement set, so every plugin pod found the directory.

## M2 — one shared mounter under four readers of 6 GiB

`spec.sharing.readOnly` CR, four reader pods pinned to the node, each
`cat`ting all six 1 GiB objects; the worker's cgroup `memory.peak` and
whether it was OOM-killed.

| arm | `--memory-target` | worker limit | cache | readers | slowest reader | worker peak | terminated |
|---|---|---|---|---|---|---|---|
| no cache (control, added after the first pass) | 682 MiB | 1Gi | none | 4/4 | **43 s** | **597 MiB** | no |
| fix (two thirds of the limit) | 682 MiB | 1Gi | 768 MiB, emptyDir | 4/4 | 97 s | 1029 MiB | no |
| old (mount-s3's 95% default) | 973 MiB | 1Gi | 768 MiB, emptyDir | 4/4 | **164 s** | 1032 MiB | no |
| `workers.sharedResources` 4Gi | 2730 MiB | 4Gi | 768 MiB, emptyDir | 4/4 | 115 s | 3214 MiB | no |
| fix, cache **placed** | 682 MiB | 1Gi | 768 MiB, instance store | 4/4 | **60 s** | 1034 MiB | no |

Campaign 3's shape again (98 / 185 / 97 s): the 95% target is 1.7×
slower at the same limit, 4Gi buys nothing, nothing is killed. Placing
the cache took the shared mounter from 97 s to 60 s under the same four
readers. The control the first pass lacked says the rest
(`measure-nocache-control.log`, run after it with the chart plain; the
mounter's argv checked live: `--read-only --memory-target 682`, no
`--cache`, `/tmp` empty): with no cache at all the same four readers
finish in **43 s** and the worker peaks at 597 MiB. Under concurrent
readers of a set larger than the cache, the cache costs 2.3× on the
emptyDir and 1.4× placed, the same shape as M1; and the 1029–1034 MiB
peaks of every cache arm — the cgroup ceiling — are the cache's page
charge, not the mounter's need.

The M2 rows' `note` column in the TSV reads 0 on every arm: the count was
taken seconds after Ready and the event recorder had not caught up — the
events were there a moment later (all four members of each emptyDir arm
at 15:37:39 and 15:39:27; `kubectl get events --field-selector
reason=CacheOnRootDisk`). The script now counts after the reads, as M1
does.

## S24 + S30 on real nodes

`run-legs.sh S24 S30` against the real-S3 rig, with the second worker
cordoned: **52 ok, 0 bad** (`legs-S24-S30.log`), the same tally as the
kind run of the step 3 images. What real nodes add to what kind showed:

- S24: a sharing CR that names no cache runs its shared mounter with no
  `--cache` and no cache directory after the reads, logged, and no note;
  its control (shared-c's class recreated against the CR with a 256 MiB
  cache) gets the flags, a populated directory, the note naming the CR,
  the size and the emptyDir, and the device line.
- S30: with `workers.cacheHostPath=/var/lib/flint-s3-cache-s30` — a
  directory on the node's ROOT disk, by design — the plugin mounted the
  root as a type-Directory hostPath on all three nodes, the startup sweep
  removed `s3w-orphan` and left `planted` alone, the sharing CR without a
  cache got the chart's 512 MiB in a 0700 hostPath under the root with no
  emptyDir, and the note fired BY THE DEVICES (`259:2` twice) naming the
  directory and the size — the placed shape of the note, which the M1
  placement on the instance store never produced. A per-pod CR without a
  cache kept its emptyDir; one naming 128 MiB was placed at its own
  ceiling; both directories went with their mounters; a leftover of the
  same name was emptied before the next worker; a root absent on the
  nodes failed the plugin pod with `hostPath type check failed` in 30 s;
  the chart refused a relative path and a zero size at render time.

## P8 — the rotation soak, and an oracle that was wrong for the rig

The rotation soak rewritten after campaign 3 (a reader on the second
worker reads every 5 s for 30 min; rotations counted where they land, in
the soak worker's `creds.json`) had never run on a real cluster. Run here
alongside M1, from `aws-passthrough.sh` on its own through the new
`SUITE=` knob of `run-legs.sh` (`p8-first-pass.log`):

| | reads | errors | the rotation count | broker issued | verdict |
|---|---|---|---|---|---|
| first pass, counting distinct `AccessKeyId`s | 360 / 1829 s | 0 | **1 key** | 14 → 273 | BAD |
| second pass, counting distinct `Expiration`s | 360 / 1829 s | 0 | **25 expirations** (1 key) | 15 → 189 | 3 ok, 0 bad |

The first verdict was the drill's, not the product's: the rig's broker is
`backend=static`, which hands out ONE key set and mints a synthetic
`Expiration` per exchange so clients refresh (`broker.rs`). A count of
distinct keys reads 1 there by construction, while the broker's counter
said the worker had exchanged credentials over two hundred times. The
sampler now records the expiration beside the key, the oracle is ≥10
distinct expirations — fresh from every exchange on every backend, so a
stalled rotation still reads 1 — and the key count is a note (1 under
static; more only when the backend mints). Checked live before the
second collect: the worker's file carried the static key and an
expiration 108 s out. The second pass ran with the M2 no-cache control on
the other worker (`p8.log`).

## Teardown

Both drivers ended with the rig's own teardown (helm uninstall, workers
reaped, the node sleepers down). Trove project 167 was deleted at
16:31:52Z and the three instances read `terminated` by 16:34:15Z; the
bucket was purged and deleted and the IAM user's key, policy and user
deleted between 16:31:53Z and 16:32:02Z. Verified 16:34:24Z
(`teardown-verify.log`; rolesanywhere for EC2, trove-admin for S3 and
IAM): 0 non-terminated instances in us-west-1 and in us-east-1, 0
volumes, 0 open or active spot requests, 0 elastic IPs, 0 `trove-s3a*`
and 0 non-default security groups, trove orphans matched 0 with no ghost
rows, 0 buckets in the account, the IAM user NoSuchEntity. No KMS key
and no VPC endpoint were made this time. The cluster lived 15:10Z to
16:34Z — about $0.17 of spot compute at $0.04 per node-hour, plus cents
of S3 requests and gp3.

One trap in the teardown script itself, kept here because it is the
third time this shape has appeared: its instance wait filtered on a
`Name=s3a*` tag trove does not set, read zero at once, and the script
printed "verified" while the unfiltered count still said 3 instances, 3
volumes and the cluster's security group. The verification above is the
unfiltered one, taken after the instances had left.

## Files

- `measure-20261001-152026.tsv`, `measure.log` — M1 and M2 as the script wrote them
- `devprobe-s3a-aws-1.log` — the two disks' raw bandwidth on the measured node
- `legs-S24-S30.log` — the two cache legs on real nodes
- `p8-first-pass.log`, `p8.log` — the soak with the key-counting oracle, then with the expiration-counting one
- `teardown-verify.log` — the zero set, after the instances left
- `measure-nocache-control-20261001-160103.tsv`, `measure-nocache-control.log` — the M2 no-cache control arm, run after the first pass
