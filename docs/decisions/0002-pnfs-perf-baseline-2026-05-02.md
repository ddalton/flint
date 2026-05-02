# ADR 0002: pNFS perf baseline — first head-to-head measurement

**Date**: 2026-05-02
**Status**: Informational (a measurement, not a decision)
**Harness**: `tests/lima/pnfs/bench.sh`
**Builds tested**: `kind-no-spdk` HEAD (commits through `97f496a`)

## Why

ADR 0001 punted the architecture work; ADR 0001's "what should we do
this week" pointer was: validate that pNFS actually beats single-server
NFS for the workloads we want to ship before investing in CSI integration
and audit Tasks #4/#5. This document records the first measurement.

## Setup

Single Mac host running everything; Lima VM running the kernel NFS
client and `fio`.

```
macOS host
├── flint-nfs-server  on :20480 (single-server baseline)
├── flint-pnfs-mds    on :20490
├── flint-pnfs-ds     on :20491 (DS1)
└── flint-pnfs-ds     on :20492 (DS2)

Lima VM "flint-nfs-client"
└── kernel NFSv4.1 client + fio-3.36
    └── mount: nfs4, minorversion=1, nconnect=4, rsize=wsize=1 MiB
```

Workload: `fio --bs=1M --numjobs=4 --size=256M` (1 GiB total).
`--direct=0`, `--end_fsync=1`, `--ioengine=libaio --iodepth=16`.
Client page cache dropped (`echo 3 > /proc/sys/vm/drop_caches`)
between phases. Same fio invocation against both servers; only the
mount target differs.

## Results

| Workload         | single-server NFS | pNFS (2 DSes) | ratio |
|------------------|------------------:|--------------:|------:|
| Sequential WRITE | 132.6 MiB/s       | 278.5 MiB/s   | **2.10×** |
| Sequential READ  | 270.3 MiB/s       | 267.4 MiB/s   | 0.99× |

### Striping verification

Per-DS apparent file sizes after the run (via `stat -f %z`):

```
DS1:  ~1.04 GiB across 4 sparse files (260 MiB / file)
DS2:  ~1.07 GiB across 4 sparse files (268 MiB / file)
MDS:  0 bytes
```

Each DS file is *sparse* — the kernel writes the full logical extent
to both DSes (one as the canonical file, the other as the mirror's
phantom shape) but each DS only allocates blocks for its assigned
8 MiB stripes. The 1 GiB workload split across 2 DSes at 8 MiB stripe
size = ~64 stripes per DS, alternating. MDS holds metadata only,
matching RFC 8881 §13.

LAYOUTGET fired for every fio job: `grep LAYOUTGET /tmp/flint-pnfs-mds.log`
shows 4 invocations returning 2 segments each. The kernel actually
used the layout — this is not a fall-back-to-MDS-direct measurement.

## What this tells us

### The good

**Striping wins for writes — 2.10× speedup.** Two parallel TCP
connections (kernel client → DS1, kernel client → DS2) sustain double
the aggregate write throughput of a single-TCP path to one server.
Each DS commits its share of bytes independently; the bottleneck of a
single server's `pwrite + fsync` serialization disappears.

This is the result that justifies pNFS for write-heavy workloads
(checkpoint storms, log fan-in, bulk ingest). It's a *real* protocol
win, not a measurement artifact.

### The neutral

**Reads are flat.** 270 MiB/s on both configurations means we're
hitting some shared bottleneck other than per-server NFS protocol
overhead — likely loopback TCP throughput between macOS host and
Lima VM kernel, or macOS HFS+/APFS read-cache rate, or fio's own
maximum sequential-read drive on this client.

For sequential reads on a single Mac host, single-server NFS is
*already* fast enough that adding striping doesn't help. To see
read parallelism wins from pNFS, we need either (a) a real
cross-host environment where the per-server NIC is the bottleneck,
or (b) more reader pods so per-client throughput stops being the
limit. The Lima setup has neither.

### The honest caveat

This benchmark cannot answer the cross-host scaling question — the
one that matters most for the ML/Spark/genomics workloads pNFS
targets. With MDS+DS1+DS2 sharing one Mac kernel, "wire" traffic is
just kernel-internal memory copies. A real result requires:

- 3+ physical machines (one per pNFS component, or one MDS + N DSes
  + M clients).
- Multiple client pods, each pulling sustained sequential reads, to
  exercise aggregate bandwidth scaling (the "100-pod ML data loader"
  shape).
- Per-NIC throughput as the actual bottleneck.

Until then, "pNFS read aggregate scales linearly with DS count" is a
*hypothesis* supported by protocol design and the write result, not
a measurement.

## Implications for next steps

The write win is a green light to invest. **Specifically:**

1. **CSI integration becomes worth doing.** A `layout: pnfs`
   StorageClass parameter that writes 2× faster than the existing
   `layout: single-server` path is a shippable feature even on a
   single host. Workloads that fan out parallel writes (checkpointing,
   log ingest, batch processing output) get the benefit immediately.

2. **The next benchmark must be cross-host.** Before claiming any read
   scaling, set up a real 3-node test cluster (cloud VMs or the user's
   bare metal) and re-run with M client pods × N DSes. The Lima number
   stops being load-bearing past this point.

3. **Audit Tasks #4 + #5 are still required for production.** A 2×
   write win on an unreliable data plane (DS death corrupts in-flight
   writes) is not shippable to a real customer. CB_LAYOUTRECALL +
   state persistence remain the production-readiness gate.

## Reproducing

```bash
# All three services need to be built (already produced by
# `make build-pnfs` and `make build-nfs-server`).
cd spdk-csi-driver && cargo build --release \
  --bin flint-pnfs-mds --bin flint-pnfs-ds --bin flint-nfs-server

# Lima VM must be up; fio is auto-installed on first run.
make lima-up

# Run the benchmark — 2-3 min total.
tests/lima/pnfs/bench.sh
```

JSON per-run results land in `/tmp/flint-pnfs-bench-{single,pnfs}-{read,write}.json`
for inspection.

## Re-running on real hardware

When you next have a multi-node test cluster, the harness needs three
changes:

1. Replace `host.lima.internal` with the cluster-routable MDS IP.
2. Run DSes on different physical nodes (not different ports on one host).
3. Run multiple client pods in parallel to drive aggregate load.

The fio command and the JSON-parsing logic stay the same. The pass
threshold should also stay (read ≥ 1.3× single-server) — that's the
honest claim we're making.
