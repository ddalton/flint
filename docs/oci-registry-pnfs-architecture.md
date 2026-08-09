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
