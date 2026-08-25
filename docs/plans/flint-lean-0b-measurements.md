# flint-lean Phase 0b — first measured numbers (2026-08-25)

Rig: the re-provisioned `flint-drill` Lima VM (aarch64, 4 vCPU, 8 GiB,
ext4), MinIO single-node on loopback (`drill-minio` unit, bucket
`agentws`), release `flint-sync` cross-built
`aarch64-unknown-linux-musl` (commit `72bc289`). **These are FLOOR
numbers**: loopback MinIO is not the deployment proxy — the plan's 0b
gate closes only when re-measured PROXY-SHAPED (§5 0b/0c); what this
run establishes is the sidecar-side cost structure and the shape of
the axes.

## The e2e leg first (git + sqlite, real binaries)

checkout(empty) → `git init` + 2 commits + branch + sqlite writes +
delete, across 3 barriers → fresh checkout elsewhere:

- `git fsck --strict` clean; both commits and both branches present.
- sqlite `integrity_check` ok, all rows present.
- The deleted file did NOT resurrect, and published on the second
  scan (the two-consecutive-scans rule observed on the wire).
- Trees byte-identical EXCEPT **empty directories do not round-trip**
  (`.git/objects/{info,pack}`, `.git/refs/tags`) — the manifest is
  file-based. Benign for git (it recreates them lazily); recorded as
  a v1 residual for the CRD docs. A dir-marker entry is the v2 lever
  if a workload ever needs it.

## Bytes axis (1 GiB = 16 × 64 MiB, whole-object path)

| Op | Wall | Rate | Peak RSS |
| --- | --- | --- | --- |
| publish | 8.0 s | **8.0 s/GiB** | 138 MiB |
| fresh checkout | 3.3 s | **3.3 s/GiB** | 76 MiB |

The historical 8–13 s/GiB planning band holds at its floor on
loopback. RSS ≈ 2 × whole_put_max + base, as the buffering predicts.

## File-count axis, 100k entries (~64 B files, 100 dirs × 1000)

