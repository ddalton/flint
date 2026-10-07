# bench — head-to-head CSI benchmark harness

The plan, the questions it answers and the rules it follows are in
[`docs/plans/flint-csi-benchmark-plan.md`](../docs/plans/flint-csi-benchmark-plan.md).
This directory is the harness. It is driver-agnostic: a driver is named
by a StorageClass and by the process patterns its CPU is counted under.

| File | What it does |
|------|--------------|
| `matrix.tsv` | the fio tests (Q1–Q3): name, rw, block size, queue depth, jobs, read mix |
| `run-fio.sh` | one StorageClass: PVC + fio pod, precondition, the matrix × REPS, raw JSON out |
| `sampler.yaml` | a DaemonSet that counts the driver's CPU and RSS on every node during each fio window |
| `report.py` | result directories in; per-test median [min–max] table and a long CSV out |
| `drivers/<driver>/` | StorageClasses, `env.sh` (the driver's `CPU_PATTERNS`), install notes |

## Run one driver

```sh
. drivers/flint/env.sh
SC=flint-bench-r3 DRIVER=flint-r3 CPU_PATTERNS="$CPU_PATTERNS" \
  OUT=results/flint-r3 ./run-fio.sh
./report.py results/flint-r3 results/mayastor-r3 --csv results/summary.csv
```

Defaults are the plan's: 100 GiB volume, 90 GiB file, 30 s ramp, 120 s
measured, 3 repetitions. The whole matrix repeats, so each test's
repetitions are spread over the run, not back to back.

## What the numbers mean

- **fio** runs with `direct=1`, `libaio`, on an ext4 file (the volume's
  filesystem), after one full sequential write of the file. Latency
  percentiles are completion latency (`clat`).
- **storage cores** is the sum over all nodes of CPU used by the processes
  matching `CPU_PATTERNS`, measured from 1 s after the ramp to 1 s before
  the end. A polling engine (SPDK) uses about 1 core per reactor even when
  idle; that is real cost and is counted.
- **idle** is the same CPU measurement with no I/O, taken once per run
  after preconditioning. The report shows it first, so the rise above
  the floor is visible.
- **node busy cores** is from `/proc/stat`. On kind every node shares the
  host's counters, so there it is the whole host, repeated per node.
- **Ranges.** A difference smaller than the overlap of two ranges is
  reported as no difference.

## Phase 0: kind on one host

`drivers/flint/kind-up.sh` brings up a **released** Flint (the published
image, and the chart and `tests/system/kind-spdk.sh` from the release
tag) on a 3-node kind cluster with one LVM volume per node. It exists to
validate this harness. **Numbers from kind are not results:** all nodes
share one kernel and one NVMe, the network is loopback, and SPDK runs
with kind-mode small memory pools.

## Not built yet

pgbench (Q4), RWX (Q5), node loss and rebuild (Q6, Q7), provisioning and
snapshot timing (Q8), the other drivers' install scripts, and the EC2
cluster bring-up.
