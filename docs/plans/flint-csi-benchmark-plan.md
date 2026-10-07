# Flint CSI head-to-head benchmark — plan (DRAFT, 2026-10-07)

Status: **draft, nothing provisioned.** No cluster is created until the
plan and cost below are approved.

## 1. Purpose

Publish a benchmark of Flint against the Kubernetes block-storage
drivers people actually compare it with, that **anyone can rerun on
commodity cloud hardware** from published scripts. The goal is a
credible public result, not a flattering one: every place Flint loses
is reported alongside every place it wins.

## 2. Questions, fixed before any run

Writing these down first keeps the report from choosing its questions
after seeing the numbers.

| # | Question | Metric |
|---|----------|--------|
| Q1 | Small-block latency, replicated | 4 KiB randread / randwrite at QD1: p50, p99, p99.9 (µs) |
| Q2 | Small-block IOPS ceiling, replicated | 4 KiB rand r/w, 70/30 mix, QD32 × 4 jobs: IOPS |
| Q3 | Sequential throughput | 128 KiB and 1 MiB seq read / write: MiB/s |
| Q4 | Cost of a database workload | pgbench TPS + p99 latency, `synchronous_commit=on` |
| Q5 | Shared (RWX) throughput | N clients on one RWX volume, N = 1, 2, 3 (one per node): aggregate MiB/s |
| Q6 | Recovery after node loss | time back to full redundancy for a 50 GiB volume; I/O during rebuild |
| Q7 | Durability under node loss | lost acknowledged writes (must be 0) |
| Q8 | Operations | PVC-to-pod-Running for 50 PVCs; snapshot and clone time |
| Q9 | CPU cost | storage-daemon cores and RSS per node, idle and under Q2 |

Q7 is a pass/fail correctness result, not a performance number.

## 3. Contenders

Pin exact versions on the day of the run and record them in the results.

| Driver | Engine | RWX path | Notes |
|--------|--------|----------|-------|
| **Flint** (release current at run time) | SPDK lvol + ublk; NVMe-oF/TCP replica legs | pNFS | single SPDK reactor core by default (`scripts/spdk-block-ceiling.sh`) |
| **OpenEBS Mayastor** | SPDK, NVMe-oF/TCP | none native | reserves dedicated io-engine cores by default |
| **Longhorn v2 data engine** | SPDK | share-manager NFS | check its support status at the tested version and state it in the report |
| **Rook-Ceph** | RBD (block), CephFS (RWX) | CephFS | the most widely deployed non-SPDK option; mons, mgr, OSDs and an MDS share the 3 nodes |

Deferred to a later round to keep cost down: **Longhorn v1** (the most
widely deployed option overall). The harness is written so adding it
later is one install script.

Each contender runs with **its own documented production
recommendations** (hugepages, core reservations, filesystem), replica
count **3**, plus one replica-count-**1** pass for Q1–Q3 to separate
engine cost from replication cost.

### CPU fairness

Drivers reserve different amounts of CPU (Flint 1 reactor core by
default; Mayastor reserves more). Raw speed alone would then compare
CPU budgets, not engines. So:

- **Pass A — defaults:** each driver as shipped. This is what users get.
- **Pass B — equal budget:** each SPDK driver pinned to 1 polling core
  (Q1–Q3 only).
- Q9 is reported next to every throughput number, so the reader sees
  IOPS per core as well as IOPS.

## 4. Environment (AWS, all spot)

| Role | Count | Instance (proposed) | Why |
|------|-------|---------------------|-----|
| Storage + control plane + load | 3 | i4i.2xlarge (8 vCPU, 64 GiB, 1 × 1,875 GB NVMe instance store), us-east-2 | local NVMe on every node, like the hyperconverged deployments these drivers target |

- **No dedicated control-plane or client nodes.** k3s with the control
  plane on storage node 1. fio and pgbench run on the storage nodes
  (hyperconverged, how these drivers are usually deployed). With 3
  replicas every write still crosses the network to two other nodes.
- **8 vCPUs hold a storage daemon's polling cores, fio and the control
  plane.** Pin fio to cores the daemon does not use, and record per-core
  CPU so any contention shows up in the data, not as noise.
