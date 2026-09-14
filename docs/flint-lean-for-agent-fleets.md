# flint-lean on a Kubernetes cluster

> **Delivery changed (2026-09-03).** A workspace now reaches a pod as
> ONE `csi:` volume served by the `s3.csi.chert.us` node driver
> (`flint-s3-csi-chart`), not as a webhook-injected sidecar. The label
> `chert.us/lean-workspace`, the injected `flint-sync` container, the
> per-namespace credential Secret and the webhook described below are
> the RETIRED shape; the workspace protocol (claim, checkout gate,
> `.flint/publish`, boundaries, drain on delete) is unchanged and the
> syncer binary is the same. What a pod writes today:
>
> ```yaml
> spec:
>   serviceAccountName: agent            # listed in the CR's spec.consumers
>   volumes:
>     - name: ws
>       csi:
>         driver: s3.csi.chert.us
>         volumeAttributes: { chert.us/workspace: proj1 }
>   containers:
>     - volumeMounts: [{ name: ws, mountPath: /workspace }]
> ```
>
> and the CR gains `spec.uid` (the uid the syncer runs as — the app's)
> and `spec.consumers.serviceAccounts`. See
> `docs/plans/csi-node-mount-design.md` §0 and §3.5.
>
> **Read-only agents.** An agent that should see the workspace and never
> change it mounts read-only: its SA is listed in
> `spec.consumers.readOnlyServiceAccounts` (read-only whatever the pod
> asks), or its volume sets `readOnly: true`. Its tree is bound read-only
> (a write fails with `EROFS`), its syncer follows every writer's
> boundary and publishes nothing (`.flint/AGENTS.md`, "Read access"), and
> `flint-s3-broker` issues it a credential that cannot write — a session
> policy on `sts`, `"access": "read"` to a `rest` door, or
> `broker.static.readSecretRef` on `static`. Without a read key a
> `static` broker says `readEnforcement: cooperative` on `/v1/status`:
> the mount and the syncer keep that pod read-only, the bucket does not.
> One workspace carries read-write and read-only agents at once. Set
> `chert.us/on-behalf-of: <user>` in `volumeAttributes` to name the
> signed-in user on the broker's audit lines. See
> `docs/plans/flint-lean-per-user-access-design.md` §10.

Give every agent pod its own workspace: **plain local files** checked out
of your S3 bucket at pod start, published back on a cadence — or when the
agent says so. No FUSE, no NFS, no privileged pods, no mount to wedge.

```
        agent pod
        ├─ your container ──▶ /workspace   plain files, real POSIX, local speed
        │                        ▲   │
        │            checkout ───┘   └─── publish (cadence, or on demand)
        │                                     │
        └─ flint-sync sidecar ────────────────┴──▶ S3
              injected by a webhook · unprivileged · no /dev/fuse
```

The trade, stated up front: the workspace is a **local copy**. It must
fit the pod's disk, writers merge at the manifest rather than editing
one live tree (two agents editing one file get last-boundary-wins with
the loser's bytes preserved), and other readers see the last published
boundary rather than your last write. If that is not
your shape, see [when not to use lean](#when-not-to-use-lean).

Everything below was run end to end on 2026-08-26 against the published
chart. Commands are copy-pasteable, not illustrative.

---

## Before you start

| | |
|---|---|
| An S3 bucket | must already exist. Versioning is not required. Nothing here creates or deletes buckets. |
| Kubernetes | **1.29+** — the sidecar is injected as a *native sidecar* (`initContainer` with `restartPolicy: Always`), which is 1.29 or newer. |
| Tools | `kubectl`, `helm` 3.8+ (OCI support) |
| Credentials | see [credentials](#credentials) — the agent container never holds one |

Nothing is installed on your nodes. The sidecar is an ordinary
unprivileged container.

---

## 1. Install the operator

```sh
kubectl create namespace flint-system

helm install flint-lean \
  oci://registry-1.docker.io/dilipdalton/flint-lean --version 0.3.0 \
  -n flint-system
```

That installs three things: the `FlintLeanWorkspace` CRD, the operator,
and a mutating webhook. The webhook provisions **its own TLS certificate
and `MutatingWebhookConfiguration` at startup** — there is no
cert-manager dependency and nothing to pre-create.

It pulls two images and nothing else:

| image | pulled by | what runs |
|---|---|---|
| `dilipdalton/flint-lean-operator` | the `flint-system` deployment | `flint-lean-operator` — the controller and webhook |
| `dilipdalton/flint-sync` | every opted-in agent pod | the syncer |

**flint-lean does not install or require flint-lite.** No CSI driver, no
NFS hub, no `FlintShare` CRD — the chart bundles exactly one CRD and its
RBAC covers only `flintleanworkspaces`. (The operator image is the same
build artifact as `flint-lite-operator`, republished under the lean name
from an identical digest: one image, two names, so nothing is duplicated
and nothing called "lite" appears in a lean install.)

Check it came up:

```sh
kubectl -n flint-system get pods
# flint-lean-59d5b4f886-s5hp8   1/1   Running

kubectl -n flint-system logs deploy/flint-lean | tail -3
# webhook cert generated and stored in flint-lean-webhook-cert
# mutating webhook flint-lean-inject applied
# flint-lean-operator: watching FlintLeanWorkspace
```

> **If `helm install` fails with `permission denied` on
> `~/Library/Caches/helm`**, something ran `helm` as root with your `HOME`
> and took ownership of your cache. Either `sudo chown -R "$USER"
> ~/Library/Caches/helm`, or run with a private cache:
> `HELM_CACHE_HOME=/tmp/helmcache helm install …`

---

## Credentials

The operator does bucket-admin work (posture probes, a stale-upload
sweep) under its own identity. Each workspace's syncer uses the
credential its CR names. **The agent container is given neither** — the
webhook stamps the secret onto the sidecar container only, and process
namespaces are not shared, so a compromised agent cannot read it.

```sh
kubectl -n flint-system create secret generic s3 \
  --from-literal=AWS_ACCESS_KEY_ID=... \
  --from-literal=AWS_SECRET_ACCESS_KEY=... \
  --from-literal=AWS_REGION=us-west-1

# and the same secret in the namespace your agents run in
kubectl -n agents create secret generic s3 --from-literal=... 
```

Point the operator at it by reinstalling (or `helm upgrade`) with
`--set operatorCredentialsSecret=s3`, or leave it empty to use the pod's
ambient chain (IRSA and friends).

The keys must be named `AWS_*` **verbatim** — they are passed to the
syncer as environment.

> **Plan of record:** a project-scoped S3 proxy holds the real
> credentials and no pod stores one at all. Until that lands,
> `credentialsSecretRef` is the interim posture.

---

## 2. Create a workspace

One CR per project subtree.

```yaml
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata:
  name: proj1
  namespace: agents
spec:
  projectId: team-a/proj1        # durable claim identity
  bucket: my-bucket
  keyPrefix: tenants/proj1       # the subtree this workspace owns
  credentialsSecretRef: s3
  floorSecs: 60                  # publish cadence — and the RPO
```

```sh
kubectl apply -f workspace.yaml
kubectl -n agents get flintleanworkspace proj1 -o yaml | grep -A6 conditions
```

You want `SpecAccepted: True`. `SyncerObserved: Unknown`
with reason `NoLiveSyncer` is normal for a workspace no pod has ever
published; once one has, the condition names the binary that ran the
last boundary (several pods may share one workspace; the status does not
count them).

---

## 3. Opt a pod in

One label. That is the whole integration.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: agent-1
  namespace: agents
  labels:
    chert.us/lean-workspace: proj1     # ← the webhook keys on this
spec:
  containers:
    - name: agent
      image: your-agent:latest
      workingDir: /workspace
```

The webhook injects the sidecar, adds the workspace volume, and mounts
it into **every** container in the pod. The syncer runs the checkout
*before* your container starts — the injected `startupProbe` gates it —
so your first line of code sees a complete tree.

```sh
kubectl -n agents get pod agent-1
# agent-1   2/2   Running        ← 2/2 = your container + the sidecar
```

If the workspace is missing or refused, the pod is **rejected at
admission** rather than started against an empty directory.

> **Do not declare your own volumeMount at `/workspace`** — the
> injection would collide and admission fails. If your image needs that
> path, move flint's with `spec.mountPath: /flint` on the CR.

---

## 4. Publish on demand (optional, recommended)

Cadence alone forces a bad choice: publish often and waste work, or
publish rarely and let a reader see a half-finished tree. Instead, let
the agent declare a coherent point when it finishes a unit of work:

```sh
# in the agent container
printf '{"nonce":"task-42"}' > /workspace/.flint/publish

# wait for the answer
until [ -f /workspace/.flint/publish.ack ]; do sleep 1; done
cat /workspace/.flint/publish.ack
# {
#   "status": "ok",
#   "nonces": [ "task-42" ],
#   "seq": 1,
#   "manifest_etag": "\"caa5498c...\"",
#   "boundary": "sentinel",
#   "completed_unix": 1787772547,
#   "report": { "uploaded": 0, "deleted": 0, "parked": 0, ... }
# }
```

`uploaded: 0` here just means nothing had changed since the last
boundary — the point the agent declared is already true, which is still
an honest `ok`.

`status: "ok"` means **the named boundary is in the bucket** — not
queued, not scheduled. A boundary that lost its turn at the publish
fence is retried by the cadence, so an ack is late rather than refused.

Costs nothing when unused: measured at 20 bucket requests per 22 s idle
with the verbs off, and 20 with them on.

---

## 5. Verify

```sh
aws s3 ls s3://my-bucket/tenants/proj1/ --recursive
# tenants/proj1/.flint/lean/claim
# tenants/proj1/.flint/lean/epoch
# tenants/proj1/.flint/lean/current            ← the manifest POINTER
# tenants/proj1/.flint/lean/manifests/000…-<uuid>   ← entries, write-once
# tenants/proj1/files/...          ← your tree lives here
```

The `files/` prefix is the workspace. `.flint/lean/` is control state.
To see which clock installed the current boundary:

```sh
aws s3api head-object --bucket my-bucket \
  --key tenants/proj1/.flint/lean/current \
  --query 'Metadata."flint-boundary-source"'
# "cadence" | "sentinel" | "quiescence" | "drain" | ...
```

`current` is a few hundred bytes and names the generation holding the
entries, so this HEAD stays cheap however large the project grows. A
single `.flint/lean/manifest` object was the pre-2026-09 layout; on a
migrated workspace that key holds a refusal doc, not a manifest, so read
`current` and follow its `entries_key`.

---

## What it costs

Measured on real S3 (us-west-1, one i4i.large node, 2026-09-12; the whole
drill, its controls and its rig are in
`lean/e2e/perf/results/door-drill-2026-09-12.md`). Every cell is the
range over three reps, never a mean. "Host engine" is the syncer binary
alone on the node; "as deployed" is the chart, the `s3.csi.chert.us` node
plugin, the worker pod and the tenant pod, timed from `kubectl apply` of
the tenant pod to Ready (checkout) or from the `publish` touch to its ack.

| | host engine | as deployed |
|---|---|---|
| checkout, 6 × 1 GiB | **24.1–25.0 s** | **118.4–161.1 s (n=2)** (reps 2,3: rep 1 paid a lease lockout the drill itself caused) |
| checkout, 20,000 × 8 KiB | **5.3–5.9 s** | **22.9–24.9 s** |
| checkout, 4 GiB + 2,000 × 16 KiB | **19.6–19.9 s** | **92.4–115.1 s** |
| publish, 6 × 1 GiB | **18.3 s** | **19.0–19.4 s** |
| publish, 20,000 × 8 KiB | **19.5–20.4 s** | **20.8–22.6 s** |
| publish, 4 GiB + 2,000 × 16 KiB | **50.5–51.7 s** at `uploadPartParallelism: 1` (the v1.51.0 default); **13.9–14.4 s** at 8, the default since the upload window gained its byte bound (`uploadInflightMb`, 256) | **50.8–50.9 s** (at 1) |
| idle | ~5 bucket requests per tick, per workspace | — |

The deployed column carries what a pod pays that the engine does not —
scheduling, the worker pod, the lease claim, the loop mount, the bind —
a mostly fixed cost of roughly the small tree's gap. Peak worker RSS at
the default read window (128 MiB in flight): 403–437 MiB on the 1 GiB
objects, 229 MiB on the mixed tree, 72 MiB on the small one, under the
worker's 1Gi limit.

**The file count binds before the byte count** — the manifest is exactly
linear in entries, which is why the v1 cap is ~250k files. For scale,
the loopback floors: 100k files check out in 49.5 s (first publish 65 s,
idle tick 1.85 s); 1M files in 7 m 05 s, with a 264 MiB manifest at
~277 B/entry.


---

## When not to use lean

| you need | use |
|---|---|
| a tree too large for the pod's disk, or lazy access to a slice of a huge dataset | FUSE (`docs/flint-fuse-architecture.html`) |
| two pods writing one tree and seeing each other live; byte-range locks; shared sqlite/git across pods | the hub (flint-lite) |
| readers that cannot tolerate seeing the last boundary instead of the last write | the hub |

All three front ends share one bucket format and are mutually
convertible, so choosing lean now does not strand the data.

For agents collaborating on code, do **not** reach for a git-over-S3
remote: use plain `git` against a real git host and let flint-lean keep
the workspace durable. See `docs/flint-lean-git-workflow.md`.

---

## Tearing down

```sh
kubectl -n agents delete pod agent-1
kubectl -n agents delete flintleanworkspace proj1   # leaves bucket data intact
helm uninstall flint-lean -n flint-system
```

Deleting the CR does not delete your data. The bucket subtree is the
durable artifact; a new workspace pointed at the same `keyPrefix` picks
it up.
