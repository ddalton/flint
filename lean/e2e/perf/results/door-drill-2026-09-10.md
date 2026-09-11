# Four doors to the same bytes — a live read/write drill

**Cluster** runcr, 3 x i4i.large spot, us-west-1, one AZ.
**Node** runcr-aws-2, instance-store NVMe at `/mnt/nvme`.
**Bucket** `flint-lean-ranged-20260910`, same region as the node (deleted at teardown).
**Date** 2026-09-10/11. **n = 3**, arms interleaved within each rep.
Every figure is a RANGE over the three reps, never a mean.

Every arm reads THE SAME OBJECTS: flint-lean writes its tree at
`<prefix>/files/<path>`, so a passthrough mount aimed at
`ranged-drill/<w>/files` and an `aws s3 cp` of the same prefix see
byte-for-byte what a lean checkout materialises. Same bucket, same
region, same node, same NIC. Only the door varies.

## The workloads

| name | shape | bytes | why |
|---|---|---|---|
| `big` | 6 x 1 GiB | 6,442,450,944 | one object is one stream — where ranging should win |
| `small` | 20,000 x 8 KiB | 163,840,000 | every object under the threshold — ranging must be a NO-OP |
| `mixed` | 1 x 4 GiB + 2,000 x 16 KiB | 4,327,735,296 | the shape a real checkpoint tree has |

## 1. Read: the four doors, cold, 32-wide

| workload | lean shipped | lean ranged | raw S3 `aws s3 cp` | passthrough (mount-s3) |
|---|---|---|---|---|
| `big` | 71.33-71.88 s | 21.24-23.96 s | 20.16-21.74 s | **10.12-12.90 s** |
| `small` | 16.92-19.44 s | **16.40-17.39 s** | 94.0-96.1 s | 265.6-271.9 s |
| `mixed` | 57.02-57.83 s | **15.30-18.30 s** | 23.9-25.7 s | 34.3-35.6 s |

Lean-with-ranging wins `small` and `mixed` outright and ties the CLI on
`big`, where passthrough wins by never writing the bytes to disk.

`small` showing no ranged effect is the POINT, not a null result: every
object there is under the threshold, and `ranged=0` on every one of
those rows proves the path never fired.

### Re-read, and why it decides more than the cold numbers

| | lean (local tree) | passthrough |
|---|---|---|
| `big` | 0.702-0.714 s | 10.57-13.01 s |
| `small` | — | 70.5-75.4 s |
| `mixed` | — | 18.8-19.4 s |

Passthrough caches NOTHING: no `--cache` in its argv, and
`FOPEN_KEEP_CACHE` appears nowhere in mountpoint-s3's filesystem, so the
kernel drops a file's page cache on every `open()`. `big` warm is
therefore indistinguishable from `big` cold — all 6 GiB re-streamed. The
3.8x `small` does gain warm is the METADATA cache
(`superblock.rs` `serve_lookup_from_cache`), not data: 20,000 opens each
skip a lookup round trip. On six files that is noise, which is exactly
why `big` shows nothing.

Caveat: lean's 0.71 s is a 6 GiB tree served from page cache on a 16 GB
node. A tree that does not fit falls back to NVMe, and the only NVMe
number here is single-threaded (17.0 s for 6 GiB).

### Metadata surface

`find -type f`, no bytes read: `big` 21-36 ms, `mixed` 228-261 ms,
`small` 1.93-2.26 s. That last one is an S3 round trip per entry, caused
by the `Minimal` metadata TTL that mount-s3 defaults to when `--cache`
is unset.

### The controls

- **cache drop moved 24x** — 16.84-17.04 s cold vs 0.702-0.714 s warm on
  a local NVMe tree, through the same `drop_caches` the P legs use. "Cold"
  means cold.
- **concurrency moved 4.3x** — `small` serial 1140.7-1156.3 s vs 32-way
  265.6-271.9 s. The rig can see fan-out.
- **fan-out on `big` did NOT move** — 71.27-71.54 s at fanout=1 against
  71.33-71.88 s at fanout=32. The guard FAILED the first run for this,
  correctly: see §2.

## 2. Why the shipped default was slow — the guard was right

`fetch_inflight_max_bytes` is 512 MiB and `checkout.rs:282` sizes each
entry's permit from `entry.size`, clamped to the budget. Any object at
or above the budget takes the WHOLE window, so checkout is serial across
objects no matter what `FLINT_SYNC_FANOUT` says. That is why collapsing
fan-out 32 -> 1 changed `big` by 0.4%.

Raising the window is NOT the fix:

