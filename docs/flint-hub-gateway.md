# flint-hub-gateway — one door in front of every hub's file API

`flint-hub-gateway` is a proxy. A project service calls it with **one**
credential and a project id; it finds that project's share, wakes it if
it is parked, and forwards the request to that share's hub.

It exists because of a deliberate gap. A hub's HTTP file API answers at
`status.apiEndpoint`, which is a **headless in-cluster Service name**:
addressable, and not routable from anywhere else. The operator refuses
to render anything routable per share, for two reasons that do not go
away with configuration —

- **3000 ClusterIPs exhaust a GKE-default /20 (4096) at about 2048
  shares**, below the fleet size already validated. Worse, once the
  allocator is dry a brand new share's *consumer* Service cannot be
  created either, so one tenant's share count would stop every other
  tenant from mounting anything.
- **NetworkPolicy cannot be the guard.** It is off by default in both
  charts, `hubNamespaces` defaults to `[]` so turning it on protects no
  hub at all, some CNIs ignore every rule in silence, and
  `externalTrafficPolicy` is unset so an ipBlock allowlist cannot see
  the real client anyway.

One gateway *can* be guarded — an Ingress with TLS, a NetworkPolicy, an
audit log, a WAF, whatever you already run. So the exposure is
concentrated in one object you configure once, instead of spread across
the fleet. That is the whole trade. See
`docs/plans/hub-api-service-design.md` for the full argument.

## What it proxies

```
GET    /v1/projects/{id}/volumes                      list a project's volumes
POST   /v1/projects/{id}[/volumes/{vol}]/wake         bring it up, and keep it up

GET    /v1/projects/{id}[/volumes/{vol}]/files?path=&recursive=&cursor=&limit=
GET    /v1/projects/{id}[/volumes/{vol}]/files/content?path=   [Range, If-None-Match]
PUT    /v1/projects/{id}[/volumes/{vol}]/files/content?path=   [If-Match, If-None-Match]
DELETE /v1/projects/{id}[/volumes/{vol}]/files/content?path=   [If-Match]
POST   /v1/projects/{id}[/volumes/{vol}]/files/folder          {"path": "..."}
POST   /v1/projects/{id}[/volumes/{vol}]/files/move            [If-Match]

GET    /healthz     unauthenticated, names no share
GET    /readyz      unauthenticated, 503 until the share cache has listed
```

The request and response semantics are the hub's, unchanged: same
listing shape, same `ETag`/`If-Match` compare-and-swap (v1.30.0), same
`Range` support, same `Retry-After` on a 503. Read
`docs/flint-lite-operator.md` for the file API itself.

**There is no route to `/status`, of any shape.** The hub serves an
unauthenticated `/status` on the same listener as the file API — the
tier's recovery point, the epoch holder, the NFS client list, the
lifecycle phase. The gateway never holds a caller-supplied path: it
matches a request against a closed table of six operations and asks that
table for a `&'static str`. There is no traversal handling to get wrong
because there is nothing to traverse.

## Two credentials, and they are not the same one

```
project service ──Bearer(gateway token)──▶ gateway ──Bearer(per-share)──▶ hub
```

**Inbound** is one shared token, from a Secret in the gateway's own
namespace, projected as a file. It is re-read every 10 seconds and
compared per request, so rotating it is a Secret edit — no restart, and
no window where either value is rejected if you overlap them.

**Outbound** is per-hub, and the gateway holds no per-hub secrets. A
token is a pure function of the share's immutable identity:

```
token(share) = base64url_nopad(
    HMAC-SHA256(root, "flint-fileapi/v1:" || endpoint || ":" || bucket
                      || ":" || keyPrefix || ":" || version))
```

One key produces every hub's credential, so there is nothing to store
and nothing to fan out. This matters for a reason worth stating plainly:
**the alternative is `get secrets` in every workspace namespace**, and
those namespaces hold each tenant's S3 credentials in the same place as
the API token. The gateway's ClusterRole grants
`get,list,watch,patch` on `flintshares` and nothing else — a regression
test asserts it cannot read Secrets, create shares or delete them.

The caller's own credential is **never** forwarded upstream. A hub that
received it would be holding the key to every other project.

### Provisioning has to derive the same value

