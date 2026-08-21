# Flint-lite — the standalone hub

One pod, real POSIX, zero consumer footprint. Flint-lite runs
`flint-pnfs-mds` in `mode: standalone`: the full NFSv4.2 server —
enforced byte-range locks, close-to-open coherence, atomic rename —
with the pNFS machinery off. No DS fleet, no SPDK, no CSI stack on the
hub cluster, and **nothing at all installed on consumer clusters**:
they mount the hub with the NFS client already in every node's kernel.

Choose lite when the workload is agent fleets / shared workspaces whose
tools need POSIX (git, sqlite, compilers) across one or many clusters,
and one hub's throughput is enough. With [the S3 tier](#the-s3-tier)
on, the PVC is just the working set: durability lives in a bucket, and
a lost PVC is a rebuild, not a loss. Choose the full pNFS profile when a
single pod's NIC is the bottleneck — the hub speaks the same protocol
and stores the same volume format, so graduation is adding DSes and
turning layouts on, not a migration.

## The hub

```bash
helm install flint-lite ./flint-lite-chart \
  --namespace flint-lite --create-namespace
```

`flint-lite` is its **own chart** — four objects (ConfigMap, RWO PVC,
Service, single-replica Deployment) and nothing else: no CSI stack, no
SPDK, no CRDs (the full chart's snapshot CRDs are cluster-scoped and a
lite hub never needs them). A full pNFS cluster is the
`flint-csi-driver-chart`'s job, in its own release. Requires image 1.26.0 or
newer (the first release carrying `mode: standalone` and the tier;
multi-arch amd64 + arm64).

Storage: the PVC uses the **cluster's default StorageClass** unless
`persistence.storageClassName` says otherwise. Any CSI driver's
RWO volume works — EBS gp3, GCE PD, Ceph, local-path — the hub writes
plain files; NVMe is a performance choice, never a requirement. Size
for the working set (`persistence.size`, default 20Gi).

Restart semantics: strategy `Recreate` plus the RWO attach fence means
two hub pods never run concurrently, and the sqlite state on the PVC
keeps the server id stable — client filehandles, locks and sessions
survive a pod restart.

## Reaching the hub from other clusters

NFS is one long-lived TCP flow to one port; anything that routes TCP
works.

- **Same cluster / flat or peered pod networks**: the default
  `ClusterIP` Service is enough.
- **Across clusters over a cloud LB**: `--set service.type=LoadBalancer`
  (use `service.annotations` for internal-LB annotations). Mind LB
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
  mountOptions: [nfsvers=4.1, proto=tcp, hard, nconnect=4, noatime]
  nfs:
    server: 203.0.113.10   # the hub Service's LB IP / routable address
    path: /
```

**`nconnect>=2` is not optional.** Without it the kernel opens exactly
one connection and silently refuses every additional trunk — no error,
on either side, ever — and one TCP flow is also what fails to fill the
bandwidth-delay product on any path longer than a rack. All the
connections land on the same pod; a share is one hub by design.

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
  --set nfs.mountOptions='{nfsvers=4.1,proto=tcp,hard,nconnect=4,noatime}'
```

Same `nconnect` caveat as recipe A — the provisioner passes these
options straight to the node's mount, so leaving it out gets one
connection per node and no error saying so.

Then `storageClassName: flint-lite` on any PVC mints a workspace. The
provisioner is a single small Deployment; the data path is still the
node kernel's NFS client straight to the hub.

## The S3 tier

With `tier.enabled` the hub adds a cold tier over one S3 bucket
prefix: every mutation is captured durably, closed generations publish
to the bucket on a flush cadence, cold files evict from the PVC at a
disk watermark, and evicted files hydrate back on first touch. The PVC
becomes the working set; the bucket becomes the durability story.

```yaml
# values-tier.yaml
tier:
  enabled: true
  bucket: my-team-flint         # must already exist, versioning ON
  keyPrefix: vol1/              # one prefix = one volume = one hub
  credentialsSecret: flint-tier-s3   # "" = ambient (IRSA / node role)
  # endpoint: http://minio.minio.svc:9000   # non-AWS stores
  # settings: { watermarkPct: 90 }          # knobs, schema-checked
```

```bash
kubectl -n flint-lite create secret generic flint-tier-s3 \
  --from-literal=AWS_ACCESS_KEY_ID=... \
  --from-literal=AWS_SECRET_ACCESS_KEY=...
helm install flint-lite ./flint-lite-chart \
  --namespace flint-lite --create-namespace -f values-tier.yaml
```

