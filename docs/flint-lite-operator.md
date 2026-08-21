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

The secret named by `credentialsSecretRef` is loaded with `envFrom`, so
its KEYS become environment variables verbatim and must be the names the
AWS SDK reads:

```console
$ kubectl -n workspaces create secret generic flint-s3 \
    --from-literal=AWS_ACCESS_KEY_ID=... \
    --from-literal=AWS_SECRET_ACCESS_KEY=...       # + AWS_SESSION_TOKEN if temporary
```

Naming them anything else (`accessKeyId`, say) leaves the SDK with no
credentials at all. It then falls back to the instance role, and on a
node where IMDS is unreachable from pods that surfaces as a startup
crash loop reading `bucket <name> unreachable: dispatch failure` — which
names the bucket, not the credentials.

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

**The enforcement stops at the cluster boundary, and the bucket does
not pick it up.** Arbitration reads one reflector over one API server,
so two clusters can each admit a share on the same prefix and neither
will see the other. The store-side epoch catches that only when the
prefixes are *equal*: the epoch object is keyed on the exact prefix
string, so `tenant-a/` and `tenant-a/sub/` mint DIFFERENT epoch
objects, never contend, and produce two hubs writing overlapping bytes
with no fence anywhere and no error on either side. Equal prefixes at
least serialize into a takeover; nested ones do not.

So if shares are created from more than one cluster, uniqueness has to
be owned upstream of Kubernetes — one prefix per project, never nested,
never reused, `UNIQUE` on the normalized `(endpoint, bucket, prefix)`
in whatever database allocates them. There is no way to add that check
here, and `spec.bucket`/`spec.keyPrefix` are immutable once set, so a
wrong prefix is a byte migration rather than an edit.

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

## Reaching a share from another cluster

`status.address` is what a consumer mounts, and by default the operator
derives it from the Service it created. **Every derived answer except a
LoadBalancer's ingress is in-cluster-only:**

| `spec.service.type` | derived `status.address` | routable from another cluster? |
|---|---|---|
| `ClusterIP` (default) | `<name>.<ns>.svc.cluster.local:<port>` | no |
| `NodePort` | `<name>.<ns>.svc.cluster.local:<port>` | **no — and this one is a trap** |
| `LoadBalancer` | the ingress hostname/IP | yes, once the LB is up |

The NodePort row is the trap: the Service really does open a node port,
but the address the operator advertises is still the in-cluster DNS
name. A workload-cluster client reads `status.address`, tries to mount
it, and fails on a name it cannot resolve — the port it needed was
never in the string.

`spec.service.advertiseAddress` is the answer. Set it to whatever the
consumer should actually dial and the operator copies it into
`status.address` verbatim:

```yaml
spec:
  service:
    type: NodePort
    nodePort: 32049
    advertiseAddress: "10.0.4.7:32049"     # a peered-VPC node address
    # advertiseAddress: "hub-a.corp.internal:2149"
    # advertiseAddress: "[2001:db8::1]:2049"    # IPv6 needs brackets
```

Three things worth knowing:

- **It advertises, it does not provision.** The operator still creates
  exactly the Service you asked for; only what it *reports* changes. The
  in-cluster path keeps working untouched, so co-located consumers are
  unaffected.
- **The port is mandatory.** A bare host is refused at admission,
  because an NFS client handed one silently uses 2049 — precisely wrong
  for a port-per-project layout. Unbracketed IPv6 is refused for the
  same reason: its colons make the port unrecoverable.
- **It is mutable, and nothing recalls existing mounts.** Unlike
  `bucket`/`keyPrefix` this is not identity — an endpoint can legitimately
  move. Clients already mounted keep using the old address until they
  remount.

NFS is one long-lived TCP flow, so prefer a flat or peered network to a
cloud load balancer, and mind LB idle timeouts if you use one.

### Mount options for a cross-cluster mount

The address is half of it; these are the other half. A WAN-ish path
between client and hub changes which options matter, and one of them
fails **silently** if you leave it out.

