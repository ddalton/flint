# OCI model registry on flint pNFS — deployment architecture

How to serve large model images from an OCI registry whose blob store
is a single RWX pNFS PVC, striped across the DS fleet. This is the
"many-GB sequential blobs, many concurrent pullers" case that pNFS
striping is best at. Measured numbers below are from the 2026-07/08
fleet campaigns (i4i-class workers) — treat them as expectations, not
guarantees; the trunking scaling rematch is still pending.

Companion docs: `pnfs-operator-runbook.md` (operations + trust model),
`cluster-bringup-runbook.md` (bringup and mount-option verification).

---

## Quick model

```
image pullers (containerd on every worker node)
        │  HTTPS blob GETs
        ▼
  registry Service / Ingress
        ▼
  registry Deployment, N replicas — SPREAD ACROSS NODES (see §4)
        │  each pod: filesystem blob store on ONE shared RWX pNFS PVC
        ▼
  node kernel NFSv4.1 client (nconnect >= 2)
        │ metadata (OPEN/LAYOUTGET)          │ striped data I/O
        ▼                                    ▼
  flint-pnfs-mds (1..k shards)      flint-pnfs-ds StatefulSet (count = M)
  state on a replicated flint PVC   each DS on its own replicated flint PVC
```

The MDS is out of the data path: the client kernel gets a layout at
first I/O and then reads/writes stripes directly against the per-DS
Services. Blob bytes never funnel through a single server.

## 1. Who the NFS client is (read this first)

**The NFS client is the node's kernel, not the registry pod.** kubelet
calls the CSI node plugin (NodeStage), which runs `mount -t nfs4`
against the MDS on the host; the pod gets a bind mount. Everything
else in this doc follows from that:

- Per-client throughput ceilings are **per node**. Registry replicas
  co-scheduled on one node share one `nfs_client` and its ceiling;
  replicas on different nodes each bring their own. Spreading the
  Deployment across nodes is therefore a throughput feature, not just
  an availability nicety.
- `nconnect` is a property of the node's shared `nfs_client` — every
  pNFS PVC on a node mounts the same MDS ip:port, so later mounts
  inherit the first mount's connection count. Set it via the driver
  default, **not** per StorageClass (`values.yaml` documents this).
- NetworkPolicy for port 2049 must admit **node CIDR** traffic
  (`pnfs.networkPolicy.nodeCIDRs`), because mounts originate from the
  node kernel, not from a pod IP.

## 2. Why striping accelerates model blobs

Model image layers are multi-GB content-addressed blobs, read and
written sequentially — the ideal stripe workload.

- `pnfs.flint.io/stripeSize` (default 8 MiB): a file smaller than this
  lives whole on one DS. Model blobs stripe; the small JSON manifests
  land on a single DS each, which is fine and cheap.
- `pnfs.flint.io/stripeWidth` (empty = all DSes): maximum per-file
  bandwidth, but the failure domain becomes the whole fleet. For a
  model store where any blob may be pulled hot, full width is usually
  the right trade; narrow it if you want blast-radius isolation more
  than peak single-blob speed.

Per-file parallelism is the product of three multipliers, and each has
a ceiling:

| multiplier | knob | ceiling |
|---|---|---|
| DSes per file | `stripeWidth` | DS fleet size |
| transports per DS | `nconnect` + `dataServers.multipathServices` | kernel cap: 16 transports/DS |
| bytes per transport | — | ~700–900 MiB/s per sunrpc connection (measured) |

**The trunking gate:** extra per-DS addresses (`multipathServices`)
appear in GETDEVICEINFO and the kernel opens one extra transport per
address — but only if the mount carries `nconnect >= 2`. Below that
the kernel caps the DS client at one transport and **silently refuses
every trunk candidate** (nfs4_set_ds_client, v6.1–v6.8).

Measured reference points:
- `nconnect=4` saturates a 25 Gbps-class wire: 2857 MiB/s (runbd).
- Single-node peak with trunking: 5634 MiB/s (runbi).
- One Linux client kernel tops out ~5–6 GB/s regardless of DS count
  (runbh) — past that, add **nodes**, not width.
- DS-count scaling is real and near-linear in **aggregate across
  clients**; that is exactly what N spread registry replicas are.

## 3. Chart configuration

