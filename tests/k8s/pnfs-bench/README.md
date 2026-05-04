# Cross-host pNFS bench (Kubernetes)

Goal: produce the publishable headline number for "does the architecture
scale cross-host?" — the single biggest unknown in the project at HEAD.
Single-host loopback bench (`make test-pnfs-nconnect`) hit a 1.6× write
ceiling against single-server NFS, but the bottleneck was below the
per-connection layer (kernel page cache + APFS journal contention,
loopback TCP saturation). This bench separates the kernels.

## Topology

The harness assumes **1 control + 4 worker** Kubernetes cluster with
each role pinned to its own kernel:

```
control-node    : k8s API + scheduler. NOT used for workload (would
                  inject scheduler/etcd noise into the bench numbers).
worker-1        : MDS pod (control-plane only — no I/O on this node).
worker-2        : DS1 pod (local NVMe / hostPath PV; serves stripes).
worker-3        : DS2 pod (same shape as worker-2).
worker-4        : client pod — fio in a Job, mounts via NFSv4.1 against
                  worker-1's MDS Service.
```

The 4-worker layout produces the headline N=2 number ("does cross-host
DS striping beat single-server"). For the **scaling-curve** answer
(N=1 vs N=2 vs N=3 DSes), point the script at workers in order and run
the sweep at each N — the harness supports this via `DS_NODES`.

If you only have 3 workers, the bench still runs (MDS + client share
worker-1, DS1 on worker-2, DS2 on worker-3). The headline number is
~10–15% noisier from MDS+client co-location. Documented honestly in
the output table.

## Per-host requirements

* **Linux 5.10+** with NFSv4.1 client modules. Ubuntu 22.04 / 24.04 fine.
* **≥10 GbE between workers.** 1 GbE is the floor that *will* saturate
  before the architecture does (a single DS can drive ~110 MiB/s on
  1 GbE; we already hit 270 MiB/s loopback). On 1 GbE the client NIC
  is the ceiling, not us — bench is then "does it work cross-host"
  not "how fast does it scale."
* **Local NVMe / fast SSD** on the DS workers (workers 2 & 3). EBS gp3
  / NFS-attached storage on the DS hosts adds a hidden network hop you'd
  be measuring instead of pNFS. Use ephemeral instance store on EC2
  (`c6gn`/`m7i` with NVMe) or `hostPath` PVs onto a local SSD device.
* **Same AZ / same switch.** Cross-AZ is an interesting follow-up but
  a different question and warrants its own ADR.

## Running

```bash
# 1. Build + push the pNFS image to a registry the cluster can pull.
#    (Existing build script in `deployments/build-and-deploy-fixes.sh`
#    handles this; the bench reuses the resulting image tag.)

# 2. Set node names + run.
export KUBECONFIG=~/.kube/your-cluster
export MDS_NODE=worker-1
export DS_NODES="worker-2 worker-3"
export CLIENT_NODE=worker-4
export PNFS_IMAGE=docker-sandbox.infra.cloudera.com/ddalton/pnfs:latest

make test-pnfs-cross-host

# 3. Result:
#    tests/k8s/pnfs-bench/results/cross-host-results-YYYY-MM-DD.tsv
#    Markdown table printed to stdout for paste-into-doc.
```

Set `DS_NODES="worker-2"` for the N=1 baseline; `"worker-2 worker-3
worker-4"` for N=3 if you have a 5-worker cluster.

## What the bench measures

Same shape as `make test-pnfs-nconnect` for direct comparison:

* **Workload sweep:** `bs={4K,1M}` × `jobs={1,4,8}` × `{read,write}`.
* **Aggregate MiB/s** per fio run, dumped to TSV with one row per `(N_DS,
  bs, rw, jobs)` cell.
* **Per-DS allocation** at the end (sanity that bytes really crossed
  both hosts, not just one).

## What the bench does NOT measure

* **Multi-client correctness.** Single client only. Listed as the next
  follow-up after this bench produces clean numbers.
* **Latency at small bs.** We measure throughput shape, not p99 latency.
* **Cold cache vs warm cache.** Client cache is dropped between phases;
  server-side host page cache is hot.

## Pass criterion

Honest target on 25 GbE cross-host with 2 DSes:

* **WRITE aggregate ≥ 1.8× single-server NFS.** Below this, the
  per-host pNFS overhead is eating the parallelism win and the
  architecture isn't earning its complexity.
* **READ aggregate ≥ 1.5× single-server NFS.** Reads tied at loopback
  due to TCP saturation; cross-host should finally exceed.

Anything ≥ 2.5× write at N=2 confirms the architectural promise; the
script reports the slope so the curve at higher N is comparable.