`status.address` is `host:port`, and `mount` will not take it whole —
`host:2049:/` is refused. Split it: the host goes before the colon, the
port goes in `-o port=`. Doing it this way also survives a
`spec.service.advertiseAddress` on a port other than 2049, which is the
whole reason the field carries one.

```
ADDR=$(kubectl get fsh tenant-a -n workspaces -o jsonpath='{.status.address}')

mount -t nfs4 -o vers=4.1,nconnect=4,hard,timeo=600,retrans=2,\
noatime,actimeo=30,port=${ADDR##*:} ${ADDR%:*}:/ /mnt/project
```

The export is the SERVER ROOT — `:/`, not the hub's on-disk
`/data/exports`, which is a path inside the container and is refused
with `NFS4ERR_NOENT`.

- **`nconnect>=2` is mandatory, not a tuning knob.** The kernel opens
  ONE connection without it and silently refuses every additional
  trunk — no error, on either side, ever. One TCP flow across a
  higher-latency path is also exactly what fails to fill the
  bandwidth-delay product, so this is both the correctness scar and the
  throughput lever. `nconnect=4` is a reasonable start. All the
  connections land on the same pod, because there is only ever one.
- **`hard`, and not `soft`.** Agents run git and sqlite; a soft mount
  turns a server blip into silent write corruption rather than a wait.
  The cost is that a hub which goes away hangs its clients in
  uninterruptible sleep — which is why suspend-while-mounted is
  something to configure deliberately (below) rather than discover.
- **`actimeo` is the traffic a read-mostly cross-cluster mount actually
  generates.** Attribute revalidation, not data. The default (up to
  60s for directories) is usually fine; lower it only if agents need to
  see each other's metadata changes faster, and know you are buying
  that with round trips. Close-to-open consistency is unaffected — a
  file closed by one client is seen whole by the next one to open it,
  whatever `actimeo` says.
- **`noatime`** — atime updates are writes, and they cross the boundary
  too.

**Suspend and a cross-cluster mount interact badly by default.** A
partition between the workload cluster and the hub makes the agents
INVISIBLE rather than absent: they stop driving NFS operations, the hub
sees a quiet share, and if the front door's heartbeat is also cut, both
suspend signals go quiet while every agent is alive and blocked. The
hub then suspends underneath them. Set
`spec.idle.suspendWithSessions: false` on any share mounted from
another cluster — it refuses to suspend while a client still holds a
lease. Note the residual honestly: NFSv4 leases expire, so a long
enough partition drops the lease count to zero on its own and the
window closes rather than the risk disappearing.

## The front-door contract

The "front door" is whatever service owns projects — it decides a
project exists, brokers access to it, and is the only thing that knows
a person is about to use one. This section is the contract between it
and the operator. Everything here is deliberately boring: the front
door speaks plain Kubernetes, and the operator does the rest.

### One project, one name, one prefix

Derive the share's name from the project id — `fs-<project-id>` — and
label it `flint.io/project-id`. The derived name is what makes
ensure-live idempotent: two front-door replicas racing a first touch
issue the same `create`, one gets `409 AlreadyExists`, and both
proceed. Allocating names any other way lets that race create two
shares on one bucket prefix, and conflict arbitration then permanently
`Failed`s one of them.

The label is the index in the other direction (`kubectl get flintshares
-o wide` prints it as PROJECT). **Uniqueness of the prefix itself is
not something this cluster can enforce** — see the section above.

### ensure-live

```
GET  flintshares/fs-<id>          → 404? create it
PATCH metadata.annotations         → flint.io/requested-at: <now, RFC3339>
                                     flint.io/wake-intent: warm   (optional)
poll GET until .status.phase == Ready
read .status.address               → mount it, or call the file API
```

Four things about that loop:

- **The wake is explicit and the annotation is the whole mechanism.**
  An NFS operation against a suspended hub does not wake it — it hangs,
  because there is nothing listening to notice. The file API is the
  opposite: it fails fast with 503 rather than hanging. Either way,
  something must write `requested-at` or the share stays down.
