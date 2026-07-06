# ADR 0005: durable (lvol-backed) pNFS data servers — the replication cost

**Date**: 2026-07-06
**Status**: Accepted
**Prior art**: ADR 0004 (cross-host linear scaling on emptyDir DSes —
the r1 baseline this ADR compares against),
`docs/plans/pnfs-durable-ds-plan.md` (Phases 0–4 landed; this is Phase 5)

## Question

The durable-DS milestone puts each pNFS data server's export tree on a
replicated flint volume (`numReplicas: 2`) instead of emptyDir, so a
DS survives node loss. That is not free: every DS write now fans out
over a raid1 (one local lvol leg + one NVMe-oF leg to a peer), and a
lost leg triggers a rebuild. ADR 0004 proved r1 (emptyDir) DSes scale
~linearly (seq read 6.02× at N=4). This ADR answers: **what does
2-way replication under the DS cost, and does it break that scaling?**

## Rig

- **Hardware**: i4i.large (us-west-1, AL2023, kernel 6.1, 2 vCPU /
  16 GiB / one 468 GB instance NVMe / burst-10 Gbps NIC — same class
  as ADR 0004). Bench nodes were on-demand i4i.large added to the
  standing `runn` cluster (trove project 33) and torn down the same
  day; the 3 original spot workers kept running the live pNFS fleet.
- **Two measurement layers:**
  1. **Backing-layer A/B** — fio *inside* two live chart-deployed DS
     pods on identical hardware: `flint-pnfs-ds-1` on a `numReplicas:1`
     PVC (one NVMe-oF hop) vs `flint-pnfs-ds-3` on a `numReplicas:2`
     PVC (raid1: local leg + NVMe-oF leg). This isolates the
     replication cost from all pNFS-protocol overhead — it is the DS's
     own view of its disk.
  2. **Cross-host pNFS** — the ADR 0004 harness
     (`tests/k8s/pnfs-bench/`) with the new `BENCH_STORAGE_CLASS`
     option pointing DS export trees at a `numReplicas:2` class, on 4
     dedicated on-demand DS nodes (LVS on instance NVMe), MDS + client
     nodes separate, csi-node kept off the bench nodes.
- **Caveat inherited from ADR 0004**: i4i.large NICs are burst-credit
  (0.78 G base / 10 G burst); absolute MiB/s are burst-window numbers.
  The durable results are the **r1:r2 ratios** and the **scaling
  shape**, not the absolute rates.

## Result 1 — replication is ~free on throughput (the headline)

Backing-layer A/B, fio direct, same i4i.large NVMe, r1 vs r2 PVC:

| workload            | r1 (1 hop) | r2 (raid1) | r2/r1 |
|---------------------|-----------:|-----------:|------:|
| seq write 1M (2 GiB)| 319 MB/s   | 316 MB/s   | 0.99× |
| seq read  1M (2 GiB)| 440 MB/s   | 1100 MB/s  | 2.50× |
| 4k randwrite qd16   | 30030 IOPS | 29371 IOPS | 0.98× |
| 4k randread  qd16   | 42604 IOPS | 52413 IOPS | 1.23× |

**Writes cost ~nothing** (0.98–0.99×): raid1 issues both legs in
parallel, and at these rates neither the local NVMe nor the peer
NVMe-oF leg is the bottleneck, so the mirror does not serialize.
**Reads are FASTER under r2** (1.23–2.50×): raid1 serves reads from
the local lvol leg, which beats the r1 single-remote-NVMe-oF hop. The
replication write cost this ADR set out to "quantify a constant" for
is, on this hardware, ~0% throughput — the real costs are elsewhere
(below).

## Result 2 — degraded and rebuild windows

r2 DS with the remote leg detached mid-fio (raid degrades to 1/2 but
stays online — measured in the Phase 4 replica-under-DS drill and here):

| phase                    | seq write | 4k randwrite |
|--------------------------|----------:|-------------:|
| healthy (2/2)            | 316 MB/s  | 29371 IOPS   |
| degraded (1/2)           | 334 MB/s  | 29676 IOPS   |
| during active rebuild    | 261 MB/s  | —            |

Degraded is if anything *faster* (no peer-leg write). The rebuild
window is the one real throughput cost: **~18% seq-write dip while
resync runs**, and resync of a 2 GiB working set completed in **~9 s**
(re-add → 2/2 operational). pNFS clients saw none of this — the
Phase 4 drill wrote through the whole degrade+rebuild cycle with a
1 s max stall and clean checksums.

## Result 3 — scaling shape preserved with r2 backing

Cross-host, r2-backed DS export trees, 4 DS nodes, multi-client
aggregate read (each client reads its own 2 GiB dataset, cold; layouts
fan reads across all 4 DSes; `bs=1M`, 4 jobs/client):

| clients | aggregate  | per-client | scale |
|--------:|-----------:|-----------:|------:|
| 1       | 530 MiB/s  | 530 MiB/s  | 1.00× |
| 2       | 1284 MiB/s | 642 MiB/s  | 2.42× |
| 3       | 1993 MiB/s | 664 MiB/s  | 3.76× |

Aggregate read scales ~linearly with client concurrency and **the
N=4 r2 aggregate (1993 MiB/s) matches ADR 0004's N=4 r1 number
(1978 MiB/s) within noise**. Replication under the DS does not cap
aggregate read bandwidth — reads come off each DS's local lvol leg, so
the fan-out across 4 DSes scales exactly as the emptyDir fleet did.
Per-client holds ~650 MiB/s (the single-client i4i.large ceiling),
confirming the clients, not the DSes, are the per-stream limit.

Single-client cross-host (one i4i.large client, `nconnect=16`, 1 MiB
reads) was client-NIC-bound as expected — 612 MiB/s at N=1 vs 706 at
N=4, i.e. a single client cannot drive 4 DSes and this says nothing
about DS scaling (ADR 0004 made the same point; it used 3 clients).

## Decision

**Ship lvol-backed DSes as the durable pNFS tier.** The advertised
cost, stated plainly:

- **Throughput: ~0%.** 2-way replication does not measurably slow DS
  writes on burst-NIC NVMe hardware, and improves reads.
- **Capacity: 2×.** Each logical byte is stored twice. This is the
  real price of durability and the one to put in front of users.
- **Rebuild: a brief, bounded window.** ~18% write dip during resync;
  seconds for small working sets; invisible to pNFS clients (Phase 4).

The emptyDir (r1/ephemeral) tier stays available for scratch workloads
that want the last few percent and can lose a DS. The README/chart
durability language should quote the 2× capacity cost and the
~0%-throughput / faster-reads finding, not a scary write-amplification
number — on this class of hardware there isn't one.

## Threats to validity

- Burst-NIC hardware hides steady-state amplification: on a
  sustained-bandwidth NIC where the peer NVMe-oF leg competes with
  local writes, r2 seq-write could drop toward 0.5×. The ~1.0× here
  is an i4i.large-class result; re-measure on the target production
  NIC before quoting it there.
- The cross-host sweep's DS export PVCs sometimes placed their second
  leg on a non-bench node (the replica scheduler picks any
  LVS-capable node) — noted, not controlled; it dilutes the
  "per-bench-node write doubles" worst case rather than inflating the
  result.