Whoever creates a share writes its token Secret. That value must equal
what the gateway computes, and two implementations of an HMAC that must
agree byte-for-byte is a fleet-wide outage waiting for a typo. So use
the binary as the oracle rather than reimplementing it.

**For a share that already exists, use `--derive-for`** — it reads the
FlintShare and derives from *its own* spec, so the binding cannot
disagree with what the serving gateway computes:

```bash
flint-hub-gateway --root-key-file=/path/to/key --derive-for workspaces/fs-proj-a-data
# binding: endpoint="http://minio.workspaces.svc:9000" bucket="tenant-bucket" …
# -> a 43-character token; write it to that share's Secret under `token`
```

The hand-typed form exists for provisioning a share that does not exist
yet:

```bash
flint-hub-gateway --root-key-file=/path/to/key \
  --derive-token 'http://minio.workspaces.svc:9000,my-bucket,proj-a/data/,1'
```

**Get one field wrong there and the token is perfectly well-formed,
perfectly wrong, and rejected by every request.** This is not
hypothetical: the kind drill's own provisioning omitted `spec.endpoint`
on its first run — an empty endpoint is a legal value, so nothing
complained — and the failure surfaced three legs later as a 502 on an
upload. Cross-check with `--derive-for` once the share exists.

⚠️ **`spec.endpoint` is part of the binding and is mutable.** The CRD
lets an endpoint move (`spec.bucket` and `spec.keyPrefix` are
CEL-immutable; the endpoint is not). Changing it invalidates every token
derived before the change, and the share's file API is 401 until its
Secret is rewritten. Nothing warns you.

The fields are `<endpoint>,<bucket>,<keyPrefix>,<version>`; endpoint is
empty for real S3. `version` comes from the share's
`flint.io/api-token-version` annotation (absent = 1) — **bumping it and
rewriting that one Secret revokes one project**, which is the thing a
single shared token cannot do.

During a rotation a hub may still hold the previous value. The gateway
retries once at `version - 1` and logs loudly when that succeeds, so a
hub running a stale token is visible rather than intermittent. The one
exception is an upload: its body is a stream that the first attempt has
already consumed, and buffering up to 5 GiB on the chance of a rotation
is the wrong trade. An upload gets the 401 with a message saying so.

If you want none of this, `hubTokenSecretRef` sets one token every hub
accepts. It is simpler and gives up single-project revocation. Note what
it does *not* give up: per-hub secrets protect a project from **other
callers of the API**, and once the gateway is the only caller there are
none — a compromise of the gateway opens every project in either mode.

## A project may have several hubs

**Nothing in the operator ties a project to one share.**
`conflict::overlaps` keys fleet uniqueness on
`(endpoint, bucket, prefix-subtree)`, and nothing in the operator reads
`flint.io/project-id` at all. So a project with a `data/` prefix and a
`models/` prefix is two FlintShares, two hubs, two independent epochs —
a legal and unremarkable configuration.

(One *hub* serving several volumes is a different thing entirely, and is
**not implemented** — see `docs/plans/multi-volume-hub-design.md`. The
model here is one volume, one hub, N of them per project.)

Label each share with the volume it is:

```yaml
metadata:
  labels:
    flint.io/project-id: proj-a
    flint.io/volume-id: data      # optional; defaults to the CR name
```

and address it as `/v1/projects/proj-a/volumes/data/files`.

| the project has | `…/files` | `…/volumes/{v}/files` |
|---|---|---|
| no share | 404 `NoSuchProject` | 404 |
| one volume | **served** | served if `{v}` matches, else 404 |
| several volumes | **409 `MultipleVolumes`, listing them** | served |
| two shares, same volume id | 409 `AmbiguousVolume` | 409 |

The bare path on a multi-volume project is **under-specified, not
wrong**, so the 409 carries the list:

```json
{"reason": "MultipleVolumes",
 "error": "this project has 2 volumes; address one as …",
 "volumes": ["data", "models"]}
```

Picking one instead would serve `models/` to a caller that meant
`data/`, and the reflector's iteration order is not stable across a
watch reconnect — so it would not even be *consistently* the wrong one.

`GET /v1/projects/{id}/volumes` is the same information ahead of time,
and it is what a UI wants to render a project. It is answered entirely
from the cache: **it touches no hub**, so listing a project neither
wakes a parked volume nor counts as activity against the idle ladder for
a live one.

