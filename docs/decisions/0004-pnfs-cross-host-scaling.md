# ADR 0004: pNFS cross-host linear scaling — measured

**Date**: 2026-07-05
**Status**: Accepted (the load-bearing claim is now measured)
**Prior art**: ADR 0002/0003 (single-host baselines, superseded numbers),
`docs/plans/pnfs-performance-plan.md` (Phases 0–2 landed; this is Phase 3)

## Question

Does flint-pNFS aggregate bandwidth scale ~linearly with data-server
count when each DS has its own NIC and disk? Every prior number was
one Mac, loopback TCP, one shared APFS disk. This ADR records the
first cross-host measurement.

## Rig

- **Hardware**: 8 × i4i.large spot (us-west-1, AL2023, kernel 6.1,
  2 vCPU / 16 GiB / 468 GB instance NVMe / burst-10 Gbps NIC),
  provisioned as a disposable extension of the standing `runk` cluster
  (trove project 28) and terminated the same day.
- **Topology**: 1 MDS node, 4 DS nodes (ext4 on dedicated instance
  NVMe), 3 client nodes (kernel NFSv4.1 client,
  `nfs_layout_nfsv41_files`, `nconnect=4`, rsize/wsize 1 MiB). MDS/DS
  ran the post-Phase-2 binaries (fd caches + RPC pipelining,
  commit 769280e) as host processes.
- **Method**: N ∈ {1,2,4} DSes; per config: 48 GiB unique dataset
  (3 clients × 4 jobs × 4 GiB, > DS RAM so reads are cold), page
  caches dropped on every node between phases. Aggregate = sum of
  per-client fio `bw_bytes`. DS network tx counters captured per
  phase — every read number below is corroborated by on-the-wire DS
  tx within a few percent. fio phases: seq 1M (iodepth 8, 4 jobs/client),
  4k randread (direct=1, QD32×2 jobs/client), small-file dataloader
  (1024 × 1 MiB files/client, shuffled reads).

## Results (aggregate, M=3 clients)

| Phase | N=1 | N=2 | N=4 | N=4/N=1 |
|---|---:|---:|---:|---:|
| seq read 1M | 328.5 MiB/s | 695.7 | **1978.3** | **6.02×** |
| seq write 1M (end_fsync) | 264.9 | 526.6 | 1060.6 | **4.00×** |
| rand read 4k direct | 112.4 (28.8k IOPS) | 239.6 | 525.9 (134.6k IOPS) | **4.68×** |
| small-file shuffled read | 889.0 | 1319.1 | 1335.0 | 1.50× (client-bound) |

Single client (M=1) seq read: 340.6 → 903.4 → 1039.2 MiB/s — one
client's throughput scales with stripe width until its own NIC is the
limit (~8.7 Gbps at N=4). This is the per-consumer pNFS payoff: a
single dataloader process reads at ~1 GiB/s from 4 DSes.

**Pass criteria** (set in the performance plan before the run):
- Aggregate seq read ≥ 0.8·N× the N=1 number: **PASS** — 2.12× at
  N=2 (gate 1.6×), 6.02× at N=4 (gate 3.2×). Super-linear because the
  N=1 baseline pays 12-stream interleave contention on one DS.
- Small-file byte spread 40–60% at N=2: **PASS** — 48.4% / 51.6%
  measured by DS tx during the shuffled-read phase; at N=4 all four
  DSes served within ±3% of each other on every phase.
- MDS CPU flat as N grows: **PASS** — 0% during every data phase at
  every N (sampled mid-read). The MDS is fully out of the data path.

## Pipelining A/B, cross-host (N=4, FLINT_NFS_MAX_INFLIGHT=0 control)

| Phase | pipelined | sequential | delta |
|---|---:|---:|---:|
| small-file shuffled read | 1335.0 | 1024.7 | **+30%** |
| seq read 1M M=3 | 1978.3 | 1837.9 | **+7.6%** |
| seq write / seq read M=1 | ~equal | ~equal | 0 (bandwidth-bound) |
| rand 4k direct QD64/client | 525.9 | 600.1 | **−12%** |

Pipelining wins where it was built to win — the metadata-heavy
dataloader shape (+30%) and multi-stream aggregate reads — and loses
on deep-queue tiny-op workloads, where clients pipeline so hard that
every op takes the spawn path and the per-op task overhead exceeds
overlap gains on µs-scale NVMe reads (consistent with the loopback
microbench in the plan). Default stays ON;
`FLINT_NFS_MAX_INFLIGHT=0` is the knob for IOPS-dominated
deployments. Follow-up recorded in the plan: per-op-cost-adaptive
dispatch (inline small READs even when the connection has backlog).

## Bug found by the bench

**DS basename collision (P1, pre-GA blocker for multi-dir use).** The
DS's MDS-issued-filehandle fallback rebases every file to
`<data-dir>/<basename>` — files with the same basename in different
directories silently share one backing file (first observed as
"48 GiB written, 18 GiB on disk": three clients' `lay.0.0` collided).
Filed for Phase 4; the bench worked around it with globally unique
file names. Fix direction: key DS-local storage by MDS file-id (the
pNFS v2 filehandle already carries one) instead of path basename.

## Honest caveats

- **NIC burst credits.** i4i.large baseline is ~0.78 Gbps with burst
  to 10 Gbps; every number above ran inside the burst envelope on
  fresh instances. Long-sustained workloads on this instance class
  would settle far lower; the *scaling shape* (per-DS ceiling × N) is
  the durable result, not the absolute MiB/s.
- **Client-bound tails**: small-file read plateaus at ~1.3 GiB/s from
  N=2 (3 clients × ~445 MiB/s each); M=1 seq read caps at one
  client NIC. More/fatter clients would extend both.
- **sfwrite is unreliable** (~4.7 GiB/s at every N): 1 GiB/client of
  buffered small-file writes measures client page cache, not the wire.
- **Same-AZ, one run each.** Numbers are single-run (no variance
  bars); DS tx corroboration and cross-N consistency are the sanity
  checks. Spot capacity was all us-west-1c.
- **Host processes, not pods.** The k8s deployment story for MDS/DS
  (DaemonSet, service discovery, CSI wiring) remains Phase 4 work;
  this measures the protocol/data path.

## Decision

The load-bearing claim holds: flint-pNFS scales aggregate read/write
bandwidth linearly (to super-linearly) with DS count through at least
N=4, with the MDS flat at 0% CPU, balanced DS utilization, and
meaningful single-client scaling. pNFS graduates from "architecture
that should scale" to "architecture measured to scale." Remaining
gates before advertising it are Phase 4's durability items (DS
replication/backing, write verifier, DS-death drills) and the
basename-collision fix above.
