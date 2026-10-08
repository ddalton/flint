# 2026-10-07 Phase 1 on EC2 — Flint 1.58.0, replica count 1 and 3

**Cluster:** trove project 171 `bench2`, us-east-2c, 3 × i4i.2xlarge spot
(8 vCPU, 64 GiB, one 1,875 GB NVMe instance store each), Amazon Linux
2023, kernel 6.18.51, Kubernetes v1.34.12, Cilium (vxlan, **WireGuard
off**). Up 21:12Z → torn down 00:21Z (2026-10-08), verified in four US
regions.

**Flint:** 1.58.0 from the published chart (`drivers/flint/ec2-up.sh`,
`ec2-values.yaml`): chart defaults incl. one SPDK reactor per node,
three-container mode, nvmeof block backend, pNFS and dashboard off; disks
initialized through the node agent. StorageClasses `flint-bench-r1/r3`
(thick). fio on bench2-aws-2; the r1 volume was local to it, r3 had one
replica per node, all `in_sync`.

**Settings:** 100 GiB volume, 90 GiB ext4 file preconditioned by one full
write, 30 s ramp, 120 s measured, 3 repetitions of the whole matrix.
**r3 was stopped by the user at 00:19Z during rep 3**: every test has 2
repetitions, 4 tests have 3 (the interrupted rep-3 `iops_mixed70_4k` was
discarded). `summary.md` / `summary.csv` are `report.py flint-r1 flint-r3`.

## Ceilings (`ceilings/`)

- Network, host to host, 4 streams, 15 min: **11.91 Gbit/s, flat every
  minute** — the ENA `bw_*_allowance_exceeded` counters rose on sender and
  receiver, i.e. AWS's shaping cap; no drain to the 4.687 Gbit/s baseline
  within 15 min.
- Raw instance store, identical on all three nodes: 200,000 randread /
  110,130 randwrite IOPS (4K, QD32×4), 1,356 MiB/s read, 1,061 MiB/s
  write, QD1 4K ≈ 26 µs read / 27 µs write. These are AWS's per-instance
  caps.

## Results (median [min–max])

| Test | r1 | r3 |
|---|---|---|
| 4K randread QD1 p50 / p99 µs | 129 / 165 | 218 / 261 |
| 4K randwrite QD1 p50 / p99 µs | 48 / 72 | **2,671 / 6,128** |
| 4K randread QD32×4 IOPS | 187,317 [186,211–198,978] | 156,251 [149,352–161,690] |
| 4K randwrite QD32×4 IOPS | 110,130 (= disk cap) | **572 [557–600]** |
| 4K 70/30 QD32×4 IOPS | 185,454 | 7,966 [2,278–13,653] (n=2) |
| 1M seq read MiB/s | 1,356 (= disk cap) | 2,438 (n=2; reads spread over replicas) |
| 1M seq write MiB/s | 1,061 (= disk cap) | 320 (n=2) |
| storage cores (cluster) | 3.01 idle and loaded | 3.01 |

## Findings

1. **r3 small writes are ~190× slower than r1 and do not scale with queue
   depth** (572 IOPS at 128 in flight; 2.7 ms at QD1). A fixed per-write
   cost that serializes. Not the disk (caps far away), not the network (ENA
   allowance counters on the fio node +23 out / +229 in over the whole r3
   run), not the reactor (`spdk-reactor-samples/`: at most 61% busy on
   the writer during 1M seq write, ~20% on the replicas). **This must be
   found and fixed before Flint is compared with replicating drivers.**
   Same signature as kind's dry run (82 IOPS at QD1, r3).
2. r1 reaches the instance's disk caps on throughput and write IOPS: on
   this instance, r1 throughput tests measure the disk, not the driver.
3. QD1 4K reads cost Flint ~100 µs over the raw disk (129 vs 26 µs); writes
   ~21 µs. Reads at QD1 are the r1 path's weak point.
4. Process CPU cannot see a polling engine's load (always ~1.0 core per
   reactor); the harness now samples SPDK reactor busy/idle ticks when a
   driver defines `REACTOR_TICKS` (Flint does).

## Files

`ceilings/` network + raw disk; `flint-r1/`, `flint-r3/` raw fio JSON, CPU
samples, PV, ENA counters (`flint-r3/net-after-manual-*.txt` were read by
hand after the stop); `spdk-reactor-samples/`; `ec2-up.log`; `run-phase1.sh`
as run; `phase1.status`.