```yaml
# values-registry.yaml (excerpt)
pnfs:
  enabled: true
  storageClass:
    create: true
    name: flint-pnfs           # reclaimPolicy Retain by default — right for a model store
    stripeSize: ""             # default 8 MiB is right for GB-scale blobs
    stripeWidth: ""            # "" = stripe across ALL DSes
  server:
    enabled: true
    mds:
      count: 1                 # shard only if metadata ops become the wall
    dataServers:
      count: 6                 # the stripe fleet; scaling UP later is safe
      spreadAcrossNodes: true
      multipathServices: 1     # +1 trunked transport per DS; needs nconnect>=2 on the mount
```

Constraints that bite if ignored:

- `images.pnfs` and `images.flintCsiDriver` **must move in lockstep**
  — stripe geometry is half CSI (class parameters), half MDS (the
  width actually granted at LAYOUTGET).
- Snapshots and clones are **refused** for pNFS volumes; don't pair
  the class with a VolumeSnapshotClass.
- Expansion is a metadata-only acknowledgement (capacity is pool-side
  at the DSes); it will not wedge, but it reserves nothing.
- Keep the control token ON (default) and, in production, enable
  `pnfs.networkPolicy` with correct `nodeCIDRs` — the data path is
  `sec=null`, so reachability IS the trust boundary (see the operator
  runbook's "Trust model").
- **AWS: keep DSes and registry nodes in one AZ.** Striping fans every
  byte across the fleet; cross-AZ transfer at $0.02/GB will dwarf the
  instance bill on a model-pull workload.

## 4. The registry itself

Any registry with a filesystem blob store works unmodified: CNCF
Distribution (`registry:3`, storage driver `filesystem`), zot, or
Harbor's registry component. Uploads write to a temp path and
link/rename into the content-addressed store — fine on the shared
namespace.

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: registry-blobs
spec:
  accessModes: [ReadWriteMany]
  storageClassName: flint-pnfs
  resources:
    requests:
      storage: 2Ti            # accounting only; capacity is DS-pool-side
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: registry
spec:
  replicas: 4
  selector:
    matchLabels: { app: registry }
  template:
    metadata:
      labels: { app: registry }
    spec:
      # One replica per node, or co-located replicas share one
      # nfs_client ceiling and buy nothing (§1).
      topologySpreadConstraints:
        - maxSkew: 1
          topologyKey: kubernetes.io/hostname
          whenUnsatisfiable: DoNotSchedule
          labelSelector:
            matchLabels: { app: registry }
      containers:
        - name: registry
          image: registry:3
          env:
            - name: REGISTRY_STORAGE_FILESYSTEM_ROOTDIRECTORY
              value: /var/lib/registry
          volumeMounts:
            - name: blobs
              mountPath: /var/lib/registry
      volumes:
        - name: blobs
          persistentVolumeClaim:
            claimName: registry-blobs
```

Scale reads by adding replicas (more nodes = more NFS clients = more
aggregate DS bandwidth). The registry is stateless above the PVC, so
HPA on CPU/network works.

**Verify the mount actually got what you asked for** (kernel loss is
silent):

```
kubectl exec <registry-pod> -- grep nfs4 /proc/mounts
kubectl -n <ns> logs ds/flint-csi-node -c flint-csi-driver | grep 'pNFS\] mount -t nfs4'
```

If those disagree, the loss is in the kernel, not the driver.

## 5. Network sizing

What a **single registry node** can see, per client NIC:

| wire | usable | what fills it | is striping visible? |
|---|---|---|---|
| 10 Gbps | ~1.2 GB/s | 1–2 connections | No — one DS already exceeds it. Striping helps only fleet-aggregate. |
| 25 Gbps | ~3 GB/s | width ≥ 2–3, `nconnect=4` | Yes — this is the sweet spot; we measured the wire become the ceiling. |
| 100 Gbps | ~12.5 GB/s | nothing single-node | Partially — the client kernel walls at ~5–6 GB/s. Fill the fabric with replicas across nodes. |

AWS note: "up to N Gbps" instance networking is **burst**; budget on
the guaranteed baseline the API reports.

End-to-end honesty check: the puller's wall-clock is often dominated
by **layer decompression on the pulling node** (gzip ≈ low hundreds of
MB/s per stream), not the registry read path. Ship model layers zstd
or uncompressed, or the storage acceleration stops at the registry's
HTTP socket.

## 6. Model-weight distribution for AI inference (KServe / vLLM / Triton)

The registry's blob shape — WORM, multi-GB, sequential, many concurrent
pullers — is also exactly the model-weight distribution problem, and
KServe's modelcar path (`oci://`, experimental v0.12, stabilized v0.14)
makes them literally the same artifact. Weights are storage-bound, not
GPU-bound: PCIe gen4 H2D moves ~26 GB/s measured (gen5: 64 GB/s spec),
while the common baselines deliver aws-cli ~375 MB/s, HF hub ~500 MB/s
default, s5cmd ~4.3 GB/s best-case — a 140 GB 70B-FP16 model is 5–20
minutes of cold start on the download path, plus a second full read
because the classic storage-initializer copies to local disk first.
Storage decides whether scale-to-zero and spot GPU pools are viable;
steady-state decode never touches it.

**The integration ladder — each rung is a shippable product, and only
the last needs libflint:**

| rung | what | per-node ceiling | fork? |
|---|---|---|---|
| L0 | KServe `pvc://` direct-mount (default in current releases: PVC mounted at `/mnt/models`, **no initializer copy**) on a flint RWX file-layout PVC | 2,857 MiB/s deployable (`nconnect=4`, runbd); 5,634 MiB/s trunked peak (runbi, rematch pending) | none |
| L0.5 | Run:ai Model Streamer over the same mount — it reads NFS natively, with concurrent tensor reads overlapped with H2D (measured ~2x end-to-end vLLM readiness, up to ~6x on the load step vs naive loaders) | same wall, but overlap hides H2D entirely | none (registered vLLM `load_format`) |
| L1 | pnfs-block layout mount (kernel ≥ 6.11 nodes): raw NVMe extent reads, no NFS data path; read layouts are non-exclusive by design (many readers, one publisher) — rig-proven 2026-08-10 | device/wire speed, past the kernel `nfs_client` wall | none (CSI class) |
| L2 | libflint loader plugin: `@register_model_loader` (vLLM ≥ 0.10 — pluggable **without forking vLLM**, same seam Run:ai and tensorizer use); userspace extent reads into pinned buffers | NIC-limited | plugin package, no fork of vLLM or KServe |

Load-time expectations for one node (storage-bound, overlap assumed):

| model | bytes | @0.5 GB/s (HF) | @2.85 GB/s (L0) | @5.6 GB/s (peak) | @12 GB/s (L1/L2) |
|---|---|---|---|---|---|
| 8B FP16 | 16 GB | 32 s | 5.6 s | 2.9 s | 1.3 s |
| 70B FP16 | 141 GB | 282 s | 49 s | 25 s | 12 s |

Fleet scale-out is where flint separates from per-node caches: N nodes
scaling up simultaneously each bring their own client wall, aggregate
limited by the DS fleet — no registry throttling, no N× re-downloads,
and the store survives spot GPU churn by construction.

**GPUDirect honesty check:** cuFile over an nvme-tcp block device runs
in **compatibility mode** — a CPU bounce buffer, not peer-to-peer DMA.
No NVIDIA GDS document lists TCP as a direct-path transport; the direct
path needs NVMe-oF over RDMA (or NFSoRDMA) with XFS/EXT4 O_DIRECT. So
the true storage→HBM endgame is the RDMA workstream's territory (the
planned HBv3 rig), not a TCP-rig deliverable. Compat-mode is still fine
in practice — the streamer overlap hides the bounce.

