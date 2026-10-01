# flint-lite NFS proxy — step 6, the real-hub rig (plan, not run)

Design of record: `flint-lite-nfs-proxy-design.md` §7a and §8 step 6.

**CHOSEN 2026-09-30: option 1, the build box plus one short AWS
session.** Phases A–D run on the build box (8 cores, 30 GiB, NVMe, Linux
6.12 with tlshd), on kind with real hub images and RustFS, at no cost:
- A: per-hub cost;
- B: wake times, against RustFS, plus one pass against real S3;
- C: proxy CPU per op and the mTLS arm;
- D: about 500 live stubs + 10,000 parked.

One AWS session (~2–3 h, ≈ $1–2, all spot) then covers only what the box
cannot: phase E (real flint-spdk: the subsystem cap and a real
csi-node roll) and the cross-node half of C. It needs 1 × m7i.large
control plane, 2 × i4i.xlarge hubs and 1 × m7i.xlarge client/proxy,
≈ $0.36/h. The rig scripts are debugged on the box first, so the
session runs finished scripts. The full-cloud shape below is kept for
reference. Still to settle before the AWS session: rc images on Docker
Hub or private ECR.

**Box phases A–D RUN 2026-09-30** (`tests/lima/nfs-proxy-census/results-box-step6/`):
- A: a real hub is ~90 MiB RSS at any size, ~5 m CPU idle, 10–12 s to
  Ready.
- B: wakes do not depend on file count: 13–14 s from suspend, ~27 s from
  hibernate, of which ~12 s is outside the pod.
- C: the proxy spends ~0.18 ms CPU per metadata op, about the hub's
  own, so one core serves ~5,500 ops/s; mTLS adds ~10% on metadata,
  ~40% on bulk reads.
- D: 500 live hung the box; at 150 live + 10,000 parked the operator
  sits at 235 MiB / 181 m, with 183 apiserver req/s; the proxy's relist
  measured +100 MiB, the operator's relist was not triggered.

Left for the AWS session: phase E, the cross-node half of C, and D's
relist.

## 1. What it is for

Every scale number so far came from **stubs** (the 3,000-share rig ran
`flint-hub-stub`) or from **kind** (the 20,000-share memory run parked
CRs with no hubs at all). Nothing has measured what a **real hub** costs.
That one constant, times 1,000 active projects, is what decides
multi-volume (§7a "What this means for multi-volume"). The rig answers
five questions:

| # | Question | Why it matters |
|---|---|---|
| A | What does one real hub cost, idle and working, at 1k / 5k / 10k files? | ×1,000 = the active-side node bill; the density case for multi-volume |
| B | How long does a wake take, from suspend and from hibernate (bucket import), by file count? | Hibernate is now the inactive default; a wake is what a returning user waits for |
| C | What does the proxy cost and carry, against a direct mount of the same hub, with and without mTLS? | One proxy replica carries every byte (§7a); step 7 (multi-replica) is sized from this |
| D | Does the control plane hold at ~1,000 live + 10,000 parked? | Operator/proxy CPU and memory, apiserver rates; the relist spike at scale, **estimated, never measured** |
| E | What limits flint-spdk puts on hubs per node, and does a real csi-node roll recover? | The SPDK subsystem cap (never checked); `noderoll` against REAL EIO (kind only proved the control flow) |

## 2. Cluster (all spot, control plane included; us-west-1)

Spot prices read 2026-09-30 (`describe-spot-price-history`):

| Pool | Instances | For | $/h each | $/h |
|---|---|---|---|---|
| control plane | 1 × m7i.xlarge | apiserver, etcd | 0.10 | 0.10 |
| hubs | 4 × i4i.xlarge (NVMe) | flint-spdk + 30 real hubs (~8/node) | 0.105 | 0.42 |
| proxy | 1 × m7i.xlarge | flint-nfs-proxy alone, so its CPU is measurable | 0.10 | 0.10 |
| clients | 2 × m7i.xlarge | load generators (kernel NFS client, tlshd) | 0.10 | 0.20 |
| stubs (phase D only) | 10 × m7i.large | 1,000 `flint-hub-stub` pods (110 pods/node) | 0.05 | 0.50 |

Two i4i.xlarge cost less than one i4i.2xlarge (0.27–0.33 $/h), hence four.
Operator and gateway run on the proxy node.

**Cost estimate:** $0.82/h for phases A–C and E, plus $0.50/h while phase
D's stub pool is up (~2 h). About 8–9 h of cluster time gives
**≈ $8.5 compute**. On top of that: S3 seeding (30 hubs × ~5k files ≈
160k PUTs ≈ $0.80), EBS roots and one NLB (≈ $0.50). **Total ≈ $10–12;
proposed cap $20.** It can be split into two sessions (A–C, then D–E)
with a teardown between. Every drill records a zero-set teardown check,
as in earlier campaigns.

## 3. Before any cluster (on the box, no cost)

