# Spark-on-pNFS flight-data benchmark — plan

**Date**: 2026-07-07
**Status**: Proposed — Phase 0 dry-run done 2026-07-07 (see "Phase 0
dry-run results"); two blockers found before the scaling sweep can run
**Prior art**: ADR 0004 (`docs/decisions/0004-pnfs-cross-host-scaling.md`,
cross-host linear scaling with `fio`), `docs/plans/pnfs-performance-plan.md`
(Phases 0–3 landed), `docs/plans/mds-performance-plan.md` (metadata path
not yet scaled), `tests/k8s/pnfs-bench/` (existing k8s harness)

## Thesis

pNFS lets a Spark cluster treat the aggregate NVMe of many nodes as **one
shared POSIX filesystem whose read bandwidth scales with data-server (DS)
count** — without the "one NFS box is the bottleneck" ceiling, and without
giving up a shared namespace the way node-local scratch or manual sharding
would.

Concretely: hold the Spark job and cluster fixed, scale DS count
1→2→4(→8), and show the **input-scan stage time drops ~linearly** while a
single-server flint NFS baseline flatlines at one node's NIC/disk. This is
ADR 0004's headline claim retold through a real application instead of
`fio`.

## Why this is worth doing

ADR 0004 already proved 6.02× aggregate seq-read at N=4 DSes — but with
`fio`, MDS/DS as **host processes** (not pods), and single-run
methodology. Two gaps remain:

1. **No real-application evidence.** A synthetic `fio` sweep is not a
   consumer. A Spark scan is.
2. **No k8s/CSI deployment evidence.** ADR 0004 explicitly lists "the k8s
   deployment story for MDS/DS (DaemonSet, service discovery, CSI wiring)"
   as Phase 4 work. This benchmark forces that path to work end-to-end.

## Dataset — US DOT / BTS On-Time Performance

- **BTS "Reporting Carrier On-Time Performance"** (TranStats): monthly
  CSVs from 1987, ~200M rows, ~30–40 GB raw CSV. Public, canonical,
  Kaggle mirrors exist.
- **Scan-and-aggregate shaped**: group-bys over year/carrier/airport,
  delay percentiles — I/O-bound full-column scans, light on shuffle.
  That's the read-bandwidth-bound profile pNFS accelerates; a
  shuffle-heavy job would measure network, not storage.
- **Scaled to a cold-read size**: convert CSV→Parquet and replicate years
  to ~200–500 GB so the working set exceeds DS page cache and reads are
  genuinely cold (same discipline as ADR 0004's "48 GiB > DS RAM").

Alternative for more scale: OpenSky Network state-vectors (Zenodo/S3).
BTS is the cleaner story and stays.

## Critical design choice — dataset file layout (couples to the MDS plan)

`docs/plans/mds-performance-plan.md` (2026-07-07) shows every
`OPEN/LAYOUTGET/CLOSE/LAYOUTRETURN` funnels through one MDS with
`worker_threads=4` hardcoded and `return_on_close=true` per-open churn
(~230 ms/op under fsstress). Therefore the Parquet layout decides which
path we stress:

- **Few large Parquet parts (512 MB–1 GB, big row groups)** → workload
  stays **data-path-bound**, where pNFS wins. **Use this for the
  headline.**
- **Many small part-files** → hammers the MDS metadata path → measures
  the MDS ceiling, not DS scaling.

This becomes an explicit secondary experiment (Phase 4 below): sweep file
count/size to find the data-path→metadata-path crossover. The result
directly motivates the MDS plan's Tier 1/2 (configurable worker threads,
`return_on_close=false`).

## Cluster — 10 × i4i

**Instance sizing.** ADR 0004's i4i.large (2 vCPU, ~0.78 Gbps baseline
NIC, bursts to 10) is too small for a *sustained* Spark job — you would
fall out of the NIC burst envelope and measure burst-credit exhaustion,
not scaling. Use **i4i.2xlarge (8 vCPU, ~12 Gbps NIC, 1.875 TB NVMe)** or
**i4i.4xlarge (16 vCPU, ~25 Gbps, 3.75 TB NVMe)** so executors have cores
and the NIC baseline is real.

DS backing store must be the **instance NVMe**, not `emptyDir` on the root
volume and not EBS — an EBS-backed DS adds a hidden network hop you'd be
measuring instead of pNFS (ADR 0004 / bench README both flag this).

### Both topologies (run each)

