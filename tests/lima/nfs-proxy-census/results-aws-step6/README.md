# step 6 on AWS: phase E and the cross-node half of C (2026-10-02)

Plan: `docs/plans/flint-lite-nfs-proxy-step6-rig-plan.md`. Rig:
`../step6-aws.sh`. The box phases A–D are in `../results-box-step6/`.

**The cluster:**
- trove project `nfsp6`, us-west-1, 4 × i4i.xlarge, all spot (control
  plane included);
- Amazon Linux 2023, kernel 6.18, Kubernetes 1.34, Cilium (WireGuard
  encryption on, trove's default: every cross-node byte is encrypted);
- up ~2 h, about $0.85; torn down 19:01Z;
- **verified empty:** 0 non-terminated instances, 0 volumes, 0 open spot
  requests and 0 non-default security groups in us-west-1, and trove
  reports no orphans and no projects.

**What ran:**
- flint-spdk from HEAD `66e47b9a`: driver, hub and operator images
  `dilipdalton/*:step6-66e47b9a` (unreleased tags), with `spdk-tgt` 1.7.0
  as the chart pins it;
- hubs on two nodes, the proxy, operator and client on the third;
- a RustFS bucket in-cluster.

## E1: the SPDK target's subsystem cap (`E-cap.txt`)

`max_subsystems` is **1,024** on every node: SPDK's default, which
flint never overrides. The design's "about 100 per node" was a guess,
and it was wrong by 10×. At 1,000 active hubs over about 10 nodes
the target is nowhere near its cap.

## E2: a real flint-csi-node roll under 8 live hubs (`E-roll*.txt`)

- **Setup:**
  - 8 hubs on flint-spdk disks, 4 per hub node;
  - one writer per hub through the proxy: a numbered record per line,
    fsync'd, about 4 a second.
- **The roll:** the DaemonSet (OnDelete) was restarted; flint's roll
  controller replaced the pods one node at a time, all four in 214 s.
- **Result: all 8 hubs rode through it.**
  - Every log held exactly 1..1,050, with no gap and no repeat, and
    N equal to the last acknowledged write.
  - The worst fsync stall per writer was 4.9–5.1 s: one stall, while
    that node's SPDK target restarted.
  - Every hub kept its pod (0 restarts) and stayed Ready.
- **Contrast:** the v1.14-era behaviour noderoll was built for (EIO to
  every consumer on a rolled node) does not occur on HEAD.
- **Finding: noderoll can never fire on the real chart.** It reads
  `containerStatuses`, but the chart runs `spdk-tgt` as a native
  sidecar (an init container with `restartPolicy: Always`), whose
  status is in `initContainerStatuses`. The kind drill (step 5) used a
  stand-in DaemonSet with `spdk-tgt` as a regular container, so it
  could not see this. On HEAD the restart is not needed; noderoll is
  dead code on real deployments, not a live bug. Deciding whether to
  fix it or remove it is open.

The first roll (`roll-1-no-writers.log`) ran with no writers, because a
`pkill -f` matched its own shell. It is kept, but it measures nothing
about writes.

## C, cross-node: direct vs proxy (`C-run.log`)

- **Layout:**
  - the hub (c1, 5,000 × 4 KiB files) on node 1;
  - the client pod and the proxy on node 3;
  - client→hub (direct) and proxy→hub cross the network, encrypted by
    WireGuard.
- **Method:**
  - three reps, interleaved;
  - wall times include one `kubectl exec` each (about 0.3 s), which
    hits both arms equally;
  - the proxy's CPU is read from the node (`/proc/<pid>/stat`), since
    its image has no shell.

| Workload | direct | through the proxy | proxy CPU |
|---|---|---|---|
| stat (every file, twice) | 888–927 ops/s | 647–673 ops/s (~72%) | 0.35 ms per op |
| create + write 4 KiB + unlink | 373–392 ops/s | 312–329 ops/s (~84%) | 0.41–0.43 ms per op |
| sequential write, O_DIRECT, 512 MiB | 132–145 MiB/s | 127–132 MiB/s | 0.9–1.1 s per GiB |
| sequential read, O_DIRECT, 512 MiB | 215–230 MiB/s | 219–244 MiB/s | 0.9–1.1 s per GiB |

- **Cross-node, the proxy costs about 28% of metadata throughput and
  nothing measurable on bulk transfer.** Bulk is bound elsewhere (the
  WireGuard path and the hub).
- **Its CPU per metadata op is about 3× the box figure:** 0.35 ms
  against 0.11 ms with host processes and 0.18 ms on kind. On an EC2
  VM each of the proxy's two extra network round trips costs more. So
  one proxy core serves about 2,800 stat ops/s here. That is the number
  to size step 7 by on cloud nodes.

## Rig defects found and fixed on the way

All are fixed in `step6-aws.sh`; each surfaced on the real cluster:
- **flint chart:**
  - trove pre-installs flint chart 1.43.0. An in-place upgrade hit an
    immutable StorageClass provisioner and an OnDelete DaemonSet, so
    the rig now does a fresh install with pNFS off.
- **Cluster:**
  - there is no default StorageClass;
  - the NVMe disks are uninitialized (initialized through the node
    agent);
  - there is no LoadBalancer controller (the proxy Service is
    ClusterIP).
- **Rig code:**
  - a leftover port-forward sent every disk-init call to node 1; the
    rig now checks that the agent's reply names the node;
  - `pkill -f` matched its own shell, twice; it is now anchored;
  - `rollout status` refuses an OnDelete DaemonSet;
  - macOS bash 3.2 has no associative arrays;
  - a 512 MiB staged file put the 8 GiB root disk under disk pressure;
    sequential data is now streamed;
  - the proxy's CPU read returned 0 silently; it now fails loudly.