**Forking KServe buys nothing.** KServe stops touching bytes when the
pod starts; every rung above lands in CSI, the mount, or a vLLM loader
plugin. The pilot is rung L0 with zero new code: flint RWX PVC +
`storageUri: pvc://`, measure cold-start TTFT against the S3 baseline.

## 7. KV-cache tier — the verdict (ultracode, 2026-08-10)

Ten-agent investigation (six researchers, synthesis, adversarial panel
both directions + arithmetic check). Full breakeven table in the
session record; the surviving conclusions:

**Yes, in a named band — and the honest headline is "cheaper," not
"faster."** Wall-clock is a *gate*, not the objective: the market
already prices KV hits at ~0.1× input tokens (Anthropic/OpenAI/
DeepSeek), i.e. a hit is valued at ~90% of the prefill *compute* it
frees. A 70B/128k hit frees 36–107 GPU-seconds ($0.03–0.09 at
$3/GPU-hr) while the load consumes ~zero GPU. Under that lens, load
wins on cost at any flint bandwidth whenever the latency fits the SLO
and reuse is real.

- **Wall-clock:** 70B-GQA fp16 KV at flint's 5.6 GB/s peak is parity
  with measured TP4-FP8 prefill (ratios 0.85–1.20 at every prefix);
  at plain `nconnect=4` (~3 GB/s) recompute wins GQA wall-clock. BUT
  with **fp8 KV** (a routine serving choice, 160 KiB/token) the 70B
  breakeven drops to ~2.4–3.4 GB/s — deployable flint is already at
  parity-or-win. **MLA models (DeepSeek-V3/R1, 68.6 KiB/token) are a
  load win in every cell even at 3 GB/s** (2.9–4.2×); DeepSeek's own
  production disk cache (13 s → 0.5 s at 128k) is the existence proof.
