# The flint-lite operator — a fleet of shares

The [lite chart](flint-lite.md) is the right packaging for one hub, or
ten. Past that, one helm release per volume becomes the problem:
release-secret sprawl, an imperative upgrade per share, no drift
repair, and the `--reuse-values` trap that has bitten this project on
real clusters (a `helm upgrade --reuse-values` reuses the OLD chart's
computed values).

The operator replaces that with one custom resource per volume:

```yaml
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: tenant-a
  namespace: workspaces
spec:
  bucket: my-team-flint        # absent = tier off (the PVC is the data)
  keyPrefix: tenant-a/         # immutable, must end in "/"
  credentialsSecretRef: flint-s3   # absent = IRSA / instance role
  persistence:
    size: 20Gi
  settings:                    # typed: a typo is refused at admission
    hydrateWarmAfterImport: true
```

```console
$ kubectl get flintshares -A
NAMESPACE    NAME       PHASE   ADDRESS                                    BUCKET          PREFIX
workspaces   tenant-a   Ready   tenant-a.workspaces.svc.cluster.local:2049 my-team-flint   tenant-a/
workspaces   tenant-b   Starting                                           my-team-flint   tenant-b/
```

What it buys, concretely:

- **No reusable release state.** Every reconcile re-renders from the CR
  plus operator defaults. There is nothing to reuse and nothing to go
  stale.
- **The knobs are schema.** `spec.settings` mirrors the server's own
  `TierKnobs`, so `watermarkPCT: 90` is rejected by the API server
  instead of being silently ignored by a YAML parser that drops unknown
  keys. Unset knobs stay unset, so they take the SERVER's default — the
  CRD deliberately carries no defaults of its own.
- **Fleet-wide invariants a per-release install cannot check.** Most
  importantly: at most one share per bucket subtree (below).
- **One lever for fleet upgrades.** Shares that do not pin
  `spec.image` follow the operator's default hub image.

## Install

```console
helm install flint-lite-operator \
  oci://registry-1.docker.io/dilipdalton/flint-lite-operator \
  -n flint-system --create-namespace
```

Then apply FlintShares anywhere in the cluster. The operator renders
the same four objects the lite chart renders — ConfigMap, RWO PVC,
Service, single-replica Recreate Deployment — and a golden test in the
suite fails the build if the two ever diverge. Both remain supported.

## The three things it will not do

1. **It never touches a bucket.** No create, no delete, no lifecycle.
   Deleting a share deletes Kubernetes objects; the data in S3 is
   exactly as durable as it was a moment before.
2. **It never garbage-collects a PVC.** The claim carries no
   ownerReference — deliberately, because Kubernetes' GC does not know
   what `reclaim: Retain` means and would collect it anyway. Deleting a
   share keeps its claim unless `spec.reclaim: Delete` says otherwise.
3. **It will not run two hubs on one bucket subtree.** See below.

## Uniqueness, and why it is strict

At most one FlintShare may own a given `(endpoint, bucket, prefix
subtree)` — across all namespaces. `tenant-a/` and `tenant-a/sub/`
count as the same subtree: sweeps and the `.flint/` control objects of
the outer share span the inner one.

A second share is refused: phase `Failed`, a `Conflict` condition
naming the winner (oldest wins), no hub. If a running share BECOMES a
loser, it is scaled to zero.

This is stricter than "don't waste money on two pods". The store-side
epoch is a lease: two live hubs do not fight — the loser waits, and
crash-loops before its listener ever binds — but when the holder dies
for a lease window, the other hub judges it dead, TAKES the prefix
over, imports it, and serves that data at its own address, to whoever
mounts it. On ordinary pod churn, an unarbitrated duplicate is a
cross-tenant data leak. The epoch protocol is defense-in-depth against
a mistake; the operator's job is not to make one.

Kubernetes cannot express this in CEL or a ValidatingAdmissionPolicy
(both see one object), so it is enforced at reconcile time from the
controller's cache. Delete the winner and the survivor is promoted on
the next reconcile.

## Lifecycle and status

`spec.lifecycle: Suspended` scales the hub to zero and keeps the PVC —
the share stops costing compute and wakes instantly (the epoch re-claim
on the same state.db is immediate). Everything else stays: claim,
Service, config.

Phases: `Pending` → `Starting` → `Ready`, plus `Suspended` and
`Failed`.

**`Starting` is not a problem.** A tiered hub does real work before its
listener binds: it claims the volume epoch — which may WAIT OUT a dead
holder's lease, ~60s by default — and, on a fresh state, imports the
whole bucket. Minutes of `Starting` on a large DR restore is expected;
the startupProbe budgets for it (`spec.startupFailureThreshold`, in 10s
periods, default 60) and liveness does not begin until it passes.
Killing a `Starting` hub kills a takeover or an import.