(On EKS prefer IRSA: leave `credentialsSecret` empty and annotate a
ServiceAccount instead. When upgrading an existing release, re-supply
your values file — don't rely on `--reuse-values`.)

**Before enabling:**

- **The bucket must exist** — the hub never creates buckets — and
  should have **versioning on**: the recovery paths for accidental
  deletes lean on delete markers.
- **One prefix = one volume = one hub.** The hub claims a volume epoch
  in the bucket at startup and heartbeats it; a second release on the
  same prefix doesn't corrupt anything — the epoch guard makes the two
  hubs fence each other in turn, which is an availability outage on
  purpose. Give each volume its own prefix (or bucket).
- **`.flint/` under the prefix is reserved** for tier control objects
  (the epoch cell, DR manifests). Don't write, delete, or lifecycle
  them away.
- **Consumers never get bucket credentials.** They mount NFS; the
  bucket trusts exactly one principal — the hub.

**What to expect in operation:**

- **RPO is the flush cadence.** The per-file flush floor defaults to
  60s (`tier.settings.flushFloorSecs`) — that floor is what caps a hot
  file's S3 request bill, so lower it deliberately, not casually. A DR
  manifest rides every publish barrier, so the bucket alone is always
  restorable to the last barrier.
- **Evicted files read back transparently but not instantly.** At the
  disk watermark (default 85%) cold files are truncated to stubs;
  metadata stays truthful (`ls -l`/`df` show logical sizes). The first
  read of an evicted file parks the client (NFS4ERR_DELAY — kernel
  clients retry silently, applications just see a slow open) until the
  **whole file** restores; writes get a reserved hydration slot so a
  writer is never starved by readers. Restores fetch up to
  `tier.settings.hydrateFetchParallel` (default 6) ranged GETs
  concurrently — one S3 stream is ~80–200 MB/s, so the fan-out divides
  a large file's cold-read time; raise it (with
  `hydrateConcurrency`) on fat-NIC hubs, mindful that peak restore
  buffering ≈ `hydrateConcurrency × hydrateFetchParallel × 8 MiB`.
- **A cold TREE can restore eagerly instead.** Per-file hydration is
  the wrong shape for a single-threaded tool walking a freshly
  DR-restored workspace (`grep -r`, a build) — it pays one round-trip
  per file. `tier.settings.hydrateWarmAfterImport: true` makes the hub
  bulk-restore every stub after an import that ran (DR reinstall,
  bucket adopt), smallest files first, on a dedicated pool
  (`hydrateWarmConcurrency`, default 16; demand reads never queue
  behind the fill, and a read of a file mid-fill simply joins its
  restore). The fill stops short of the eviction watermark rather than
  fight it, survives hub restarts (a durable note re-arms it), and
  logs one `tier warm fill done` line when the tree is hot. Off by
  default — a fill re-downloads every byte, so switch it on for
  workloads that will sweep the tree anyway.
- **Full disks degrade politely.** Admission answers NOSPC while
  `avail − reserve` can't cover a write (databases see NOSPC, never
  EIO), and a preallocated ballast next to the state db releases at
  critical fullness so bookkeeping keeps committing.
- **Startup can legitimately run minutes before the NFS port opens** —
  fencing and import run *before* the listener, because an unfenced
  hub must never serve. A routine pod restart re-claims instantly
  (same PVC state); replacing a hub whose predecessor died waits out
  the dead holder's lease (~60s at default heartbeat knobs); a fresh
  PVC over a non-empty bucket imports the namespace first. The chart's
  startupProbe budgets for all of this (`tier.startupFailureThreshold`,
  default 60 × 10s) — a pod in Running/not-Ready is working, not stuck:
  `kubectl logs` shows the claim and import progress.
- **Only the namespace rebuild is pre-listener.** A DR restore reads
  the manifest — one GET — and materializes the whole tree from it, and
  the listener opens as soon as that is done. Bucket objects the
  manifest does NOT describe (foreign uploads, or pre-existing data
  being adopted) are folded in afterwards, by a sweep that runs behind
  the live listener: it is a full prefix LIST plus a HEAD per unknown
  object, and making every client wait for it would buy nothing, since
  the manifest already rebuilt the real tree. `/status` reports phase
  `sweeping` while it runs, and `sweep.completed` when it is done; an
  interrupted sweep is recorded durably and resumes at the next start
  rather than losing the remaining keys.
- **A manifest the bucket HAS but cannot be read is a refusal, not a
  warning.** Only the manifest carries directories, symlinks, modes and
  owners, so importing without it would serve a flattened tree — and
  then publish that back over the real one. The hub logs loudly, sets
  `importRefused` in `/status`, restores nothing, and retries at the
  next start. A bucket with *no* manifest is different and ordinary:
  that is the adopt path.
- **Watch one log line.** The tier reporter prints at most one line
  per interval (`📊 🪣 tier last 60s: …`, `FLINT_TIER_REPORT_SECS`) and
  is silent when idle — silence with a clean install is health. It
  escalates to `🚨` WARNs for the two states worth paging on:
  time-to-full below threshold, and oldest-unflushed age beyond
  threshold (a wedged flush is *never* silent — its signature is zero
  activity plus a growing backlog age).