| workload | 512 MiB window | 8 GiB window |
|---|---|---|
| `big` | 71.33-71.88 s | 30.54-34.60 s (2.1x) |
| `mixed` | 57.02-57.83 s | 54.93-55.19 s (**1.04x — nothing**) |

`mixed`'s critical path is a SINGLE 4 GiB object, and no window makes one
object go wider than one stream. Only ranging splits it. And per
`lib.rs:285-291` the window is a MEMORY bound — `get_whole` holds each
object in RAM — so 8 GiB of window is 8 GiB of RSS in a sidecar that
ships with no memory limit.

### How much is left on the read path: not much

A 2x2 over the two independent ways to have more bytes in flight, on `big`:

| | budget 512 MiB | budget 8192 MiB |
|---|---|---|
| parallelism 4 | 21.27-23.35 s | **17.96-18.77 s** |
| parallelism 16 | 19.75-19.89 s | 20.35-20.39 s |

Both levers give ~1.15-1.25x and they DO NOT COMPOSE — doing both is
worse than either alone. The ceiling is the disk:

| local NVMe write, 6 GiB, lean's pattern | MB/s |
|---|---|
| buffered, 1 writer | 279 |
| buffered, 6 writers | 267 |
| O_DIRECT, 1 writer | 267 |
| O_DIRECT, 6 writers | 263 |

263-279 MB/s across every configuration; parallelism does not move it.
So sizing the permit to what a ranged fetch actually holds
(`chunk x parallelism` = 64 MiB, not the object size) is worth ~1.2x —
real, cheap, and not a headline.

## 3. Write: the same defect, unfixed, worth more

`compose_parts_and_complete` walked one object's parts in a `for` loop,
awaiting each `upload_part().send()`. `fanout` spreads uploads ACROSS
objects, so a tree whose critical path is one large object got no
concurrency at all.

| workload | lean barrier | `aws s3 cp` 32-way | 1-way (control) |
|---|---|---|---|
| `big` | 36.05-37.30 s | 19.43-19.46 s | 84.8 s |
| `mixed` | 61.09-61.46 s | 18.99-20.13 s | 117.4 s |

Unlike the read path the disk is NOT in the way: the CLI hit 331 MB/s
reading the same disk, under the 378 MB/s measured. Lean simply was not
asking for the bandwidth.

### With parallel parts

| workload | 1 part at a time | 8 parts | CLI 32-way |
|---|---|---|---|
| `big` | 33.79-35.33 s | 33.47-35.54 s (**overlapping — no effect**) | 19.40-19.49 s |
| `mixed` | 61.82-62.32 s | **24.34-25.01 s (2.5x)** | 18.89-18.93 s |

`big` not moving is the drill's own validity check, written into its
header before the run: `big` already gets six-way concurrency across
objects from `fanout`, so if it had moved as much as `mixed`, something
other than part parallelism changed.

The residual gap is the DOUBLE READ: `upload_compose`
(`barrier.rs:911-923`) reads the whole file to compute its CRC, then
reads it again part by part. At 378 MB/s that pre-pass is ~11 s of
`mixed`'s 24.3 s — subtract it and lean's upload is FASTER than the CLI.
Closing it needs per-part CRCs combined by length, because forge's
one-pass push (`crc64: None`) depends on in-order accumulation and is
pinned to the sequential path by construction.

## 4. The use case the drills did not cover

Cold start, edit three files, publish:

```
cold checkout, whole 4.03 GiB tree   61,276 ms
publish 3 edited files                  785 ms   (up=3 del=0)
```

**78x**, and ~20x even with ranging on. The write half is already
incremental — `scan.rs` classifies by size+mtime against the baseline,
stat-only, and found exactly the three modified files. The read half has
no answer: to touch three files you must materialise all 2,001.

Note `del=0` holds BECAUSE the tree was whole. A scoped checkout would
have left 3 local paths against 1,998 baseline entries, and the second
barrier would publish 1,998 deletions.

## What this changed

- `range_get_min_bytes` default 0 -> 8 MiB (this drill is the measurement
  its doc comment demanded). Effective threshold is 16 MiB: a single part
  is "one whole GET with extra steps" and `fetch_ranged` declines it.
- `S3Store::part_parallelism`, default 1, honoured only when the caller
  supplies `crc64` — forge is on the sequential path by construction.

## What this drill cannot say

It reads 100% of every tree, on one node, with one reader, in one AZ. It
says nothing about sparse access, multi-reader contention, cross-AZ, or
any tree larger than local disk — which is the case where lean has no
answer at all and passthrough wins structurally.
