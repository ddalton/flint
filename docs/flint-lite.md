# Flint-lite — the standalone hub

One pod, real POSIX, zero consumer footprint. Flint-lite runs
`flint-pnfs-mds` in `mode: standalone`: the full NFSv4.2 server —
enforced byte-range locks, close-to-open coherence, atomic rename —
with the pNFS machinery off. No DS fleet, no SPDK, no CSI stack on the
hub cluster, and **nothing at all installed on consumer clusters**:
they mount the hub with the NFS client already in every node's kernel.

Choose lite when the workload is agent fleets / shared workspaces whose
tools need POSIX (git, sqlite, compilers) across one or many clusters,
and one hub's throughput is enough. Choose the full pNFS profile when a
single pod's NIC is the bottleneck — the hub speaks the same protocol
and stores the same volume format, so graduation is adding DSes and
turning layouts on, not a migration.

## The hub

```bash
helm install flint-lite ./flint-csi-driver-chart \
  --namespace flint-lite --create-namespace \
  --set lite.enabled=true
```

`lite.enabled` is a **profile**, not a component: it renders exactly
four objects (ConfigMap, RWO PVC, Service, single-replica Deployment)
and suppresses everything else in the chart. It refuses to render
alongside `pnfs.enabled` — a hub cluster gets this release and nothing
else from this chart. Requires an image newer than 1.25.2 (the first
tag carrying `mode: standalone`).

Storage: the PVC uses the **cluster's default StorageClass** unless
`lite.persistence.storageClassName` says otherwise. Any CSI driver's
RWO volume works — EBS gp3, GCE PD, Ceph, local-path — the hub writes
plain files; NVMe is a performance choice, never a requirement. Size
for the working set (`lite.persistence.size`, default 20Gi).

Restart semantics: strategy `Recreate` plus the RWO attach fence means
two hub pods never run concurrently, and the sqlite state on the PVC
keeps the server id stable — client filehandles, locks and sessions
survive a pod restart.

## Reaching the hub from other clusters

NFS is one long-lived TCP flow to one port; anything that routes TCP
works.

- **Same cluster / flat or peered pod networks**: the default
  `ClusterIP` Service is enough.
- **Across clusters over a cloud LB**: `--set lite.service.type=LoadBalancer`
  (use `lite.service.annotations` for internal-LB annotations). Mind LB
  idle timeouts — NFS connections are long-lived and quiet periods are
  normal; prefer peered/flat networks where available.
- **Keep hub and consumers in one AZ** where you can: inter-AZ bytes
  are billed both directions and a chatty POSIX workload adds up.

## Consumers — recipe A: static PV (zero install)

Nothing to install. The in-tree `nfs:` PV type makes the kubelet mount
the hub with the node kernel's NFS client:

See `deployments/lite-consumer-static-pv.yaml` for the full manifest.
The essentials:

```yaml
apiVersion: v1
kind: PersistentVolume
metadata:
  name: flint-lite-shared
spec:
  capacity: { storage: 100Gi }   # informational for NFS PVs
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions: [nfsvers=4.1, proto=tcp, hard]
  nfs:
    server: 203.0.113.10   # the hub Service's LB IP / routable address
    path: /
```

Bind it with a PVC (`storageClassName: ""` + a `volumeName` pin), and
every pod that mounts the PVC shares the hub's namespace. Kubelet
mounts once per node; pods on one node share a kernel client and page
cache, while cross-node and cross-cluster coherence (locks,
close-to-open) is the hub's job.

## Consumers — recipe B: dynamic PVC-per-workspace

For StorageClass semantics — each PVC becomes a fresh subdirectory of
the hub — deploy the standard
[nfs-subdir-external-provisioner](https://github.com/kubernetes-sigs/nfs-subdir-external-provisioner)
in each consumer cluster:

```bash
helm repo add nfs-subdir-external-provisioner \
  https://kubernetes-sigs.github.io/nfs-subdir-external-provisioner/
helm install flint-lite-workspaces \
  nfs-subdir-external-provisioner/nfs-subdir-external-provisioner \
  --set nfs.server=203.0.113.10 \
  --set nfs.path=/workspaces \
  --set storageClass.name=flint-lite \
  --set nfs.mountOptions='{nfsvers=4.1,proto=tcp,hard}'
```

Then `storageClassName: flint-lite` on any PVC mints a workspace. The
provisioner is a single small Deployment; the data path is still the
node kernel's NFS client straight to the hub.

## Operational notes

- **One hub per dataset.** The hub is the coherence authority; two hubs
  over one tree is split-brain. (When the S3 tier ships this becomes
  "one hub per bucket", enforced by a bucket epoch guard.)
- **Backup today is the PVC** (snapshot it with its CSI driver's
  tooling). The S3 cold tier will move durability into the bucket.
- **Ceiling**: one pod — every byte through one NIC and one process.
  Measured on the dev rig: hundreds of MiB/s sequential, a few hundred
  metadata ops/s per client. When that's the bottleneck, graduate to
  the pNFS profile.
- **Dashboards/alarms**: the F68a "data through the MDS" warning is
  posture-aware — all-MDS I/O is the *norm* in lite, and the server
  logs it as telemetry, not alarm.
