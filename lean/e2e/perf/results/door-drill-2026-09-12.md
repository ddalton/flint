# Four doors to the same bytes — the 2026-09-12 re-run

**Why a re-run.** The 2026-09-10 table (`door-drill-2026-09-10.md`) was
measured before the changes that drill motivated shipped: the ranged GET
became the default at 8 MiB, parallel multipart parts, the single-read
publish, the sharded fetch drivers with mimalloc, the syscall trims, and
a raw HTTP read path behind a flag. Every lean number there is now a
number about a binary nobody runs. This run puts the shipped syncer at
HEAD (`f7d44444`) beside the same three doors on the same shape of node.

**Cluster** runcu, 2 x i4i.large all-spot (control plane included),
us-west-1, one AZ. **Node** runcu-aws-1: 2 vCPU, 16 GiB, one 468 GB
instance-store NVMe at `/mnt/nvme` (xfs), kernel 6.18.44, only cilium
and kube-proxy beside the drill — the flint CSI stack and flux were
removed first. **Bucket** `flint-lean-door-20260912`, versioned, same
region (deleted at teardown). **Date** 2026-09-12/13. **n = 3**, arms
interleaved within each rep; controls once. **Every figure is a RANGE
over the reps, never a mean.** Drivers: `doors.sh` (the engines on the host)
and `csi-doors.sh` (the doors as deployed), both in this directory.

**The NIC caveat, stated before any number.** The EC2 API gives
i4i.large a guaranteed 0.781 Gbps (93 MiB/s) and a 10 Gbps burst. Every
large-object figure below runs inside the burst window: they compare
the doors to each other on this node, and none of them is a rate this
instance guarantees. A door's number here is not a number about a
bigger NIC.

**Two rigs, one node.** The first four tables measure the ENGINES: `flint-sync`
and `mount-s3` run on the host as bare binaries, which is where the variant
arms (raw reads, the 09-10 settings, fan-out 1, a cached mount) can be
compared and where the ceilings are. Table 5 measures the DOORS AS
DEPLOYED on the same node: the `flint-s3-csi` chart, the `s3.csi.chert.us`
node plugin, one worker pod per volume, and a restricted tenant pod that
reads and writes through the plugin's mount — a FlintPassthroughMount with
ambient identity, and a FlintLeanWorkspace whose checkout runs inside
NodePublishVolume. The 2026-09-10 passthrough figures came from the
deployed door — the plugin's worker pod's mount-s3, read through its
kubelet bind path from the host. Today's host figures come from a host-run
mount-s3 of the same version and flags, and the two differ by more than
the reps do (see §5 and §6). Every deployed number is the product a pod
gets.

Every arm reads THE SAME OBJECTS: lean publishes its tree at
`<prefix>/files/<path>`, so a mount-s3 mount of `ranged-drill/<w>/files/`
and an `aws s3 cp` of that prefix see byte-for-byte what a lean checkout
materialises. Same bucket, region, node, NIC. Only the door varies.

## The workloads

| name | shape | bytes | why |
|---|---|---|---|
| `big` | 6 x 1 GiB | 6,442,450,944 | one object is one stream — where ranging wins |
| `small` | 20,000 x 8 KiB | 163,840,000 | every object under the threshold — the request-rate door |
| `mixed` | 1 x 4 GiB + 2,000 x 16 KiB | 4,327,735,296 | the shape a real checkpoint tree has |

## The arms

Read, cold (page cache dropped before every leg), interleaved:

| arm | what it is |
|---|---|
| `L-ship` | `flint-sync checkout`, shipped defaults, no overrides |
| `L-raw` | the same with `FLINT_SYNC_RAW_READS=true` (off by default) |
| `L-0910` | fan-out 32, 512 MiB in flight, ranged at 8 MiB, 16 MiB x 4 — the 2026-09-10 "ranged" arm's settings on today's binary, so the binary's gains and the defaults' gains can be told apart |
| `L-slow` | fan-out 1, rep 1 only: the positive control for the fetch window |
| `P-32` | mount-s3, `--metadata-ttl minimal`, no cache, 32-wide `cat` — the 2026-09-10 passthrough door's flags and fan-out, on a host-run mount-s3 |
| `P-1` | the same 1-wide, rep 1 and `small` only: the fan-out control |
| `Pw-32` | the identical read again, nothing dropped |
| `PC-32` | mount-s3 `--cache <NVMe dir>` (metadata TTL 60 s), cold, cache dir wiped — the door as a deployment that cares about re-reads would configure it |
| `PCw-32` | the identical read again through the populated cache |
| `P-meta` | `find -type f` through the no-cache mount, no bytes |
| `S-32` | `aws s3 cp --recursive`, 32 concurrent requests |
| `ctl` | the drop_caches control: a local NVMe tree cold vs warm, once per rep |

Write, a fresh prefix per rep and arm, the seed tree on NVMe, page cache
dropped before each so every arm reads the seed cold:

| arm | what it is |
|---|---|
| `LW` | `flint-sync barrier` from the seed dir, shipped defaults |
| `SW` | `aws s3 cp --recursive` seed -> S3, 32 concurrent |
| `PW-32` | 32-wide `cp` of the seed into a mount-s3 mount of the fresh prefix |