| Config | Layout | Answers |
|---|---|---|
| **Disaggregated** (scaling proof) | 1 MDS node · fixed 4 Spark-executor nodes · vary DS nodes 1→2→4 among the rest | "Does scan time scale with DS count?" — clean attribution |
| **Converged** (realistic) | DS DaemonSet on all 9 workers + Spark executors on all 9 | "How fast can the whole cluster go?" — max aggregate NVMe, muddier attribution |

Rationale for both: disaggregated isolates the scaling variable (only DS
count changes); converged is how you'd actually deploy and gives the
max-throughput headline. Report them as two distinct results, not one
blended number.

## Baseline — single-server flint NFS

Primary contrast, same dataset + same Spark job:

- **Baseline**: single-server flint NFS (a `StorageClass` **without**
  `parameters.layout: pnfs`, backed by one node's NVMe). All splits read
  through one NIC/disk — the honest "just use one NFS box" alternative.
- **Treatment**: flint pNFS (`parameters.layout: pnfs`, striped across N
  DS NVMe) provisioned by the CSI driver, mounted **RWX** on every
  executor.

Same driver on both sides — the driver branches on `parameters.layout`
(see `deployments/pnfs-csi-storageclass.yaml`), so this isolates the
striping win and nothing else.

## Metrics (reuse ADR 0004's instrumentation discipline)

- Spark **job wall-clock** + **input-scan stage time** (event log / Spark
  UI).
- **Aggregate read bandwidth** = sum of DS NIC tx during the scan,
  corroborated on-the-wire as ADR 0004 did.
- **Per-DS byte/tx balance** — reuse the existing 40–60% stripe-balance
  check (`cross-host-bench.sh` already sums bytes per DS).
- **MDS CPU** during the job — should stay low for the large-file layout;
  watch it climb in the small-file sweep.
- **Scaling ratio**: job time / scan bandwidth at N=1 vs 2 vs 4.

## Phases

### Phase 0 — CSI/pod pNFS provisioning (the real lift)

ADR 0004 ran MDS/DS as host processes. This benchmark needs the pod path
working end-to-end:

- MDS Deployment + DS DaemonSet bound to **instance NVMe** (not
  `emptyDir` — `deployments/pnfs-ds-daemonset.yaml` currently uses
  `emptyDir`; point it at a hostPath on the NVMe mount).
- CSI provisioning a **RWX `flint-pnfs` PVC** end-to-end
  (`deployments/pnfs-csi-storageclass.yaml`, chart templates
  `flint-csi-driver-chart/templates/pnfs-{mds,ds}.yaml`).
- **Confirm the driver supports `ReadWriteMany` for pNFS** (it is NFS
  underneath, so it should — but verify; the general README's example PVC
  is RWO and pNFS RWX has not been exercised in the k8s harness).

Gate: a plain pod can mount the RWX PVC and two pods on different nodes
see each other's writes.

### Phase 0 dry-run results (2026-07-07, 3-node cluster)

Executed on a scaled-down cluster (`dilip-spark`, us-west-2a): 3 ×
i4i.2xlarge workers (8 vCPU / 64 GiB / 1.875 TB instance NVMe) + 1
tainted i4i.large control-plane. Converged sanity, **not** the scaling
sweep (only 3 workers; no disaggregation, no N=4). flint 1.11.0 chart
from `dilipdalton/*` on Docker Hub.

**What worked end-to-end:**
- Node prep: these are *minimal* Ubuntu 22.04 AWS nodes missing
  `linux-modules-extra`, so `ublk_drv` / `nvme-tcp` were absent —
  installed the package, loaded the modules, reserved 8 GiB hugepages,
  restarted kubelet to surface `hugepages-2Mi`. (Node-init did **not**
  need vfio/IOMMU: the SPDK target uses an `io_uring` bdev over the
  kernel NVMe device, not a userspace-bound PCI device.)
- flint install: hit a **hostPort 9809 clash with the pre-existing AWS
  EFS CSI node plugin** (both want the standard CSI liveness port) —
  remapped `healthCheck.csiDriverPort=9820`; excluded the control-plane
  (no hugepages) via `affinity`.
- LVS init on `/dev/nvme1n1` on all 3 workers (`blobstore_initialized`,
  ~1868 GB free each). Note: the node-agent `/api/disks/initialize`
  keys on **PCI address** (`0000:00:1f.0`), not the `/dev/nvme1n1` path
  the runbook shows.
- pNFS fleet up (MDS + 2 DS on flint block PVCs), both DSes registered.
- **A real Spark executor read 2.02 M rows of NYC flight data
  (nycflights13) off the pNFS mount** and aggregated it (flights + avg
  delays by carrier, top routes), then wrote the summary back onto pNFS
  and it was re-read from an independent mount. This is the Phase-0
  "the pod path works on real hardware" milestone, achieved via a
  direct MDS mount (see Finding 1 for why not via CSI).

**Two blockers found — both must be fixed before the Phase 2 harness
can use a `flint-pnfs` PVC as designed. These are exactly the kind of
defect a real application surfaces that the `fio` bench did not:**

1. **CSI per-volume mount is not isolated (P1 — blocks the RWX-PVC
   plan).** Every `flint-pnfs` PVC — including a freshly created one —
   NodePublishes the **shared MDS export root**, not an isolated
   per-volume filesystem. Observed: a new PVC's mount showed leftover
   files from a *deleted* PVC plus a 20 GiB sparse file named after each
   PV (`pvc-<uuid>`), and files written through one pod's mount were not
   visible to a later mount of the same PVC (init-container writes lost
   to the app container). The driver appears to create one sparse
   backing file per volume inside a single shared export but mount the
   export root instead of that per-volume image. Consequence: RWX
   isolation and write-persistence across mounts can't be relied on, so
   the plan's "provision a `flint-pnfs` RWX PVC and mount it on every
   executor" step does not hold on this build. **Workaround used:**
   mount the MDS export directly with the kernel NFSv4.1 client
   (`mount -t nfs4 -o minorversion=1,nconnect=8 flint-pnfs-mds:/ ...`)
   and use a subdirectory — which is also the more faithful pNFS data
   path (matches ADR 0004's method). Fix direction: NodePublish must
   mount the per-volume subtree/image, and DeleteVolume must reclaim it.
   Until then, Phase 2 mounts the MDS export directly (a privileged
   sidecar with `mountPropagation: Bidirectional` sharing the mount into
   the Spark container works).

2. **Spark's default `file://` output committer fails on the pNFS
   mount (P2 — affects write-back, not the read path).** Writing Parquet
   fails with `java.io.IOException: Mkdirs failed to create
   .../_temporary/0/_temporary/attempt_.../` — Java `File.mkdirs()` via
   Hadoop `LocalFileSystem` cannot create the committer's deep temp tree
   on the NFS mount. Reproducible even single-threaded (`coalesce(1)`)
   and as root, so it is not a permission or concurrency race — it is an
   NFS close-to-open visibility interaction with the rename-based
   committer. The **read path is unaffected** (full scan + aggregation
   succeed). **Workaround used:** write results with plain file I/O
   (collect the small summary to the driver, `open().write()`) — that
   persists to pNFS cleanly and is re-readable from another mount. Fix
   direction for real Spark-on-pNFS: use a no-rename / direct output
   committer, or a shallow output path, rather than the default
   FileOutputCommitter staging.

Both are recorded here rather than as a new ADR because the scaling
sweep (the ADR-worthy result) hasn't run yet; fold them into that ADR
when it does. Neither blocks the *architecture* — the pNFS data path
reads/writes correctly via the direct mount; they block the *CSI
packaging* and the *default Spark write committer*, respectively.

### Phase 0 baseline (N=2 converged, 2026-07-07) — pre-node anchor

Taken *before* scaling the cluster, so the post-node N=4 run is a real
comparison. Same 3-worker cluster, **converged**: MDS + ds-0 on one
node, ds-1 on a second, Spark client pinned to the third (free) node.
pNFS via **direct kernel NFSv4.1 mount** (`nconnect=8`, 1 MiB
rsize/wsize) — CSI PVC bypassed per Finding 1. Dataset: nycflights13
replicated to **12 CSV files × 1.33 GB = 15.94 GB / 161.65 M rows**
(few-large-file → data-path-bound). Page cache dropped on all 3 nodes
before every run.

| Metric | N=2 (2 DS) | Notes |
|---|---|---|
| Raw seq read, cold (1 client, 1 stream) | **507 MB/s** (483 MiB/s) | `cat *.csv \| dd`, 15.94 GB in 31.4 s — the storage anchor |
| Spark analytical scan (CSV, `local[*]`) | 161.65 M rows aggregated in **180 s** (~88 MB/s eff.) | **CPU/CSV-parse-bound, NOT storage** |
| Per-DS byte balance | **50.0 / 50.0** | ds-0 15.985 GB, ds-1 15.985 GB (identical on both the Spark and raw-read runs) — striping is even at N=2 |
| MDS in data path | no | export shows 4 KiB sparse; all bytes served by DSes |

How to read it:
- **Striping is balanced (50/50)** — rotation works at N=2.
- **The Spark-CSV number is CPU-bound, not a storage number.** Parsing
  161 M CSV rows on 8 vCPU in `local[*]` throttles to ~88 MB/s while
  the same data reads raw at 507 MB/s. Two independent causes, two
  independent fixes — Parquet is the biggest lever but not the only one,
  and does not by itself *guarantee* storage-bound:
    - **Fewer CPU cycles/byte → Parquet.** Columnar + pre-typed binary +
      light Snappy decode is far cheaper than tokenizing text; it also
      compresses (fewer wire bytes) and prunes columns (a 3-of-20-column
      query reads ~3/20 of the data). Shifts the balance toward
      storage-bound; a *full* Parquet scan still spends some CPU on
      decompression.
    - **More cores → distribute.** Parsing is embarrassingly parallel;
      this leg was CPU-bound partly because it was `local[*]` on one
      node. The real sweep runs executors across all 8+ nodes, so parse
      throughput scales with aggregate cores — enough cores makes even
      CSV storage-bound.
  Use **Parquet + distributed executors** for the application runs. The
  parse-free **raw fio/dd read** leg (507 MB/s here) stays the honest
  storage anchor regardless.
- **507 MB/s is single-client, single-stream** (one node NIC; `cat|dd`
  is one sequential pipe) — likely client/stream-bound, not DS-bound,
  so this exact metric may not scale linearly by itself. The N=4 run
  must hold client/stream count fixed and should use **multi-job fio**
  (ADR 0004 style) to expose DS scaling; treat 507 MB/s as the
  single-stream floor, not the aggregate ceiling.

Caveats: converged (DS nodes also run MDS/other pods), single AZ,
single run, CSI bypassed. The staged 15.94 GB dataset is left on the
pNFS export (`/bench/flights`) for reuse. Post-node plan: re-run N=2 on
the *disaggregated* rig first (measure the co-location cost), then N=4,
both with Parquet + multi-client fio.

### Phase 1 — data prep

Download BTS CSVs → a Spark job converts to Parquet (large parts, big row
groups) → land on the pNFS PVC. Doubles as a write-path exercise. Record
write aggregate bandwidth and MDS CPU during ingest.

### Phase 2 — harness

Spark-on-k8s: executors mount the RWX PVC and read via `file://`. A
scan/aggregate job (e.g. avg departure delay by (Year, Carrier); flight
counts by (Origin, Dest); delay percentiles) plus a wrapper capturing
wall-clock + DS tx + MDS CPU. **Extend `tests/k8s/pnfs-bench/`** — it
already has node-pinning, DS-registration wait, and per-DS-byte plumbing
— rather than starting fresh.

### Phase 3 — DS-count scaling sweep (headline)

Disaggregated: fix Spark nodes, vary DS 1→2→4→8. Converged: run at full
width. Both vs the single-server NFS baseline. Produce the headline
table: scan time and aggregate read GiB/s per (topology, N, backend).

Pass criteria (set now, recorded regardless of outcome, ADR 0004 style):
- Aggregate scan bandwidth at N DSes ≥ 0.8·N × the N=1 pNFS number
  (the ADR 0004 gate, now under a real workload).
- Single-server NFS baseline flat as the cluster grows (confirms the
  ceiling pNFS removes).
- MDS CPU flat across N on the large-file layout.

### Phase 4 — file-size/count sweep (feeds the MDS plan)

Same job, vary Parquet part size / count to find the
data-path→metadata-path crossover. Where MDS CPU and per-op latency start
to dominate is the quantified motivation for the MDS plan's Tier 1/2.

### Phase 5 — write-up

New ADR under `docs/decisions/` following the 0004 pattern (rig, results
table, pass criteria, pipelining/tuning A/B if run, honest caveats,
decision). Not created up front — filled in once results exist.

## Honest caveats to bake in

- **NIC burst credits** (ADR 0004's caveat) — mitigated by larger i4i,
  but report sustained vs burst numbers separately; a long Spark scan
  settles below the burst rate on small instances.
- **Ephemeral DSes** — the perf plan Phase 4 ships pNFS as an explicit
  *scratch tier*; losing a DS loses its stripes. Frame the flight dataset
  as reproducible scratch, not durable storage. Do not advertise
  durability.
- **Single-run variance** unless repeats are budgeted; use DS-tx
  corroboration and cross-N consistency as the sanity checks, as ADR 0004
  did.
- **Same-AZ only** — cross-AZ is a different question and its own ADR.

## Non-goals

- Durability / DS replication (tracked in `pnfs-durable-ds-plan.md`).
- Snapshots/clones on pNFS volumes (unsupported in the no-SPDK pNFS mode;
  see README).
- Shuffle-heavy Spark workloads — they measure network, not storage; keep
  the job scan-dominated.