```json
{"project": "proj-a", "volumes": [
  {"volume": "data",   "phase": "Ready", "hubPhase": "Serving",
   "bucket": "tenant-bucket", "keyPrefix": "proj-a/data/", "serving": true},
  {"volume": "models", "phase": "IdleSuspended",
   "bucket": "tenant-bucket", "keyPrefix": "proj-a/models/", "serving": false}]}
```

`serving: false` is not an error — a parked volume wakes on first use.

### How a project's shares are found

1. `flint.io/project-id` label — the documented index, already a printer
   column on the CRD.
2. Failing that, the derived name `fs-<project-id>` (`sharePrefix`).

The label wins when both match, so relabelling actually moves a project
rather than being shadowed by whatever object happens to be called
`fs-<id>`. Within a project, the volume is the `flint.io/volume-id`
label, or the CR's own name when it carries none — which keeps a project
that never adopted the label addressable anyway.

**Two shares claiming one project *and* volume is a 409, never a
guess.** Watching every namespace is the fleet posture, so two tenants
can each hold a `proj-a`. Every tie-break rule serves one tenant's files
to someone asking for the other's. The candidates go in the gateway's
log; the caller gets a 409 that names neither, because they cannot fix
it. That is deliberately a *different* answer from `MultipleVolumes`:
reporting a duplicate as a choice would send the caller round a loop
that cannot terminate — it would ask for the volume it was offered and
be offered it again.

## Whether before where

`status.apiEndpoint` is a stable formula, not a liveness signal. A
parked share still publishes one and the name simply does not resolve,
because a headless Service with no pods has no EndpointSlice. So the
gateway reads `status.phase` first:

| phase | what the caller gets |
|---|---|
| `Ready` + endpoint | proxied |
| `Ready`, no endpoint | 503, or 501/409 — carrying the operator's own `ApiEndpointPublished` reason |
| `Pending` / `Starting` / `Reprovisioning` | waits (already coming up; nothing is armed) |
| `IdleSuspended` / `Hibernated` | **wake armed**, then waits |
| `Suspended` | 409 — an admin decision, and a wake request does not override it |
| `Failed` | 409, naming the share that won the bucket subtree |
| `Terminating` (or a deletionTimestamp) | 410 |
| no status yet | 503, retry in 5 |

`status.hubPhase` only ever *downgrades* a `Ready` share. Absent means
"the operator's poll did not land this pass", **not** "the hub is not
serving" — treating it as a refusal would make the gateway unusable
against any hub the operator failed to poll once.

### Waking

A parked share is woken by patching `flint.io/requested-at` — the same
level-triggered annotation the front door uses, merge-patched so the
gateway never becomes a field owner and never fights whoever else writes
it. The wake **persists**, so a request that times out has still made
the share come back; the caller retries and does not need to ask again.

`wakeWaitSecs` (default 25) bounds how long one request will hold. An
idle-suspended share is back in roughly 20–30s. A hibernated one is a
full DR import from the bucket and will time out here by design — a UI
should show that as "restoring", not as an error.

**The wait watches the CR, never the hub.** This is not an optimisation.
A file-API call *counts as activity* on a share — including a 304 — so a
gateway that polled hubs to find out whether they were up would pin
awake every share it ever touched and quietly disable the idle ladder
the fleet's economics rest on.

### `POST …/wake` — for consumers that mount, and for keeping them alive

File requests wake a share on their own; this endpoint exists for the
callers that are **not** making file requests.

```json
POST /v1/projects/proj-a/volumes/data/wake
{"project":"proj-a","volume":"data","phase":"Ready",
 "address":"10.96.1.7:2049","serverId":"…","apiEndpoint":"http://…",
 "requested":true}
```

`address` is what a consumer mounts. So an agent that wants an NFS mount
does not need a Kubernetes client, the `frontDoor` ClusterRole, or any
knowledge of share names: one POST brings the hub up and returns the
mount target.

**It stamps on every call, including when the share is already Ready —
and that is the point.** The idle ladder suspends only when *two*
independent signals agree: `flint.io/requested-at` is stale **and** the
hub's own activity clock says idle. A consumer doing file I/O is held up
by the second signal for free. A consumer that holds a mount and does no
I/O — an agent computing in memory for twenty minutes, which is exactly
the case `idle::decide` calls out — has only the first, and **nothing
else in the system will stamp it**. Call this on a timer well inside
`suspendAfterSecs` and the share stays up.