Conditions carry the detail: `Ready`, `ConfigCurrent`, `Conflict`,
`AdoptionBlocked`, `CredentialsFound`, `PersistenceCurrent`.

## Changing settings, and what actually restarts the hub

The hub parses its config **once, at boot**, and has no reload path;
credentials ride `envFrom`, fixed at container start. So a settings
edit reaches a RUNNING hub only if the pod restarts.

The operator hashes the rendered config into a `checksum/config`
pod-template annotation (and the Secret's contents into
`checksum/creds`), so any change rolls the Deployment — one Recreate
bounce, brief, with an instant epoch re-claim on the same PVC.

- `spec.restartPolicy: Immediate` (default) rolls on the spot.
- `spec.restartPolicy: Manual` writes the new ConfigMap but leaves the
  pod alone and reports `ConfigCurrent=False`; you roll it with
  `kubectl rollout restart deploy/<share>` in your own window. (Note
  the honest caveat: the ConfigMap is already updated, so an unrelated
  restart also picks up the new settings.)

The same annotation was added to the flint-lite chart in the same
change — without it, `helm upgrade` with changed `tier.settings`
updated the ConfigMap and never reached the running hub, while
`kubectl get cm` showed the new values.

## Migrating an existing chart release to a FlintShare

This is the riskiest operation in the whole document; read it before
starting.

The chart's children have fixed, release-unprefixed names
(`flint-lite`, `flint-lite-data`, `flint-lite-config`). A second
Deployment on the same RWO claim can land on the SAME NODE — RWO is
*node*-granular, and WaitForFirstConsumer or a local-path PV force it —
which gives two hub pods, two sqlite writers, one `state.db`. The epoch
cannot save you there: both pods read the same state.db and recognize
themselves as its holder.

So the operator adopts **in place** and refuses to create a second
Deployment while a foreign pod still mounts the claim
(`AdoptionBlocked`).

**Path A — name the share `flint-lite` (adoption in place, no
downtime beyond one bounce).** The CR-derived names then match the
chart's exactly, and the operator server-side-applies over the
existing objects:

```yaml
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: flint-lite            # == the helm release's object names
  namespace: <release ns>
spec:
  existingClaim: flint-lite-data
  persistence: { size: 20Gi }  # ignored for an adopted claim; keep it truthful
  # ... the same bucket/keyPrefix/credentials the release used
```

1. Apply the CR. Expect at most ONE pod replacement (the pod template
   gains the checksum annotation and the operator's labels).
2. Confirm `PHASE=Ready` and that the hub kept its identity — same
   PVC, same `server_id` in the logs, clients unaffected.
3. Retire the helm release WITHOUT deleting the objects it created:
   either delete the release secrets
   (`kubectl delete secret -l owner=helm,name=<release>`), or annotate
   all four objects `helm.sh/resource-policy: keep` before
   `helm uninstall`. A plain `helm uninstall` after adoption deletes
   the adopted objects — ownerReferences do not protect them from
   helm.

**Path B — a differently-named share.** Scale the chart's Deployment to
zero first (`kubectl scale deploy/flint-lite --replicas=0`), then apply
the CR with `existingClaim: flint-lite-data`. Until that old pod is
gone the operator holds `AdoptionBlocked` and creates nothing — that
fence is deliberate; do not work around it.

Either way the bucket is untouched, and the claim is never deleted by
the migration.

## Upgrades

The operator applies its own compiled-in CRD at startup. This is not
belt-and-braces: helm NEVER upgrades `crds/`, so a chart-only CRD
freezes at whatever schema the cluster first installed, and a
structural schema silently PRUNES unknown fields — a knob added in a
later flint release would be accepted by `kubectl apply`, dropped by
the API server, and quietly take its server default.

The CRD carries `flint.io/crd-schema-version`, and an operator refuses
to apply over a version NEWER than its own, so a briefly-restarted old
replica cannot stomp a new schema mid-rollout. `manageCrd: false`
disables the mechanism for clusters whose policy forbids it — then
apply `cargo run --bin crdgen` output by hand on every upgrade, or
accept the pruning.

## Limits (v1alpha1)

- One served version, no conversion webhook. `v1alpha1` may change.
- No admission webhook: what CEL can express is in the CRD
  (identity immutability, prefix syntax), the rest is reconcile-time.
- `Hibernated` (delete the PVC, wake via DR import + warm fill) is not
  implemented: it needs a verified final flush on SIGTERM, since
  hibernating an unflushed hub loses the RPO window permanently.
- Two SPELLINGS of one endpoint are not recognized as the same store by
  the uniqueness check (the epoch still fences them).
- PVC expansion is passed to the StorageClass; a size SMALLER than the
  existing claim is refused in status rather than retried forever.