Lean's wall clock includes its **commit**: every materialised file is
fsynced to stable storage before the baseline and marker that vouch for
it are written (`checkout.rs`, "at least as durable as its
description"). Neither `aws s3 cp` (no fsync) nor a mount (no disk)
pays that. The `fetch` column is the window without it.

## The guards

- **bytes / files** — every read arm reports the seeded totals; an arm
  that "wins" having read less has not won, and a door serving an empty
  tree reads as a blazing result. Dot-paths are excluded by one rule for
  every door (a lean checkout leaves `.flint-sync/` and a 300-byte
  `.flint/capabilities.json` beside the files — the shakedown's "7
  files, seeded 6").
- **ranged** — `L-ship` and `L-0910` report `ranged>0` on `big`/`mixed`
  and `ranged==0` on `small`: a threshold that never fires is a perfect
  null that reads like "no effect".
- **ctl** — local cold must exceed 2x warm, or drop_caches is not
  dropping and every "cold" figure is a warm read wearing a hat.
- **par** — `small`: `P-1` must exceed 2x `P-32`, or fan-out does not
  move this rig.
- **slow** — `small`: `L-slow` must exceed 2x `L-ship`. Not `big`: there
  every object is fetched as parallel ranges, so per-object fan-out has
  nothing left to add (the shakedown showed `L-slow` within 3% of
  `L-ship` on `big`, exactly as 2026-09-10 found).
- **write** — every write arm lands the seeded count and bytes under
  `<prefix>/files/` as S3 LISTS them, never as the arm reports. The
  first cut of this guard used `list-objects-v2 --query` and would have
  passed a tree 5% landed: the CLI prints one aggregate per 1000-key
  page.

## 1. Read: the four doors, cold

| workload | lean shipped (wall) | lean shipped (fetch only) | passthrough no-cache | passthrough `--cache`, cold | `aws s3 cp` 32-way |
|---|---|---|---|---|---|
| `big` | 24.09-25.00 s | 16.18-17.98 s | 9.13-9.83 s | 17.29-18.01 s | 21.50-22.44 s |
| `small` | 5.29-5.93 s | 4.55-5.18 s | 63.25-69.24 s | 32.11-34.91 s | 69.23-70.45 s |
| `mixed` | 19.58-19.95 s | 14.63-15.69 s | 15.02-15.43 s | 14.31-14.55 s | 21.58-22.77 s |

The lean variants, wall (fetch):

| workload | `L-ship` | `L-raw` | `L-0910` | `L-slow` (rep 1) |
|---|---|---|---|---|
| `big` | 24.09-25.00 s (16.18-17.98 s) | 24.86-24.94 s (16.73-17.91 s) | 24.86-24.92 s (16.90-17.77 s) | 25.09 s (n=1) (17.14 s (n=1)) |
| `small` | 5.29-5.93 s (4.55-5.18 s) | 4.64-4.98 s (3.97-4.34 s) | 15.90-16.86 s (15.32-16.23 s) | 252.17 s (n=1) (251.70 s (n=1)) |
| `mixed` | 19.58-19.95 s (14.63-15.69 s) | 19.15-21.18 s (13.30-17.32 s) | 18.73-19.23 s (13.14-13.54 s) | 43.21 s (n=1) (40.24 s (n=1)) |

Controls:

- **cache drop** — local NVMe 6 GiB tree cold 16.78-17.24 s vs warm 0.630-0.636 s (26x at the least). "Cold" means cold.
- **fan-out (passthrough)** — `small` 1-wide 1079.27 s (n=1) vs 32-wide 63.25-69.24 s.
- **fan-out (lean)** — `small` fan-out 1 252.17 s (n=1) vs shipped 5.29-5.93 s; on `big` 25.09 s (n=1) vs 24.09-25.00 s — ranges make per-object fan-out moot there, as 2026-09-10 found.

**What the ranges say.**

- **`small` is lean's door, by an order of magnitude.** 20,000 x 8 KiB
  through the shipped syncer: 5.29-5.93 s wall, 4.55-5.18 s of it
  fetching — 3,900-4,400 files/s. The host-run passthrough mount serves
  the same tree 32-wide in 63.2-69.2 s (290-320 files/s) and `aws s3 cp`
  at 32 concurrent in 69.2-70.5 s (280-290 files/s): **10.7-13.1x** and
  **11.7-13.3x**. The mount and the CLI land within 10% of each other;
  what separates the syncer from both is its sharded fetch drivers at the
  shipped fan-out — `L-0910` below is the same binary at the 09-10
  fan-out and window, and it is 3x slower.
- **`big` is passthrough's door, and lean's fetch is at the disk.** Six
  1 GiB objects stream through the mount in 9.13-9.83 s (625-675 MiB/s,
  nothing written) against lean's 24.9-25.0 s wall — **2.5-2.7x** — of
  which 16.5-18.0 s is the fetch (340-370 MiB/s) and 7.0-8.4 s is the
  commit. The drop_caches control read the same 6 GiB tree cold from
  this NVMe in 16.8-17.2 s: lean's fetch window on `big` lands on the
  disk's own time for the tree, so the door is not what bounds it here.
  `aws s3 cp` is 21.5-22.4 s, slower than the mount by 2.2-2.5x on the
  same objects.
- **`mixed` is a draw on the fetch and the commit decides it.** Lean's
  fetch is 14.6-15.7 s against the mount's 15.0-15.4 s wall (the ranges
  straddle); the 4.3-5.1 s commit puts lean's wall at 19.6-20.0 s,
  **1.27-1.33x** behind. `aws s3 cp`: 21.6-22.8 s.

**The variants.**

- `L-raw` (the raw HTTP read path, off by default) moves `small` by
  1.06-1.28x on the wall and nothing on `big`/`mixed` — the effect the
  path was measured to have, on a rig where the per-file cost is already
  low.
- `L-0910` (the 2026-09-10 arm's fan-out and window on today's binary) is
  **2.7-3.2x slower than the shipped defaults on `small`** and equal on
  `big`/`mixed`. That is the split the arm was built to make: on `small`
  the gain since 09-10 is the defaults — the sharded drivers and the
  fan-out — not the binary; on `big` the ranged path was already there
  and the binary's later changes did not move it.
- `PC-32` (mount-s3 with an NVMe `--cache`) is a trade the numbers state
  plainly: cold `big` costs **1.8-2.0x more** than the no-cache mount
  (17.3-18.0 s — the cache is written as it is read) while cold `small`
  costs **1.8-2.2x less** (32.1-34.9 s; the only other flag that changes
  with `--cache` is the metadata TTL, minimal to 60 s). Its re-read gains
  are in §2.

**The controls held.** Local cold vs warm 26-27x, so drop_caches drops.
`P-1` vs `P-32` on `small` 15.6-17.1x, so fan-out moves this rig.
`L-slow` vs `L-ship` on `small` 42-48x, so the fetch window is the
window; on `big` `L-slow` is 25.1 s against 24.9-25.0 s, as expected
where every object is already fetched as parallel ranges.

## 2. Re-read: what a warm second read costs at each door

| workload | passthrough no-cache: cold, then warm | passthrough `--cache`: cold, then warm | local tree warm (lean's re-read) |
|---|---|---|---|
| `big` | 9.13-9.83 s then 9.38-9.72 s | 17.29-18.01 s then 4.39-4.79 s | 0.630-0.636 s |
| `small` | 63.25-69.24 s then 64.66-67.39 s | 32.11-34.91 s then 11.65-11.86 s | — (not measured; a local tree) |
| `mixed` | 15.02-15.43 s then 15.04-15.50 s | 14.31-14.55 s then 3.77-3.82 s | — (not measured; a local tree) |

A warm second read through the no-cache mount costs what the cold one
did, on every workload: the ranges overlap. Mountpoint does not keep the
kernel's page cache across opens (no `FOPEN_KEEP_CACHE`; the 2026-09-10
run traced this), so a re-read is a re-stream. With `--cache` the second
read is served from NVMe: `big` 4.4-4.8 s (a **3.6-4.1x** gain over its
own cold read, **1.9-2.2x** over the no-cache mount), `small` 11.6-11.9 s
(1,700 files/s — still a FUSE round trip per file, now without the
network), `mixed` 3.8 s. Lean's re-read is a local directory: the same 6
GiB tree warm in 0.63-0.64 s, and cold from NVMe in 16.8-17.2 s (the
control), which is the floor a tree larger than RAM falls to.

## 3. Metadata surface

| workload | `find -type f` through the no-cache mount |
|---|---|
| `big` | 0.021-0.036 s |
| `small` | 1.693-1.824 s |
| `mixed` | 0.174-0.206 s |

`find -type f` with no bytes read: milliseconds on `big` and `mixed`,
1.69-1.82 s on `small` — 11,000-12,000 entries/s through the mount with
the minimal metadata TTL. A lean tree's listing is a local `find`.

## 4. Write: the same tree published through each door

| workload | lean barrier (`LW`) | `aws s3 cp` 32-way (`SW`) | 32-wide `cp` into a mount (`PW-32`) |
|---|---|---|---|
| `big` | 18.31-18.35 s | 18.77-19.09 s | 17.91-18.25 s |
| `small` | 19.50-20.35 s | 58.34-59.31 s | 1057.47-1071.86 s |
| `mixed` | 50.54-51.67 s | 16.89-17.09 s | 107.00-110.45 s |

**What the write ranges say.**

- **`small` write is lean's door by an even wider margin than the read.**
  The 20,000-file tree publishes through the barrier in 19.5-20.4 s,
  against 58.3-59.3 s for `aws s3 cp` and 1,057-1,072 s (about 18 minutes) for a
  32-wide `cp` into a mount-s3 mount. The mount is catastrophic here: it
  completes one PUT per file at close, so 20,000 closes serialize into
  52-55x the barrier's time. Publishing many small files is what
  lean's parallel upload lane is for.
- **`big` write is a wash — all three saturate the NIC.** Six 1 GiB
  objects publish in 17.9-19.1 s across the doors: the barrier
  uploads the six objects 32-wide, `aws s3 cp` uploads them 32-wide, and
  even the mount keeps six PUTs open at once. With six objects there is
  enough cross-object concurrency to fill the link, so no door is
  ahead.
- **`mixed` write is where lean is SLOWER, and the cause is the single
  large object.** The 4 GiB checkpoint plus 2,000 small files publishes
  through the barrier in 50.5-51.7 s against 16.9-17.1 s for
  `aws s3 cp` — 3.0x slower. The 2,000 small files upload
  32-wide and finish quickly; the critical path is the one 4 GiB object,
  and the shipped syncer uploads its multipart parts **serially**
  (`FLINT_SYNC_UPLOAD_PART_PARALLELISM` defaults to 1 — cross-object
  fan-out is 32, within-object is 1). A single upload stream cannot
  burst past roughly one connection's throughput, which is why
  `80-82 MiB/s` lands near the instance's guaranteed 93 MiB/s while
  `aws s3 cp`, which uploads that object's parts concurrently, bursts
  well past it. On `big` the same serialization is hidden because six
  objects already give the concurrency one object cannot; on the
  checkpoint shape — one dominant object — it is the whole story. §4a
  measures the fix.

### 4a. Upload part parallelism on the checkpoint shape

The `mixed` result above is the shipped default's one clear write
regression, and it is a single knob. This arm publishes `mixed` at
`FLINT_SYNC_UPLOAD_PART_PARALLELISM` of 1, 4, 8 and 16 on the same host
binary — one dimension moved, everything else the `mixed` seed and the
same fresh-prefix, cache-dropped, listing-checked discipline.

| `FLINT_SYNC_UPLOAD_PART_PARALLELISM` | `mixed` publish, n=3 | vs `aws s3 cp` 32-way (16.9-17.1 s) |
|---|---|---|
| 1 (the shipped default) | 50.7-58.6 s | 3.0-3.5x slower |
| 4 | 15.2-23.9 s | 1.4x slower, to 1.1x faster (noisy) |
| 8 | 13.8-14.4 s | **1.2x faster** |
| 16 | 14.4-15.3 s | 1.1x faster |

**The knob works, and the knee is 8.** Uploading the 4 GiB object's
parts 8-wide instead of one at a time takes the publish from 50.7-58.6 s
to 13.8-14.4 s — **3.6-4.2x** — and past the `aws s3 cp` figure that beat it
serially. 16 adds nothing over 8: the link is saturated at 8, the same
place the read side saturates (the `big` fetch ran at 340-370 MiB/s and a
single object at 82 MiB/s, so ~5 streams already fills the pipe). The
2,000 small files are unaffected by this knob — objects under
`whole_put_max` (64 MiB) are single PUTs with no parts to parallelize —
so the gain is entirely the one large object, which is the whole point:
the checkpoint shape is one dominant object.

**Why this is not a default change in this release.** The win is real
and the memory cost is real and they do not coincide. An upload part is
read whole into RAM before its PUT (`read_local` → a 64 MiB buffer), and
the upload window is bounded only by the object count
(`buffer_unordered(upload_fanout)`, default 32) — there is no
bytes-in-flight gate like the read path's `fetch_inflight_max_bytes`. So
peak upload RSS is `min(large-objects, 32) x part_parallelism x 64 MiB`.
On the checkpoint shape (one large object) at 8-wide that is ~512 MiB,
comfortable. But a tree with many large objects publishing at once would
be `32 x 8 x 64 MiB = 16 GiB` on a 16 GiB node — an OOM, on a workload
this drill did not measure. A default must be safe for every shape, and
the shipped default of 1 already sits behind an unbounded upload window
(2 GiB worst case at 64 MiB parts); multiplying that by 8 without first
adding a byte bound trades a checkpoint win for an OOM on a tree that
merely holds many large files.

**The shippable outcome — the knob is now an operator field.** The
knob lived in `flint-sync` (`FLINT_SYNC_UPLOAD_PART_PARALLELISM`, default
1) but nothing set it: the lean operator stamps a FIXED env list from the
`FlintLeanWorkspace` spec (`sync_env.rs`) and the knob was not in it, and
there was no CR field. So every deployment ran serial parts, with no way
to say otherwise. `2c160a2f` adds the small, additive plumbing: the spec
gains `uploadPartParallelism` (default 1), the node plugin stamps it as
`FLINT_SYNC_UPLOAD_PART_PARALLELISM`, and the chart CRD carries the
measurement and the memory warning in its field doc. It is opt-in and
default-off, so nothing changes unless a deployment that knows its
checkpoint shape and its pod memory raises it (8 needs ~512 MiB of
headroom per concurrent large object). Ships in v1.51.0. The default
change itself is the fast-follow: add a bytes-in-flight bound to the
upload path, mirroring the read path's, then raise the default to 8
safely — its own one-line measurement, not this release's.

## 5. The doors as deployed — chart, node plugin, worker pod, tenant pod

| workload | lean as deployed: pod Ready (syncer fetch) | lean, host engine (fetch) | passthrough as deployed, cold / warm | passthrough, host engine cold | lean publish as deployed (`Ld-W`) | passthrough write as deployed (`Pd-W`) |
|---|---|---|---|---|---|---|
| `big` | 118.43-205.83 s (no phase line) | 24.09-25.00 s (16.18-17.98 s) | 9.46-10.28 s / 9.16-10.13 s | 9.13-9.83 s | 18.98-19.43 s | 17.52-18.45 s |
| `small` | 22.86-24.90 s (no phase line) | 5.29-5.93 s (4.55-5.18 s) | 67.12-68.87 s / 66.39-68.60 s | 63.25-69.24 s | 20.81-22.55 s | 812.49-828.72 s |
| `mixed` | 92.42-115.05 s (no phase line) | 19.58-19.95 s (14.63-15.69 s) | 16.21-17.06 s / 16.07-16.54 s | 15.02-15.43 s | 50.82-50.92 s | 90.72-96.33 s |

kubectl exec round trip (control): 0.10-0.12 s per call.

How these were taken. The lean worker image is HEAD's `flint-sync` (built
from the same binary the host arms ran, loaded into containerd, never
published); the plugin, the passthrough worker (mount-s3 1.24.0, the
same pin the host arm used) and the lean operator are the 1.50.0 images.
The plugin's state directory — where a lean workspace is a loop-mounted
ext4 image, `tree.img`, under `sizeLimitGib` — was bind-mounted onto the
NVMe, because the node's root volume is an 8 GB gp3 disk (3,000 IOPS,
125 MB/s) that could neither hold nor feed a 6 GiB tree; that is a
node-level choice any deployment on instance-store hardware makes, and it
is the only departure from the chart's defaults, beside `node.region`
set to the bucket's region (finding 2 below). `Ld` is timed from
`kubectl apply` of the tenant pod to `Ready`, which includes the
plugin creating the worker pod, the checkout, the loop mount and the
bind into the pod; the `fetch` column is the syncer's own phase line
from the worker's log. `Pd-32` is the same 32-wide `cat` as the host
arm, run inside the tenant pod; `Ld-W` is `.flint/publish` written from
the tenant pod and timed to its ack; `Pd-W` is the 32-wide `cp` into the
mount from the tenant pod, with mount-s3 completing each PUT at close.
The kubectl-exec round trip is measured once per rep so the short arms
can be read net of it.