It never dials the hub: this is a control operation, and probing through
`/files` to see whether the hub came up would itself count as activity.

Two things to do with the answer:

- **Compare `serverId` across wakes.** It is stable across an ordinary
  restart, and **different** after a hibernate — which deletes the PVC,
  so every stateid a client holds refers to a server generation that no
  longer exists. A changed `serverId` means remount, not resume.
- **Waking is not reachability.** `address` is `host:2049` from the
  consumer Service. Whether the caller can reach that port is a separate
  boundary — network topology plus `networkPolicy.nfsClientCIDRs`. Note
  that kubelet-driven mounts arrive from the *node* IP, which a
  podSelector cannot express. The gateway can tell you the hub is up; it
  cannot make 2049 reachable.

An admin `Suspended` is refused **before** anything is stamped — leaving
a `requested-at` that means nothing would read like a pending wake to
whoever looked next.

## The agent recipe: mount NFS without a Kubernetes client

This is the primary NFS consumer shape, and it has one sharp edge that
is worth stating before the recipe.

```
POST /v1/projects/{id}/volumes/{v}/wake     ->  {address, serverId, keepaliveSecs, mountWarning?}
mount -t nfs4 -o vers=4.2,nconnect=2,hard {address%:*}:/ /mnt/work
... every keepaliveSecs, POST /wake again ...
```

Three fields in the answer, and each is load-bearing.

**`address`** is what you mount. The agent needs no Kubernetes access,
no `frontDoor` ClusterRole and no knowledge of share names — just the
gateway's bearer token.

**`serverId`** must be compared across wakes. It is stable across an
ordinary hub restart, and **different** after a hibernate, because that
deletes the PVC — every stateid the client holds then refers to a server
generation that no longer exists. A changed `serverId` means *remount*,
not resume.

**`keepaliveSecs`**, and the `mountWarning` beside it, are the sharp
edge. The idle ladder suspends when two signals agree: the wake
annotation is stale **and** the hub's own activity clock is quiet. An
agent doing file I/O is held up by the second for free. **An agent that
holds a mount and computes in memory is not** — `idle::decide` names
this case exactly — and if the share suspends, the mount does not fail
cleanly. With a `hard` mount, I/O blocks in uninterruptible sleep; the
data is safe (`state.db` keeps `serverId` stable, so clients reclaim)
but **nothing wakes it**, because wake is level-triggered on an
annotation and an NFS client cannot write one. The mount hangs until
something else asks.

Two defences, and for agents you want both:

- **`spec.idle.suspendWithSessions: false`** on the share. Read the name
  literally: the *protective* value is the falsy one, and it is
  **opt-in**. Absent and `true` both mean "suspend even while a client
  holds a lease". The default is that way for a real reason — an idle
  NFSv4 mount renews its lease forever, so defaulting it on would pin
  every mounted share awake permanently, which is the state the ladder
  exists to end. Its cost: a client that vanished without unmounting
  holds the share up for the lease window, and a *partitioned* one holds
  it far longer.
- **The keepalive.** POST `/wake` every `keepaliveSecs` (half the
  suspend budget, so one missed call is survivable). This is driven by
  the thing that actually knows whether work is happening, which the
  lease guard is not.

`mountWarning` is present exactly when the share can be suspended out
from under a mount — i.e. the ladder is on and `suspendWithSessions` is
not `false`. If you see it, do one of the two above.

If `spec.idle` is absent entirely the ladder is off for that share,
nothing suspends, and `keepaliveSecs` is absent too.

**Waking is not reachability.** `address` is `host:2049` off the
consumer Service. Whether the agent can reach that port is a separate
boundary — network topology plus `networkPolicy.nfsClientCIDRs`. Note
that kubelet-driven mounts arrive from the *node* IP, which a
podSelector cannot express. The gateway can tell you the hub is up; it
cannot make 2049 reachable.

## Install

The gateway ships **inside the `flint-lite-operator` image** — same
crate, same build, a different `command`. Enabling it pulls no new
image. The two processes share nothing at runtime: different
ServiceAccount, different RBAC, different pods.

