# flint-lite for agent fleets

A shared POSIX filesystem for agent workspaces, durable in your own S3
bucket, that scales to zero when nobody is using it.

The shape this guide sets up, which is the one most agentic harnesses
want:

```
        your harness ──REST──▶ gateway ──▶ hub ──▶ S3
                                           │
        agent pods  ◀──NFS mount───────────┘
```

- **Writes go over REST.** One credential, one endpoint, compare-and-swap
  on every write so two agents editing one file cannot silently clobber
  each other.
- **Reads come through a mounted PVC.** The agent sees an ordinary
  directory tree. Files live in S3; the PVC is a cache, and a file that
  isn't local yet is fetched on first touch. That is the point of the
  mount — agents get `grep -r`, `git clone`, `sqlite`, compilers, and a
  corpus far larger than the disk you paid for.

**No privileged pods.** Agent pods mount an in-tree `nfs:` PersistentVolume
that kubelet mounts on their behalf — ordinary, non-root, no
capabilities, no CSI driver, no sidecar. Nothing is installed on the
clusters your agents run in.

Writing through the mount also works; see
[the two doors](#what-the-two-doors-guarantee-about-each-other).

---

## Before you start

| | |
|---|---|
| An S3 bucket | must already exist, **versioning on**. Nothing here creates or deletes buckets. |
| Kubernetes | 1.25+, `kubectl` and `helm` |
| On every node that will mount | an NFS client (`mount.nfs4`) — see [node prerequisite](#the-one-node-prerequisite) |
| If more than one cluster mounts one hub | **globally unique hostnames across the fleet** — see [one hub, many clusters](#one-hub-many-clusters-give-every-client-a-unique-name) |
| Images | `1.35.1` |
| Chart | `flint-lite-operator` `0.2.7` |

## 1. Install the operator and the gateway

Both come from the same chart and the same image — the gateway is the
same binary invoked with a different command, so enabling it pulls
nothing extra.

```bash
kubectl create namespace flint-system

# The token your harness will authenticate with.
kubectl -n flint-system create secret generic flint-gateway-token \
  --from-literal=token="$(openssl rand -base64 32)"

# The root key every per-hub credential is derived from. Never leaves
# the gateway; you will not need to read it back.
kubectl -n flint-system create secret generic flint-gateway-root \
  --from-literal=key="$(openssl rand -base64 48)"

helm install flint-lite-operator \
  oci://registry-1.docker.io/dilipdalton/flint-lite-operator \
  --version 0.2.7 \
  -n flint-system \
  --set gateway.enabled=true \
  --set gateway.tokenSecretRef=flint-gateway-token \
  --set gateway.rootKeySecretRef=flint-gateway-root
```

That is one Deployment for the operator, one for the gateway, and a
`FlintShare` CRD. Check both are up:

```console
$ kubectl -n flint-system get deploy
NAME                            READY   UP-TO-DATE   AVAILABLE
flint-lite-operator             1/1     1            1
flint-lite-operator-gateway     1/1     1            1
```

The chart **refuses to render** without an inbound token, without a hub
credential, or with both hub credentials set — so a misconfiguration is
a `helm` error, not a running open proxy.

> **The gateway exists only in chart 0.2.6 and later.** Older charts
> accept `--set gateway.enabled=true` and render nothing at all, with no
> error and no warning. If the gateway Deployment never appears, check
> `helm show chart` before checking anything else.

Put an Ingress with TLS in front of the gateway when you expose it.
`gateway.service.type` is `ClusterIP` by default and should stay that
way.

The operator never touches your bucket — no create, no delete, no
lifecycle rules. Deleting a share deletes Kubernetes objects; the data
in S3 is exactly as durable as it was a moment before.

## Credentials, end to end

Four credentials exist and they are not interchangeable. This is the
whole set — nothing else needs one.

| Credential | Who holds it | Created by |
|---|---|---|
| **Bucket keys** | the hub | you, in AWS (below) |
| **Gateway token** | your harness | `openssl rand`, step 1 |
| **Gateway root key** | the gateway only | `openssl rand`, step 1 |
| **Per-share API token** | the hub + the gateway | derived, step 3 |

### Dev: MinIO in-cluster

The quickest working setup, and the one every drill in this repo runs
against. No cloud account, no IAM.

```bash
kubectl create ns minio
kubectl -n minio apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: { name: minio }
spec:
  replicas: 1
  selector: { matchLabels: { app: minio } }
  template:
    metadata: { labels: { app: minio } }
    spec:
      containers:
        - name: minio
          image: quay.io/minio/minio
          args: ["server", "/data"]
          env:
            - { name: MINIO_ROOT_USER,     value: "flintdev" }
            - { name: MINIO_ROOT_PASSWORD, value: "flintdev123" }
          ports: [{ containerPort: 9000 }]
          volumeMounts: [{ name: data, mountPath: /data }]
      volumes: [{ name: data, emptyDir: {} }]
---
apiVersion: v1
kind: Service
metadata: { name: minio }
spec:
  selector: { app: minio }
  ports: [{ port: 9000, targetPort: 9000 }]
EOF
kubectl -n minio rollout status deploy/minio
```

Make the bucket and turn versioning on, through a port-forward:

```bash
kubectl -n minio port-forward svc/minio 9000:9000 &
export AWS_ACCESS_KEY_ID=flintdev AWS_SECRET_ACCESS_KEY=flintdev123 AWS_DEFAULT_REGION=us-east-1
aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://flint-dev
aws --endpoint-url http://127.0.0.1:9000 s3api put-bucket-versioning \
  --bucket flint-dev --versioning-configuration Status=Enabled
```

The credentials Secret is the same shape as anywhere else — MinIO's root
user under the **`AWS_*`** names:

```bash
kubectl -n workspaces create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID=flintdev \
  --from-literal=AWS_SECRET_ACCESS_KEY=flintdev123
```

Then point the share at it with **`spec.endpoint`**, which is the only
line that differs from a real-S3 share:

```yaml
spec:
  bucket: flint-dev
  endpoint: http://minio.minio.svc:9000   # forces path-style addressing
  region: us-east-1                       # any value MinIO accepts
  credentialsSecretRef: flint-s3
```

> **A dev setup hides the permission problem.** MinIO's root user can do
> everything, so the grants below are never exercised until you move to a
> real bucket. That is exactly how the missing
> `s3:ListBucketMultipartUploads` went unnoticed — every local drill
> passed. Budget a first-boot failure on your first real bucket, and read
> the hub's log: it names the action.

### Production: a bucket-scoped identity

Create the bucket with versioning on, then an identity scoped to that
bucket only, carrying the grants from
[what those credentials must be allowed to do](#what-those-credentials-must-be-allowed-to-do).
On AWS that is `aws s3api create-bucket` + `put-bucket-versioning`, then
`aws iam create-user` / `put-user-policy` / `create-access-key`; the
resulting key pair goes into `flint-s3` under the `AWS_*` names. With
**IRSA**, omit `credentialsSecretRef` entirely and annotate the hub's
ServiceAccount with a role ARN carrying the same policy.

Any S3-compatible store works the same way — Ceph RGW, MinIO in
production, anything that honours `spec.endpoint`.

### How your harness gets its token

The gateway token is a Secret you created; read it back the same way any
consumer would:

```bash
kubectl -n flint-system get secret flint-gateway-token \
  -o jsonpath='{.data.token}' | base64 -d
```

That single value authenticates every REST call for every project. It is
the one credential your harness needs — it never sees a per-share token,
and it never needs bucket keys.

### Rotating any of them

| What | How | Restart? |
|---|---|---|
| Gateway token | edit the Secret; re-read every 10s | no |
| Per-share token | bump `flint.io/api-token-version`, rewrite that share's Secret | no |
| Bucket keys | rewrite `flint-s3`; label it `flint.io/credentials=true` so the operator notices immediately | no |
| Gateway root key | rewrite, then re-derive **every** share's token | gateway rollout |

The root key is the only one whose rotation is fleet-wide — every
per-share token is a function of it, so treat it as the thing you change
least often.

## 2. Create a workspace

**You write this resource yourself.** There is no auto-provisioning: the
gateway can read and wake shares but is denied `create` and `delete` on
purpose, so whatever owns projects decides that a workspace exists.
Until you build that service, it is you and `kubectl`.

```bash
kubectl create ns workspaces
kubectl -n workspaces create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID=... \
  --from-literal=AWS_SECRET_ACCESS_KEY=...
```

> **The key names are load-bearing.** The secret is loaded with
> `envFrom`, so its **keys become environment variables verbatim** and
> must be the names the AWS SDK reads. Calling one `accessKeyId` leaves
> the SDK with no credentials at all; it falls back to the instance role
> and the hub crash-loops while the share sits at `Starting`:
>
> ```
> ERROR ❌ Server error: Configuration error: tier: bucket posture
> refused: bucket my-team-flint unreachable: dispatch failure
> ```
>
> Note what that message names: **the bucket, not the credentials.**
> If you see it, check the Secret's key names first.

### What those credentials must be allowed to do

The guide's own drill found this the hard way on real S3: a
sensible-looking least-privilege policy is **not enough**, and the hub
refuses to start rather than run half-configured.

```
❌ tier: bucket posture refused: s3:ListBucketMultipartUploads denied
   — the A9 startup sweep cannot run: 403 AccessDenied
```

`s3:ListBucketMultipartUploads` is a **bucket-level** action, which is
what makes it easy to miss — every other multipart permission is
object-level. This is the working policy:

```json
{"Version":"2012-10-17","Statement":[
 {"Sid":"BucketLevel","Effect":"Allow","Resource":"arn:aws:s3:::MY-BUCKET","Action":[
   "s3:ListBucket",
   "s3:GetBucketLocation",
   "s3:GetBucketVersioning",
   "s3:ListBucketVersions",
   "s3:ListBucketMultipartUploads",
   "s3:GetLifecycleConfiguration"
 ]},
 {"Sid":"ObjectLevel","Effect":"Allow","Resource":"arn:aws:s3:::MY-BUCKET/*","Action":[
   "s3:GetObject","s3:PutObject","s3:DeleteObject",
   "s3:GetObjectVersion","s3:DeleteObjectVersion",
   "s3:AbortMultipartUpload","s3:ListMultipartUploadParts"
 ]}]}
```

Two notes on reading the startup log, because it tells you exactly where
you stand:

- **Errors are fatal and name the action.** `bucket posture refused: <action>
  denied` means add that permission; nothing else is wrong.
- **`cannot read lifecycle configuration` is a warning, not an error.**
  The hub starts. It means it could not verify that a multipart-abort
  lifecycle rule exists, so a crashed flush's orphaned parts will bill
  until something aborts them. Grant `s3:GetLifecycleConfiguration` to
  close it — and note that an organisation SCP can deny this even when
  your identity policy allows it, which is what happened on the drill
  cluster.

The same posture check runs against non-AWS stores. A Ceph RGW or MinIO
user needs the equivalent grants, and the same message tells you which.

```yaml
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: fs-proj-a
  namespace: workspaces
  labels:
    flint.io/project-id: proj-a       # how the gateway finds it
spec:
  bucket: my-team-flint               # must exist, versioning ON
  keyPrefix: proj-a/                  # immutable, must end in "/"
  credentialsSecretRef: flint-s3      # absent = IRSA / instance role
  persistence:
    size: 50Gi                        # the CACHE, not the corpus
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: flint-api-token # created in step 3
  settings:
    hydrateFetchParallel: 6           # parallel ranged GETs per restore
```

Size `persistence` for the working set, not the corpus. The PVC is a
cache; the bucket is the durability story, and a lost PVC is a rebuild,
not a loss.

**One prefix, one volume, one hub.** The operator refuses a second share
on the same bucket subtree across every namespace. That is not
frugality: two live hubs on one prefix is a cross-tenant data leak the
moment one is judged dead and the other takes the prefix over.

A project may have several volumes — give each its own `keyPrefix` and
a `flint.io/volume-id` label, and address them as
`/v1/projects/{id}/volumes/{vol}/…`.

## 3. Give the share its API token

The gateway holds no per-hub secrets. Each hub's credential is a pure
function of that share's identity:

```
token = HMAC-SHA256(root, "flint-fileapi/v1:" || endpoint || ":" || bucket
                          || ":" || keyPrefix || ":" || version)
```

One root key produces every hub's credential, so there is nothing to
store and nothing to fan out — and the gateway is granted no access to
Secrets in your workspace namespaces, which is where each tenant's S3
credentials live. The other half of that bargain: **something must write
the same derived value into the share's own token Secret.** With no
projects service, that something is you.

```bash
kubectl -n flint-system exec deploy/flint-lite-operator-gateway -- \
  flint-hub-gateway --root-key-file=/etc/flint/gateway-root/key \
  --derive-for workspaces/fs-proj-a
```

Take that value and write it where the share expects it:

```bash
kubectl -n workspaces create secret generic flint-api-token \
  --from-literal=token="<the derived value>"
```

**Prefer `--derive-for <ns>/<name>` over typing the binding by hand.**
It reads the resource and derives from the share's own fields, so it
cannot disagree with what the serving gateway computes — same code,
same object. The manual form
(`--derive-token '<endpoint>,<bucket>,<prefix>,<version>'`) is the one
step of this design with no feedback: an omitted endpoint (empty is
legal, so nothing complains), a `keyPrefix` missing its trailing slash,
or a mistyped version each yields a token that is perfectly valid,
perfectly wrong, and rejected by every request.

Order does not matter. The token Secret is a projected volume, so a
share created before its Secret exists waits in `ContainerCreating` and
starts the moment the Secret lands. **The share does not reach `Ready`
until the token exists** — so if a share is stuck, check the Secret
before you check anything else.

```console
$ kubectl get flintshares -A
NAMESPACE    NAME        PHASE   ADDRESS                                     BUCKET          PREFIX
workspaces   fs-proj-a   Ready   fs-proj-a.workspaces.svc.cluster.local:2049 my-team-flint   proj-a/
```

Rotating a token is a Secret edit: the hub re-reads it every 10s and the
gateway re-derives, so neither side restarts. Revoke one project by
bumping its `flint.io/api-token-version` annotation and rewriting that
Secret. One field to watch: `spec.endpoint` is part of the binding and
**is** mutable — changing it invalidates every token derived for that
share.

## 4. Write through the REST API

```
GET    /v1/projects/{id}/volumes
POST   /v1/projects/{id}/wake                 -> see below

GET    /v1/projects/{id}/files?path=&recursive=&cursor=&limit=
GET    /v1/projects/{id}/files/content?path=  [Range, If-None-Match]
PUT    /v1/projects/{id}/files/content?path=  [If-Match, If-None-Match]
DELETE /v1/projects/{id}/files/content?path=  [If-Match]
POST   /v1/projects/{id}/files/folder         {"path": "..."}
POST   /v1/projects/{id}/files/move           [If-Match]
```

One `Authorization: Bearer` header, the gateway token from step 1.

```bash
curl -X PUT \
  -H "Authorization: Bearer $GATEWAY_TOKEN" \
  -H 'Content-Type: application/octet-stream' \
  --data-binary @model.safetensors \
  "$GW/v1/projects/proj-a/files/content?path=/models/model.safetensors"
```

Every object carries an `ETag`. Read it, edit, and write back under
`If-Match` and a competing write answers `412` instead of silently
losing your edit:

```bash
ETAG=$(curl -sI -H "Authorization: Bearer $GATEWAY_TOKEN" \
        "$GW/v1/projects/proj-a/files/content?path=/notes.md" \
        | grep -i '^etag:' | cut -d' ' -f2 | tr -d '\r')

curl -X PUT -H "Authorization: Bearer $GATEWAY_TOKEN" \
  -H "If-Match: $ETAG" --data-binary @notes.md \
  "$GW/v1/projects/proj-a/files/content?path=/notes.md"
```

Each endpoint is an NFS compound dispatched **in-process through the
hub's own dispatcher**, not a second reader of the export directory. So
it inherits everything the mount path does: a cold file hydrates from S3
and the caller gets `503` + `Retry-After` rather than a body of zeros,
symlinks are listed but never followed, uploads take the write gate, and
every write produces the capture notes the tier publishes to S3 from.

There is **no route to `/status`** through the gateway, of any shape —
the hub's unauthenticated status document (recovery point, epoch holder,
NFS client list) is not reachable from outside.

## 5. Read through a mounted PVC

### Getting the address right

**Do not put `status.address` in the PV.** The share reports an
in-cluster DNS name, and an in-tree `nfs:` volume is resolved **by
kubelet on the node** — not by the pod, and not by cluster DNS. Most
nodes cannot resolve `*.svc.cluster.local`, so the mount does not fail;
it **hangs**. The pod sits in `ContainerCreating` while a `mount.nfs`
process retries on the node forever. How much it tells you varies by
platform — drilled on kind it produced **no event at all**, and on
Amazon Linux 2023 exactly **one** `FailedMount` — so in neither case is
there anything that looks like an error until you go looking.

What to use instead, by where your agents run:

| Agents run… | `nfs.server` |
|---|---|
| **In the hub's cluster** | the share's Service **ClusterIP** — kube-proxy programs it on every node, so nodes reach it without cluster DNS |
| **In another cluster, or on-prem** | a `LoadBalancer` / `NodePort` address, published via `spec.service.advertiseAddress` |
| **Anywhere, with DNS you run** | a name your **nodes** resolve — not a `.svc.cluster.local` one |

For the same-cluster case:

```bash
kubectl -n workspaces get svc fs-proj-a -o jsonpath='{.spec.clusterIP}'
# 10.96.200.117
```

That address is stable for the life of the Service. Suspend and
hibernate both keep it — only deleting the share releases it.

For the cross-cluster and on-premises cases, tell the operator what
consumers should actually dial and it copies that into `status.address`
verbatim:

```yaml
spec:
  service:
    type: LoadBalancer            # or NodePort, on OpenStack/bare metal
    # nodePort: 32049
    advertiseAddress: "10.0.4.7:32049"   # a routable node or LB address
```

The port is **mandatory** — a bare host is refused at admission, because
an NFS client handed one silently uses 2049. NFS is one long-lived TCP
flow, so prefer a flat or peered network over a cloud load balancer, and
mind LB idle timeouts if you use one: quiet periods on an NFS connection
are normal, and an LB that reaps idle flows will break mounts that were
working.

### The PersistentVolume

```yaml
apiVersion: v1
kind: PersistentVolume
metadata:
  name: proj-a
spec:
  capacity: { storage: 100Gi }        # informational for NFS
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions:
    - nfsvers=4.1
    - proto=tcp
    - hard
    - nconnect=4
    - noatime
    - sec=sys                         # NOT optional — see below
  nfs:
    server: 10.96.200.117             # the ClusterIP, not the DNS name
    path: /                           # the SERVER ROOT
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: proj-a
  namespace: workspaces
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""                # bind to the PV above, not a class
  volumeName: proj-a
  resources: { requests: { storage: 100Gi } }
```

Agent pods mount the claim like any other volume — **no privilege, no
root, no extra images**:

```yaml
spec:
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
    fsGroup: 1000
  containers:
    - name: agent
      image: your-agent:latest
      volumeMounts:
        - { name: workspace, mountPath: /workspace }
  volumes:
    - name: workspace
      persistentVolumeClaim: { claimName: proj-a }
```

Every pod mounting that claim shares one filesystem, with real
close-to-open coherence across nodes and clusters. Kubelet mounts once
per node, so pods on one node share a kernel client and page cache.

**`nconnect>=2` is not optional.** Without it the kernel opens exactly
one connection and silently refuses every additional trunk — no error,
on either side, ever — and one TCP flow will not fill the
bandwidth-delay product on any path longer than a rack.

### The one node prerequisite

An in-tree `nfs:` volume is mounted **by kubelet, on the node**, using
the node's own `mount.nfs4`. If it is missing the pod does not fail
cleanly — it sits in `ContainerCreating` with the reason buried in an
event:

```console
$ kubectl describe pod agent | tail -3
  Warning  FailedMount  ... mount.nfs4: not found
```

Most managed node images (GKE COS, EKS AL2023, AKS Ubuntu) ship it.
Minimal images may not; install `nfs-common` (Debian/Ubuntu) or
`nfs-utils` (RHEL family) in your node bootstrap.

### One hub, many clusters: give every client a unique name

This is the common shape — agents in several clusters, all mounting the
same hub — and it has a prerequisite that is easy to miss because
nothing reports it.

**An NFSv4.1 client identifies itself by hostname and nothing else.** The
Linux client builds its `co_ownerid` as:

```
Linux NFSv4.2 <nodename>
```

No address. No cluster. No uniquifier, unless you set one. And RFC 8881
requires the server to treat an identical `co_ownerid` as *the same
client coming back after a reboot* — so flint cannot tell two clusters
apart, and is not permitted to guess.

The collision is not exotic. A fleet that applies one manifest in every
cluster gets the same pod names in every cluster, and two clusters built
from the same template on the same VPC CIDR get the same node names.
Captured from two clusters mounting one hub, byte-identical:

```console
$ tcpdump -r mount.pcap -A | grep 'Linux NFSv4'
cluster B → Linux NFSv4.2 agent
cluster C → Linux NFSv4.2 agent
```

When it happens, the second client's mount is read as the first one
rebooting, and the first one's session and open state are discarded. The
hub says so, but at `info`, and it reads like routine housekeeping:

```console
EXCHANGE_ID: case 5 (client reboot detected) — deferring cleanup of clientid 36
```

**Which name matters depends on who mounts.** A `PersistentVolume` is
mounted by kubelet in the node's namespace, so the identity is the
**node's** hostname. A pod that runs `mount` itself uses the **pod's**
hostname. Whichever applies, it has to be unique across every cluster
that mounts this hub — not just within one.

Two ways to get there:

- **Name the nodes uniquely.** Include the cluster name in the node
  hostname. If your clusters share a VPC CIDR and derive hostnames from
  the private IP, they *will* collide.
- **Set the uniquifier.** `nfs.nfs4_unique_id=<something-unique>` as a
  kernel module parameter on each node — a drop-in under
  `/etc/modprobe.d/`, applied at node bootstrap. Read it back at
  `/sys/fs/nfs/net/nfs_client/identifier`; `(null)` means it is not set
  and the hostname is doing all the work.

To check a running fleet, read the identity each client actually sends
and count the distinct values. If the count is lower than the number of
clients, you have a collision.

**A second reason, which shows up only when something goes wrong.** State
loss on a collision is the obvious hazard, but a shared name also breaks
the server's ability to *tell you* about a loss. When a lease expires, the
server may release that client's locks — that is deliberate, and it is
what lets a partitioned cluster's ranges become available again. The
protocol's way of reporting it is a status flag on the next `SEQUENCE`,
and that flag is addressed to a **client id**, not to a cluster. Two
clusters sharing one `co_ownerid` share one client id, so whichever
cluster's traffic arrives first can consume the notification — and the
cluster that actually lost the range never hears about it. It carries on
believing it holds a lock the hub has already handed to someone else.

Unique client names are what make that report deliverable to the cluster
it concerns. This is a modelled result, not a measured one: see
`formal/FlintClientIdentityLeaseNotify.cfg` and its `...Unique`
counterpart, which differ in exactly that one setting.

### Credentials and permissions on the mount

**The mount carries no credentials.** There is no secret, no key, no
token on the NFS path. Identity is whatever the client asserts, and the
server takes it at its word — there is no root-squash.

**`sec=sys` is not optional, and leaving it out fails quietly.** Without
it the kernel negotiates `sec=null`, which sends **no credential at
all**. Measured on a real mount, the difference is total:

| | `sec=null` (the default you get) | `sec=sys` |
|---|---|---|
| File created by a uid-1000 pod | owned `0:0` | owned `1000:1000` |
| A non-root pod writing into a root-owned `0755` dir | **succeeds** | obeys the mode |
| Effective identity of every client | root | the pod's own uid |

So a mount without `sec=sys` gives every agent root on the share and
makes ownership meaningless — and nothing anywhere reports this. Check a
live mount with:

```console
$ mount | grep workspace
10.96.200.117:/ on /workspace type nfs4 (rw,...,nconnect=4,sec=sys,...)
```

Two more consequences worth designing around:

- **Reachability is the access control.** Anything that can route to the
  hub's port 2049 can claim any uid. Keep hubs on the cluster network,
  and if agents run outside it, put them on a peered or flat network
  rather than exposing 2049.

  The chart can restrict who reaches 2049:

  ```bash
  --set networkPolicy.enabled=true \
  --set 'networkPolicy.hubNamespaces={workspaces}' \
  --set 'networkPolicy.nfsClientSelectors[0].podSelector.matchLabels.app=agent'
  ```

  Two ways to get a policy that reads correctly and protects nothing:
  **`hubNamespaces` defaults to `[]`**, so enabling the policy without
  it guards no hub at all; and several CNIs ignore NetworkPolicy in
  silence, so confirm yours enforces it before relying on it. The
  gateway is admitted to the hubs' 8080 automatically — you do not
  repeat its selector.
- **With `sec=sys`, files are owned by the uid that created them.** Run
  every agent for one workspace under the **same** `runAsUser` and
  `fsGroup`, or give them a shared group and group-writable
  directories — otherwise agent B cannot write what agent A created.
  Files written over REST are owned by the hub's own identity, so if
  your agents will also write through the mount, keep them in one group
  with `fsGroup` set.

### Cold reads: what an agent actually experiences

At the disk watermark (default 85%) cold files are truncated to stubs.
Metadata stays truthful — `ls -l` and `df` show logical sizes — so a
tree looks complete whether or not it is local.

The first read of an evicted file parks the client until the **whole
file** restores from S3. Kernel clients retry silently; the application
just sees a slow open. Restores issue up to `hydrateFetchParallel`
(default 6) ranged GETs concurrently, and one S3 stream is roughly
80–200 MB/s, so the fan-out divides a large file's cold-read time.
Writers get a reserved slot and are never starved by readers.

Drilled end to end: with the file confirmed gone from local disk — the
hub's own copy at **0 bytes, 0 blocks**, the data only in the bucket — an
agent `sha256sum`'d it through the mount and got back **byte-identical**
content, with no error and nothing in its way. Transparency is the
claim, and it holds.

**Do not size your expectations from a lab number.** That test ran
against in-cluster object storage and finished in milliseconds; a real
bucket over a real network is a different order of magnitude, and the
time scales with file size divided by your fan-out. Measure it on your
own path with your own file sizes before promising an agent a latency.

**If your agents sweep the tree** — `grep -r`, a build, a full index —
per-file hydration is the wrong shape, because it pays one round trip
per file. Set `hydrateWarmAfterImport: true` and the hub bulk-restores
every stub after an import, smallest files first, on a dedicated pool,
stopping short of the eviction watermark. It survives hub restarts and
logs one `tier warm fill done` line when the tree is hot. Off by
default, because a fill re-downloads every byte.

## On a real cluster: cloud and on-premises

Everything above is the same everywhere. Four things differ by platform,
and all four live in the `FlintShare`.

### Storage for the hub's cache

The PVC uses the cluster's **default StorageClass** unless you say
otherwise. Any RWO volume works — the hub writes plain files, so NVMe is
a performance choice, never a requirement.

```yaml
spec:
  persistence:
    size: 50Gi
    storageClassName: gp3            # AWS
    # storageClassName: csi-cinder-high-speed   # OpenStack / Cinder
    # storageClassName: rook-ceph-block         # on-prem Ceph
```

### Bucket credentials

```yaml
# AWS with IRSA / instance role — no Secret at all
spec:
  bucket: my-team-flint
  region: us-east-1
  # credentialsSecretRef omitted = ambient (env / IRSA / IMDS)
```

```yaml
# On-prem: Ceph RGW, MinIO, or any S3-compatible store
spec:
  bucket: my-team-flint
  endpoint: https://rgw.corp.internal:8443   # forces path-style addressing
  region: us-east-1                          # any value the store accepts
  credentialsSecretRef: flint-s3
```

`spec.endpoint` is what makes a non-AWS store work — absent means real
S3. **It participates in the share's identity**, so it is part of the
derived API token: changing it invalidates every token for that share,
and re-deriving is the fix.

### Reaching the hub

Covered in [getting the address right](#getting-the-address-right).
Briefly: same cluster → the Service ClusterIP; another cluster or
on-prem → a `LoadBalancer` (Octavia on OpenStack, NLB on AWS — use
`spec.service.annotations` for internal-LB annotations) or a `NodePort`,
published with `spec.service.advertiseAddress`.

Prefer a flat or peered network to a load balancer. NFS is one
long-lived TCP flow and quiet periods are normal, so an LB that reaps
idle connections will break mounts that were working fine.

### An NFS client on the nodes

| Node image | Status |
|---|---|
| Amazon Linux 2023, Ubuntu, GKE COS, AKS Ubuntu | ships `mount.nfs4` |
| Bottlerocket, Talos, minimal/custom images | check before you rely on it |

Install `nfs-common` (Debian/Ubuntu) or `nfs-utils` (RHEL family) in
node bootstrap if it is missing. This is a **node** dependency, not a
pod one — no image of yours needs anything.

### Keep the hub and its agents in one zone

Inter-AZ traffic is billed in both directions and a chatty POSIX
workload adds up faster than it looks. Co-locate with
`spec.nodeSelector` where your scheduler does not already do it.

## What it performs like

Measured on a 3-node AWS cluster (`i4i.large`, us-west-1), agent pod and
hub on **different nodes**, against a local-disk control in the **same
pod at the same instant**. The ratio is the portable finding; the
absolute numbers belong to that hardware and travel nowhere.

| 512 MiB sequential | NFS | local disk | ratio |
|---|---|---|---|
| write | 104 MB/s | 173 MB/s | 0.6× |
| read | 164 MB/s | 147 MB/s | 1.1× |

| 2000 × 4 KiB files | NFS | local disk | ratio |
|---|---|---|---|
| create | 16,172 ms (123/s) | 1,293 ms | **12.5× slower** |
| stat | 2,565 ms | 1,964 ms | 1.3× slower |
| delete | 8,007 ms | 36 ms | **222× slower** |

**Streaming is fine; metadata is the constraint.** Reading a tree is
close to native because attribute caching absorbs the `stat` traffic.
Creating and deleting are not cacheable — each is a synchronous round
trip — and that is what dominates the tools agents actually run.

Design around it:

- **Do small-file-heavy work on local scratch**, persist artifacts.
  `npm install`, a build tree, or `git clone` of a large repository will
  be paced by create/delete, not by bandwidth. An `emptyDir` for
  `node_modules` and the workspace for source and outputs is the usual
  shape.
- **Move bulk content through the REST door as one stream** rather than
  thousands of individual creates through the mount.
- **Reading is cheap.** Agents that walk, grep and read a shared corpus
  are the case this is good at.

### Hydration from S3

A 1 GiB file, confirmed evicted (0 blocks on the hub's disk), read back
through the mount **byte-identical in 131 s** — single-stream, because
that drill pins `hydrateFetchParallel: 1` to make eviction observable.
The default is **6**, so a default-configured share should be materially
faster. That figure has not been measured here, and dividing by six
would be arithmetic rather than a measurement — so budget from your own
file sizes and fan-out.

## What the two doors guarantee about each other

They are one filesystem: the REST API dispatches in-process through the
same server the mount talks to. But the *timing* is NFS timing, and this
is the part most likely to surprise you.

- **A REST write shows up on an established mount immediately** — as
  long as the agent opens the file to look. Measured on a live mount
  with the default `actimeo`: a newly created file and an overwrite of a
  file the agent had already read were both visible in **under a
  second**. That is close-to-open consistency doing its job — every
  `open()` revalidates with the server, so the attribute cache does not
  sit between your harness and your agents.

  Two cases where staleness is still real, and they are the ones to
  design against:

  - **A file the agent holds open across the write.** It is reading a
    snapshot from `open()` time; nothing will tell it otherwise. Reopen
    to resynchronise.
  - **A cached directory listing.** `readdir` results are cached for up
    to `acdirmax` (60s by default), so a long-running `ls` loop can miss
    a new file for a while even though opening it by name works.

  Lower `actimeo` only if the second case actually bites you, and know
  you are buying it with round trips.
- **`If-Match` is detection between API callers, not exclusion against
  the mount.** Two REST writers get `412`. A REST writer and a mounted
  writer racing on the same file are ordered by the server but the ETag
  will not save you. If both doors write, use byte-range locks from the
  mount side, or keep each file's writes on one door.
- **An ETag changes when a file is evicted or hydrated**, because both
  rewrite the local inode. Nothing the user did changed. Treat a lone
  `412` as "re-read before you write", not as evidence of a concurrent
  editor.

Close-to-open consistency between *mounted* clients is unaffected by any
of this: a file closed by one agent is seen whole by the next to open
it.

## Keeping a workspace awake

Scale-to-zero is off unless you ask for it:

```yaml
spec:
  idle:
    suspendAfterSecs: 900             # 15m quiet -> scale to zero, keep the PVC
    suspendWithSessions: false        # read the warning below
```

A share suspends only when **two** signals agree: no wake request
recently, and the hub's own activity clock is quiet.

> **An agent that thinks for twenty minutes trips both.** It is touching
> nothing on the filesystem, so the hub sees an idle share; nothing is
> stamping the wake annotation, so the front door looks absent. The hub
> scales to zero underneath a live `hard` mount — and a `hard` mount
> whose server is gone blocks in uninterruptible sleep. **An NFS client
> cannot write a Kubernetes annotation, so nothing wakes it.** Not data
> loss; an indefinite hang.

Two defences, and you want both:

- **`spec.idle.suspendWithSessions: false`** refuses to suspend while a
  client still holds a lease. **The default is to suspend anyway; the
  protective value is opt-in.** The residual, stated honestly: NFSv4
  leases expire, so a long enough partition drops the lease count to
  zero on its own.
- **A heartbeat while any session is live.** `POST /wake` *is* the
  heartbeat — it re-stamps on every call even when the share is already
  `Ready`, and it tells you how often to call it, so you do not have to
  derive the timer yourself:

  ```console
  $ curl -sX POST -H "Authorization: Bearer $GATEWAY_TOKEN" \
      "$GW/v1/projects/proj-a/wake"
  {
    "project": "proj-a",
    "volume": "data",
    "phase": "Ready",
    "address": "fs-proj-a.workspaces.svc.cluster.local:2049",
    "serverId": "9f1c…",
    "apiEndpoint": "fs-proj-a-api.workspaces.svc.cluster.local:8080",
    "requested": true,
    "suspendAfterSecs": 900,
    "keepaliveSecs": 450,
    "mountWarning": "this share can be suspended while mounted; set spec.idle.suspendWithSessions: false"
  }
  ```

  Three fields a harness should actually read:

  - **`keepaliveSecs`** — the interval to call `/wake` at. It is half
    the suspend budget, so one missed beat still survives.
  - **`mountWarning`** — present exactly when this share can be scaled
    to zero out from under a live mount. If you are holding a mount and
    this field appears, you have the hazard above. Treat it as an error
    in your provisioning, not as advice.
  - **`serverId`** — compare it across wakes. Stable across an ordinary
    restart; **different after a hibernate**, because that deletes the
    PVC and every filehandle a client holds becomes invalid. A changed
    `serverId` means remount, not resume. Absent means *not observed* —
    never treat absent as changed, or every first wake looks like a
    remount.

    **Measured caveat: `status.serverId` is only published when the idle
    ladder is configured.** A share with `monitoring.enabled: true` and
    no `spec.idle` block never reports one — drilled at 180s on a Ready
    share, still empty; adding `spec.idle.suspendAfterSecs` produced it
    within 12s, matching the hub's own `/status` exactly. The field is a
    byproduct of the idle poll, not of monitoring. In practice a share
    with no idle ladder never hibernates, so the value could not have
    changed anyway — but if your harness keys remount decisions off
    `serverId`, know that it stays empty until you arm the ladder.

Do **not** heartbeat by polling files. `/status` is deliberately not
activity and the file API deliberately is, so a UI that refreshes a
listing on a timer pins that project awake forever and the ladder never
fires. A conditional GET answering `304` pins it exactly as hard as a
re-download.

Leave `idle:` out entirely and none of this applies — the hub stays up.

For a fleet crawl that must not start 2000 hubs, add `?wake=false` to a
files request: it refuses with `503 Parked` instead of waking anything.
`GET /volumes` touches no hub at all.

## Tearing a workspace down

**Unmount before you remove the server.** Order matters, and getting it
wrong is not a tidy failure.

A `hard` mount whose server disappears puts its clients into
uninterruptible sleep. Those processes cannot be killed — not with
`SIGKILL`, not with `--force --grace-period=0` — because the kernel is
waiting on I/O that will never complete. On a node this can wedge the
container runtime itself: `docker rm -f` returns *"tried to kill
container, but did not receive an exit event"*, and the only way out is
restarting the runtime.

This is not theoretical; it is the same mechanism as the suspend hazard
above, and it happened while drilling this guide.

The order that works:

1. Scale the consumers to zero and let their mounts go away.
2. Confirm nothing holds the claim (`kubectl get pods -o wide`).
3. Then delete the `FlintShare`.

`spec.reclaim` decides what happens to the PVC — `Retain` by default.
The bucket is never touched by any policy, so the data outlives all of
this.

If you are already wedged: the mount holder cannot be killed, so bring
the **server** back instead. Recreating the share at the same address
lets the blocked I/O complete and the processes exit normally.

## What this is not

- **Not a parallel filesystem.** One hub is one pod and one NIC. If a
  single pod's bandwidth is your ceiling, the full pNFS profile speaks
  the same protocol and stores the same volume format — graduating is
  adding data servers, not migrating.
- **Not multi-writer across regions.** Keep hub and agents in one
  availability zone where you can; inter-AZ bytes are billed both
  directions and a chatty POSIX workload adds up fast.
- **Not a bucket manager.** Create the bucket, turn versioning on, and
  hold the lifecycle policy yourself.
- **Not a place for secrets you would not give every agent.** Anything
  that can reach 2049 can read the tree as any uid.

## How this guide was verified

Every instruction here was executed against a real cluster by
`tests/regression/agent-fleet-doc-drill.sh`, which runs the guide's own
commands rather than a paraphrase of them. Run it yourself:

```bash
./tests/regression/agent-fleet-doc-drill.sh          # kind + in-cluster MinIO
KEEP=1 ./tests/regression/agent-fleet-doc-drill.sh   # leave the cluster up
```

What it establishes, and why each has a control:

| | Verified by |
|---|---|
| Wrong `AWS_*` key names never reach `Ready` | a share built on `accessKeyId` crash-loops with the error quoted above |
| Nothing in the YAML is silently pruned | every field read back after admission |
| A `.svc.cluster.local` server does not mount | pod stuck `Pending`, **zero** `FailedMount` events |
| The ClusterIP does mount, unprivileged | non-root pod, no capabilities, kubelet does the mount |
| `sec=sys` is load-bearing | same pod, same server, one option removed ⇒ `0:0` instead of `1000:1000` |
| REST writes reach a live mount | new file and overwrite, both under a second |
| Eviction really hydrates | hub's local copy at 0 bytes/0 blocks before the read |
| `suspendWithSessions: false` works | an unprotected control share **did** suspend under a live mount in the same run |

The last one is the pattern to notice: a leg that only shows the good
case proves nothing, because "it stayed up" is also what a broken idle
ladder looks like. Three legs found real errors in earlier drafts of
this guide — the DNS-name PV, the missing `sec=sys`, and an overstated
warning about cache staleness that the measurement disproved.

## Where to go next

| | |
|---|---|
| The hub on its own, without the operator | [`flint-lite.md`](flint-lite.md) |
| Every field, the idle ladder, the S3 tier | [`flint-lite-operator.md`](flint-lite-operator.md) |
| The gateway in full | [`flint-hub-gateway.md`](flint-hub-gateway.md) |
| How the pieces sit together | [`flint-lite-architecture.pdf`](flint-lite-architecture.pdf) |