| Op | Wall | Peak RSS | Notes |
| --- | --- | --- | --- |
| first barrier (100k PUTs) | 117 s | 194 MiB | ≈ 854 PUTs/s, SEQUENTIAL v1 loop |
| manifest document | — | — | **27 MiB ≈ 283 B/entry** (the review's 250–350 B estimate confirmed) |
| no-change barrier | 2.8 s | 222 MiB | the idle tick: full walk + manifest seq check |
| one-change barrier | 1.9 s | 222 MiB | walk + 1 PUT + whole-manifest CAS |
| fresh checkout | 49.5 s | 120 MiB | ≈ 2,020 files/s, SEQUENTIAL v1 loop |

## File-count axis, 1M entries (1000 dirs × 1000 files)

| Op | Wall | Peak RSS | Notes |
| --- | --- | --- | --- |
| create 1M local files | 46 s | — | rig-side setup, not a lean number |
| first barrier (1M PUTs) | 29 m 41 s | 1.86 GiB | ≈ 561 PUTs/s sequential (rate decays as MinIO's metadata tree grows) |
| manifest document | — | — | **264 MiB ≈ 277 B/entry — exactly linear from 100k** |
| no-change barrier | 27.5 s | 1.33 GiB | early exit works post-`72bc289`; dominated by the 264 MiB manifest GET for the seq check (see finding 4) |
| one-change barrier | 27.6 s | 2.18 GiB | 1 PUT + 264 MiB manifest GET + CAS PUT |
| fresh checkout | 16 m 24 s | 1.14 GiB | ≈ 1,016 files/s sequential; ran with brief contention from a concurrent diagnostic process (see finding 5) — treat as an upper-ish floor |

## What the run found beyond numbers

1. **Rotation churn on clean handoffs** (fixed, `72bc289`): every CLI
   claim rotated the manifest — double seq bumps, no-change early
   exit defeated, and a multi-MB GET+PUT per claim at scale. Rotation
   now fires only for the unreleased-foreign takeover.
2. **The v1 upload/checkout loops are sequential.** 561–854 PUTs/s and
   1,000–2,000 GETs/s are single-stream loopback rates; a bounded
   fan-out (the hub's hydrate already has one) is the obvious first
   lever BEFORE buying proxy capacity — it multiplies directly
   against these numbers. At 1M files it is the difference between a
   30-minute and a ~3-minute first publish.
3. **Manifest cost is exactly linear** (283→277 B/entry from 100k to
   1M), so the whole-document CAS at 1M entries is a ~264 MB PUT per
   changed barrier + the same GET on every 412 retry. Manifest I/O,
   not file I/O, dominates the changed-idle path on big trees.
4. **The no-change tick GETs the whole manifest just to read `seq`.**
   27.5 s and 1.3 GiB RSS of idle cost at 1M entries. Lever: HEAD the
   manifest and compare its ETag against the persisted
   `baseline.manifest_etag` — the idle tick then costs one walk + one
   HEAD. Not yet implemented; queued.
5. **THE OCCUPANCY LOCK WAS MISSING** (fixed, this commit): a second
   flint-sync pointed at the same workspace self-recognized the lease
   via the persisted incarnation id, DEPOSED THE LIVE SIBLING, and
   both wrote the tree — observed on this rig as tmp-rename ENOENT
   collisions when a diagnostic re-run raced the live 1M checkout.
   Self-recognition is only sound if the previous process is provably
   gone; the state dir now takes an exclusive flock at open (the
   hub's `is_single_occupant` gate, rediscovered the hard way), and
   the battery pins it.
   Two smaller fixes from the same chase: `LeanError::Io` carried no
   path (barrier writes now label op+path), and the atomic-write tmp
   name used `with_extension` (collides for `a.txt`/`a.md`; now a
   suffixed file name).
6. **Empty directories do not round-trip** (git e2e; recorded in plan
   §3). Benign for git; dir-marker entry is the v2 lever.

## Lever validation (same rig, same day — commit with the levers)

Fan-out (bounded `buffer_unordered`, default 16) + HEAD-not-GET idle
tick + skip-baseline-rewrite-on-unchanged-scan-set:

| Op | Sequential | With levers | Note |
| --- | --- | --- | --- |
| 100k first publish | 117 s | **65 s** | loopback MinIO's single disk is now the wall — through a latency-bound proxy the multiplier approaches the fan-out width |
| 100k idle tick | 2.8 s / 222 MiB | **1.85 s / 85 MiB** | no manifest parse |
| 1M fresh checkout | 16 m 24 s | **7 m 05 s** | all 1M files verified |
| 1M idle tick | 27.5 s / 1.33 GiB | **26.1 s / 762 MiB** | the 264 MiB manifest GET is GONE (the proxy-transfer win); the residual is baseline.json parse (238 MB) + the 1M-file walk — structural v1 costs, ≈6 s at the 250k cap |

## Tentative v1 caps that fall out (to firm up proxy-shaped)

- **File-count cap: 250k entries** for v1. At 250k the extrapolated
  costs are ~66 MiB manifest, ~7 s idle tick (until finding 4's HEAD
  lever lands), ~5 min sequential first publish / ~4 min checkout —
  workable; at 1M, first-publish/checkout wall time (~30/16 min
  sequential) and the ~264 MiB manifest PUT per changed barrier are
  operationally hostile until fan-out (finding 2) and the HEAD lever
  (finding 4) land. Raise the cap when those two ship and the
  proxy-shaped re-measurement confirms.
- **Bytes: no new constraint below the emptyDir budget** at the
  whole-object path (8 s/GiB publish, 3.3 s/GiB checkout, RSS ≈
  2×whole_put_max); >64 MiB files still await the multipart compose
  wiring.
- **Sidecar container sizing:** RSS scales with entry count — ~220 MiB
  at 100k, ~1.3–2.2 GiB at 1M. The webhook's derived limits should
  budget ~2.2 KiB × entries + 150 MiB base until the manifest paths
  stream.