**Disaster recovery** — losing the PVC (or the whole cluster) is a
rebuild, not a loss: reinstall the chart with the **same bucket and
keyPrefix**. The new hub takes over the epoch, restores the namespace
from the bucket manifest as evicted stubs, and hydrates content on
demand; everything flushed before the last barrier comes back
byte-identical. Set `hydrateWarmAfterImport: true` for eager DR
restore — the whole tree re-downloads up front (smallest files first,
resuming across hub restarts) instead of paying a round-trip on each
first touch. This exact loop — uninstall with the PVC destroyed,
reinstall, hydrate-on-read — is `make test-lite-kind-tier-e2e` leg 4.

Tuning beyond the identity fields goes through `tier.settings`
(rendered verbatim into the server config; unknown keys are refused at
render so a typo'd knob can't silently take its default). The defaults
are the economics model's assumptions — treat them as a contract, not
a starting point.

## Status, and files without a mount

`monitoring.enabled` adds a second listener to the hub — off by
default, on its own port, and deliberately **not** on the Service. Two
things live there.

`GET /status` reports the hub's phase, its epoch, tier gauges, client
activity, and `rpoClean` — "can the bucket rebuild this volume right
now?", which is the question to answer before deleting a PVC. It binds
*before* the tier starts, so a slow epoch claim or a long DR import is
visible as progress rather than as a wedge.

`monitoring.fileApi` adds an HTTP file API: list, download, upload,
delete, mkdir, move. It exists so a browser or a control plane can work
with a share without mounting it — useful when the consumer is a web
service rather than a pod, and necessary at fleet scale, where holding
hundreds of kernel mounts is not an option.

```yaml
monitoring:
  enabled: true
  port: 8080
  fileApi:
    enabled: true
    tokenSecret: flint-api-token    # Secret with key `token`
```

Every request needs `Authorization: Bearer <token>`; there is no
token-optional mode, because the surface can rewrite any file in the
share. Do not add this port to the Service — the Service carries NFS
and may become a LoadBalancer.

Under the hood each endpoint is an NFS compound dispatched in-process,
not a second reader of the export directory, so it inherits everything
the NFS path already does: a cold file hydrates from S3 and the caller
gets 503 + `Retry-After` instead of a body of zeros; symlinks are
listed but never followed; uploads take the write gate and produce the
capture notes the tier publishes from. Objects carry an `ETag` (the
same change attribute a mounted client orders its cache by), so a
caller can read, edit and write back under `If-Match` and be told `412`
rather than silently losing the edit — detection between API callers,
not exclusion against the mount. The full endpoint table is in
[the operator guide](flint-lite-operator.md#the-hubs-http-surface-status-and-files-without-a-mount).

The token is per hub, which is fine at one hub and becomes the problem
at a hundred: a service that browses every project would need every
project's secret. Don't build that store —
[the fleet-auth note](plans/file-api-fleet-auth.md) derives each share's
token from one root key instead, so the caller holds one key and
recomputes rather than looking anything up.

## Many shares: the operator

This chart is one release per hub, which is the right shape at one to
ten of them. For a fleet — bucket-per-tenant, dozens of workspaces —
there is an operator: install it once, then each volume is a
`FlintShare` custom resource, `kubectl get flintshares -A` is the
fleet, and the tier knobs become a typed schema instead of a free-form
map. It renders the same four objects this chart renders (a golden test
in the suite fails the build if they drift), so moving is adoption, not
migration. See [the operator guide](flint-lite-operator.md) — including
the migration path from an existing release of this chart, which has a
sharp edge worth reading before you start.

## Operational notes

- **One hub per dataset.** The hub is the coherence authority; two hubs
  over one tree is split-brain. With the tier on this is "one hub per
  bucket prefix", enforced live by the epoch guard — and, if you run
  the operator, refused up front across the whole fleet.
- **A settings change needs a restart.** The hub parses its config once
  at boot and has no reload path. `helm upgrade` with a changed
  `tier.settings` rolls the hub because the pod template carries a
  `checksum/config` annotation; if you edit the ConfigMap by hand
  instead, nothing happens until the pod restarts.
- **Backup**: with the tier on, durability lives in the bucket (RPO =
  the flush cadence) and the PVC is a rebuildable working set. Without
  the tier, the PVC *is* the data — snapshot it with its CSI driver's
  tooling.
- **Ceiling**: one pod — every byte through one NIC and one process.
  Measured on the dev rig: hundreds of MiB/s sequential, a few hundred
  metadata ops/s per client. When that's the bottleneck, graduate to
  the pNFS profile.
- **Dashboards/alarms**: the F68a "data through the MDS" warning is
  posture-aware — all-MDS I/O is the *norm* in lite, and the server
  logs it as telemetry, not alarm.
- **Network exposure**: `networkPolicy.enabled` renders a default-deny
  ingress policy for the hub with explicit holes for 2049 and the
  monitoring port. Off by default — an empty client list admits
  nothing, and only you know your node CIDRs (kubelet-driven mounts
  arrive from the NODE ip, so a podSelector cannot express that client
  set). It needs a CNI that enforces NetworkPolicy; where the CNI does
  not, every rule is ignored silently. Turning it on also closes 50051,
  the MDS gRPC control plane — images 1.28.0+ do not start it in
  standalone mode at all, and the policy is what covers an older one.