- **The reuse gate is everything:** bimodal by workload — coding
  agents ~96% token-weighted hits, multi-turn chat ~91%, vs one-shot
  summarization 6% and code completion 0.1%; >50% of Mooncake's
  blocks are *never* read back, so economics must be hit-rate-weighted
  and the write premium is paid on every miss-destined block.
- **Why shared beats node-local NVMe** (the real null hypothesis, per
  the skeptic panel): capacity *is* hit rate (Mooncake 30%→50% at 50×
  capacity; Novita 40%→80% hit with TTFT −56% and 2× throughput on a
  shared-FS L3), cross-instance hits relax prefix-affinity routing,
  and spot-GPU churn — routine on inference fleets — evaporates
  node-local caches. Those three deltas are flint's actual claim; a
  per-node SSD matches it on bandwidth.
- **Placement kills or saves the economics:** freed-GPU value is
  ~$0.0014–0.004/GB moved (70B, fp8). Any per-GB transfer fee ≥ that —
  cross-AZ at ~$0.02/GB is 5–15× over — erases the entire margin.
  Intra-AZ placement is a hard prerequisite, not a footnote.
- **SLO burden:** this tier sits on the prefill admission path of
  essentially every request in agentic serving (~100:1 prefix:append
  resend). A connector MUST carry a fallback-to-recompute timeout —
  an F69-class stall on the mount becomes a TTFT incident otherwise.

**Integration shape (v1): the FILE layout, not block.** One file per
≥256-token chunk (LMCache convention; 80 MiB for 70B, 17 MiB for
V3-MLA), write-temp + rename, TTL delete — behind existing no-fork
seams (vLLM OffloadingConnector FS spec, per llm-d's shipped 16.8×
precedent on plain POSIX; SGLang HiCache storage backend; LMCache
connector). Never one file per 16-token vLLM block: 8,192 opens per
70B/128k hit ≈ 0.94 s of pure metadata at the measured 8,689
open-cycles/s/shard, vs 512 opens ≈ 59 ms with chunks. The block
layout is **disqualified for KV churn today by its own control
plane**: `fresh_only` never reuses freed extents (a churning volume
hits NoSpace at bytes-*ever*-written), the O(rows) in-transaction
verify caps lifecycle rate ~2.4–10× under a 10k-blob TTL workload's
needs, and eviction's recall→fence→**quarantine** leaks arena under
routine pod preemption with an operator in the loop. Revisit block
for KV only after the three declared debts land (windowed verify,
merge policy, write_zeroes-capable MDS initiator); the hot set stays
in DRAM regardless — flint is the L3 capacity/retention tier, and
RAM-to-RAM pooling (Mooncake TE, 87–190 GB/s RoCE) stays out of reach
until the RDMA workstream delivers.

## Appendix: how CephFS does the same thing

CephFS (the RWX path in ceph-csi) gets its parallelism identically —
files striped into RADOS objects (default 4 MiB) across OSDs, client
talking directly to OSDs, MDS metadata-only — with one substitution:
the client computes placement itself via CRUSH instead of being
granted a layout. Differences that matter for this workload:

- Per-node fan-out is automatic: the Ceph client opens one TCP
  connection per OSD it touches, so wide parallelism needs no
  `nconnect`/trunking equivalent and has no silent-refusal gate.
- The per-node shared-client coupling exists there too (kernel CephFS
  mounts to the same cluster share one client instance by default),
  but it is opt-out (`noshare`), which NFS's shared `nfs_client` is
  not.
- The per-node ceiling still exists — single kernel client throughput
  is bounded by client CPU (messenger checksums, copies), same
  single-digit-GB/s order as ours.
- Exporting Ceph **as NFS** (Ganesha gateway) forfeits the direct
  client→OSD path; every byte funnels through the gateway. flint's
  pNFS keeps standard NFS *and* the direct data path — that is the
  point of it.