- **Phase distinguishes the waits, and they are very different.**
  `Pending` is objects being applied, `Starting` is a pod booting — and
  for a tiered share, `Starting` also covers claiming the volume epoch
  (which can wait out a dead holder's lease) and importing the bucket.
  Surface those as distinct states or the UI will look hung.
- **`wake-intent: warm`** tells the hub to pull the working set back
  during import rather than hydrating on first touch. Use it when a
  person is opening the project; omit it for a background poll. The
  operator consumes it once the hub is serving, and clearing it does
  not restart anything.
- **Read `status.serverId` and keep it with the mount.** If it changes,
  the share came back on a fresh PVC and every stateid a client still
  holds is stale. A change means remount; an unchanged id across a
  restart means carry on.

### What create and wake actually do

Both paths end in the same place — one pod serving one volume — but
they start from very different amounts of nothing, and the waits are
not comparable. A UI that shows one spinner for both will be wrong
about at least one of them.

**Nothing in the data path starts a hub.** Neither door is a trigger.
An NFS mount against a scaled-to-zero share hangs: the Service has no
endpoints, so there is nothing to notice the attempt and a hard mount
retries forever. The file API is the opposite failure and the better
one — it refuses immediately rather than hanging. Either way the client
is not what wakes the share, which is why ensure-live writes
`requested-at` **before** it hands an address to anyone.

**Create — the project has never existed.** The front door `create`s
the CR; the operator applies four objects (a ConfigMap holding
`mds.yaml`, a PersistentVolumeClaim, a Service, a Deployment) and the
pod boots. For a tiered share the boot is the long part: it claims the
volume epoch and imports the bucket before its listener binds.
`Pending` covers the applies, `Starting` covers all of the rest.

**Wake from `IdleSuspended` — the disk survived.** The Deployment goes
back to one replica and the hub re-claims the epoch against the same
`state.db`. Nothing is imported because nothing was lost. Measured on a
real cluster at **41s to `Ready`**.

**Wake from `Hibernated` — the disk was deleted.** The PVC is
re-created, the hub imports the bucket manifest, and the epoch claim
takes the *slow* path: hibernating destroys the volume and therefore
the `serverId`, so self-recognition cannot short-circuit the lease and
the claim waits out the full `lease_misses × heartbeat`. Measured at
**79s against 13s** for the same-identity case. Every long-parked
project pays that on its first open.

**The wake is level-triggered, and that cuts both ways.** The operator
acts on the annotation being *present*, not on the write event — so a
`requested-at` stamped while the operator was down is honoured when it
returns, and no wake is ever lost to a dropped watch. The other side is
that **a share cannot wake while no operator is reconciling**. The
drill measured exactly that: with the operator scaled to zero a stamped
share sat at `IdleSuspended` for 245s untouched, then reached `Ready`
41s after the operator came back, from the same annotation nobody had
re-written. "It fails safe, only a delete hangs" is wrong — run two
replicas, and see *Is the operator alive?* below.

### One project, one hub, one disk

Every share gets **its own PVC**: `<share>-data`, `ReadWriteOnce`,
sized by `spec.persistence.size`, which is required — there is no
default, because capacity is a decision. One project, one hub pod, one
claim, nothing shared between projects. That is what makes
`kubectl delete flintshare` a complete cleanup.

The claim being `ReadWriteOnce` is not by itself the guard against two
hubs on one disk — RWO is enforced per *node*, and a Deployment will
happily start a replacement while the old pod is still terminating. So
the strategy is `Recreate` (the operator never deliberately runs two)
and the hub takes an exclusive `flock` on its state directory for the
life of the process, which is the fence that also covers the cases the
operator cannot see: evictions, node drains, and
`kubectl delete pod`.

The exception is `spec.existingClaim`, which adopts a claim the
operator did not create. It then never re-declares that claim's size or
class, and never deletes it — not even under `reclaim: Delete`.

**For a tiered share the disk is a cache, not the copy.** The bucket
holds the project; the PVC holds the working set, the eviction markers
and `state.db`. That is what makes it safe for hibernate to take the
disk away, and it is also the number to watch:
`spec.persistence.size` is a working-set budget, not a project-size
budget, and eviction works against it. **For a share with no
`spec.bucket` the PVC is the only copy** — which is precisely why
hibernate refuses one.

At fleet scale the disks follow the ladder rather than the roster: 3000
projects with 300 live holds **300 PVCs, not 3000**. A hibernated
project holds none, and a project whose CR has been deleted holds no
Kubernetes objects at all.

### Writing files on a user's behalf

The front door is a web service handling untrusted input, which means
the same project gets written from two browser tabs, from a retried
upload, and from an agent, all at once. Three obligations follow, and
they are contract, not advice:

- **Send `If-Match` on every write you make on a user's behalf, and
  handle `412` by re-reading rather than retrying the write.** Take the
  tag from the `etag` on the listing entry or the download — a listing
  carries one per file, so a browse gives you everything you need
  without a stat per file. A `412` means the file changed under your
  user since they read it; retrying the same body destroys whatever
  arrived in between, which is precisely what the tag exists to
  prevent. Without `If-Match` both writers get `201` and one edit is
  discarded silently.
- **Use `If-None-Match: *` for create, and do not trust it to
  serialise a race.** It is checked with a stat, not inside the
  compound, because NFS has no operation that fails a compound
  *because* a name resolved. Two callers racing a create can both pass
  it. It makes the single-writer case correct; it is not a lock.
- **Know what the guarantee covers, and it is narrower than it looks.**
  `If-Match` becomes a `VERIFY` inside the same NFS compound as the
  rename. But a COMPOUND is not atomic, so two writers can both pass
  their VERIFY before either lands its RENAME, and one update dies with
  both callers seeing `201`. This is **detection, not serialisation**.

  Measured, eight writers appending to one file, 200 writes: the
  unconditional control loses 168-174 of them every time. `If-Match`
  loses **32-66 on an idle machine and 90-102 under CPU load** — so the
  benefit ranges from 5x down to **under 2x**, and the residual from
  16% to **51%**.

  That spread is the important part, and it points the wrong way. CPU
  contention widens the server-internal gap between the VERIFY and the
  rename by descheduling a task inside it, so **the guard is weakest
  exactly when concurrent writers are most likely** — a busy hub is
  precisely where you were counting on it. Size your expectations from
  the loaded number, not the idle one. Use it as the safety net it is — it turns the common
  two-tab case from silent corruption into a retry — but if your product
  lets several users edit one file at once, you still need a merge or a
  single-writer discipline above this API. It does not fence a client
  that has the volume mounted either, and it is not a transaction: an
  upload is single-shot, so a `412` means redo the whole write.

One behaviour to expect on a tiered share: an entity-tag changes when
a file is evicted to S3 or hydrated back, because both rewrite the
local inode and the tag is derived from its change attribute. Nothing
the user did changed, but a conditional write against a tag held across
that boundary answers `412`. It fails closed — a re-read and retry,
never a lost edit — and a share with no tier does not do it at all. Do
not treat a lone `412` as evidence of a concurrent editor; treat it as
"re-read before you write".

### Keepalive is mandatory, not optional

Re-touch `requested-at` on a timer shorter than `suspendAfterSecs`, for
as long as any session is live. This is not belt-and-braces — it is
half of the suspend decision. The hub's own activity clock is the other
half, and it cannot see an agent that spent twenty minutes thinking
without touching the filesystem. That agent looks idle to the hub and
is kept alive by the heartbeat alone.

Two failure modes to design against:

- **Do not keepalive from a liveness poller.** `/status` is
  deliberately not activity, and the file API deliberately is. A UI
  that refreshes a listing on a timer pins that project awake forever
  and the ladder never fires. Poll `/status`; do not walk `/files`.
  **A conditional GET that answers `304` is no different here** — it is
  cheap in bytes and identical in wake-up, so revalidating on a timer
  pins a project exactly as re-downloading does.
- **Do not let a wrong clock look like demand.** A `requested-at` more
  than one `suspendAfterSecs` in the future is discarded, and the share
  reports an `ImplausibleRequest` event. That is a guard, not a
  feature — stamp it from a sane clock.

### Is the operator alive?

Wake only happens while an operator is reconciling, so "waking" and
"nothing will ever happen" look identical from the CR. They are
distinguishable exactly one way: read the operator's leader-election
Lease (`flint-lite-operator`, in the operator's namespace) and check
that `renewTime` is recent. The `frontDoor` role grants that read and
nothing else.

Do **not** try to infer it from the share. `observedGeneration` does
not lag after an annotation-only patch (metadata changes do not bump
`metadata.generation`), and `lastTransitionTime` only moves when a
condition actually changes — so "nothing has changed in 60s" is true of
every healthy, settled share in the fleet.

### Why did my project suspend?

In order of what to look at:

1. `kubectl describe flintshare fs-<id>` — the `IdleEligible`
   condition carries the reason it was held or the fact that it was
   not, in words.
2. Events: `IdleSuspended`, `Woken`, `HibernateStarted`,
   `SuspendedWithActiveSessions`, `ImplausibleRequest`.
3. `.status.phase` — `IdleSuspended` (the ladder did it, a wake
   request will bring it back) versus `Suspended` (an admin set
   `spec.lifecycle`, and a wake request will NOT override it).

## Locking down the network

Once shares are reachable across clusters, "8080 is never on the
consumer-facing Service" stops being a design principle and starts
being a control that something has to enforce. Both charts ship
`networkPolicy`, off by default:

```yaml
networkPolicy:
  enabled: true
  hubNamespaces: [workspaces]        # where FlintShares live
  nfsClientCIDRs: ["10.0.0.0/16"]    # who may reach 2049
  apiClientSelectors:                # who may reach 8080, besides the operator
    - podSelector:
        matchLabels:
          app.kubernetes.io/name: projects-frontdoor
```

Three things to know before turning it on:

- **It needs a CNI that enforces NetworkPolicy.** Cilium and Calico do.
  On a cluster whose CNI does not, every rule here is ignored in
  silence — the objects exist, `kubectl get networkpolicy` looks
  healthy, and nothing is enforced. kind's default CNI is in that
  group, which is why no e2e leg asserts enforcement.
- **An empty client list admits nothing.** That is deliberate: a policy
  that falls open when unconfigured reads as protection and is not. If
  you enable this without setting `nfsClientCIDRs`, every mount breaks.
  Kubelet-driven mounts arrive from the NODE ip, not a pod ip, so node
  CIDRs are usually what you want — a podSelector cannot express that
  client set at all.
- **A policy only covers its own namespace.** `hubNamespaces` is the
  list helm renders a hub policy into. **A share created in a namespace
  that is not listed is unprotected, and nothing detects that** — keep
  the list in step with where shares are created.

The operator's own policy admits no ingress whatsoever, because the
operator serves nothing: it watches the API server and applies objects,
all of it outbound.

Selecting the hub pods at all is also what closes **50051**, the MDS
gRPC control plane. It binds `0.0.0.0`, carries `DeleteVolume`, and is
unauthenticated unless `FLINT_PNFS_CONTROL_TOKEN` is set — which
nothing in these charts sets. Hub images 1.28.0 and newer do not start
it in standalone mode at all (there are no data servers to register and
no CSI driver in front of it, so every verb on it is either meaningless
or destructive); the policy is what covers an older image.

## The hub's HTTP surface: status, and files without a mount

`spec.monitoring` turns on a second listener in the hub — off by
default, on its own port, **ClusterIP-only and never added to the
Service**. That Service carries NFS and may be a LoadBalancer; the file
API below can rewrite every file in the share, so the two must not share
a door. The port is declared as a `containerPort` for legibility only.

```yaml
spec:
  monitoring:
    enabled: true          # /health and /status
    port: 8080
```

`GET /status` answers with the phase, the epoch, import and warm-fill
reports, tier gauges, client activity, and the RPO predicate:

```json
{
  "phase": "serving",
  "epoch": { "held": true, "number": 7 },
  "activity": { "idleSecs": 412, "browseOps": 91, "dataOps": 3308 },
  "rpoClean": true,
  "rpo": { "dirtyFiles": 0, "tombstones": 0, "manifestCurrent": true, "beyondRpo": 0 }
}
```

`phase` moves `starting` → `claimingEpoch` → `importing` →
`reconciling` → `serving`, and may sit at `sweeping` afterwards while
foreign bucket keys are folded in behind the live listener. `sweeping`
serves normally — the tree is already whole.

Two things about it are load-bearing:

- **It binds BEFORE the tier and before the NFS listener.** The epoch
  claim can wait out a dead holder's lease and a DR import walks the
  whole bucket; that whole window is invisible to anything watching only
  port 2049, and it is exactly when you most want to tell "importing"
  from "wedged". Poll `phase`.
- **`importRefused`, when present, is the most important field on the
  document.** It means the bucket holds a manifest the hub could not
  read, so the namespace was NOT restored: the export does not reflect
  the bucket, and publishing forward from it would overwrite the real
  tree. The hub restores nothing and retries at the next start.
- **`rpoClean` is `null`, never `true`, when the tier is off.** It means
  "the bucket can rebuild this volume right now" — the question you must
  answer YES to before deleting a PVC. A share with no bucket has no
  answer, and reading absence as `true` would delete the only copy of
  the data. `rpo.why()` explains every `false`.

### The file API

```yaml
spec:
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: flint-api-token   # Secret with key `token`
      maxDownloadBytes: 268435456
```

Six endpoints, all requiring `Authorization: Bearer <token>`:

| Method | Path |
|---|---|
| GET | `/files?path=&recursive=&limit=&cursor=` |
| GET | `/files/content?path=` (Range, `If-None-Match`) |
| PUT | `/files/content?path=` (`application/octet-stream`, `If-Match`, `If-None-Match: *`) |
| DELETE | `/files/content?path=` (`If-Match`) |
| POST | `/files/folder` — `{"path": "/a/b"}` |
| POST | `/files/move` — `{"from": "/a", "to": "/b"}` (`If-Match`, on `from`) |

This exists so a project service can browse and edit a share **without
mounting it**. It cannot hold kernel mounts at fleet scale: pod-spec
volumes are fixed at creation, a runtime `mount(2)` needs privilege,
and a suspended hub puts every mount holder into uninterruptible sleep.

Each endpoint is a translation of an NFS compound, dispatched
**in-process through the hub's own compound dispatcher** — not by
reading the export directory. That is what makes it safe rather than a
second, worse filesystem: it inherits eviction handling (a cold file
hydrates and the caller gets 503 + `Retry-After`, never a body of
zeros), export confinement and the symlink rules, locking, the write
gate, the space reserve, and the tier's capture notes. A direct write
would be a write the bucket never hears about.

Things worth knowing before you wire it up:

- **There is no token-optional mode.** With neither `tokenSecretRef` nor
  `FLINT_FILE_API_TOKEN` in the environment, the routes are not mounted
  at all and the hub logs why.
- **It refuses with 503 until the phase reaches `Serving`.** A listing
  taken mid-import would show a partial tree as though it were the whole
  one.
- **Browse-driven hydration is real, billed S3 egress.** A click on a
  cold file pulls it out of the bucket. `maxDownloadBytes` (default 5Gi)
  is the per-request cap; larger files are fetched with `Range`. Note
  the cap bounds the RESPONSE, not the egress: the first `Range` of a
  cold object still hydrates the whole object.
- **Downloads above `streamThresholdBytes` (default 8Mi) stream.** Below
  it a body is buffered whole and a mid-read change is a clean 409
  before any byte ships; above it memory stays O(chunk) whatever the
  file size, and a mid-read change ends the stream with an error so the
  connection resets — never a short body under 200. Buffering everything
  bounded hub memory by the download cap instead, which at 1:1 hubs per
  project is a fleet-wide cost: a 512 MiB request moved `VmHWM` from
  30 MB to 541 MiB, and under a 256Mi limit it was OOM-killed.
- **Every call counts as activity**, which is what keeps a project a
  user is looking at from being suspended under them. Poll `/status`
  for liveness — it deliberately does NOT count — or an automated
  refresh will pin every share in the fleet awake forever.
- **Uploads are temp-plus-rename.** A concurrent reader sees the old
  file or the new one, never a mixture, and a crashed upload leaves a
  recognisable `.flint-upload.*` temp rather than a corrupt file under
  the real name.
- **Conditional requests are supported, and they detect rather than
  prevent.** Every object carries an `ETag`, on downloads, on upload
  responses, and on every listing entry — so a UI can list a directory
  and then write conditionally without re-reading each file. Sending it
  back as `If-Match` turns the write into a VERIFY inside the same NFS
  compound as the rename or remove it guards, and a compound stops at
  its first error, so a file that changed under you is never replaced:
  the answer is `412` and the write is refused whole. `If-None-Match: *`
  is create-if-absent. `If-None-Match` on a GET revalidates to `304`,
  which on a cold file is the difference between a header and billed S3
  egress.

  The tag is the fattr4 CHANGE attribute plus the fileid — **the same
  validator a mounted client uses**, so an entity-tag and a mounted
  process's change value name one version of one file.

  What it is not is a lock. An NFS compound is not atomic, so a writer
  on the mount can still land between the VERIFY and the rename. This
  closes the lost update between two API callers — two browser tabs, a
  retried upload, two agents on one project — which is the race this
  surface actually runs into. It does not fence the mount. That is not a
  gap against some stronger HTTP idiom: it is exactly the strength of
  NFS's own `VERIFY`, which this is re-exposing rather than reinventing.

  Three rough edges worth knowing. `If-None-Match: *` is checked with a
  stat rather than in the compound, because NFS has no operation that
  fails a compound *because* a name resolved — two callers racing a
  create can both pass it. An `If-Match` list of several tags is
  refused with `400` rather than half-honoured; send one tag or `*`.
  And **on a tiered share a tag changes when a file is evicted or
  hydrated**: both rewrite the local inode, and the tag is derived from
  its change attribute, so a tag held across that boundary answers
  `412` with nothing the user did behind it. It fails closed — re-read
  and retry, never a lost edit — and a share with no tier does not do
  it at all.
- **A 304 still counts as activity.** Cheap in bytes, identical in
  wake-up. Revalidating on a timer pins a project awake exactly as
  re-downloading on a timer does — use `/status` for liveness.
- **Symlinks are data, not paths to follow.** They appear in listings
  with their target; `GET /files/content` on one answers 409. The server
  does not dereference a link on a caller's behalf — in NFS that is the
  client's job, and doing it server-side would resolve the target in the
  *hub's* namespace, which holds its state database and its cloud
  credentials.

## The idle ladder: winding a share down when nobody is using it

Off by default, and **each rung is opt-in on its own**:

```yaml
spec:
  monitoring:
    enabled: true              # REQUIRED — the ladder reads the hub's /status
  idle:
    suspendAfterSecs: 900      # 15m idle → scale to zero, KEEP the PVC
    hibernateAfterSecs: 86400  # 24h down → delete the PVC (needs spec.bucket)
```

Absent means off because defaulting it on would auto-suspend every
share in an existing fleet — including tier-off ones whose consumers
mount `status.address` as a plain PV and have never heard of the wake
annotation. Their mounts would simply hang.

### The two rungs are not one setting with two numbers

**Suspend** scales the hub to zero and keeps everything else: the CR,
the Service, the ConfigMap, and the PVC. Waking is a pod start and an
epoch re-claim on the same state database — seconds. Safe for any
share, tiered or not.

**Hibernate** deletes the PVC. At that moment the bucket is the only
copy, so it requires `spec.bucket` (refused at admission otherwise) and
the operator will not act on a timer alone — see below. Waking is a
full DR import.

**The CR is never deleted by either.** Only an explicit
`kubectl delete flintshare` does that, and `spec.reclaim` decides what
happens to the disk then. The bucket is never touched under any policy.

### Suspending needs two independent signals

A share comes down only when **both** are true:

1. the front door's `flint.io/requested-at` annotation is older than
   `suspendAfterSecs`, and
2. the hub's own `/status` reports `activity.idleSecs` past the same
   threshold.

Each covers the other's blind spot. An agent that computes in memory
for twenty minutes without touching the filesystem looks idle to the
hub — the heartbeat keeps it alive. A workload that mounted without the
front door in the loop has no heartbeat at all — the hub's own clock
keeps it alive. It also avoids comparing clocks: the annotation is
judged on the front door's, idleness on the hub's, and neither has to
agree with the operator's.

**A hub that cannot be polled is never suspended.** An unreachable hub
is an unknown hub, not an idle one, and the `HubReachable` condition
says so.

### Hibernation is verified at drain time, not assumed

The drain's real outcome is unobservable from the operator: the hub
exits 0 whether or not it flushed, scale-to-zero deletes the pod so no
exit code survives, and the operator has no bucket credentials to check
the epoch mark itself. So hibernation is **verify-then-delete**:

1. scale the share back to **one** (`idle-state: HibernateVerifying`),
2. poll `/status` until `rpoClean` is true,
3. scale to zero and let the hub drain, flush and release the epoch,
4. wait for the pod to be genuinely gone,
5. only then delete the PVC.

`rpoClean: null` — a share with no bucket — is a **refusal**, not a
pass. A3's fast epoch re-claim is what makes the extra wake cheap
enough for this to be the default posture rather than a compromise.

### Waking

The front door touches an annotation:

```sh
kubectl annotate flintshare fs-myproject \
  flint.io/requested-at="$(date -u +%FT%TZ)" --overwrite
```

That is the whole protocol. Keep touching it on a heartbeat shorter
than `suspendAfterSecs` while a session is alive. What the two rungs
then cost to come back — and why nothing wakes while the operator is
down — is in *What create and wake actually do*.

**`spec.lifecycle: Suspended` always wins.** It is an admin decision
and a wake request does not override it — which is why the phases are
distinct: `Suspended` means "an admin said no", `IdleSuspended` means
"will wake on request", and a front door that cannot tell them apart
retries forever against a share that is never coming back.

Phases: `Pending` → `Starting` → `Ready`, plus `Suspended`,
`IdleSuspended`, `Hibernated` and `Failed`.

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
- Waking a hibernated share re-creates the PVC and drives a full DR
  import; `wake-intent: warm` reaches the hub's config and bulk-fills
  during that import, but it is consumed once and only at wake — there
  is no way to ask a *running* hub to warm itself.
- The idle ladder's hibernate rung has never completed a full round
  trip on a real cluster: the drill's attempt was correctly *deferred*
  by a bug it exposed, so "PVC destroyed, then the bytes come back
  identical" is proven for suspend/wake and still only inferred for
  hibernate/wake.
- The file API is single-shot: no chunked or resumable upload, no
  byte-range PATCH. Large uploads that fail are retried whole — and
  `If-Match` does not make one a multi-request transaction.
- Conditional writes detect a lost update between API callers; they do
  not exclude a writer on the mount, and `If-None-Match: *` is a check
  with a race window rather than an atomic create.
- A recursive listing is bounded (50k entries, depth 32) and reports
  `truncated: true` rather than silently returning a short list.
- Two SPELLINGS of one endpoint are not recognized as the same store by
  the uniqueness check (the epoch still fences them).
- PVC expansion is passed to the StorageClass; a size SMALLER than the
  existing claim is refused in status rather than retried forever.