1. **Images.** Published 1.56.0 carries neither `flint-nfs-proxy` nor
   `flint-nfs-client-identity`, nor any of steps 2b–5. So the rig needs
   release-candidate images built from HEAD: `flint-pnfs` (hub) and
   `flint-lite-operator` (operator, proxy, gateway, client identity).
   **Choice for you:** push an rc tag to Docker Hub (as earlier campaigns
   did, e.g. `1.22.0-rc4`; outward-facing), or a private ECR repo. The CSI
   driver (flint-spdk) is planned at published 1.56.0. Before the run,
   diff its source tree against the tag to confirm nothing on the
   flint-spdk staging path changed since; if something did, it gets an
   rc too.
2. **Rig scripts**, each checked on kind on the box first where possible:
   - `seed`: FlintShares with a real S3 bucket, N files each written
     through the proxy mount; **guard:** the bucket's object count under
     each prefix equals N before any measurement.
   - `collect`: metrics-server plus a sampler of per-pod CPU and RSS
     (hubs, proxy, operator) and node totals, at fixed intervals into TSV.
   - `wake`: force suspend or hibernate (annotation), then time
     first-byte and a full `ls -R` through the proxy. **Guards:** the PVC
     was really gone (hibernate) and the serverId changed after.
   - `ab`: the `hub-perf-drill.sh` workloads (metadata storm, small
     files, sequential), through the proxy vs a direct mount of the same
     hub. Three pairs, ranges reported. mTLS on and off as separate arms.
     **Guard:** the proxy's second-target counter stays 0, and the
     proxy's own byte count matches the workload.
   - `fleet`: `fleet-scale.sh` adapted to proxy mode, plus
     `step5-scale-kind.sh`'s CR-only seeding for the parked 10,000.

## 4. Phases

**A. Per-hub constants (~1.5 h).** 30 real hubs, 10 each at 1k, 5k and
10k files (sizes drawn from a mixed 1 KiB–1 MiB distribution). Measure
after 10 min idle: RSS, CPU, `state.db` size, pod start to Ready. Then
under a light agent-like load (the `ab` metadata workload at a low,
fixed rate) on 10 hubs at once: RSS and CPU per hub. **Output:** a
per-hub cost table, and the 1,000-hub extrapolation with its assumptions
stated.

**B. Wake times (~1.5 h).** Per file-count tier, three wakes each from
suspend (pod start) and from hibernate (CR-only → import from the
bucket). Report first-byte and full-listing times as ranges. **Output:**
the latency a returning user sees, and whether hibernate-by-default
(1 day) needs a different threshold.

**C. Proxy throughput and cost (~2 h).** Clients on the client pool,
hubs on the hub pool, proxy alone on its node. Arms: direct / proxy /
proxy + mTLS. For each: ops/s and MiB/s per workload, and proxy CPU per
1k ops and per 100 MiB/s. Then 2, 4 and 8 concurrent clients to find the
proxy's first saturation. **Output:** the step 7 sizing (at what load a
single proxy replica is the limit). Cilium WireGuard is on by default on
trove clusters and caps inter-node throughput. **Decide per arm and
record it.**

**D. Control plane at scale (~2 h, stub pool up).** 1,000 live stub hubs
plus 10,000 CR-only parked shares, all behind the proxy. Measure operator
and proxy CPU/RSS settled, apiserver request rates by resource (as
runbv did), and a **forced relist**: restart the operator's watch by
bouncing the apiserver, then record the operator's memory spike. That is
the number the 1 Gi limit rests on. **Guards:** the counts, the operator
and proxy restart counts, and the HubReachable count at 1,000.

**E. flint-spdk (~0.5 h).** Read the target's configured subsystem cap
on a hub node, and stage volumes past 100 on one node to find the real
limit. Then a real `flint-csi-node` rollout under 8 live hubs: the hubs
see real EIO, and `noderoll` must restart each. **Guards:** a writer on
each hub resumes with no gap, and exactly one restart per hub.

## 5. How the numbers decide multi-volume

- **Density.** If the idle RSS per real hub is R MiB, 1,000 active hubs
  reserve ~R GiB of node memory, plus a 110-pods-per-node floor of
  ~10 nodes. Multi-volume pays when R × 1,000 costs clearly more than
  those ~10 nodes' memory. R = 128 MiB (today's request) ≈ 125 GiB, about
  4 i4i.xlarge of memory alone. The rig replaces 128 with a measurement.
- **Rolls.** Phase E gives the restart cost per hub on a roll; at ~100
  hubs per node, that times 100 is the outage multi-volume would shrink.
- **Neither** makes the proxy optional (§7a): it is kept either way.

## 6. Risks

- **Spot reclaims mid-phase:** record it, and rerun that phase. A
  reclaim during E is itself a useful data point.
- **Kernel on trove nodes:** the mTLS arm needs Linux ≥ 6.5 and tlshd on
  the client nodes. Check the AMI first; if it is older, C runs without
  the mTLS arm and says so.
- **1,000 stubs** need 10 small nodes; 500 halves that and is still
  5× the last rig. **Choice for you.**
- **Nothing is deployed:** this rig migrates nothing and upgrades
  nothing. Every cluster is created fresh and torn down.

## 7. Decisions needed before provisioning

1. Approve the cluster and the ≈ $10–12 estimate ($20 cap), in one
   session or two.
2. rc images: Docker Hub rc tag or private ECR.
3. Phase D at 1,000 stubs, or 500.
4. mTLS arm in phase C (needs kernel ≥ 6.5 on the client pool).