- **Ceph is the most memory- and CPU-hungry contender here:** one OSD
  per node (Ceph's guidance is about 4 GiB of RAM per OSD) plus three
  mons, a mgr and, for Q5, an MDS. Use Rook's documented resource
  settings for small clusters and record them; if Ceph cannot run its
  recommended footprint on i4i.2xlarge, say so in the report rather
  than quietly shrinking it.
- One AZ, cluster placement group; Ubuntu 24.04 (has `ublk_drv` and `nvme_tcp`).
- **Network is "up to 12 Gbps" burst on a 4.687 Gbps baseline (about
  560 MiB/s); burst credits will drain.** The baseline, not the burst,
  is likely the bound on replicated write throughput.
  Before the matrix, run `iperf3` for 15 minutes between storage nodes
  and record the sustained rate. Throughput runs must sit past the
  credit drain. A short run would measure the burst, not the bound.
- Same filesystem (ext4) and mount options on every driver's volume.

## 5. Method

**Order.** One contender at a time on the same nodes. Between
contenders: uninstall, `blkdiscard` the instance-store NVMe, reboot,
verify the device reads clean. After all contenders, rerun the first
one: if it differs from its first run by more than the run-to-run
range, the environment drifted and the comparison is void.

**Repetition.** Each fio point runs 3 times × 120 s after a 30 s ramp.
Report the median and the min–max range. A difference smaller than the
overlapping ranges is reported as "no difference".

**Preconditioning.** Write the whole volume before any read test. A
read of an unallocated thin volume measures the zero-fill path, not
the disk.

**Ceilings, measured first:**
- raw fio on the instance-store NVMe (the disk ceiling);
- `iperf3` between storage nodes (the network ceiling).

Every result is also shown as a fraction of the relevant ceiling.

**Q6/Q7 — node loss.** Reuse the `tests/chaos` Postgres ledger oracle
(`verify-db.sh`): pgbench load with acknowledged sequence numbers
appended to a ledger; stop a storage node's instance hard (EC2 stop,
not a pod delete); measure time to full redundancy; any acked seq
missing from the database is a FAIL. Three trials per contender.

**Q5 — RWX.** Flint pNFS vs Longhorn share-manager NFS vs CephFS.
Mayastor has no native RWX and is marked "n/a", not omitted.

## 6. Pitfalls this plan guards against

- **Burst networking measured as sustained** — §4 iperf3 gate.
- **Thin-volume zero reads** — §5 preconditioning.
- **Comparing CPU budgets instead of engines** — §3 Pass B and Q9.
- **Caching** — `direct=1` for fio; drop page caches between runs;
  pgbench scale chosen to exceed node RAM.
- **Single-run noise** — 3 runs, ranges, and the return-to-first rerun.
- **Misconfigured competitors** — each driver follows its own
  documented production settings, and the exact configs are published
  so anyone can check them (§9.3).

## 7. Deliverables

- `bench/` directory in the repo: install scripts per contender, the
  fio job files, pgbench and chaos drivers, a collector writing raw
  JSON, and a report generator.
- Raw results (fio JSON, CPU samples, versions, configs) committed.
- A write-up with charts: where Flint wins, where it loses, and why.

## 8. Phases and cost

| Phase | Where | What | Duration (est.) |
|-------|-------|------|-----------------|
| 0 | Linux build box | build the harness; dry-run the fio matrix and collector against Flint only | 1–2 days of work, no AWS cost |
| 1 | EC2 | ceilings + Flint full matrix; validates harness on real nodes | ~4 h |
| 2 | EC2 | Mayastor, Longhorn v2, Rook-Ceph; return-to-first rerun of Flint | ~4 h each, ~14 h total |
| 3 | — | report, optional maintainer review, publish | — |

**Cost estimate: about $8 of EC2, budget $20.** Spot prices checked
2026-10-07: i4i.2xlarge $0.145–0.155/hour in us-east-2 (vs $0.27–0.35
in us-east-1 and $0.25–0.31 in us-west-2). 3 nodes × ~$0.15 × ~18 hours
≈ $8; EBS root volumes add well under $1. The $20 budget covers reruns
and price movement. Re-check prices on the day. The harness must run
the whole matrix unattended, and the cluster is torn down between
phases.

## 9. Open decisions

1. ~~Instance size~~ — **decided 2026-10-07: i4i.2xlarge in us-east-2**
   (spot $0.145–0.155/hour, about 2.4× i4i.xlarge's $0.06 there).
2. **Publication:** blog post, repo `docs/`, or both.
3. **Maintainer review:** optional. Contacting Mayastor and Longhorn
   maintainers before publishing lets them catch a misconfiguration
   before it is public. At minimum, publish the exact configs and take
   corrections afterwards.
