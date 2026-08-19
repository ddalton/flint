# flint-lite

One pod, real POSIX, zero consumer footprint. This chart deploys the
**flint-lite hub**: `flint-pnfs-mds` in `mode: standalone` — a full
NFSv4.2 server (enforced byte-range locks, close-to-open coherence,
atomic rename) with the pNFS machinery off. It renders exactly **four
objects** (ConfigMap, RWO PVC, Service, single-replica Deployment) and
nothing else: no CSI stack, no SPDK, no CRDs, no privileged pods.

Consumers need **nothing from flint installed**: any cluster mounts the
hub with the NFS client already in every node's kernel (in-tree `nfs:`
PV, or nfs-subdir-external-provisioner for dynamic PVC-per-workspace).

The full operator guide — consumer recipes, the S3 tier, DR runbook —
is [`docs/flint-lite.md`](../docs/flint-lite.md). A full pNFS/SPDK
cluster is the separate `flint-csi-driver-chart`'s job.

Running a **fleet** of hubs (bucket-per-tenant)? One helm release per
volume stops scaling somewhere around ten. `flint-lite-operator-chart`
manages them as `FlintShare` custom resources instead — same four
objects, rendered by a controller
([`docs/flint-lite-operator.md`](../docs/flint-lite-operator.md)),
adopting an existing release of this chart in place.

## Quickstart

```bash
helm install flint-lite ./flint-lite-chart \
  --namespace flint-lite --create-namespace
```

The hub's PVC comes from the **cluster's default StorageClass** (any
CSI driver's RWO volume — EBS, GCE PD, Ceph, local-path); set
`persistence.storageClassName` to choose one, `persistence.size`
(default 20Gi) for the working set.

> **Image note:** `mode: standalone` and the S3 tier need hub image
> **1.26.0 or newer** (multi-arch: amd64 + arm64) — the chart's
> appVersion default. Older MDS binaries refuse the mode at boot.

## With the S3 cold tier

The PVC becomes the working set; durability lives in a bucket (a lost
PVC is a rebuild from the bucket, not a loss):

```yaml
# values-tier.yaml
tier:
  enabled: true
  bucket: my-team-flint            # must already exist, versioning ON
  keyPrefix: vol1/                 # ONE prefix = ONE volume = ONE hub
  credentialsSecret: flint-tier-s3 # "" = ambient (IRSA / node role)
```

```bash
kubectl -n flint-lite create secret generic flint-tier-s3 \
  --from-literal=AWS_ACCESS_KEY_ID=... \
  --from-literal=AWS_SECRET_ACCESS_KEY=...
helm install flint-lite ./flint-lite-chart \
  --namespace flint-lite --create-namespace -f values-tier.yaml
```

Rules that matter (details and the why in `docs/flint-lite.md`):

- The bucket must **pre-exist with versioning on**; the hub never
  creates buckets.
- **One prefix = one volume = one hub.** A second release on the same
  prefix is fenced by the volume epoch — an availability outage on
  purpose, never corruption.
- `.flint/` under the prefix is reserved for tier control objects.
- With the tier on, **first startup can legitimately run minutes before
  the NFS port opens** (epoch claim, DR import — all pre-listener). The
  startupProbe budgets for it; a Running/not-Ready pod is working.
- Tuning knobs go through `tier.settings` and are schema-checked at
  render — a typo'd knob refuses instead of silently defaulting.

## Values

| Key | Default | Meaning |
|---|---|---|
| `image.repository` / `image.name` / `image.tag` | `dilipdalton` / `flint-pnfs` / chart appVersion | Hub image; `image.ref` overrides the whole reference |
| `service.type` / `service.port` | `ClusterIP` / `2049` | `LoadBalancer` for cross-cluster; `service.nodePort` with `NodePort` |
| `persistence.storageClassName` | `""` (cluster default) | Any CSI driver's RWO class |
| `persistence.size` | `20Gi` | Size for the working set, not the dataset |
| `tier.*` | disabled | S3 cold tier — see above and `values.yaml` |
| `logLevel`, `resources`, `nodeSelector` | `info`, `{}`, `{}` | Hub pod knobs |

Migrating from the old `flint-csi-driver-chart` lite profile: the keys
are identical minus the `lite.` prefix (`lite.tier.bucket` →
`tier.bucket`).