**What deploying found, before a number was taken.** The point of
measuring the doors as deployed is that the deployment is part of the
door, and this one found three things the host arms could not.

1. *Ambient identity is dead behind this CNI.* The first setup ran the
   workers with `identity.mode: ambient` (the node role via IMDS) and
   every one failed its first request: "No signing credentials
   available". A test pod showed why — the pod could reach S3 (403) but
   not `169.254.169.254` (4 s timeout): cilium blocks the link-local
   metadata address from pods, which is the secure default. The
   production path is the broker (`identity.mode: broker`), and the
   drill runs it with a static Secret minted from the NODE's IMDS
   credentials, so the workers hold what the node role holds and
   nothing more. `ambient` still works where the CNI passes IMDS
   through; it is not the mode to document for a fleet.
2. *A lean workspace could not name its bucket's region.* With
   credentials in hand the lean worker crash-looped — `get_whole: 301
   PermanentRedirect` — while the three passthrough workers beside it
   ran. The node plugin's `AWS_REGION` is one node-wide value
   (`FLINT_S3CSI_REGION`, chart `node.region`, default `us-east-1`), and
   a `FlintLeanWorkspace` had no field to override it; a
   `FlintPassthroughMount` has had `region` since its first release, and
   mount-s3 takes it on its argv. The bucket is in us-west-1. The drill
   re-rendered the plugin with `node.region: us-west-1`; the product fix
   is `spec.region` on the workspace (`4d44780e`), carried as
   `FLINT_SYNC_REGION` and preferred to the node default at the
   credential step — unit-tested and read through, not yet driven
   through a cluster, since that needs the 1.51.0 plugin image this
   release builds.