```bash
kubectl -n flint-system create secret generic flint-gateway-token \
  --from-literal=token="$(openssl rand -base64 32)"
kubectl -n flint-system create secret generic flint-gateway-root \
  --from-literal=key="$(openssl rand -base64 48)"

helm upgrade --install flint-lite-operator \
  oci://registry-1.docker.io/dilipdalton/flint-lite-operator \
  -n flint-system \
  --set gateway.enabled=true \
  --set gateway.tokenSecretRef=flint-gateway-token \
  --set gateway.rootKeySecretRef=flint-gateway-root
```

The chart **refuses to render** without an inbound token, without a hub
credential, or with both hub credentials set — all three are refused at
`helm template` time rather than becoming a CrashLoop, or worse, a
running open proxy.

`gateway.service.type` is `ClusterIP` by default. Put an Ingress in
front of it rather than reaching for `LoadBalancer`: the hop from
outside should be TLS-terminated and authenticated at the edge as well
as here.

If `networkPolicy.enabled` is on, the gateway is **admitted to the hubs'
8080 automatically** — both halves are rendered by this chart, and
requiring an operator to repeat the gateway's selector in
`apiClientSelectors` would only produce an outage the first time someone
forgot.

### Turn `readOnly` on if you can

A browse UI needs no mutating verb. `gateway.readOnly: true` is the
difference between a compromise that reads every project and one that
rewrites them, and it is checked before the share is even looked up.

## Where the root key should live

In preference order, from `docs/plans/file-api-fleet-auth.md` §5. The
property to optimise is not "encrypted at rest" — it is **how long a
compromise keeps paying out**.

1. **Delegate the MAC to a KMS** (`GenerateMac`, `HMAC_SHA_256`) or
   Vault Transit. The key never enters the process, revocation is an IAM
   change, and every mint lands in CloudTrail. Not implemented here yet;
   `Binding::message()` is public precisely so it can be.
2. A secrets manager read once at startup via IRSA.
3. A Kubernetes Secret in the **gateway's own namespace**, projected as
   a file — what the chart does today. Acceptable, and the weakest.

It must never live in a share's namespace (that is the blast radius this
design keeps small), never be projected into a hub (a hub that could
recompute another project's token turns a single-project compromise into
a fleet one), and never be given to the operator (which has no need to
mint and carries a fleet-wide blast radius of its own).

## What this does not solve

- **No per-end-user identity.** The hub sees one caller and one
  credential. Who looked at which file is a question only the project
  service can answer, and it must answer it in its own audit log. If
  that log is the compliance story, write it *before* the call.
- **No expiry.** A derived token is valid until its version changes.
  Audience-scoped ServiceAccount tokens (`file-api-fleet-auth.md` §8)
  are what fix that, and they belong on the hub rather than in a proxy.
- **NFS is untouched.** Port 2049 is AUTH_SYS and reachability is the
  boundary there. A token on the HTTP door does not compensate for an
  open 2049.
- **No cross-cluster hop.** The gateway resolves shares in the cluster
  it runs in. A fleet spread over several clusters needs one gateway per
  cluster and a router above them.

## Verification

- `cargo test --lib lite_gateway` — 73 tests. The proxy ones stand up
  **two independent fake hubs on real ports**, bind the gateway on a
  third, and drive it with a real HTTP client; each hub names itself in
  every response, so a cross-routed request fails on the body the caller
  received. One test writes the request bytes to the socket by hand,
  because `reqwest` normalises `..` out of a path before sending and
  would otherwise make the traversal tests unable to fail. A second
  two-hub rig models **one project with two volumes**, and checks that
  each volume reaches its own hub with its own prefix-bound credential.
  The fake hubs enforce `content_length_limit` on their upload route
  exactly as the real one does — a stand-in that accepts what the
  product refuses does not test the product, and that gap let a 411 on
  every upload reach the drill.
- `tests/regression/gateway-kind-e2e.sh` — the cluster drill: real
  hubs, real S3 (in-cluster MinIO), one project with two volumes, and
  the four things no local test can answer (the derived token against a
  real hub, the headless endpoint resolving, the wake path, RBAC).
- `tests/regression/chart-render-pass.sh` — the chart's three refusals,
  the RBAC (no Secrets, no create, no delete), the whole-directory token
  mount, the auto-admitted NetworkPolicy peer, and that
  `maxUploadBytes` renders as an integer rather than `5.36870912e+09`.