3. *The rig's own setup said "done" on a failure.* The warm pod (a
   throwaway lean mount so the first measured leg pulls no image) never
   became Ready under finding 2; `kubectl wait` failed, nothing checked
   it, `SETUP DONE` printed and the phase exited 0 — and the launcher
   started the measured run on that. The run itself would have recorded
   every lean leg as `FAIL` (each arm's Ready wait is checked), so no
   bogus time could have landed; the cost was the time. The waits are
   fatal now. It is the same lesson as the host write run's pre-seeded
   prefixes: a step that fails quietly is indistinguishable from a step
   that is fast.
4. *The lean worker was OOM-killed checking out `big`.* With the region
   fixed the first measured lean leg never became Ready: the worker died
   `OOMKilled` at its checkout, three times over, each restart paying
   the unclean-death claim lockout. The plugin-wide worker limit is 1Gi
   (`node.workers.resources`, for mount-s3 and the syncer alike) and the
   syncer's default read window was 512 MiB in flight at fan-out 32 —
   two defaults that never met, because every host arm of every drill
   ran without a cgroup. Peak RSS of the host checkout (`VmHWM` sampled
   at 200 ms): `big` 1105 MiB at 512, 1005 at 256, **403–437 MiB at 128
   (n=3) in the same wall time (24.9–25.7 s vs 24.1–25.0 s)**; `mixed`
   298 vs 229 MiB, equal time; `small` 72 MiB. The window past 128 MiB
   buys nothing on this NIC. The default moves to 128 (CRD, binary,
   config; `131b575f`), the rig's CRs name it explicitly
   because this cluster's 1.50.0 CRD persisted 512 into them, and the
   measured lean legs below run at the v1.51.0 default.
5. *The deployed lean checkout is 3.5–6x the engine's, and it is the
   syncer's own checkout inside the worker, not the plumbing around
   it.* `Ld` reads 118–206 s on `big` where the host engine reads
   24.1–25.0 s with the same binary and the same 128 MiB window
   (rep 1's 206 s also paid the lease lockout left by finding 4's OOM
   loop; reps 2–3 are 118 and 161 s), 92–115 s on `mixed` against
   19.6–20.0 s, 22.9–24.9 s on `small` against 5.3–5.9 s. The deployed
   row has no `fetch` figure: the daemon (`flint-sync run`) prints no
   phase line, so that column reads "no phase line" and the ranged
   guard for these legs is judged by `L-ship`'s, on the same binary.
   Timestamps from the workers' own logs (sampled every 3 s over reps
   2–3) put the cost where it is: the worker starts 1–2 s after its
   tenant pod is scheduled and holds the lease 1 s later (no lockout —
   the previous leg's worker released cleanly); its checkout then takes
   **156 s on `big`, 88–109 s on `mixed`, 19 s on `small`**; the pod is
   Ready 4–5 s after the checkout completes. The passthrough workers,
   which write nothing to disk, match the host to within a second. The
   deployed tree is a loop-mounted ext4 image (`tree.img`) under the
   plugin's state directory, written from inside a 1Gi cgroup whose
   limit counts the page cache. Inside that worker, `dd` of 4 GiB with
   `fsync` into the loop-mounted tree runs at **98 MB/s** (117 MB/s
   without the fsync, 127 MB/s into the container's own overlay, which
   sits on the root EBS volume); the same `dd` on the node's NVMe
   outside any cgroup runs at **309 MB/s**. A 6 GiB checkout that must
   land at ~100 MB/s cannot finish under ~66 s, and the cgroup's
   dirty-page limit — a fraction of 1Gi — makes the fetch wait on the
   writeback instead of overlapping it, which is the rest of the 156 s.
   Nothing in the syncer is the cause: the same binary, window and
   bucket read 25 s on the host. The follow-up is the plugin's tree
   substrate — a directory quota (XFS project quotas) instead of an
   ext4 image over a loop device, or a worker memory limit sized so the
   writeback window is not the bottleneck — measured here, not changed
   in this release. Until then the deployed lean checkout of a
   many-GiB tree is disk-bound at roughly a third of the engine's rate,
   and the passthrough door, which writes nothing, is unaffected.

## 6. Against the 2026-09-10 numbers

| workload | door | 2026-09-10 | 2026-09-12 | change (bracket of the two ranges) |
|---|---|---|---|---|
| `big` | lean, then-shipped vs now-shipped, wall | 71.33-71.88 s | 24.09-25.00 s | 2.85-2.98x faster |
| `big` | lean, 09-10 ranged settings vs the same settings on today's binary, wall | 21.24-23.96 s | 24.86-24.92 s | 1.04-1.17x SLOWER |
| `big` | lean, 09-10 ranged wall vs today's shipped FETCH (no fsync in either) | 21.24-23.96 s | 16.18-17.98 s | 1.18-1.48x faster |
| `big` | passthrough AS DEPLOYED (09-10 was deployed too) | 10.12-12.90 s | 9.46-10.28 s | 0.98-1.36x (straddles 1: unresolved) |
| `big` | passthrough, host engine (not like-for-like) | 10.12-12.90 s | 9.13-9.83 s | 1.03-1.41x faster |
| `big` | `aws s3 cp` 32-way | 20.16-21.74 s | 21.50-22.44 s | 0.90-1.01x (straddles 1: unresolved) |
| `big` | write: lean barrier | 33.47-35.54 s | 18.31-18.35 s | 1.82-1.94x faster |
| `big` | write: `aws s3 cp` 32-way | 19.40-19.49 s | 18.77-19.09 s | 1.02-1.04x faster |
| `small` | lean, then-shipped vs now-shipped, wall | 16.92-19.44 s | 5.29-5.93 s | 2.85-3.67x faster |
| `small` | lean, 09-10 ranged settings vs the same settings on today's binary, wall | 16.40-17.39 s | 15.90-16.86 s | 0.97-1.09x (straddles 1: unresolved) |
| `small` | lean, 09-10 ranged wall vs today's shipped FETCH (no fsync in either) | 16.40-17.39 s | 4.55-5.18 s | 3.17-3.82x faster |
| `small` | passthrough AS DEPLOYED (09-10 was deployed too) | 265.60-271.90 s | 67.12-68.87 s | 3.86-4.05x faster |
| `small` | passthrough, host engine (not like-for-like) | 265.60-271.90 s | 63.25-69.24 s | 3.84-4.30x faster |
| `small` | `aws s3 cp` 32-way | 94.00-96.10 s | 69.23-70.45 s | 1.33-1.39x faster |
| `small` | write: lean barrier | — | 19.50-20.35 s | — |
| `small` | write: `aws s3 cp` 32-way | — | 58.34-59.31 s | — |
| `mixed` | lean, then-shipped vs now-shipped, wall | 57.02-57.83 s | 19.58-19.95 s | 2.86-2.95x faster |
| `mixed` | lean, 09-10 ranged settings vs the same settings on today's binary, wall | 15.30-18.30 s | 18.73-19.23 s | 1.02-1.26x SLOWER |
| `mixed` | lean, 09-10 ranged wall vs today's shipped FETCH (no fsync in either) | 15.30-18.30 s | 14.63-15.69 s | 0.97-1.25x (straddles 1: unresolved) |
| `mixed` | passthrough AS DEPLOYED (09-10 was deployed too) | 34.30-35.60 s | 16.21-17.06 s | 2.01-2.20x faster |
| `mixed` | passthrough, host engine (not like-for-like) | 34.30-35.60 s | 15.02-15.43 s | 2.22-2.37x faster |
| `mixed` | `aws s3 cp` 32-way | 23.90-25.70 s | 21.58-22.77 s | 1.05-1.19x faster |
| `mixed` | write: lean barrier | 24.34-25.01 s | 50.54-51.67 s | 2.02-2.12x SLOWER |
| `mixed` | write: `aws s3 cp` 32-way | 18.89-18.93 s | 16.89-17.09 s | 1.11-1.12x faster |

## What this drill cannot say

It reads 100% of every tree, on one node, with one reader, in one AZ,
inside the NIC's burst window. It says nothing about sparse access,
multi-reader contention, cross-AZ, a guaranteed rate, or any tree larger
than local disk — which is the case where lean has no answer at all and
passthrough wins structurally.
