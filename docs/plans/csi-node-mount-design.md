# Replacing the flint-passthrough and flint-lean webhooks with a CSI node DaemonSet — design of record

Status: **RESEARCH — no code; produced 2026-09-02.** Nothing in this
document is built. It is the design of record for the question the user
asked, written so that the first line of code can be judged against it,
and it names the three facts that must be verified before that line is
written (end of this header). Read §9 before any code.

Provenance: multi-agent design workflow over the repo at `a9f3facd`
(v1.44.0) — 8 recon reports (4 over the code: `code-auth`, `code-csi`,
`code-lean`, `code-passthrough`; 4 over the web: `web-istio-precedent`,
`web-fuse-csi-priorart`, `web-k8s-csi-mechanics`, `web-knox-jwt`), then
3 independent designs (minimal-change-first, operability-first,
security-first), scored by the architect on (a) correctness against the
recon, (b) security of the identity chain, (c) operability, (d) fidelity
to the ask, (e) honesty about unknowns. Tally: **operability-first
8/8/9/9/9 = 43**, minimal-change-first 8/6/8/8/9 = 39, security-first
7/9/6/8/9 = 39. Operability-first is the spine. Every idea grafted from
the other two is marked **[graft: minimal]** or **[graft: security]** at
the point it lands. Repo citations are `path:line` as read by the recon
at `a9f3facd`; external citations are `[recon §n]` with the URL list in
§12. A claim about Kubernetes, CSI, Knox, Mountpoint or the repo that
carries no citation is labelled **ASSUMPTION**.

**The answer, in one paragraph.** *Passthrough: yes.* The privileged
native sidecar exists only to obtain `mountPropagation: Bidirectional`
(`spdk-csi-driver/src/passthrough/inject.rs:15-28`), which the API server
permits only on privileged containers ([web-k8s-csi-mechanics §6.1],
`validation.go:1473-1476`); a CSI *ephemeral inline* volume served by a
privileged node DaemonSet that performs the `mount(2)` and hands the
`/dev/fuse` fd to an unprivileged, flint-owned "worker" pod (the AWS
Mountpoint CSI v2 mechanism, [web-fuse-csi-priorart §2]) removes the
privileged container, the tenant-chosen image, the tenant-namespace
credential and the webhook from the tenant pod in one move, and the
tenant namespace can enforce PodSecurity `restricted` because
`spec.volumes[*].csi` is on the restricted allow-list
([web-k8s-csi-mechanics §1.5]). *Lean: yes, conditionally* — the lean
sidecar is not a mount but a process that lives with the pod (claim →
checkout → barrier loop → drain on SIGTERM, `lean/sidecar/src/bin/flint_sync.rs:340-374,583-604`);
CSI supplies the two moments it needs — kubelet blocks every container
on `NodePublishVolume` and calls `NodeUnpublishVolume` only after every
container has exited ([web-k8s-csi-mechanics §4.3, §5]) — but the loop in
between still needs a pod-lifetime process, which this design hosts in
the same flint-owned worker pod, running the unchanged `flint-sync`
binary. *Per-pod, per-project mounts are native to CSI*: one
`NodePublishVolume` per pod with a pod-unique volume id, kubelet-asserted
pod identity and, with `tokenRequests`, a pod-bound ServiceAccount JWT
([web-k8s-csi-mechanics §1.2, §2, §3.3]). *Knox*: the pod is never given a
Knox JWT and, with the K1 cert gate (§4.3), cannot mint one; the node
never holds a Knox credential; one broker
(`flint-s3-broker`, STS-shaped) turns the kubelet-minted pod identity into
short-lived SigV4 keys via Knox, because no S3 wire accepts a bearer
([web-knox-jwt §6.1]) — and every Knox-dependent step is marked
VERIFIED or UNVERIFIED in §4.

**The three things that must be verified before code** (§9 has the
full ranked list and the experiment for each):

1. **What the customer's project-scoped S3 proxy accepts on the wire** —
   SigV4 with keys it issues from a JWT (STS-shaped or a credentials
   endpoint), or only `Authorization: Bearer <Knox JWT>` on the data path.
   Everything in §4's back half hinges on it ([web-knox-jwt §8 Q1]).
2. **Which Knox, and whether it verifies Kubernetes projected tokens** —
   Apache 2.1.0 vs a Cloudera build with IDBroker; and the `typ`-header
   question (K8s projected tokens carry no `typ`; Knox 2.1.0's default
   allowed set is `{JWT}`; KNOX-3434 — opened 2026-09-02/03; the date is UNVERIFIED (one day
   after this document's own date, most likely JIRA server time from
   the REST fetch; a re-fetch of `created` with its UTC offset settles
   it) — says they "would fail verification") ([web-knox-jwt §3.3]).
3. **The lean tree-delivery and drain mechanics on a real kubelet** —
   that a bind mount made on the host under a running worker pod's
   hostPath is what the syncer sees, that a 10 GiB checkout survives
   retried `NodePublishVolume` calls under kubelet's 2-minute deadline and
   2m2s backoff cap, and that `NodeUnpublishVolume` runs after a
   `--grace-period=0 --force` delete ([web-k8s-csi-mechanics §4, §5]).

---

## 0. Decisions after the design, and what the build found

Written 2026-09-03, after the code and the kind drill. This section
overrides the header and §10.1 where they differ.

**Delivery.** The user: *"don't worry about upgrade related to
webhooks. This is not deployed anywhere yet."* The CSI node DaemonSet is
therefore THE delivery, not a second one. §10.1's coexistence machinery
(`delivery: webhook | csi | both`, the two cross-guards, the L1 bridge)
is NOT built; the webhooks, their cert bootstrap and the sidecar
injectors are removed outright.

**Is an operator still needed?** *Passthrough: no.* Its CRD is the whole
control surface and the node plugin reads the CR itself; the
`flint-passthrough-operator` binary had no reconcile, only the webhook.
*Lean: a thin controller stays* — claim stamping with both adopt arms,
bucket posture and the MPU sweep (`lean_operator/reconcile.rs`) are
bucket-side work no pod should do — but the syncer claims the LEASE
itself, so a lean volume mounts with the controller absent (S11 runs on
a rig with no lean operator installed). The MWC, the cert Secret, port
9443 and the sidecar injector are gone from that binary.

**Is a registrar needed?** Yes: `csi-node-driver-registrar` is a
sidecar of the DaemonSet (the chart's `csiSidecars`), the only way a
node plugin's socket reaches kubelet; S1 asserts `CSINode` lists the
driver and that a plugin restart re-announces it within seconds.

**Knox.** *"I don't directly access apache knox … access happens
through REST API … the application enforces security using JWT tokens
… a s3 proxy is used, so the pod does not need to have access to the
actual s3 credentials."* So Knox is out of the pod path and out of
flint's: the broker's `rest` backend presents the kubelet-asserted
identity (cluster issuer, namespace, ServiceAccount, pod uid) to the
application's own JWT-enforcing REST API, and the application answers
with project-scoped keys from the proxy (K0 of §4.3: the application
trusts the cluster's token issuer, or the broker's own credential to
that API). `static` is the rig/interim arm, `sts` the arm for a proxy
that speaks `AssumeRoleWithWebIdentity`. In every arm the worker reads
its keys from a loopback door (`AWS_CONTAINER_CREDENTIALS_FULL_URI`) —
the CRT's web-identity provider is HTTPS-only, so the door, not
`AWS_WEB_IDENTITY_TOKEN_FILE`, is how a plain-http broker delivers.

**Agents on different clusters sharing one project's artifacts** (user
question, 2026-09-03). The chart is installed per cluster; nothing in
flint spans clusters — the bucket prefix is the meeting point, exactly
as with the webhooks. What changes is the *control*, which is layered,
not "whoever creates the CR":

1. RBAC on the CRD decides who may NAME a bucket/prefix in a namespace
   and list which ServiceAccounts may consume it (`spec.consumers`).
2. RBAC on pods and ServiceAccounts decides who may RUN under a listed
   SA; the SA is kubelet-asserted, never chosen by the pod spec.
3. The broker backend decides what a `(cluster issuer, namespace, SA,
   CR)` tuple is GIVEN. Under `static` the CR is effectively the whole
   control on that cluster (one key, bucket-agnostic) — fine for a rig,
   not for a fleet. Under `sts`/`rest` the CR is only a name and the
   application or proxy is the authority: each cluster's projected
   tokens carry that cluster's `iss`, so a project policy can say
   "cluster A's SA `trainer` and cluster B's SA `eval` may touch project
   P", and a CR on a cluster the policy never named yields nothing.
4. For lean, two clusters on one prefix are arbitrated by the same
   mechanisms as two pods on one cluster: the claim cell's `projectId`
   (adopt-own / refuse-foreign) and the epoch lease (one writer; a
   second syncer waits or takes over after the quiet polls). CSI does
   not change those semantics; passthrough has none beyond S3's.

**The drills.** `s3csi/e2e/run-s3csi.sh` — one kind cluster, MinIO in
it, 16 legs (S1-S10, S11, S13, S15, S19, SU) including the lean arm.
`s3csi/e2e/multi/run-multi.sh` — TWO kind clusters and ONE MinIO
running outside both, for the cross-cluster question: M1 passthrough
(bytes written on one cluster read on the other, and a second prefix
that sees none of it), M2 identity (a pod-bound token minted by cluster
1 is refused by cluster 2's broker at TokenReview, while cluster 2's
own token is authenticated and refused only for its missing
registration), M3 lean (a project seeded on cluster 1 is checked out
complete and byte-correct by a cold pod on cluster 2, a file written
there is drained back and read on cluster 1, and while cluster 1 holds
the workspace lease cluster 2's pod waits rather than starting ungated).

**The three "verify before code" items.** (1) The proxy's wire is
SigV4 with keys the application issues — the `rest` backend; no
bearer-on-the-data-path arm is built. (2) Knox is not on the path (see
above). (3) Lean mechanics measured on a real kubelet — the S11/S13
record in §11.1.

**What the kind drill found in the design (2026-09-03).**

- *Mountpoint's own ACL.* §3.4 step 9 said the kernel's `allow_other`
  makes `--allow-other` on the mount-s3 argv redundant in fd mode. It
  is not: Mountpoint's FUSE session (fuser) enforces an owner-only ACL
  in userspace and answers every lookup/getattr/open/statfs from a uid
  other than the daemon's with `EACCES` unless the flag is given. With
  the daemon at uid 1001 that refused root's readiness `statfs` and no
  publish could ever finish. Measured on the node with a hand-mounted
  matrix (kernel option × daemon flag); the flag is passed in both
  shapes now (`inject.rs::mounter_args_for`).
- *The kubelet finalizes exited pods.* §3.6's admission policy allowed
  DELETE only from the node ServiceAccount and the kube-system
  controllers. The KUBELET issues the final delete of every pod that
  has exited; refused, it retries every 10 s forever (`status_manager
  "Failed to delete status for pod"`) and the worker sits
  `Succeeded`+`deletionTimestamp`. The policy now admits a kubelet's
  delete of workers bound to its own node (`system:node:<nodeName>`).
- *A retried publish must not adopt an exited predecessor.* The cleanup
  of a failed publish deletes its worker; kubelet's retry can arrive
  before the object is gone, and adopting it reported the old exit as
  the new failure. `worker::ensure` now deletes (grace 0) and waits out
  a terminating or exited namesake before creating.
- *A second view of the plugin directory breaks mount propagation.* The
  DaemonSet mounted the kubelet root `Bidirectional` AND the plugin
  directory again at `/csi` with no propagation. The FUSE source mount,
  made through that private view, existed only in that container's
  mount namespace: a DaemonSet roll lost it, and the next unpublish
  failed with `rmdir: Resource busy` forever (kubelet retries a
  `TearDown` for the life of the pod). The plugin now reaches everything
  through the one propagated mount, socket included. The republish
  liveness probe also asserts the source IS still a mount point — a
  plain directory answers `statfs` and `readdir` perfectly well, which
  is exactly what a lost mount looks like.
- *The broker's registration table is in memory.* A broker restart (a
  roll, an eviction, the drill's own outage control) left every mounted
  volume unable to refresh: the exchange was refused "no live publish
  registration" until the key expired, and the tenant lost the mount.
  The plugin now re-registers before EVERY refresh — the registration
  is idempotent and node-authenticated, and a failure there is an
  outage (keep the cached key), not a refusal (drop it).
- *The lean syncer needs `AWS_REGION` in its environment.* mount-s3
  takes `--region` on its argv, so passthrough never showed this; the
  syncer's SDK client fails its first request as a bare "dispatch
  failure" without one. Every credential arm now carries the node's
  configured region unless the arm's own Secret named one.
- *The credential document must always carry `Token`.* The door served
  `{AccessKeyId, SecretAccessKey, Expiration}` for an arm with no
  session token. mount-s3's CRT provider accepts that; the AWS **Rust**
  SDK's JSON credentials parser treats `Token` as required and rejects
  the document, which surfaces in the syncer as the same bare "dispatch
  failure" a missing region gives. `Token` is now always present, empty
  when there is none — the two clients in this design do not agree on
  what the container-credentials document may omit, so the union of
  their requirements is the contract.
- *The credential door has to be bound before the child is spawned.*
  The worker bound its loopback door inside the thread it spawned for
  it, concurrently with the mounter/syncer. A child that reaches the
  door first gets connection-refused, which an AWS SDK also reports as
  a bare "dispatch failure". The bind now happens in `main`, before the
  spawn, and a bind failure is fatal with its own message rather than a
  mystery in the child's log.
- *A same-filesystem bind is invisible to the classic mount-point test,
  and that DELETED A TENANT'S DATA.* `is_mountpoint` compared `st_dev`
  with the parent's. A `MS_BIND` of a directory into a target on the
  same filesystem keeps the device number, so every lean bind (tree and
  target both under `/var/lib/kubelet`) read as "not a mount". A
  republish therefore missed the `published` branch, fell through to
  "an unfinished publish: start over", and `cleanup` removed the volume
  directory — the live tree — under a running agent. The signature was
  a manifest citing only the LAST 22 of 200 seeded files: everything
  written before the wipe was gone, and the syncer published what was
  left. The test now reads `/proc/self/mountinfo`, and `cleanup`
  refuses a published lean volume outright.

  **Would the formal models have caught it?** No — and there is now a
  module that does. `formal/FlintCsiMount.tla` (12 TLC runs in
  `scripts/check-tla.sh`) models this lifecycle across CLUSTERS SHARING
  ONE PREFIX with the mount test as a FIRST-CLASS STATE VARIABLE that
  can disagree with the kernel. Under `MountOracle = "mountinfo"` the
  strict run holds; under `"dev"` TLC finds the tree loss, and a second
  run restricted to the durability invariant walks the whole path —
  sensor lies, cleanup wipes, agent writes on, the next full-snapshot
  publish overwrites the prefix — in eight steps. Two runs either side
  of `SameFsBind` are the argument in miniature: the same blind oracle
  loses data on a same-filesystem bind and HOLDS on a foreign-filesystem
  one, which is why months of green passthrough drills sat over a live
  defect. The multi-cluster mutations are in the same module because the
  bucket is the only coupling between clusters: `LeaseCheck = FALSE`
  violates single-writer exclusivity, `DrainBeforeRelease = FALSE` loses
  the departing cluster's late files to the next cluster's checkout, and
  two required-fail vacuity probes keep the tranche from going green
  over an empty road. The module also carries the failure this fleet
  actually has — a cluster reclaimed while it holds the workspace, which
  takes the node, the worker and the tree with it and leaves the prefix
  stamped with a holder that no longer exists. Durability and
  exclusivity survive it (what the app was told is in the bucket; the
  un-published tree is a known loss with a named recovery), and the
  supersede arm of the bucket's CAS cell is what lets the surviving
  cluster proceed: turn it off and TLC finds the starvation lasso, a
  project unreachable from every cluster with nothing in the data plane
  saying why.

  The reason the EXISTING models could not have caught it is the one
  `[[project_formal_models]]` already records three times over: the
  ABSTRACTION was the bug. The state machine here is correct —
  publishing → checking-out → published → drain → unpublish — and a
  model of it verifies happily. What was wrong is a PREDICATE the model
  would have taken as ground truth: "the driver can tell whether the
  target is mounted". A module that models the mount test as an
  UNRELIABLE SENSOR (an action that may answer `false` for a mounted
  volume) refutes the design in one step: `published ∧ sensor=false ⇒
  cleanup ⇒ tree removed`, against the invariant "a published lean
  volume's tree is never removed while its pod lives". That is the
  modelling lesson worth carrying: model the OBSERVATION, not only the
  state — a sensor that can lie is a state variable.

- *Three kind clusters on one 8-CPU Docker VM is past the machine.*
  Running the single-cluster and two-cluster drills at the same time
  saturated the VM; a cluster's kube-controller-manager then lost its
  leader lease on API timeouts and pods sat unscheduled. The drills run
  SEQUENTIALLY, and a failure that reads as an API timeout is the
  machine, not the driver — re-run before believing it.
- *STS codes.* `InvalidIdentityToken` is a 400 (as AWS answers it), not
  a 403, so a client can tell "not a token" from "no entitlement".

## 1. The question, restated precisely

The ask, verbatim: *"The flint passthrough and lean operators require
webhooks to inject sidecars into pods that require the s3 bucket/prefix to
be mounted as a filesystem. This has security implications as this
process can be spoofed easily and some sidecars require to be privileged.
Istio deliberately moved sidecar network setup out of a privileged per-pod
init container into a CSI plugin specifically to stop requiring
NET_ADMIN/NET_RAW on every injected pod. Can we research if the webhook
mechanism can be replaced with a CSI node Daemonset? ensure that it can
support each pod's need to have a different mount. i.e., if each pod is
mapped to a different project, then the mount is different as a project
maps to a different s3 bucket/prefix. The one caveat is that the projects
where this mechanism needs to work, the security is managed using JWT."*
Follow-up: *"The AuthN is powered using Apache Knox."*

Three corrections to the framing, each from the recon, none of which
changes the answer:

1. **Istio moved to a CNI plugin, not a CSI plugin.** The component is
   the Istio CNI node agent (`istio-cni`), a chained CNI plugin installed
   by a DaemonSet and invoked by the container runtime during pod-sandbox
   creation; Istio ships no CSI driver for traffic setup
   ([web-istio-precedent §1.3], VERIFIED). The principle transfers; the
   mechanism does not. For a *filesystem* the CSI node plugin is the
   right analogue, and it is the one Istio itself endorses for SPIRE
   (`csi.spiffe.io`: ephemeral inline, `podInfoOnMount: true`, a
   privileged DaemonSet with Bidirectional propagation on the kubelet
   pods dir, [web-istio-precedent §7]).
2. **Istio kept its mutating webhook.** The CNI removed the *per-pod
   privilege*, not the *injection*: in sidecar mode the webhook still
   injects `istio-proxy` (a native sidecar since 1.27) and, with CNI on,
   an unprivileged `istio-validation` init container; only ambient mode
   eliminates injection, by moving the proxy itself to the node
   ([web-istio-precedent §2], VERIFIED). The transferable principle is
   **privilege concentration, not webhook elimination**. This design
   does eliminate the webhook — because for a *mount* the privileged
   part *is* the whole injected part, and for lean the process can be
   hosted outside the tenant pod — but it says so as a consequence, not
   as the Istio precedent.
3. **The node agent is not unprivileged.** Istio's `install-cni` runs as
   uid 0 with `NET_ADMIN, NET_RAW, SYS_PTRACE, SYS_ADMIN, DAC_OVERRIDE`,
   AppArmor `Unconfined`, five hostPaths, `system-node-critical`; its
   README says "the Istio CNI Node Agent requires privileged node
   permissions" ([web-istio-precedent §3.1], VERIFIED). What Istio bought
   was N privileged init containers → 1 privileged pod per node, and app
   namespaces out of PSA `privileged`. That is exactly what this design
   buys, no more (§3.6 says it plainly).

Restated as a spec, the question is: **can a privileged node DaemonSet,
speaking CSI, hand each tenant pod a bucket/prefix mount chosen by that
pod's project, without injecting anything into the pod, without any
privilege or credential in the pod, with the pod's identity established
by something the pod author cannot forge, and with Knox as the authority
that says which project that identity belongs to?** The rest of this
document answers each clause.

Istio's own record settles one design boundary. Its community proposed a
CSI-driver redirection in 2020 and maintainers closed it because a design
where the privileged agent configures the pod *after* containers may be
running "would not guarantee that network redirection is setup when
application container start" ([web-istio-precedent §6], istio/istio#21981,
VERIFIED). Read-across: a mount established inside `NodePublishVolume`
is pre-start (kubelet starts no container — init containers included —
until every volume is published, `kubelet.go:2232-2245`,
[web-k8s-csi-mechanics §4.3]) and escapes that objection; anything lean
needs *after* start (barrier publish, lease renewal, final drain,
`.flint/publish` verbs) is in the category Istio kept a sidecar for, and
is why §3 keeps a pod-lifetime process — outside the tenant pod.

---

## 2. Today's mechanism and its threat model

### 2.1 flint-passthrough (privileged FUSE sidecar)

A pod carries the label `flint.io/passthrough-mount: <cr-name>`
(`spdk-csi-driver/src/passthrough/inject.rs:52`). A cluster-wide
`MutatingWebhookConfiguration` with `failurePolicy: Fail` and an
`objectSelector` on that label sends every pod CREATE in any namespace to
`flint-passthrough-operator` (`passthrough/webhook.rs:150-160`). The
handler fetches `FlintPassthroughMount <cr-name>` from the pod's own
namespace (`webhook.rs:81,183-193`), denies if absent or invalid
(`webhook.rs:83-103`), and replaces `/spec` with a copy in which a
**privileged native sidecar** (`initContainers[0]`, `restartPolicy:
Always`, `privileged: true`, `runAsUser: 0`, `runAsNonRoot: false`) runs
`mount-s3 <bucket> /flint-passthrough-vol/root …` into an `emptyDir`
with `mountPropagation: Bidirectional`, and every other container gets
that emptyDir's `root` sub-path at `spec.mountPath` with `HostToContainer`
(`inject.rs:355-498`, `:431-441`). Credentials reach `mount-s3` as
`envFrom` of a Secret in the tenant namespace whose keys are `AWS_*`
verbatim (`inject.rs:397-405`; `crds/flintpassthroughmounts.yaml:118-126`),
or the ambient AWS chain if none is named (`inject.rs:913-924`). The CR
author picks `spec.image` — the image the privileged container executes
(`passthrough/spec.rs:72-74`). The chart says the consequence out loud:
"Anyone who can create a labelled pod AND a FlintPassthroughMount in such
a namespace can obtain a privileged container. Gate the CR with RBAC"
(`flint-passthrough-chart/values.yaml:32-34`). In a namespace enforcing
PodSecurity `baseline` or `restricted` the mutated pod is rejected — the
chart calls this correct, and the documented way out is "a CSI driver
holding the mount on the node (what gcsfuse and mountpoint's own CSI
driver do)" (`inject.rs:23-26`; `values.yaml:18-33`; e2e leg A11
`passthrough/e2e/run-passthrough.sh:359-368`).

### 2.2 flint-lean (unprivileged, root, credential-holding sidecar)

Same admission shape (`objectSelector: flint.io/lean-workspace Exists`,
`failurePolicy: Fail`, CR in the pod's namespace, `Refused` phase denies —
`spdk-csi-driver/src/lean_operator/webhook.rs:64-115,166-192`). The
injector adds an `emptyDir` `flint-workspace` with the CR's `sizeLimit`
(`lean_operator/inject.rs:80-90`), mounts it at `spec.mountPath` into
every app container — and only app containers, `:113-128`; pre-existing
init containers get neither the tree nor the gate — and **appends** a
native sidecar `flint-sync run` to `initContainers` (`:229`; after any
init container the pod already has, which therefore runs ungated and
without the workspace) with a `startupProbe` `test -f <mountPath>/.flint-sync/checkout-complete`
whose `failureThreshold` is derived from the CR's inventory
(`:42-56,130-136,200-229`), stamps ~22 `FLINT_SYNC_*` env vars plus
`envFrom` the CR's `credentialsSecretRef` onto the sidecar only
(`:138-198`), and raises `terminationGracePeriodSeconds` to a derived
drain budget (`:241-252`; `lean_operator/boundary.rs:65-81`). The sidecar
sets **no `securityContext`** (`inject.rs:200-228`), the image sets no
`USER` (`docker/Dockerfile.sync.prebuilt:47-50`; its comment that "the
chart sets the security context" is not true today — `code-lean §0`), and
it holds the bucket credential in its environment while sharing an
app-writable tree that the code treats as attacker-reachable
(`lean/sidecar/src/safefs.rs:1-26`; `barrier.rs:1070-1081`). The plan of
record says "Pods hold zero bucket credentials" and the proxy holds the
real ones (`docs/plans/flint-lean-plan.md:66-73`); `credentialsSecretRef`
is explicitly interim (`docs/flint-lean-for-agent-fleets.md:115-117`).

### 2.3 What "spoofed easily" means, enumerated

| # | Surface | Today | Citation |
|---|---|---|---|
| T1 | **Label opt-in by any pod author in the namespace.** The label *value* names any CR in the namespace; no ServiceAccount, uid or identity is consulted — namespace + label is the whole authorization | any pod creatable in namespace N inherits every CR's bucket, prefix and Secret in N | `passthrough/webhook.rs:63-103`; `lean_operator/webhook.rs:64-115`; `code-auth §1.6` |
| T2 | **The credential is reachable from the tenant pod.** Static `AWS_*` keys in a tenant-namespace Secret, `envFrom` on the sidecar; readable by anyone with `secrets get`, by `exec` into the sidecar, or via a shared PID namespace | the tenant already holds the bucket credential; a compromised app plus pod-exec reads it | `passthrough/inject.rs:397-405`; `lean_operator/inject.rs:193-198` |
| T3 | **A privileged sidecar running a tenant-chosen image.** `privileged: true`, `runAsUser: 0`, Bidirectional mount into the node's mount table; `spec.image` has no allow-list | a container escape for any CR author — root, every capability, the kubelet tree mounted Bidirectional; not a mount, a node compromise | `passthrough/inject.rs:431-441`; `spec.rs:72-74`; `values.yaml:32-34` |
| T4 | **Cluster-scoped webhook targets.** MWC server-side-applied with `force()`; self-signed CA in a Secret the operator can only `get,create`; the listener does not authenticate the API server | anyone who can patch the MWC redirects admission of every labelled pod; anyone who can write the cert Secret wedges startup (get/create only, no update/delete) | `passthrough/webhook.rs:142-177,205-225`; `webhook_certs.rs:84-110`; `flint-passthrough-chart/templates/rbac.yaml:58-60` |
| T5 | **`failurePolicy: Fail` on the pod-create critical path.** A 2-replica Deployment; `Ignore` would be worse (pods start against an empty dir) | operator down ⇒ every labelled pod in every namespace refused | `passthrough/webhook.rs:142-177`; `values.yaml:77-81`; `lean_operator/webhook.rs:7-11` |
| T6 | **PSA regression.** Tenant namespaces must be `privileged` for passthrough, which admits *every* privileged pod there | the mutated pod is inadmissible under `baseline`/`restricted` | `NOTES.txt:35-42`; leg A11 |
| T7 | **FUSE death strands consumers on `ENOTCONN`**, unrecoverable in place; today the sidecar's readiness flips so the pod leaves Service endpoints | measured on kind; proven by leg A12 | `passthrough/inject.rs:173-202,466-476`; `run-passthrough.sh:371-442` |
| T8 | **Root lean sidecar, no limits, credential in env, app-writable tree** | a symlink plant turns the writer into an arbitrary-file-write primitive inside the pod; contained by `safefs` code discipline, not by the kernel | `lean_operator/inject.rs:200-228`; `safefs.rs:1-26`; `lib.rs:206-208` |
| T9 | **Forgeable acks.** `.flint/*.ack` is writable by any in-pod process | advisory by design; the authoritative signal is a remote read | `lean/sidecar/src/control.rs:19-23` |

The repo already wrote down the principle this design implements: "The
webhook must not be the security boundary. The subtree prefix derives
from namespace + ServiceAccount, and IAM principal-tag conditions enforce
the same mapping independently — a forged pod annotation buys nothing,
and a webhook bug is caught by a second, unforgeable layer"
(`docs/flint-fuse-architecture.html:376`, designed-only). §3 and §4 are
that second layer, built.

### 2.4 What the CSI design closes (forward reference)

T3, T4, T6 **closed** (no privileged container, no tenant image, no
MWC, no cert Secret; tenant namespaces `restricted`). T5 **moved**: no
admission listener, but a new pod's first exchange needs the broker
(running mounts survive to `Expiration`, §6.3); size the broker as a
critical-path Deployment with a PDB and ≥ 2 replicas. T2 **re-scoped**:
no credential is *placed* in the tenant namespace, but any process under
an entitled SA can obtain the same 15-minute keys by projecting its own
`aud=s3.flint.io` token (`projected.sources[].serviceAccountToken.audience`
is pod-author-chosen, kubernetes.io configure-service-account) and
calling the broker — `TokenReview` passes (live pod), `consumers` passes
(same SA), no plugin involvement — so entitlement is SA-granular and the
mount is a convenience, not a confinement. `readOnly`, uid and
`default_permissions` are presentation, not policy; RO must be expressed
on the CR (the broker issues RO keys for an RO CR) or at the proxy. The
per-volume registration nonce (§4.2 step 1) closes the *broker* path of
that bypass; the K0 path (§4.3) and an un-gated K1 topology remain
SA-granular. Equal to today in the interim static arm. T1 **narrowed to the
intended trust** (authorization from kubelet-asserted namespace + SA
against a default-deny consumer list, re-checked by the broker from a
pod-bound token) — Kubernetes cannot stop a namespace's pod authors from
using that namespace's SAs, so **the namespace is the project boundary**
**[graft: security]**. T7 **reshaped**: node-plugin restarts no longer
kill mounts; a worker's own death still does (FUSE semantics). T8
**improved** (non-root syncer in a container whose mount namespace holds
one tree). T9 unchanged by design. New surfaces are named in §3.6.

---

## 3. The CSI shape

### 3.1 Components

```
 tenant namespace (PSA restricted)              flint-workers (PSA privileged label; VAP-pinned, see §3.6)
 ┌──────────────────────────────────┐           ┌──────────────────────────────────────────────┐
 │ pod team-a-agent  (SA: trainer)  │           │ worker pod  s3w-<volume_id[..12]>  (1 per vol)│
 │  app (uid 1001)                  │           │  runAsNonRoot, drop ALL, seccomp RuntimeDefault│
 │   /mnt/s3 ◄── bind ──┐           │           │  nodeName pinned; tolerates all taints         │
 │  no sidecar          │           │           │                                                │
 │  no credential       │           │           │  passthrough: flint-s3-worker (PID 1)          │
 │  no privilege        │           │           │    recv fd ◄─ SCM_RIGHTS ─┐                    │
 │  no label            │           │           │    exec mount-s3 <b> /dev/fd/3 --foreground …  │
 └──────────────────────┼───────────┘           │  lean:        flint-s3-worker (PID 1)          │
 /var/lib/kubelet/pods/<uid>/volumes/           │    spawn flint-sync run   (unchanged binary)   │
   kubernetes.io~csi/<vol>/mount                │    FLINT_SYNC_ROOT=/workspace ◄─ hostPath ──┐  │
                        ▲                       │  emptyDir comm/ {mount.sock, token (0600)}  │  │
                        │ bind                  └────────────▲───────────────▲───────────────┼──┘
 ┌──────────────────────┴────────────────────────────────────┼───────────────┼───────────────┼───┐
 │ flint-s3-csi-node (DaemonSet; privileged; /var/lib/kubelet │ Bidirectional)│               │   │
 │  NodePublish: resolve CR ─ authorize (ns,SA) ─ create worker ─ (FUSE: open /dev/fuse,       │   │
 │    mount(2) src, send fd) ─ write comm/token ─ wait init/marker ─ bind src→target ─ OK      │   │
 │  Republish (~60-90 s): rewrite comm/token if changed ─ liveness probe ─ OK, never remount   │   │
 │  NodeUnpublish: (lean: delete worker w/ derived grace ⇒ drain) ─ umount ─ delete ─ rm state │   │
 │  state: /var/lib/kubelet/plugins/s3.flint.io/volumes/<vol>/{state.json, src/ | tree.img, tree/}┘   │
 └──────────────────────────────────┬────────────────────────────────────────────────────────────┘
                                    │ pod-bound SA token (aud=s3.flint.io), from kubelet
                                    ▼                                 ┌──────────────────────┐
 ┌────────────────────────────────────────────────────┐               │ Apache Knox          │
 │ flint-s3-broker  (Deployment, restricted, 1 place) │ ─ K1/K2/K3 ─▶ │ (project AuthN)      │
 │  STS façade: AssumeRoleWithWebIdentity(SA JWT)     │               └──────────────────────┘
 │  TokenReview → (ns,SA,pod-uid) → CR consumers →    │ ────────────▶ customer's project-scoped
 │  Knox step → proxy keys → {AKID,SK,Token,Expiration}│               S3 proxy (holds real keys)
 └────────────────────────────────────────────────────┘
```

| Component | Lives in | Runs as | Job |
|---|---|---|---|
| `CSIDriver s3.flint.io` | cluster | — | `attachRequired: false`, `podInfoOnMount: true`, `volumeLifecycleModes: [Ephemeral]`, `fsGroupPolicy: None`, `requiresRepublish: true`, `tokenRequests: [{audience: s3.flint.io, expirationSeconds: 3600}]`; `serviceAccountTokenInSecrets: true` once the cluster is ≥ 1.35 ([web-k8s-csi-mechanics §3.1]) |
| `flint-s3-csi-node` DaemonSet | `flint-system` | `privileged: true`; **not** `hostNetwork`, **not** `hostPID` | the CSI node server: `NodePublish`/`NodeUnpublish`/`NodeGetVolumeStats`; performs `mount(2)`; creates and reaps worker pods on its own node |
| worker pod (one per published volume) | `flint-workers` | non-root, `drop: [ALL]`, seccomp `RuntimeDefault`, `automountServiceAccountToken: false` **[graft: security]** | passthrough: receives the FUSE fd and execs the pinned `mount-s3`; lean: runs the unchanged `flint-sync run` over a plugin-owned tree |
| `flint-s3-broker` Deployment | `flint-system` | `restricted`; ClusterRole `tokenreviews: create`, the two CRDs `get,list,watch`; Secrets `get` in `flint-system` only | the STS-shaped identity exchange (§4); the only component with a standing credential |
| `flint-lean-operator` Deployment | `flint-system` | unchanged | reconcile, claim/adopt, refusals, conditions — it stays; its `/mutate` listener and MWC go |
| `flint-passthrough-operator` | — | — | retired at end state (passthrough has no controller, `passthrough/spec.rs:1-10`); optionally re-purposed as the attachment controller (§7, alternative "controller creates workers") |

Why a **new** CSIDriver and a **new, minimal** DaemonSet rather than
extending `flint.csi.storage.io`: `attachRequired`, `fsGroupPolicy`,
`tokenRequests`, `requiresRepublish` are properties of one CSIDriver
object and the block/pNFS driver needs the opposite values
(`flint-csi-driver-chart/templates/csidriver.yaml:8-19`); the driver
name is hardcoded in the binary, the registrar path, the plugin dir and
the marker dir (`spdk-csi-driver/src/main.rs:614,5073,5340`;
`node.yaml:656,678`); the existing DS is `hostNetwork`,
`system-node-critical`, `OnDelete` under a maintenance roller and hosts a
~3 GB spdk-tgt whose restart semantics are governed by ublk/NVMe
recovery (`node.yaml:8-62,166+`); and the whole template is suppressed
under the lite profile, which is where lean/passthrough customers live
(`csidriver.yaml:1`; `node.yaml:1`; neither front-end chart depends on
the CSI chart, `code-passthrough §7.11`). The `nfs-only` node mode is the
DaemonSet template (`node.yaml:75-163`); the tonic `csi` module, the
Unix-socket serving loop, `mount_util`, `mount_opts` and
`node_volume_locks` are reused verbatim (`code-csi §6.1`) **[graft:
minimal — the reuse inventory is §9.4]**. The libflint design reached the
same "second driver, adopt `tokenRequests` + `requiresRepublish` now"
conclusion (`docs/plans/libflint-and-snapshotter-design.md:249-258`).
Note for the brief: the shipped template renders `Ephemeral` by default
(`csidriver.yaml:16-18`, `values.yaml:70`), so "Persistent only" was
stale — moot, because this design creates its own CSIDriver object.

### 3.2 What the tenant writes

Today, one label. Under this design, a volume plus a `volumeMount` in
each container that needs it:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: team-a-agent
  namespace: team-a
spec:
  serviceAccountName: trainer                  # the identity that is authorized (§3.4, §4)
  securityContext: { runAsNonRoot: true, runAsUser: 1001 }
  volumes:
    - name: data
      csi:
        driver: s3.flint.io
        readOnly: true                         # presentation only; the CR/proxy decide RW (§2.4 T2)
        volumeAttributes:
          flint.io/mount: datasets             # FlintPassthroughMount <datasets> in THIS namespace
          # flint.io/workspace: team-a         # or a FlintLeanWorkspace (lean arm)
          # flint.io/uid: "1001"               # optional presentation override (§3.5)
        # nodePublishSecretRef: {name: minio-creds}   # interim static-credential arm ONLY (§4.5)
  containers:
    - name: app
      image: …
      volumeMounts:
        - { name: data, mountPath: /mnt/s3, readOnly: true }
```

`volumeAttributes` are pod-author-controlled and are treated as a
**request, never an authorization** (KEP-596: "CSI driver vendors that
support inline volumes will be responsible for secure handling of
volumes"; kubernetes.io: parameters "normally defined in the StorageClass
should not be exposed to users through the use of inline ephemeral
volumes" — [web-k8s-csi-mechanics §1.4]). Exactly one of
`flint.io/mount` / `flint.io/workspace` is accepted; `flint.io/uid` and
`flint.io/gid` are accepted as narrowing presentation overrides; every
other key is refused with a message naming it. **`bucket`, `keyPrefix`,
`endpoint`, `region`, `image`, credentials are never accepted from the
pod** — the CR is the policy object, as today.

What is lost, honestly: the one-line opt-in (1 line → ~8 per container);
"every container mounts the workspace" is no longer enforced at admission
(a container that omits the mount does not see it — no correctness hazard,
the volume exists before any container starts); CR `mountPath` becomes a
documented default (the pod's `volumeMounts.mountPath` decides; the
`.flint/` protocol is path-relative, `lib.rs:281-287`; `uds.rs:175-177` —
only `flint-sync status` reports the syncer's own absolute root,
`gauges.rs:233`, which under CSI is `/workspace`); the admission-
time refusal message becomes a `FailedMount` event on a pod sitting in
`ContainerCreating`, retried forever with backoff capped at 2m2s
([web-k8s-csi-mechanics §4.2]) — same words, worse ergonomics for a typo;
the sidecar `NotReady` flip on mounter death (§6.1); tenants cannot
`kubectl logs` the mounter by default (the plugin mirrors the worker's
last stderr lines into an Event on the tenant pod, the
`FallbackToLogsOnError` equivalent of `passthrough/inject.rs:492-495`);
the in-pod `flint-sync status|ctl|recover-staged` exec surface
(`bin/flint_sync.rs:140-190`; `run-boundary.sh:214-216` execs `-c
flint-sync`; `run-verbs.sh:147-161`; the operator's own condition text
says "Run `flint-sync recover-staged` in a pod on this workspace",
`reconcile.rs:421-426`) — under CSI the binary exists only in the worker
pod in `flint-workers`, which tenants cannot exec into: ship `status` and
`recover-staged` as an operator-side recipe (or a plugin verb keyed by
tenant pod), reword the `StagedWorkRecovered` message, and document
`curl --unix-socket <mountPath>/.flint-sync/ctl.sock` as the tenant-side
replacement for `ctl`; the pod-`runAsUser`-derived default owner
(`mount_owner` = `spec.uid` → `securityContext.runAsUser` → none,
`inject.rs:238-244`; `NOTES.txt:28-33` documents it as the user
contract): `NodePublish` never sees `securityContext`, so a pod that
today mounts as its own uid must say so in `flint.io/uid` or the CR, else
files present as 65534 (today, with nothing declared: root).
What improves: kubelet starts **no** container, init containers included,
before `NodePublishVolume` returns (`kubelet.go:2232-2245`) — the
"empty directory" race the native sidecar exists to prevent
(`passthrough/inject.rs:5-13`) is structurally impossible; a pod may mount
**several** CRs (today exactly one, `passthrough/e2e/mounts.yaml:1-3`);
the pod carries no credential, no privileged container, no derived grace
arithmetic; `restricted` PSA works.

### 3.3 What the CRs gain and lose (both CRDs; parity tests updated)

```yaml
apiVersion: flint.io/v1alpha1
kind: FlintPassthroughMount            # FlintLeanWorkspace gains the same block
metadata: { name: datasets, namespace: team-a }
spec:
  bucket: proj-a
  keyPrefix: datasets/imagenet
  endpoint: https://s3-proxy.corp
  readOnly: true
  uid: 1001                            # lean gains uid/gid (passthrough already has them, spec.rs:64-67)
  gid: 1001
  consumers:                           # NEW — authorization list; ABSENT = DENY in csi mode   [graft: security]
    serviceAccounts: [trainer, notebook]   #   ["*"] is the explicit today-equivalent, used only during migration
  identity:                            # NEW — how the broker turns pod identity into keys (§4)
    mode: knox                         #   knox | irsa | static (interim)
    # identity.projectPrincipal is NOT accepted from the namespaced CR: the
    # (namespace, SA) → project-principal binding is admin-owned (§4.2 step 2)
  # credentialsSecretRef: …            # DEPRECATED in csi mode: the pod's nodePublishSecretRef replaces it (§4.5)
  # image: …                           # REMOVED — tombstoned with a CEL rule: "false", as `driver` was
  # mountPath: /mnt/s3                 # advisory default only
```

`consumers` defaults to **deny**, not "any SA in the namespace"; the
migration opts a namespace into today's posture with an explicit `["*"]`
rather than inheriting it silently **[graft: security]**. `spec.image` is
removed — it is the privileged-escape knob (T3) — and tombstoned because
pruning a removed CRD field silently stores a CR nobody chose
(`crds/flintpassthroughmounts.yaml:85-93`). The passthrough CRD/struct parity tests (`spec.rs:188-241`;
`release.sh:459-470`, which runs `cargo test --lib passthrough::spec::`
only) cover `FlintPassthroughMount` — its CRD is hand-written because the
spec is plain serde. `FlintLeanWorkspace` is schemars-derived
(`lean_operator/crd.rs:16-37`, rendered by `bin/crdgen.rs:18`, shipped at
`flint-lean-chart/crds/flintleanworkspaces.yaml`), so `consumers`/
`identity`/`uid`/`gid` land as struct fields and its `image` tombstone is
a `#[x_kube(validation = …)]` attribute on the derive, not a hand-written
CEL block; no parity test applies to it. No `status.observedPublishes`: it would need CR status write
RBAC on the node SA; use metrics.

### 3.4 `NodePublishVolume` — passthrough

Inputs the plugin trusts are those written by kubelet **over** anything
the pod author put in `volumeAttributes` (`mergeMap(volAttribs,
getPodInfoAttrs(...))`, `csi_util.go:208-217`, [web-k8s-csi-mechanics
§2]): `csi.storage.k8s.io/pod.{name,namespace,uid,serviceAccount.name}`,
`csi.storage.k8s.io/ephemeral`, and
`csi.storage.k8s.io/serviceAccount.tokens` = `{"s3.flint.io": {"token":
"<jwt>", "expirationTimestamp": "…"}}` (`csi_mounter.go:363-420`), or the
same key in `secrets` once `serviceAccountTokenInSecrets` is on. The
volume id is `csi-<sha256(podUID + volumeName)>` (`csi_mounter.go:611-615`)
— unique per pod incarnation, unguessable, and the key for every
plugin-owned path.

```
NodePublishVolume(req):
  1  parse: selector (exactly one of flint.io/mount|workspace), narrowing attrs, readonly,
     kubelet keys, SA token; reject unknown attributes naming them; require ephemeral == "true"
  2  idempotency: if `timeout 5 mountpoint -q target` (main.rs:4671-4682 shape) ⇒ REPUBLISH path (§3.7)
  3  lock on volume_id (node_volume_locks, 90 s budget < kubelet's 2 min csiTimeout)
  4  resolve FlintPassthroughMount <selector> in pod.namespace from the informer cache
       missing ⇒ NotFound (final) naming CR + namespace;  MountSpec::validate (spec.rs:39-165) reused
  5  authorize: pod.serviceAccount.name ∈ spec.consumers.serviceAccounts  else PermissionDenied naming SA + field
  6  create-or-adopt worker pod (label flint.io/volume-id=<vid>, nodeName=<this node>, image chart-pinned,
       runAsUser=effective uid, drop ALL, seccomp RuntimeDefault, emptyDir comm (10 MiB, Memory) + cache,
       resources from chart sidecarResources (values.yaml:94-107), env per §4.4);
       not Running within 60 s ⇒ Unavailable (non-final; kubelet retries, 500 ms → 2m2s backoff);
       phase Failed (kubelet admission rejected it — OutOfcpu, OutOfmemory, VAP/PSA; a pod created with
       spec.nodeName skips the scheduler, so admission is the first gate) is final for THAT worker:
       delete it, FailedPrecondition carrying status.reason/message; only a worker still Pending at 60 s
       returns Unavailable
  7  write comm/token (0600, atomic rename) from the SA token — host-side, via /var/lib/kubelet
       (the AWS v2 provider_pod.go pattern, [web-fuse-csi-priorart §2])
  8  fd = open("/dev/fuse", O_RDWR)
     mount("mount-s3", <plugin>/volumes/<vid>/src, "fuse", MS_NODEV|MS_NOSUID|MS_NOATIME[|MS_RDONLY],
           "fd=<fd>,rootmode=040000,user_id=<uid>,group_id=<gid>,default_permissions,allow_other")
       — the AWS v2 / GCS option set verbatim ([web-fuse-csi-priorart §2 mount_linux.go, §3 csi_mounter.go]);
         root did the mount(2), so allow_other needs no user_allow_other ([web-k8s-csi-mechanics §6.3])
  9  connect /var/lib/kubelet/pods/<worker-uid>/volumes/kubernetes.io~empty-dir/comm/mount.sock;
     sendmsg({bucket, argv = mounter_args(spec, owner, target = "/dev/fd/3"), env}, SCM_RIGHTS fd);
       — a one-parameter change to inject.rs:251-254, which hardcodes fuse_target(spec) =
         /flint-passthrough-vol/root as argv[1]; it also always pushes --foreground and --allow-other
         (:255-259) — whether mount-s3's fd mode accepts --allow-other when the driver already set
         allow_other in the mount(2) data is §9.2 item 19; drop it if not
     close(fd)   — the worker's PID 1 execs `mount-s3 <bucket> /dev/fd/3 --foreground …` with the fd inherited
                   ([web-fuse-csi-priorart §7 fd mode]: no capability, no /dev/fuse, no fusermount needed)
 10  wait ≤ 60 s for FUSE INIT: statfs(src) under a deadline AND a bounded readdir of src, AND no mount.error
       timeout ⇒ umount src, Unavailable("mounter did not take over the FUSE fd")
 11  mount --bind [-o ro] src target;  write state.json {phase, worker, token_exp, verdict};  OK
```

Step 10 preserves the design's first principle — a publish that
"succeeds" with an empty directory is impossible — which the sidecar's
`fstype fuse in /proc/mounts AND test -d` probe exists for
(`passthrough/inject.rs:95-111,445-461`). Bind-from-source rather than
mounting FUSE at `target_path` keeps the lesson that a dead FUSE mount at
a volume root wedges every later container creation and pod deletion
(`inject.rs:57-85`); the source lives under the plugin dir, unreachable
from any tenant pod.

```
NodeUnpublishVolume(volume_id, target):            # only these two fields arrive (csi.proto:1513,1523)
  1  classify by state.json (the ephemeral-marker pattern, main.rs:5069-5079,5321-5324); none ⇒ OK (idempotent)
  2  teardown ladder on target: timeout test -e → timeout mountpoint -q → umount -l → timeout umount -f -l → verify
       (main.rs:5104-5290, verbatim)
  3  umount -l src   — aborts the FUSE connection; the worker's mount-s3 exits on its mount.exit/EOF
  4  delete worker pod (direct delete; does not consult the PDB);  rm -rf <plugin>/volumes/<vid>;  OK
```

### 3.5 `NodePublishVolume` — lean

Steps 1-5 as above with `FlintLeanWorkspace`, plus: refuse if
`status.phase == Refused` (today admission denies,
`lean_operator/webhook.rs:101-109`). Then:

```
  6  tree with a hard quota:  truncate -s <sizeLimitGib>G tree.img; mkfs.ext4 -q tree.img;
       mount -o loop,nodev,nosuid tree.img <plugin>/volumes/<vid>/tree; chown <uid>:<gid> tree;
       chmod 1777 tree (or fsGroup semantics via VOLUME_MOUNT_GROUP) so an app uid the CR does not
       name can still write; the CR uid is REQUIRED in csi mode for lean (no 65534 default) and the
       syncer runs as it — an app running as a different uid is a documented refusal, not a silent
       skip (today the syncer is root over a 0777 emptyDir, so the app's uid is irrelevant:
       inject.rs:200-228 sets no securityContext, Dockerfile.sync.prebuilt:47-48; §9.2 item 18)
       (the existing driver already runs mkfs and mounts in-container, main.rs:4883-4937)
       — emptyDir.sizeLimit (crd.rs:87-89) becomes a real filesystem: df is legible, the app gets ENOSPC,
         the node disk is never exhausted, and no tenant is evicted
  7  worker (syncer) pod: image chart-pinned flint-s3-worker-lean (flint-sync + the flint-s3-worker
       PID 1 that forwards SIGTERM and serves the loopback creds door, §9.4), args [run], the FIXED
       FLINT_SYNC_* list
       (inject.rs:138-192) stamped by the plugin from the CR (the every-knob-is-stamped contract test,
       inject.rs:352-408, re-homes to the plugin's render fn), FLINT_SYNC_ROOT=/workspace,
       credentials env per §4.4,
       hostPath {path: <plugin>/volumes/<vid>/tree, type: Directory} at /workspace       [graft: minimal]
       terminationGracePeriodSeconds = derived_grace_secs(spec) (boundary.rs:65-81) — the new home of
       the grace arithmetic;  wait for Running (Unavailable on timeout; phase Failed ⇒
       FailedPrecondition, as §3.4 step 6)
  8  block ≤ 100 s for <tree>/.flint-sync/checkout-complete (marker written LAST, checkout.rs:350-365)
       absent ⇒ Unavailable("checkout in progress") — no progress figures: checkout writes gauges.json
         only after materialization, right before the marker (checkout.rs:363,365), and Gauges has no
         files-done/bytes-done fields (gauges.rs:75-110); a progress file would be a second lean change;
         the syncer keeps running between calls; checkout is idempotent and resumable (checkout.rs:1-14,173-222)
       syncer exited non-zero (over-budget refusal checkout.rs:84-98, Fenced, Refused) ⇒ FailedPrecondition
         carrying its termination message (terminationMessagePolicy FallbackToLogsOnError)
  9  mount --bind tree target;  state.json;  OK
```

Why hostPath into the syncer and not an emptyDir bound outward: the
tree's lifetime must equal the **volume's** (= the tenant pod's), not the
worker pod's — a worker deleted and recreated must find the same tree and
self-recognise its lease (§5.3); a plugin-owned directory delivered by
`hostPath type: Directory` has exactly that lifetime with verified
semantics, whereas binding into a running worker's emptyDir after
container start rests on propagation behaviour that is an experiment
(§9, E3) **[graft: minimal, replacing the spine's E3-dependent step]**.
The cost: `hostPath` is baseline-forbidden, so `flint-workers` carries
the PSA `privileged` label and a `ValidatingAdmissionPolicy` is the real
guard (§3.6). If E3 passes, the namespace can drop to `restricted` by
switching the delivery — a contained change.

```
NodeUnpublishVolume (lean):
  1  classify by state.json; none ⇒ OK
  2  delete the syncer with gracePeriodSeconds = derived_grace_secs(spec)
       ⇒ kubelet SIGTERMs flint-sync ⇒ the SAME drain arm runs unchanged (flint_sync.rs:583-604;
         sentinel.rs:1180-1236): settle owed acks, cite everything, release the lease
       — the tree is quiescent by construction: kubelet drops the pod's volumes only after
         ShouldPodRuntimeBeRemoved, i.e. all tenant containers have exited
         (desired_state_of_world_populator.go:227-234, [web-k8s-csi-mechanics §5])
  3  wait for the syncer to be gone, ≤ 100 s per RPC, else Unavailable (kubelet retries; the Pod object
       stays Terminating until unmount succeeds, status_manager.go:1334-1351)
     hard ceiling derived_grace + 30 s ⇒ SIGKILL the syncer and proceed (orphans surface in the bucket as
       today's B11b; recover-staged applies, run-verbs.sh:1152-1208)
  4  syncer already gone (evicted, OOM, crashed) ⇒ run a one-shot `flint-sync drain` (NEW lean-crate
       subcommand: claim with self-recognition → drain() → release; pieces exist lease.rs:64-66,
       sentinel.rs:1180-1236) over the tree, then continue
  5  umount target;  umount tree;  rm tree.img;  rm state;  OK
```

### 3.6 Per-pod isolation on one node, where each process lives, what is privileged

| Boundary | Mechanism |
|---|---|
| Path | `target_path` is under the tenant pod's own kubelet dir; `src`/`tree` under a `0700` plugin dir keyed by volume id; no tenant pod can name either (no `hostPath` under `restricted`) |
| Process | one worker pod per volume: own uid, cgroup (memory limit per mounter — today's `sidecarResources`, kept because mount-s3 sizes prefetch against machine RAM, `values.yaml:98-107`), netns (a NetworkPolicy in `flint-workers` restricts egress to the broker and the proxy), seccomp, read-only rootfs |
| Credential | one pod-bound token file (0600, memory-backed emptyDir) and one STS session per worker; the broker never issues cross-project keys for one token |
| Cache | mount-s3 cache in the worker's own emptyDir; no host cache (the reason AWS moved it off the host, [web-fuse-csi-priorart §2]) |
| Kernel | FUSE `default_permissions` + `allow_other` with `user_id`/`group_id` = the tenant uid; `nodev,nosuid`; the syncer's mount namespace holds one tree and nothing else of value |
| Sharing | **none in v1.** AWS shares a Mountpoint Pod only when node, PV, volume id, options, auth source, fsGroup, namespace, SA and role ARN are all equal ([web-fuse-csi-priorart §2]); with per-pod credentials the equality class is one pod. Lean syncers are never shared (one writer per tree, the occupancy flock `state.rs:110-159`) |
| uid/gid | `NodePublish` does not see the pod's `securityContext` (`code-passthrough §4.3`). Effective uid = CR `spec.uid` → `volumeAttributes.flint.io/uid` → 65534 (passthrough; lean REQUIRES the CR uid, §3.5 step 6). This drops today's `runAsUser` derivation and changes the no-declaration default from root to 65534 (`inject.rs:238-244`; test `:730-738` asserts no `--uid` at all when nothing is declared) — listed in §3.2 "What is lost". Letting the pod author choose the *presentation* uid is safe: `--uid` decides what `stat` reports and who passes `default_permissions`; the credential decides what the bucket allows. `fsGroupPolicy: None` so kubelet never recursively chowns a bucket — with the default policy an inline volume (always RWO to kubelet) with `fsType` + `fsGroup` triggers a walk of the whole mount (`csi_mounter.go:474-500`, [web-k8s-csi-mechanics §9]) |

**Privilege accounting — said plainly.**

| Component | Privilege | Why it cannot be less | Must NOT have |
|---|---|---|---|
| `flint-s3-csi-node` (1 per node) | `privileged: true`; hostPath `/var/lib/kubelet` **Bidirectional**, `/var/lib/kubelet/plugins/s3.flint.io`, `/var/lib/kubelet/plugins_registry`; `/dev/fuse` via privileged device access; `priorityClassName` above tenant workloads | the API server refuses `Bidirectional` on non-privileged containers (`validation.go:1473-1476`); `mount(2)` needs `CAP_SYS_ADMIN`; FUSE needs `/dev/fuse`; AWS keeps `privileged: true` for exactly this ("Kubernetes API validator currently enforces that this is set to true for bidirectional mounts", [web-fuse-csi-priorart §2 node.yaml]); identical to the existing `nfs-only` mode (`node.yaml:152-157`) | `hostNetwork` (serves Unix sockets only), `hostPID` (it unmounts, it never signals a mounter), any Secrets RBAC, any cluster-wide pod verb |
| node SA RBAC | ClusterRole: the two CRDs `get,list,watch`; `events create,patch`; `nodes get`. **Role in `flint-workers` only**: `pods create,get,list,watch,delete` | it must read policy and place its own workers | Secrets anywhere (`docs/plans/file-api-fleet-auth.md:99-104`); pods outside `flint-workers` — ISTIO-SECURITY-2023-005 turned a node agent's cluster-wide pod DELETE into a node→cluster escalation ([web-istio-precedent §5.5]) |
| worker pod (1 per volume) | none: `runAsNonRoot`, `drop: [ALL]`, `seccompProfile: RuntimeDefault`, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, `automountServiceAccountToken: false`; passthrough: no hostPath, no `/dev/fuse`; lean: one hostPath to exactly `<plugin>/volumes/<vid>/tree` | mount-s3 in fd mode needs no capability ([web-fuse-csi-priorart §7]); `flint-sync` needs a directory | `hostNetwork`, `hostPID`, RBAC, any second hostPath |
| `flint-workers` namespace | PSA `privileged` label (hostPath is baseline-forbidden) **plus** a `ValidatingAdmissionPolicy`: creator must be the node SA AND, on CREATE, `object.spec.nodeName == request.userInfo.extra["authentication.kubernetes.io/node-name"][0]`; on DELETE, `oldObject.spec.nodeName == request.userInfo.extra["authentication.kubernetes.io/node-name"][0]` (pod-bound token metadata, stable K8s 1.32, beta 1.30 — the DS pod's default projected token is pod-bound; `oldObject` is non-null on DELETE and DELETE is a valid `OperationType`, admissionregistration/v1) — a node may create and delete workers only on itself; image ∈ chart-pinned set; `securityContext` as above; hostPath only under `/var/lib/kubelet/plugins/s3.flint.io/volumes/`; no `privileged`, no `hostNetwork`, no `hostPID`; `spec.nodeName` == label `flint.io/node`; `PodDisruptionBudget minAvailable: <integer larger than any plausible worker count>` — bare pods may not use `maxUnavailable` or percentages (kubernetes.io configure-pdb §Arbitrary workloads; `maxUnavailable` forces the controller's scale lookup, `disruption.go:822-823`, which on a Node `ownerReference` fails into `DisruptionAllowed=False reason SyncFailed`, `:980-992`, and on no controllerRef emits `Warning UnmanagedPods` — an eviction block through an error path only; the integer-`minAvailable` branch is clean, `:841-843,1005-1008`); and the Node `ownerReference` carries `controller: true` so `kubectl drain` treats workers as managed and waits on the eviction API rather than refusing without `--force` (`kubectl/pkg/drain/filters.go:236-249`) | as Istio: only the agent's namespaces are `privileged`; app namespaces `baseline`/`restricted` ([web-istio-precedent §3.3]) | tenant RBAC of any kind |
| `flint-s3-broker` | `restricted`; ClusterRole `tokenreviews: create`, CRDs read; Role `secrets get` in `flint-system` (its Knox identity under K2, its pinned client certificate under K1/endgame — §4.3; interim static keys) | it authenticates pod tokens and reads the policy object | tenant-namespace Secret reads; pod write RBAC; a bucket credential of its own |
| tenant pod | nothing — `restricted`-admissible | — | everything |

Istio's CNI move — and every production FUSE CSI driver — did not
eliminate privilege; it moved it from N tenant pods to one node agent per
node ("replacing that model with a single privileged node agent pod on
each Kubernetes node", [web-istio-precedent §1.2]); Mountpoint's and
GCS's node plugins are `privileged: true` ([web-fuse-csi-priorart §2, §3]).
This design does the same. The gain is real and specific: tenant
namespaces go to `restricted`; the privileged code is one audited binary
the cluster operator ships, not a container whose image a tenant chooses;
the privileged process holds no long-lived credential and no S3 key at
all (§4.4); and a compromise of the FUSE daemon yields an fd and a
15-minute, one-project key instead of root with `CAP_SYS_ADMIN` and a
Bidirectional mount into the host.

New surfaces this design introduces, named: **input parsing in a
privileged process** (`volumeAttributes` are attacker-controlled input to
root code — two selector keys and two integer keys are accepted;
everything that becomes argv comes from the CR through
`MountSpec::validate`; mount strings are built from typed fields, the
"argv, never a shell string" discipline of `passthrough/inject.rs:30-35`);
**the node SA can create and delete pods in `flint-workers`** (one Role,
one SA shared by every node: without the VAP's node-name rule (table
above) one compromised node can delete every worker in the cluster and
strand every S3-mounted pod on `ENOTCONN` (§6.1: a dead mounter is a
dead mount) — the ISTIO-SECURITY-2023-005 shape scoped to one namespace
but covering the whole data plane; with the rule, the blast radius is
that node's own workers. Workers a node created elsewhere would hold no
token — kubelet mints pod-bound tokens only for real tenant pods — so they
run a `mount-s3` that fails; the attachment-controller variant removes
the surface entirely, §7);
**the broker** can mint for every project — one Deployment, one job, no
tenant Secret access, every issuance a TokenReview-verified, pod-bound,
15-minute grant with an audit line `(ns, sa, pod-uid, project)` — it is
the crown jewel and the design says so **[graft: security]**;
**tokens in `volume_context`** until `serviceAccountTokenInSecrets` — the
plugin must never log `volume_context` wholesale ([web-k8s-csi-mechanics
§3.1]).

### 3.7 Republish (every ~60-90 s per pod)

`requiresRepublish: true` re-invokes `NodePublishVolume` once per pod
sync — kubelet `syncFrequency` 1m with jitter factor 0.5 — **not** the
0.1 s KEP-1855 claims (`volume_manager.go:437-440`;
`desired_state_of_world_populator.go:324-336`; `pod_workers.go:327,1529-1531`;
`defaults.go:76-78`; [web-k8s-csi-mechanics §3.4]). On a republish
(target already mounted): (a) if the SA token in the request differs from
the one on disk, rewrite `comm/token` atomically — the CRT and the Rust
SDK re-read the token file on every credential fetch ([web-fuse-csi-priorart
§7]; `aws-config-1.10.1/src/web_identity_token.rs:76-78`, `code-auth §4`);
(b) run the liveness probe on `src`/`tree` (`probe_staging_liveness`,
`mount_util.rs:174-271` — st_dev + fsync under a deadline); (c) return
`OK` even if the probe fails — kubelet must never see a republish fail on
an already-published target, because before the fix for
kubernetes#121271 it removed the mount dir and left the pod with stale
contents (`csi_mounter.go:316-324`, [web-k8s-csi-mechanics §3.5]); record
the verdict in `state.json` and, on a failed probe, emit a `Warning`
Event on the tenant pod (kubelet will not surface volume health — §6.1). **Never unmount or
remount on republish.** Load at 500 volumes per node: ~8 calls/s, each a
stat and a possible 4 KiB write.

---

## 4. The identity chain, with Knox as the AuthN authority

### 4.1 The facts that shape it

- **No S3 wire accepts a bearer.** No mainstream S3 implementation
  accepts `Authorization: Bearer` (the only verified exception is Google
  Cloud Storage's XML API); `mount-s3` speaks SigV4 through the AWS CRT
  chain with no bearer mode; `flint-store` builds a stock
  `aws_sdk_s3::Client` with no header hook (`crates/flint-store/src/s3.rs:39-74`;
  `crates/flint-store/src/lib.rs:529-691`; [web-knox-jwt §6.1], VERIFIED/
  REPORTED). **An exchange step ending in SigV4 keys is mandatory**, and
  it must happen outside the tenant pod.
- **Knox is not an S3 broker.** Apache Knox ships no S3 proxy and no
  SigV4; IDBroker (`aws-cab`) is Cloudera-proprietary (absent from
  apache/knox and Maven Central); KNOX-1204 is Open ([web-knox-jwt §4],
  VERIFIED).
- **A Knox JWT is a plain RS256 JWT with a public JWKS** at
  `knoxtoken/api/v1/jwks.json` (Knox ≥ 1.6.0), `iss` defaulting to the
  literal `KNOXSSO`, claims `sub`, `aud`, `exp`, `knox.id`, optional
  `knox.groups`; no `jti`, no `nbf` ([web-knox-jwt §1.4-1.6], VERIFIED).
  Knox 2.1.0 publishes **no** `/.well-known/openid-configuration`
  (KNOX-3141 Open, fix-version 3.0.0), so AWS STS, MinIO and Ceph RGW
  cannot consume a Knox token as-is ([web-knox-jwt §5], VERIFIED).
- **Knox 2.1.0 can trust an external issuer** (`JWTProvider` with
  `knox.token.jwks.urls` + `jwt.expected.issuer` list, KNOX-2149/KNOX-3040)
  and a `KNOXTOKEN` service behind it will mint a Knox JWT for the
  asserted principal — but Kubernetes projected tokens carry no `typ`
  header and KNOX-3434 (opened 2026-09-02/03 — JIRA server time; the
  date is UNVERIFIED, see the header) says they "would fail
  verification" on 2.1.0 ([web-knox-jwt §3.2-3.3], VERIFIED params /
  UNKNOWN composition).
- **Knox master (unreleased 3.x) is building exactly the broker this
  wants**: `knoxidf` with OIDC discovery, RFC 8693 token exchange, a
  JDBC-backed trusted-OIDC-issuer registry, and Kubernetes projected
  tokens explicitly on the list ([web-knox-jwt §3.4], VERIFIED source,
  unreleased).
- **Both flint clients already consume two Knox-agnostic bridges with
  zero code change**: STS web identity (`AWS_ROLE_ARN` +
  `AWS_WEB_IDENTITY_TOKEN_FILE`, with `AWS_ENDPOINT_URL_STS` honoured by
  mount-s3 ≥ 1.16.0 — issue #1203, changelog v1.16.0 — and by the pinned
  `aws-config 1.10.1` via `ProviderConfig::client_config()` →
  `EnvServiceConfig` (`web_identity_token.rs:244`, `env_service_config.rs:12-30`,
  code-read; pin with a unit test, §9 E5) and the container-credentials
  URI (`AWS_CONTAINER_CREDENTIALS_FULL_URI` + `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE`:
  plain `http` only to loopback / `169.254.170.2` / `169.254.170.23` /
  `fd00:ec2::23`, `https` to any host — CRT `credentials_provider_ecs.c`
  [web-knox-jwt §6.2] and, verified this session, `aws-config-1.10.1/src/ecs.rs:433-484`
  with tests `:575-666`). The kubelet-minted, pod-bound SA token is the
  only cluster-native JWT and the one Kubernetes will hand a CSI plugin
  ([web-k8s-csi-mechanics §3.3, §7]).
- **Two tokens, two jobs.** The K8s SA token is the *unforgeable pod
  identity*: minted by kubelet with `BoundObjectRef Kind: Pod, UID`,
  audience-bound, carrying `kubernetes.io.{namespace, pod{name,uid},
  serviceaccount, node}`, rejected by the API server 60 s after the
  pod's `deletionTimestamp` — the pod author cannot choose its signature,
  binding or audience ([web-k8s-csi-mechanics §7], VERIFIED). The Knox
  JWT is the *project's authority*: it says which project principal, in
  the customer's vocabulary (`sub`, `knox.groups`); it cannot see pods.
  The chain uses each for what it proves, in that order **[graft:
  security — §2.6 of that design, kept verbatim in spirit]**.

### 4.2 The chain (v1) — sequence

```
kubelet ─ TokenRequest(aud=s3.flint.io, BoundObjectRef Pod/UID, 3600 s) ─▶ SA JWT T ─▶ NodePublish volume_context
   │ (kubelet refreshes T at 80 % TTL; republish ~60-90 s rewrites the file)               │
   │                                                                                       ▼
   │                                              worker pod: comm/token (0600, memory emptyDir)
   │                                              AWS_ROLE_ARN=arn:flint:iam::project:role/<projectId>
   │                                              AWS_WEB_IDENTITY_TOKEN_FILE=/comm/token
   │                                              AWS_ENDPOINT_URL_STS=https://flint-s3-broker.flint-system.svc/
   │                                              AWS_REGION=…  AWS_EC2_METADATA_DISABLED=true
   │                                                                                       │
   │                      ┌────────────────────────────────────────────────────────────────┘
   │                      ▼  POST /  Action=AssumeRoleWithWebIdentity&RoleArn=…&WebIdentityToken=T   (unsigned)
   │            flint-s3-broker
   │              1  TokenReview{token: T, audiences: [s3.flint.io]} → authenticated, username
   │                 system:serviceaccount:<ns>:<sa>, extra pod-uid; require status.audiences ∋ s3.flint.io
   │                 and (pod-uid, RoleArn's CR, RoleSessionName) match a LIVE publish registration whose
   │                 node-name extra equals the pod's node (the registration, below)   [graft: security]
   │                 (online review, not offline JWKS: offline verifiers "do not verify the claims … to be
   │                  current"; a deleted pod's token is honoured until exp — [web-k8s-csi-mechanics §7])
   │              2  GET the CR named by RoleArn's projectId, in the TOKEN's namespace (not a request field);
   │                 require sa ∈ spec.consumers.serviceAccounts; else 403 naming the SA and the field.
   │                 The (namespace, serviceAccount) → project-principal mapping is ADMIN-OWNED and
   │                 cluster-scoped — a FlintProjectBinding cluster resource (or a ConfigMap in flint-system)
   │                 that only the flint administrator may write; the tenant CR carries identity.mode ONLY.
   │                 Under K1 the mapping lives in Knox's identity-assertion provider (Knox-admin-owned) —
   │                 the same property. A tenant-writable field must never select the principal the broker
   │                 impersonates: the CR is namespaced and tenant-writable (values.yaml:32-34), and under K2
   │                 the broker spends ITS proxyuser power on whatever principal is named
   │              3  Knox step — exactly ONE of (VERIFIED/UNVERIFIED per §4.3):
   │                   K0  no Knox in the pod path: the proxy/STS trusts the cluster issuer directly
   │                   K1  forward T as Bearer to /gateway/<T>/knoxtoken/api/v1/token?lifespan=PT15M
   │                   K2  broker's own Knox identity: knoxtoken?doAs=<principal from the admin-owned binding>&lifespan=PT15M
   │                   K3  Cloudera IDBroker present: Knox JWT → /gateway/aws-cab/cab/api/v1/credentials
   │              4  proxy step: Knox JWT → {AccessKeyId, SecretAccessKey, SessionToken, Expiration=+15m}
   │                   P-CRED  proxy credentials endpoint (Bearer Knox JWT in, keys out)
   │                   P-STS   proxy is MinIO/RGW/AWS STS (needs the OIDC discovery shim, §4.3)
   │                   P-BEARER proxy accepts ONLY a bearer on the data path — REFUSED unless forced (§7)
   │              5  audit line (ns, sa, pod-uid, project, exp); return <AssumeRoleWithWebIdentityResponse>
   ▼
 mount-s3 / flint-sync sign SigV4 to the project-scoped proxy with those keys; the CRT / SDK re-fetch
 before Expiration by re-calling the broker with the CURRENT token file
```

**The publish registration** (the binding step 1 checks). In the primary
web-identity delivery the plugin never talks to the broker and `RoleArn =
arn:flint:iam::project:role/<projectId>` carries no volume id or pod uid,
so "the pod-uid the plugin recorded" had no channel to reach the broker.
The channel is this: the plugin registers each publish at the broker over
its own node SA token — `POST /v1/volumes {volume_id, pod_uid, namespace,
sa, cr, nonce}` — where `nonce` is 32 random hex written into the worker
as `AWS_ROLE_SESSION_NAME` (sent as `RoleSessionName` by both the CRT and
the Rust SDK). The broker requires the `TokenReview` pod-uid, the
RoleArn's CR and `RoleSessionName == nonce` to match a live registration
whose `authentication.kubernetes.io/node-name` extra equals the pod's
node; unpublish deletes the registration. This is the one binding a pod
cannot self-mint: it closes the broker path of the T2 bypass (§2.4).
Echoing the pod uid in `RoleSessionName` instead would add nothing (the
uid is already in the token).

Why this shape (the spine's, with security-first's discipline grafted in):

- **The tenant pod is given nothing.** Not a key, not a Knox JWT, not even
  the broker-audience SA token — that lives in the *worker's*
  memory-backed emptyDir, written host-side by the node plugin (the AWS
  v2 `provider_pod.go` pattern, [web-fuse-csi-priorart §2]). This keeps
  and strengthens "pods hold zero bucket credentials"
  (`docs/plans/flint-lean-plan.md:66-73`; the plan had the sidecar holding
  a proxy token, `:246-247` — now not even that). What the pod can
  *obtain* is another matter: a process under an entitled SA can project
  its own `aud=s3.flint.io` token, and only the registration nonce
  (above) stops the broker honouring it (§2.4 T2).
- **The node holds no S3 key and no Knox credential.** The plugin only
  forwards kubelet's token into the worker and registers the publish
  over its own node SA token (a registration, never an exchange); the
  worker talks to the broker itself. A dump of node-plugin memory yields pod-bound tokens
  useless outside audience `s3.flint.io`, dying with their pods. This is
  the answer to the un-partitioning objection on record
  (`docs/plans/multicluster-frontdoor-review.md:50-51`): the concentration
  point holds nothing long-lived and nothing usable elsewhere; the only
  standing credential in the design is the broker's — a Knox identity
  under K2; a client certificate Knox pins under K1 and the endgame
  (§4.3, §4.7) — or none under K0 **[graft: security §1.3, amended]**. It also honours "the operator
  must never hold the token root" in spirit
  (`docs/plans/file-api-fleet-auth.md:92-104`): the one place that can
  mint for everyone is neither a node, the operator, nor a tenant.
- **No JWT code in flint.** `TokenReview` is a Kubernetes API call; the
  Knox step is HTTP; the STS façade is XML over TLS. The `jsonwebtoken`
  dependency the fleet-auth note foresaw (`file-api-fleet-auth.md:236-238`)
  stays out of v1.
- **Zero client changes.** mount-s3 ≥ 1.16.0 and the pinned Rust SDK both
  do `AssumeRoleWithWebIdentity` against an endpoint override and re-read
  the token file per fetch (§4.1). Whether the CRT trusts an in-cluster
  CA for the broker's TLS is **ASSUMPTION** — the mounter image carries
  `ca-certificates` (`Dockerfile.passthrough:17-41`); the broker's
  serving cert must chain to a CA installed in that bundle (§9 E6).
- **The fallback delivery is the loopback container-credentials door**
  **[graft: minimal + security]**: the worker's PID 1 serves
  `http://127.0.0.1:9911/v1/creds` in the worker's own network namespace
  from a `creds.json` the node plugin writes host-side after doing the
  exchange itself, gated by a per-volume random
  `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE`. VERIFIED consumable by both
  the CRT and the Rust SDK (§4.1). It needs no TLS trust in the worker
  image and works for a P-CRED-only proxy, at the cost of the node plugin
  holding S3 keys in memory per volume. The `CredentialSource` trait with
  arms `webIdentity | broker-door | static | ambient` is the seam
  (`s3csi/creds.rs`, §9.4) **[graft: minimal]**.
- **Why the broker is STS-shaped**: if the customer's proxy is MinIO or
  Ceph RGW with an STS endpoint, the broker *is* that STS and the cluster
  is registered as its OIDC provider (`config_url` = the cluster's
  discovery document; bind `system:service-account-issuer-discovery` to
  `system:unauthenticated` or mirror the JWKS — [web-knox-jwt §5.2-5.3];
  [web-k8s-csi-mechanics §7]). That is K0: Knox authenticates humans and
  the S3 side, not pods, and no flint broker is needed — **only with a
  per-SA condition at the STS** (§4.3 row K0): without one, any pod in
  the cluster can mint the audience and receives the project's policy,
  and `consumers` is never consulted.

### 4.3 Every Knox-dependent step, marked

| Step | Claim | Status | Citation | Settles |
|---|---|---|---|---|
| K1 | Knox 2.1.0 `JWTProvider` verifies a K8s projected token via `knox.token.jwks.urls=<cluster>/openid/v1/jwks`, `jwt.expected.issuer=<--service-account-issuer>`, `knox.token.audiences=s3.flint.io`, then identity-assertion maps `system:serviceaccount:<ns>:<sa>` → project user and `KNOXTOKEN` mints. **REQUIRED on the K1 topology**: `knox.token.client.cert.required=true`, `knox.token.allowed.principals=<broker client-cert DN>` (v2.1.0 `TokenResource.enforceClientCertIfRequired()` → 403), `knox.token.audiences=<a proxy-only audience>`, `knox.token.ttl` ≤ 15 min, and the broker always passes `lifespan=PT15M`. Without the cert gate any pod under a mapped SA can mint a Knox SSO token for the project principal by projecting its own `aud=s3.flint.io` token straight at the topology — bypassing `TokenReview` and `consumers` — with the caller-chosen `lifespan` up to `knox.token.ttl`, and that token is valid at every Knox topology that sets no `knox.token.audiences` (v2.1.0 `AbstractJWTFilter.validateAudiences`: "if there were no expected audiences configured then just consider any audience acceptable"), i.e. WebHDFS, Hive, … for the project principal; the §3.6 worker NetworkPolicy does not constrain the tenant pod | parameters **VERIFIED** to exist in 2.1.0; the composition is exercised in no doc; the `typ` header is the blocker — **UNVERIFIED**, KNOX-3434 predicts failure | [web-knox-jwt §3.1-3.3]; v2.1.0 `TokenResource.java`, `AbstractJWTFilter.java` | whether v1 can run with a pinned client certificate as the broker's only standing credential |
| K2 | The broker's own Knox identity calls `knoxtoken?doAs=<project user>&lifespan=PT15M` on a topology whose identity-assertion provider lists it under `hadoop.proxyuser.<broker>.users/.hosts` | `doAs` (KNOX-2714, 2.0.0) and `lifespan` **VERIFIED** to exist; whether `doAs` works with `knox.token.exp.server-managed=false` on 2.1.0 is **UNVERIFIED** (the 2.1.0 source couples the check to managed tokens); which auth provider fronts the customer's topology (Kerberos/LDAP/SSO/JWT) is **UNKNOWN** | [web-knox-jwt §2, §1.2] | how the broker authenticates to Knox at all |
| K3 | Cloudera IDBroker: `GET /gateway/aws-cab/cab/api/v1/credentials` with a Knox bearer returns AWS keys | **REPORTED** only (Cloudera pages 403'd the fetcher); not in Apache Knox | [web-knox-jwt §4.1] | if present, step 4 collapses to one call |
| K0 | The proxy/STS trusts the cluster issuer directly (MinIO `config_url`, RGW OIDC provider, AWS OIDC provider) — **ONLY with a per-SA condition at the STS**: AWS/Ceph trust policy `Condition: StringEquals <provider>:sub = system:serviceaccount:<ns>:<sa>` (the IRSA pattern); MinIO policy conditions on `${jwt:sub}` (`internal/config/identity/openid/jwt.go` `Validate`: the policy is selected by `RoleArn`, the only identity check is `aud`/`azp == ClientID`, `iss` is not compared, `role_policy` is per provider not per subject). In K0 the CR `consumers` list is not consulted — entitlement lives entirely at the STS, and any pod in the cluster can mint the audience | mechanisms **VERIFIED** for MinIO/RGW/AWS; the customer's proxy is **UNKNOWN** | [web-knox-jwt §5] | no broker at all |
| P-CRED | The proxy exposes a credentials endpoint taking a Knox bearer and returning keys | **UNKNOWN** — the single most important unknown | [web-knox-jwt §8 Q1] | the broker's back half |
| P-STS | The proxy accepts `AssumeRoleWithWebIdentity` with a *Knox* JWT | **blocked on 2.1.0** without `knox.token.issuer` set to an HTTPS URL **and** a static discovery shim served at that URL (AWS requires `iss` == provider URL; RGW wants `x5c` in the JWKS, which Knox's `JWKSResource` was not seen to emit); MinIO does not compare `iss` in the code read | [web-knox-jwt §5.1-5.4] | fallback only |
| `lifespan` | honoured on 2.1.0, capped by `knox.token.ttl` (default **30 s**; the shipped `homepage.xml` sets 120 days) | param **VERIFIED**; effective value **UNKNOWN** | [web-knox-jwt §1.3] | irrelevant to mounts: the Knox JWT is consumed inside the broker within milliseconds; the key that must last is the STS key |
| `audience` request param | honoured on 2.1.0 | **UNKNOWN** (KNOX-3424 is 3.1.0) — use `knox.token.audiences` topology config instead | [web-knox-jwt §1.2] | — |
| JWKS reachability | Knox can fetch the cluster JWKS and refreshes on rotation (`gateway.jwks.cache.refresh.interval` 15 s; EKS rotates signing keys every 7 days, REPORTED) | **UNKNOWN** for the customer's network | [web-knox-jwt §1.3]; [web-k8s-csi-mechanics §10] | K1 operability |
| Endgame | Knox 3.1.0 RFC 8693 exchange with the cluster registered as a trusted OIDC issuer (KNOX-3355 resolved 2026-09-02 for 3.1.0; KNOX-3405/3408/3412/3432/3433/3434 Open) | **VERIFIED unreleased** — do not design on it | [web-knox-jwt §3.4] | replaces K1's identity-assertion topology with one standard call |

### 4.4 What the worker gets, per credential mode

| Mode | Worker env | Refresh | When |
|---|---|---|---|
| `knox` (v1 target, STS façade) | `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE=/comm/token`, `AWS_ROLE_SESSION_NAME=<per-volume registration nonce, §4.2>`, `AWS_ENDPOINT_URL_STS`, `AWS_REGION`, `AWS_EC2_METADATA_DISABLED=true` | token file rewritten on republish; keys re-fetched by the client before `Expiration` | Knox/JWT projects |
| `knox` via the loopback door (fallback) | `AWS_CONTAINER_CREDENTIALS_FULL_URI=http://127.0.0.1:9911/v1/creds`, `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE=/comm/auth.token` | node plugin re-exchanges at republish when keys are within 3 periods of expiry; rewrites `creds.json` | P-CRED-only proxies; TLS-trust problems in the mounter image |
| `irsa` / `pod-identity` | the same files with AWS STS / EKS Pod Identity endpoints (Mountpoint CSI's `authenticationSource: pod`, [web-fuse-csi-priorart §2]) | same | AWS-native clusters |
| `static` (interim, deprecated) | `AWS_*` from the pod's `nodePublishSecretRef` written to `/comm/credentials` as a profile (0600) | none (as today) | rigs, migration |
| `ambient` | nothing; the worker's own chain | client-managed | dev |

### 4.5 The interim static arm

`static` uses CSI `nodePublishSecretRef`: a Secret in the pod's namespace
fetched by **kubelet** with kubelet's credentials and delivered in
`NodePublishVolumeRequest.secrets` (`csi_mounter.go:148-162,218-224`;
KEP-596 — [web-k8s-csi-mechanics §1.1]). The node SA therefore needs no
Secrets RBAC anywhere, preserving "no component may be granted `get
secrets` across tenant namespaces" (`file-api-fleet-auth.md:99-104`). The
plugin must **not** treat the Secret's presence as authorization — the
`consumers` check still runs; `identity.mode: static` merely tells the
broker not to exchange. A pod author can hand *any* Secret in their
namespace to the driver by naming it (KEP-596), which is exactly today's
trust level. The CR's `credentialsSecretRef` is ignored in csi mode and
deprecated: under CSI the pod author names the Secret, which they could
already read.

### 4.6 Rotation, revocation, lifetimes

| Token | Lifetime | Refreshed by | Revoked by | Max staleness |
|---|---|---|---|---|
| SA JWT (aud=`s3.flint.io`) | 3600 s (min 600, `validation.go:559-579`); `--service-account-max-token-expiration` may cap it (default UNKNOWN) | kubelet at 80 % TTL from its cache (`token_manager.go:39-43`); delivered on every republish (~60-90 s) | pod deletion: the API server rejects it 60 s after `deletionTimestamp`, which for a pod is delete-time + its grace period (`k8s.io/apiserver rest/delete.go` `BeforeDelete` sets it to now + `GracePeriodSeconds`); immediately once the object is gone (`--force`); the broker sees it via `TokenReview` | next exchange, ≤ key lifetime |
| Knox JWT | `lifespan=PT15M` (capped by `knox.token.ttl`) | never — minted fresh per exchange; no TSS renewal, no renewer whitelist needed | Knox-side revocation is seen at the next mint (TSS revocation is invisible to offline verifiers, [web-knox-jwt §1.7] — another reason the Knox JWT is consumed inside the broker, never on the data path) | ≤ key lifetime |
| STS keys | `Expiration` = 15 min (chart value) | the client re-calls the broker before expiry | SA removed from `consumers` / CR deleted / Knox refuses ⇒ next exchange fails ⇒ 403 at expiry; the plugin emits a `CredentialRefreshFailed` Event on the tenant pod (kubelet volume-health reporting is gated off by default, §6.1; KEP-1855's policy for a failed republish is "keep the container running with old credentials") | ≤ key lifetime |

**The key lifetime is the outage tolerance and the revocation latency;
15 minutes is the recommended trade.** Republish must only refresh
contents, never re-mount (§3.7). The three clocks, fastest to slowest:
(a) pod deletion → no republish → `TokenReview` fails from
`deletionTimestamp + 60 s` = delete-time + grace + 60 s (immediately
under `--force`) → no new keys — which is why the lean drain needs
longer-lived keys than passthrough (§5, final-barrier row); (b) CR edit → next exchange refused → keys die at `Expiration`;
(c) Knox revocation → refused at the next mint.

### 4.7 Endgame

- **Knox verifies the cluster's tokens directly** (K1 proven, `typ`
  settled): the broker keeps `TokenReview` (prompt revocation on pod
  deletion, which offline verification cannot give) and the `consumers`
  check (keeps "which SA may mount which CR" in flint's hands), and
  **keeps one standing credential — a client certificate that Knox pins
  with `knox.token.allowed.principals`** — because Knox cannot otherwise
  distinguish the broker from a pod presenting the same kubelet-minted
  token (§4.3 row K1); under that certificate it is a policy point that
  forwards the pod's token to Knox **[graft: security §2.5, amended]**.
- **Knox 3.1.0 `knoxidf`**: RFC 8693 `grant_type=…token-exchange&subject_token=T`
  with the cluster in the trusted-issuer registry replaces the
  identity-assertion topology ([web-knox-jwt §3.4]; unreleased).
- **`serviceAccountTokenInSecrets: true`** (beta 1.35, GA 1.36) moves the
  token out of `volume_context` ([web-k8s-csi-mechanics §3.1]).
- **Convergence with the fleet-auth endgame**: the broker's `(ns, SA) →
  project` mapping and the hub file API's planned `aud = flint:<project>`
  SA-JWT verification (`file-api-fleet-auth.md:204-242`) key on the same
  identity; the lean gateway's one shared bearer (`lean/sidecar/src/gateway.rs:35-38`)
  is the remaining odd one out.
- **P5 at the proxy** (per-PUT `x-amz-meta-flint-epoch` vs the
  `<prefix>/.flint/lean/epoch` cell, DECIDED 2026-08-26, unbuilt —
  `docs/plans/flint-lean-boundary-verbs-plan.md:314-317`, `code-auth §3`)
  rides the same door: the proxy that verifies the pod's credential is
  the proxy that can refuse a deposed writer's PUTs. This design neither
  builds nor blocks it; the epoch stamp and cell key are unchanged by
  where the writer runs.
- If the customer is on Cloudera CDP, K3 is the endgame from day one.

---

## 5. Lifecycle mapping for lean

Lean's sidecar is a loop, not a mount: claim → checkout → three-interval
run loop (publish floor, lease renew, sentinel poll) → SIGTERM drain
(`flint_sync.rs:341-374,468-490,583-604`). CSI contributes two moments —
before start and after stop — and the worker pod hosts the loop between
them. `flint-sync` changes by **one subcommand** (`drain`, §3.5) and one
SIGTERM-time credential refresh (final-barrier row) and otherwise not at
all; `flint-store` changes not at all.

| Coupling | Today | Under CSI | Verdict |
|---|---|---|---|
| **Start gate** | native sidecar **appended** to `initContainers` (`inject.rs:229`) + `startupProbe` on the marker with a derived threshold (`inject.rs:42-56,130-136`); gates app containers only — pre-existing init containers run before it and never see the tree (`:113-128` mounts app containers only) | `NodePublishVolume` blocks ≤ 100 s on the marker and returns non-final `Unavailable` with progress; kubelet retries with backoff 500 ms → 2m2s (`csi_client.go:902-924`; `exponential_backoff.go`); the syncer keeps running between calls; checkout is idempotent and resumable (`checkout.rs:1-14,173-222`); over-budget is a **final** refusal naming `maxFiles`/`maxBytes` before the first byte (`checkout.rs:84-98`). Worst-case start latency = checkout time + one backoff interval (≤ 2m2s). A wedged checkout must FAIL, never write the marker over an empty tree — chaos C12 holds because the marker is written last by the same code (`run-chaos.sh:681-700`; `state.rs:166-169`) | **better**: init containers gated too; a plugin restart does not restart the checkout (the syncer is a separate pod); no `test -f` shell requirement in the image (`Dockerfile.sync.prebuilt:25-28`) |
| **Config delivery** | ~22 `FLINT_SYNC_*` env stamped at injection; lock-step contract test (`inject.rs:352-408`); no re-read path | the plugin renders the same list into the worker spec, stamping `FLINT_SYNC_NAMESPACE=<pod.namespace>` as a literal (today it is not in the fixed list but a downward-API ref to the pod's own `metadata.namespace`, `inject.rs:174-184`, which in a worker pod would resolve to `flint-workers` and mislabel every metric series and the heartbeat echo) and copying the `prometheus.io/*` scrape annotations (`:231-239`, stamped on the tenant pod today) onto the worker; the contract test re-homes to the render fn. Spec edits still require pod recreation (`crd.rs:136-141`) | same |
| **Door relocation** | `.flint/publish`, `.flint/sync` sentinels, acks, `remote.seq`, `capabilities.json` are files under the emptyDir (`control.rs:1-23`); the opt-in UDS `.flint-sync/ctl.sock` has "no auth because the trust boundary is the pod" (`uds.rs:13-19`), and `/v1/sync` *executes* because the caller is in the pod (`uds.rs:35-39`); the gateway door is bucket-mediated (`inbox.rs:44-72`; `barrier.rs:250-280`) | the file protocol is unchanged — files under a bind mount. The UDS socket lives in the tree, reachable through the same inode from the tenant pod and the syncer; the set of processes that can reach it is {the tenant pod's containers, the syncer, node root}, which is today's set plus node root, which already owns everything. The justification for executing `/v1/sync` survives because the bind is per-pod. Gateway door unchanged. Restated in the CRD doc; no behaviour change | same |
| **Final barrier at unpublish** | native-sidecar ordering: app SIGTERMed first, then the sidecar's SIGTERM arm (3 attempts 2 s apart, break on `Fenced`, release — `flint_sync.rs:583-604`); grace derived and only ever raised (`boundary.rs:65-81`); `GraceTooShort` refuses **gated** drains over the 120 s spot ceiling (`boundary.rs:114-116,159-171` — `validate_spec` returns `Ok` for non-gated modes before the ceiling check); cadence/hybrid grace is `floorSecs + 21` with no ceiling (`boundary.rs:65-81`; `run-agent.sh:266-274` only `note`s > 300 s); a sleep-PID-1 app "sits out the entire budget before the drain is asked for anything" (`run-agent.sh:266-274`) | `NodeUnpublishVolume` (after all tenant containers exited) → delete the syncer with `gracePeriodSeconds = derived_grace_secs(spec)` → the same SIGTERM arm runs. The tree is quiescent by kubelet's guarantee, not by sidecar ordering; the tenant's grace and the drain's grace are separate budgets; **`kubectl delete --grace-period=0 --force` drains only while the syncer's cached STS keys are valid** — the API object goes and kubelet still runs unpublish locally ([web-k8s-csi-mechanics §5]), but the pod object is gone, so `TokenReview` refuses every key refresh; under a normal delete refreshes are refused from `deletionTimestamp + 60 s = t0 + G + 60 s` (`rest/delete.go` `BeforeDelete` sets `deletionTimestamp` = now + grace; service-accounts-admin: bound tokens fail 60 s after it). The drain (≤ `derived_grace + 30 s`) runs after containers exit, needs proxy keys, and refreshes them via the broker with T; `aws-smithy-runtime`'s lazy identity cache (`DEFAULT_BUFFER_TIME` 10 s) propagates a resolver error with no stale fallback and `flint-store` classifies 401/403 as `StoreError::Auth` (§6.3) — so any drain needing a refresh after that point fails and orphans. Mitigations: lean workers get keys with `Expiration ≥ 2 × (derived_grace + tenant grace) + 60 s` (a chart value distinct from passthrough's 15 min), and `flint-sync` resolves a fresh credential set at SIGTERM before the first drain PUT (bypassing the lazy cache) so a drain that starts inside the 60 s window carries keys for its whole ceiling. Today `--force` SIGKILLs the sidecar and drains nothing (B11b). The Pod object is not removed until unmount succeeds (`status_manager.go:1334-1351`), so a drain cannot be cut off by API deletion | **better** on correctness; **worse** on bounding: each attempt has a 2-min gRPC budget (the plugin returns `Unavailable` at 100 s and continues across retries — a gated drain within the 120 s ceiling finishes in two attempts); a StatefulSet name stays `Terminating` for ≤ `derived_grace + 30 s` — ~150 s for gated workspaces, `floorSecs + 51 s` with no ceiling for cadence/hybrid; `GraceTooShort` keeps its meaning as the syncer's grace; node death drains nothing, as today (`flint-lean-plan.md:407-408`; the operator's DR signature `reconcile.rs:380-431` covers it) |
| **Holder identity** | `lean-<uuid4>` in `.flint-sync/incarnation.json`, emptyDir-scoped: container restart self-recognises, pod replacement takes over after 6 quiet polls (`lease.rs:8-14,24,40-93`; `state.rs:4-8`) | the tree is keyed on the volume id = f(podUID, volumeName): container restart ⇒ no new `NodePublish` ⇒ same tree ⇒ self-recognition; pod replacement ⇒ new volume id ⇒ fresh tree ⇒ takeover rotation. **Never key on the CR name** — a replacement would self-recognise a dead pod's lease and skip rotation (`lease.rs:72-80`; `code-lean §8.3`). Syncer crash-restart over the same tree ⇒ self-recognition; the flock is released by exit and retaken (`state.rs:110-159`). `holder_id` stays `lean-<uuid>`; nothing in the bucket protocol changes | same, with a stronger identity |
| **Restart over a live tree (node plugin)** | n/a | the syncer is a separate pod; the loop mount and both binds are in the host mount table; the plugin re-adopts state from `state.json` and worker labels on start (`cleanup_ghost_mounts` pattern, `main.rs:79-146`). **The plugin's own SIGTERM never drains anything** — rolling the DS must not fence live agents (`code-lean §6.3`); trivially true because syncers are separate pods | better than a child-process design (which would SIGKILL every syncer on the node per roll) |
| **Drain / eviction** | pod SIGTERM → drain in the pod's grace | tenant eviction is a normal termination → `NodeUnpublish` → drain with its own grace. `kubectl drain` evicts tenants; the PDB (`minAvailable: <integer>`, §3.6 — bare pods may not use `maxUnavailable`) makes the eviction API refuse workers while they exist and drain retries; the plugin deletes workers directly (direct delete does not consult the PDB); workers carry `cluster-autoscaler.kubernetes.io/safe-to-evict: "true"` and an `ownerReference` to the Node with `controller: true` (so `kubectl drain` treats them as managed and waits on the eviction API instead of refusing controller-less pods without `--force`, `kubectl/pkg/drain/filters.go:236-249`) so a vanished node GC's them. Spot reclaim: nobody drains — same as today | same |
| **Budgets** | `maxBytes`/`maxFiles` refused before the first byte; `fetchInflightMb` 512 MiB (`crd.rs:79-85`); emptyDir `sizeLimit` → eviction | first two unchanged (the syncer's); `fetchInflightMb` is the syncer pod's memory request/limit; `sizeLimit` is the loop-image filesystem → `ENOSPC`, no eviction | mixed, mostly better |
| **Metrics** | `0.0.0.0:<port>` in the pod network; `PodMonitor` by label; per-namespace NetworkPolicy (`flint_sync.rs:388-440`; `monitoring.yaml:10-65`) | the syncer pod carries the port and `flint.io/tenant-namespace`/`flint.io/workspace` labels; the `PodMonitor` and NetworkPolicy move to `flint-workers` (one place) | moves |
| **Operator echo / conditions** | ride the lease renewal (`lease.rs:114-144`; `reconcile.rs:316-378`) | unchanged | same |
| **Gated-mode recovery** | assumes the emptyDir dies with the pod (`crd.rs:136-141`) | the tree is deleted at unpublish — holds | same |

---

## 6. Failure modes and mitigations

### 6.1 FUSE process death

A FUSE mount is exactly as alive as its daemon; after the daemon dies,
I/O on the mount returns `ENOTCONN` (kernel `fuse.rst` "Aborting a
filesystem connection"; gcsfuse known-issues — [web-k8s-csi-mechanics
§6.2]); kubelet classifies it as corrupted (`IsCorruptedMnt`) but re-binds
nothing into a running container; measured in this repo
(`passthrough/inject.rs:176-184`, leg A12). The candidates, and why each
lost, are in §7. Under the chosen shape (fd owned by the worker pod): a
**node-plugin** restart or upgrade touches zero mounts ([web-fuse-csi-priorart
§2 "driver-restart survival"]); a **worker's** own death strands exactly
one tenant pod's consumers — today's failure domain, A12's first half
still holds. What is lost is the in-pod readiness flip
(`inject.rs:173-202,466-476`; `values.yaml:36-44`); no CSI primitive can
flip a pod's readiness. The replacement, honestly ranked: (1) the plugin
itself emits a `Warning` Event on the tenant pod (it already holds
`events create,patch`) when its worker watch sees the mounter die —
kubelet will not do it: volume-health reporting is gated by
`CSIVolumeHealth` (Alpha, default off since 1.21 and still Alpha on
master, `kube_features.go`); on ≤ 1.36 the `VolumeConditionAbnormal` pod
event is emitted only inside that gate
(`volume_stat_calculator.go:170-173`; `csi_client.go:625-636`), kubelet
volume metrics skip volumes without a PVC ref (`volume_stats.go:112-116`),
and its wire form — `VOLUME_CONDITION` in `NodeGetVolumeStats`, the one
the existing driver advertises at `main.rs:5726-5737` — was removed from
the CSI spec ("Value 4 was VOLUME_CONDITION, an alpha API that has been
removed. reserved 4;") in favour of the alpha `NodeGetVolumeHealth` /
`GET_VOLUME_HEALTH=7` → `pod.status.volumeHealth`, behind the same gate;
advertise `GET_VOLUME_HEALTH` only as an optional extra; (2) the chart
documents a workload-side `readinessProbe: exec test -d /mnt/s3` (a dead
FUSE mount answers `ENOTCONN`); (3) the pod must still be recreated — a
property of FUSE, not of the delivery. Mounter workers use `restartPolicy:
Never`: a restarted container cannot re-acquire an fd that was passed
once — a dead mounter is a dead mount, and pretending otherwise hides it
**[graft: security]**. Lean (syncer) workers are different and the
design says so: `restartPolicy: OnFailure` — an in-place restart over the
same tree, as today's native sidecar (`restartPolicy: Always`,
`inject.rs:208`, restarts `flint-sync` in seconds on any exit — `Fenced`,
`flint_sync.rs:499,521,534`, panic, OOM — over the surviving emptyDir),
which the restart matrix (`checkout.rs:1-14`) and the persisted
`{last_token, quiet_polls}` observation (`lease.rs:11-13`;
`state.rs:57-66`) are built on. The plugin's policy on repeated `Fenced`
exits: two `Fenced` exits over one tree ⇒ the plugin stops the worker and
the next `NodePublish` returns `FailedPrecondition` naming the fence
(today: a crash loop that can retake after 6 quiet polls). Optional self-heal (§9 E2): on mounter death the
plugin can `umount -l` the dead `src`, re-mount with a fresh fd, start a
new worker and re-bind `target`; whether a running container sees the
new mount depends on its volumeMount propagation — with the default
(`None`) it does not; with `HostToContainer` on the tenant's volumeMount
new opens *may* succeed (open fds stay dead). The passthrough drill
measured "does not reach them" with the subPath-of-emptyDir design; the
CSI target is a direct bind, so the answer may differ. This is an
experiment, not a claim.

### 6.2 Node-plugin upgrade with 500 mounted pods on a node

The DS pod restarts; the 500 worker pods do not. Every FUSE fd is owned
by a mounter; every syncer keeps publishing. On start the plugin rebuilds
its table from `<plugin>/volumes/*/state.json` and by listing workers on
its node (`flint.io/node=<name>`), re-arms liveness probes and resumes
token forwarding — which kubelet drives anyway via republish. **Zero
data-plane events.** The DaemonSet may use `RollingUpdate` — the existing
`OnDelete` roller exists because spdk-tgt dies under live mounts
(`node.yaml:14-27`), which no longer applies **[graft: minimal]**. A
mount-s3 version bump applies to workers created after the chart
upgrade; old pods keep the old mounter until they cycle; the mix is one
`kubectl -n flint-workers get pods -o custom-columns=…IMAGE…` away; no
forced roll, no `ENOTCONN` wave. Contrast today: a mounter image upgrade
cannot be applied to running pods at all, and a FUSE daemon inside the DS
would strand all 500 (blob-csi: "restart csi-blobfuse-node daemonset
would make current blobfuse mounts unavailable"; yandex csi-s3 runs
GeeseFS under host systemd "to not crash mountpoints" on upgrade —
[web-fuse-csi-priorart §4-5]).

### 6.3 Knox token expiry at 3 a.m. (Knox or the broker unreachable)

Nothing dies. Each mount holds SigV4 keys with an `Expiration` the broker
chose (15 min); the CRT/SDK refresh keys by re-calling the broker with
the current token file before expiry ([web-knox-jwt §6.2]). If the broker
is down, the refresh fails, cached keys serve until `Expiration`, then
mount-s3 returns per-operation errors (403 → `EACCES`/`EIO`) — the mount
stays mounted, no `ENOTCONN`; `flint-store` classifies 401/403/
`ExpiredToken` as `StoreError::Auth` (`crates/flint-store/src/s3.rs:131-143`
— split out so the *hub's* heartbeat would stop fencing on it) but
`flint-sync` does not distinguish it: the lean crate never matches
`StoreError::Auth`/`LeanError::Auth`; the heartbeat and floor arms
special-case only `Fenced` and retry every other error as "renew failed
(retrying)" (`sentinel.rs:1064-1088`; `flint_sync.rs:498-501`), so
publishes and renewals pause and local files keep serving. The cost is
lease liveness: after `LEASE_DEAD_SECS` = 120 s without a renewal the
operator's DR signature fires (`reconcile.rs:392-431`) and a replacement
pod would take over after 6 quiet polls (`lease.rs:66,80-86`), deposing
the live syncer at its next successful renew (`lease.rs:164-167`);
`lease.rs:64-66` self-recognition runs only in `claim_step` — a running
sidecar never re-claims. **Design gap, recorded 2026-09-02**: the
outage tolerance is therefore the remaining key lifetime alone; either
the syncer must be taught to surface `Auth` distinctly and the
operator's DR signature must not depose an `Auth`-paused syncer, or a
broker outage that outlives the keys by ~120 s + 6 polls costs a
takeover of a live writer. The SA token itself is refreshed by
kubelet from its own cache, independent of Knox (`token_manager.go:39-43`).
What the operator sees: broker metric
`flint_s3_broker_exchange_total{result="knox_unreachable"}` climbing;
`CredentialRefreshFailed` events on affected tenant pods;
`FlintLeanWorkspace` `LAG` columns growing from the lease echo
(`reconcile.rs:316-378`).

### 6.4 Slow checkout vs kubelet backoff

A 1M-file / 10 GiB lean checkout (with `maxFiles` raised above the
250,000 default, `crd.rs:258-260` — otherwise it is refused before the
first byte, `checkout.rs:92-98`): the derived budget
`(110 + 15 s/GiB + files/500) × 1.5` (`inject.rs:42-56`) is
`(110 + 150 + 2000) × 1.5 = 3390 s` ≈ 56 min, which spans ~14
`NodePublishVolume` attempts under kubelet's 2-min `csiTimeout`
(`csi_plugin.go:56`) and backoff to 2m2s. Harmless: the
syncer persists across attempts, the plugin's 100 s in-call wait keeps
headroom under the deadline (the `node_volume_locks` rule,
`node_volume_locks.rs:35-43`), and the pod sits in `ContainerCreating`
with a `FailedMount` event that reads like a progress bar. **A
node-plugin restart mid-checkout does not restart the checkout** (the
syncer is a separate pod; the next retry waits on the marker as before).
Note the CSI docs' warning that "there is no guarantee that
NodePublishVolume will be called again after a failure" ([web-k8s-csi-mechanics
§1.3]) — the syncer must therefore never depend on a later call to finish
work: it finishes on its own and the marker is the durable signal.

### 6.5 Two projects, one node

Two `NodePublishVolume` calls with two volume ids; two target paths under
two pod UIDs; two workers with their own uid, cgroup, netns, emptyDir
cache, token file and STS keys; two CR lookups in two namespaces; the
plugin's per-volume state dirs are `0700 root`. Nothing is shared (§3.6).
The existing legs A2b/A7 (prefix scoping; two workspaces in one bucket,
`run-agent.sh:361-379`) are the oracle and run unchanged.

### 6.6 Pod force-delete

`kubectl delete --grace-period=0 --force` removes the API object
immediately; kubelet still runs `NodeUnpublishVolume` locally after the
containers are gone ([web-k8s-csi-mechanics §5]), so the lean drain still
runs — but it succeeds only while the syncer's cached STS keys are
valid, because the pod object is gone and `TokenReview` refuses every
refresh (§5, final-barrier row: longer-lived lean keys and a
SIGTERM-time refresh are the mitigations); today it drains nothing. For passthrough the
teardown ladder runs as usual. If kubelet itself restarts mid-way,
"Orphaned pod found, but volumes are not cleaned up" is retried every
housekeeping period (`kubelet_volumes.go:194-201`).

### 6.7 Others, briefly

- **Driver not registered on a node** (autoscaled node, DS not yet
  scheduled): the pod stays `ContainerCreating` — fail-closed, retried
  forever; a Job with `restartPolicy: Never` waits, it does not fail
  permanently. Istio's untaint pattern (`<driver>/not-ready` NoSchedule
  until the DS registers, [web-istio-precedent §5.1]) is an optional
  autoscaler nicety, not a requirement.
- **Worker eviction / OOM**: workers carry the chart's requests and a
  priority class above BestEffort tenants; a syncer evicted anyway leaves
  the tree intact (plugin-owned) and is recreated on the next republish
  (self-recognition path; a 60-90 s gap — an in-container crash, by
  contrast, restarts in place under `OnFailure`, §6.1); a mounter evicted is a dead mount (§6.1).
- **`DiskPressure` eviction of the DaemonSet pod** is the new hazard
  the existing DS solves with `system-node-critical` (`node.yaml:57-61`);
  give the new DS a class above the workloads it serves.
- **Broker compromise**: bounded by the admin-owned `(namespace, SA) →
  principal` binding table (§4.2 step 2) — not by the CR, which is
  tenant-writable — by `hadoop.proxyuser.<broker>.users` listing only
  served principals and `.hosts` the broker's egress address (K2), by
  15-minute keys, and by the proxy's project tenancy
  (`flint-lean-plan.md:438-441`).

---

## 7. Decision table

| Alternative | What it would give | Why it won or lost |
|---|---|---|
| **Keep the webhook; add an SA check and a VAP** | smallest change | the webhook stays the security boundary and still injects a privileged container with a tenant-chosen image (T3, T6); PSA unchanged; T4/T5 remain. **Lost.** |
| **Thin unprivileged webhook adding only the `csi:` volume** (ergonomics: restores the one-label opt-in) **[graft: minimal, argued both ways]** | one-line opt-in back; injects zero privilege and zero credentials | *For*: a bypassed or compromised thin webhook buys nothing the pod author could not get by writing the volume; Istio kept exactly this after CNI; its failure mode is availability, not confidentiality. *Against*: it is still "the webhook mechanism" — an MWC with `failurePolicy` on the pod-create critical path, a self-signed cert Secret, the label→CR spoof surface unchanged, and it can never be the boundary. **Decision: not in v1**; if ever shipped, off by default, documented as a convenience; the driver's authorization does not change. `inject.rs`'s volume-add and collision-refusal functions are reusable for it. |
| **GCS shape: keep the webhook, inject an unprivileged fd-receiving sidecar** | privilege out of the tenant pod, ergonomics intact | keeps the MWC, cert Secret and a credential-holding process in the tenant pod; inherits GCS's documented sidecar-lifecycle bugs (other webhooks stripping `restartPolicy`, ordering, termination — [web-fuse-csi-priorart §3]). **Lost**; retained only as the lean L1 bridge (§10.1). |
| **PVC (static PV) per project** | admin-authored policy; Mountpoint CSI's only mode | kubelet delivers pod info and tokens for PV-backed volumes too, so identity is not the difference ([web-k8s-csi-mechanics §8]); which PVC a pod references is still the pod author's choice — the label's trust level; a PV per project per namespace (3000 at the fleet scale on record); no per-pod parameters. **Lost for v1**; kept as a later admin-only mode for mounter *sharing*. |
| **Generic ephemeral volumes** (`volumeClaimTemplate`) | quota via PVC | `CreateVolume` cannot see the pod; StorageClass per project; heavier for nothing ([web-k8s-csi-mechanics §8]). **Lost.** |
| **SA → project by naming convention only** (the FUSE doc's namespace+SA prefix, `flint-fuse-architecture.html:376`) | no CR at all | nowhere for budgets, boundary knobs or per-project endpoint. **Lost as the first mapping**; it is the broker's *second* mapping. |
| **bucket/prefix/endpoint in `volumeAttributes`** | no CR lookup | the pod author becomes the policy author; KEP-596's warning ([web-k8s-csi-mechanics §1.4]). **Lost.** |
| **Mounter / syncer as a child process of the node plugin** | simplest code (~700 lines) | every DS roll strands every FUSE mount and SIGKILLs every syncer on the node; N tenants in one cgroup (`fetchInflightMb` × N); a node-root writer over tenant-writable trees turns the symlink-plant hazard into a node-level write primitive (`code-lean §6.5`). **Lost**; kept as milestone M1 on kind (semantics proof) and as the fallback where the driver may not create pods **[graft: minimal]**. |
| **Host `systemd-run` transient scopes** (Mountpoint v1, blob-csi proxy, yandex) | survives plugin restarts | root on the host, host-installed binaries, `hostPID`, SELinux/OpenShift breakage; needs a host-namespace exec the repo does not have (`code-csi §3`); abandoned by AWS in v2 ([web-fuse-csi-priorart §2]). **Lost.** |
| **Per-node unprivileged "mounter host" DS** | +1 pod per node, survives plugin restarts | all tenants' daemons in one container — no per-tenant cgroup/logs/netpol; one OOM kills all; its own upgrade is all-or-nothing. **Lost**; fallback only if E7 (pod-count capacity) fails. |
| **Per-volume worker pods with fd passing (AWS v2) — CHOSEN** | plugin-restart immunity, per-tenant kernel boundary, legible operations, no injection | cost: pod count (≤ 2× pods per node) and one worker-creation hop on the start path (the plugin creates the pod with `nodeName` set, skipping the scheduler). **Won.** |
| **Privileged helper pod doing its own mount** (reuse the mounter image byte-for-byte) | ~180 fewer lines | multiplies privileged pods instead of concentrating them; a compromise of mount-s3 (parses untrusted S3 responses) is root + `SYS_ADMIN` + a hostPath into kubelet's pod dirs. **Lost.** |
| **Mountpoint-pod sharing** (`MountpointS3PodAttachment`) | fewer pods | sharing requires equal SA/role/options; per-pod identity makes the set empty; a CRD + controller for nothing. **Lost for v1**; revisit with PV-backed project mounts. |
| **A central controller creates worker pods** (node SA has no `pods create`) **[graft: security]** | closes the node→other-node pod-create surface | one more Deployment and two watch hops on the start path, plus a race kubelet's retry must cover. **Not chosen for v1** (the namespaced Role + VAP + tokenless workers bound it); it is the hardening variant adopted if the customer's review demands it — a contained change. |
| **Extend `flint.csi.storage.io`** | one DS, in-process reuse | `attachRequired`/`tokenRequests` conflict on one CSIDriver object; hardcoded names; `hostNetwork` + spdk-tgt + `OnDelete` roller coupling; suppressed under lite (`code-csi §5.2`). **Lost.** |
| **Two CSIDriver names** (passthrough, lean) | separation | two registrars, two sockets, two DaemonSets for no security gain; one driver, two selectors. **Lost.** |
| **Deploy AWS mountpoint-s3-csi-driver unmodified** | the real thing | Persistent-only, identity only via IRSA/EKS Pod Identity, no CR, no Knox, nothing for lean ([web-fuse-csi-priorart §2]). **Lost**; the cheapest partial answer for passthrough with driver-level static keys. |
| **Knox JWT in a Secret / projected into the pod** | Knox-native | a credential in the tenant pod — the posture the plans abolish (`flint-lean-plan.md:66-73`); mount-s3 cannot present it anyway. **Lost.** |
| **K8s-SA-to-Knox exchange at the broker (K1/K2) — CHOSEN for v1** | pod-bound identity → project principal | K1 needs the `typ` question settled; K2 needs a broker Knox identity (the only standing credential). **Won**, with the endgame reducing the broker's credential to a client certificate Knox pins (§4.3 row K1, §4.7). |
| **Proxy trusts K8s SA JWTs directly (K0)** | no broker | mechanisms verified for MinIO/RGW/AWS; the customer's proxy is unknown. **Preferred if available** — it is the STS façade's own wire shape — but only with a per-SA `sub` condition at the STS (§4.3 row K0); `consumers` is not consulted in K0. |
| **Node plugin holds a `doAs` Knox service credential (no broker)** | one hop fewer | every node can impersonate every served project — the un-partitioning regression on record (`multicluster-frontdoor-review.md:50-51`); "must never hold the token root" (`file-api-fleet-auth.md:92-97`); `doAs` on 2.1.0 unverified. **Lost.** |
| **Node plugin calls Knox directly with the pod's SA token (no broker)** | no broker | the endgame's Knox capabilities are unverified; `TokenReview` + `consumers` need a home. **Lost for v1**; the endgame keeps the broker credential-less. |
| **Bearer to the proxy on S3 requests (P-BEARER)** | no exchange | mount-s3 and flint-store cannot emit a bearer ([web-knox-jwt §6.1]); a bearer-injecting shim per mount (`--no-sign-request` toward the shim) costs a hop per byte and a Knox JWT resident on the node. **Refused unless the proxy team forces it**, and then inside the worker pod, renewed by the broker **[graft: security]**. |
| **Knox as OIDC issuer → STS at MinIO/RGW/AWS (P-STS) in v1** | zero broker code | Knox 2.1.0 has no discovery doc (KNOX-3141 Open), `iss=KNOXSSO`; AWS needs `iss` == provider URL; RGW wants `x5c` ([web-knox-jwt §5]). **Endgame, not v1.** |
| **Loopback container-credentials door vs web-identity delivery** | door: no TLS trust needed, verified for both clients; web-identity: node plugin holds no S3 keys, zero exchange code in the plugin | **Web-identity is primary** (smaller concentration, the AWS v2 shape); **the door is the fallback arm** (P-CRED-only proxies, TLS-trust trouble). Both ship behind `CredentialSource`. |
| **`credential_process` / shared-credentials file** | fewer moving parts | `credential_process` REPORTED historically flaky in Mountpoint (#389, #927); whether the profile provider re-reads a rewritten file is unverified. **Lost.** |
| **In-plugin JWT verification (`jsonwebtoken`)** | offline, fast | `TokenReview` gives bound-claim freshness offline verification cannot ([web-k8s-csi-mechanics §7]); zero new crypto deps. **Lost for v1.** |
| **Lean: keep the sidecar permanently** | smaller change | a credential in the tenant pod; the webhook stays on the critical path. **Lost**; the L1 bridge only. |
| **Lean: key the node directory on CR name** | stable paths | a replacement pod would self-recognise a dead pod's lease and skip takeover rotation (`lease.rs:72-80`). **Refused.** |
| **Lean: emptyDir tree bound outward** | workers `restricted` | the tree dies with the worker, not the volume; a recreated worker loses its lease identity. **Lost**; the E3 variant (plugin-owned tree bound *into* a running worker's emptyDir) is the experiment that would let `flint-workers` drop to `restricted`. |
| **Lean: plain directory + `du` poll** | no loop devices | no hard limit; `ENOSPC` semantics lost. **Kept as an opt-out.** |
| **Lean final drain via the tenant pod's `preStop`** | familiar | there is no sidecar to receive it; the tree is not quiescent; force-delete skips it. **Lost.** |
| **CSI for passthrough only; lean keeps its webhook** | half the work | valid as a *phase*, not an end state: lean's webhook has the same spoofability, root sidecar and in-namespace credential (T1, T2, T8). |
| **Untaint / repair controllers (Istio)** | autoscaler hygiene | not needed — kubelet blocks the pod on `NodePublishVolume` (fail-closed). Optional nicety. |

---

## 8. What this does NOT solve (stated honestly)

- **FUSE death is still fatal to open files.** A worker's own crash
  strands its tenant on `ENOTCONN` exactly as today; the design changes
  *who else* is affected (nobody) and *when it happens* (never on a
  plugin roll), not the fact ([web-k8s-csi-mechanics §6.2]). The
  readiness flip is replaced by an event and a documented probe (§6.1).
- **The namespace is the project boundary.** A pod author who may create
  pods in namespace A may set `serviceAccountName` to any SA in A,
  including a listed consumer; Kubernetes does not bind "may create pods"
  to "may use SA X" without an admission policy. Two projects sharing a
  namespace are unsafe under any delivery mechanism — exactly as the lean
  plan's "dedicated bucket or prefix per project" and "the CR lives in
  the pod's namespace" already assume. An optional VAP restricting
  `serviceAccountName` for pods declaring `s3.flint.io` volumes is a
  cheap tightening, admission-side and therefore not a substitute for
  the broker check **[graft: security §3.4]**.
- **The exchange step is not optional and its back half is unknown.**
  Whatever turns a Knox principal into SigV4 keys is the customer's
  proxy, an STS-shaped endpoint, or Cloudera IDBroker; Apache Knox ships
  none of them ([web-knox-jwt §0]). This document cannot pick until §9
  item 1 is answered.
- **Knox revocation is invisible to offline verifiers**, so a revoked
  project principal is refused only at the next mint (≤ 15 min). Pod
  deletion is seen by the broker via `TokenReview` from
  `deletionTimestamp + 60 s` (delete-time + grace + 60 s; immediately
  under `--force`); keys still live to `Expiration`. Prompt revocation costs broker load.
- **P5 at the proxy is neither built nor blocked here.** Straggler
  containment (a SIGSTOPped deposed sidecar landed 7,591 PUTs after
  rotation, `flint-lean-chaos-drill.md:131-138`) still requires the
  proxy-side epoch check that is DECIDED and unbuilt (`code-auth §3`).
- **Within-project residuals of lean** (torn uploads under a forced
  barrier, mtime-granularity scan evasion, empty directories not
  round-tripping — `flint-lean-plan.md:304-322`) are untouched: the
  writer is the same binary.
- **Admission-time UX.** A typo in `flint.io/mount` is a pod stuck in
  `ContainerCreating` with an event, not a 4xx at `kubectl apply`.
- **The one-line opt-in** is gone unless the thin webhook (§7) is
  shipped, and it will not be in v1.
- **Proxy conformance under exchanged credentials** (conditional
  headers, versioning surface, `x-amz-meta-flint-*` round-trip — the gate
  at `flint-lean-plan.md:236-245`; `crates/flint-store/src/probe.rs:1-18`)
  must be re-proven with broker-issued keys; short-lived keys do not
  change the S3 dialect but a Knox-fronted proxy may.
- **Cost**: up to one extra pod per tenant pod on the node, and one
  pod-creation hop on the start path. E7 measures it (§9).

---

## 9. Preconditions and the verification experiments, ranked

### 9.1 Preconditions (facts that must hold before code)

- **P1 — the proxy's wire contract is known** (§9.2 item 1). Without it
  the broker's back half cannot be built and the `static` arm is the
  only one that ships.
- **P2 — Kubernetes ≥ 1.25** (inline volumes GA), and the fix for
  kubernetes#121271 present (kubernetes#139045 "fix(csi): preserve mount
  dir when NodePublish fails on a remount", merged 2026-05-21: v1.37.0;
  backported to 1.36.x via #139228, merged 2026-06-08; 1.35.x/1.34.x
  cherry-picks #139229/#139230 — confirm the patch level of the target
  cluster; master `csi_mounter.go:316-324` carries the
  `!mounterArgs.IsRemount` guard); `serviceAccountTokenInSecrets` is optional (1.35
  beta / 1.36 GA) ([web-k8s-csi-mechanics §1, §3.1, §3.5]).
- **P3 — mount-s3 ≥ 1.16.0** for `AWS_ENDPOINT_URL_STS` (the pinned
  1.24.0 satisfies it, `Dockerfile.passthrough:17-41`; [web-knox-jwt §6.2]).
- **P4 — a decision on record** that short-lived, pod-bound, proxy-scoped
  credentials transiting one worker per pod and no key at all in the
  node plugin answers `multicluster-frontdoor-review.md:50-51`.
- **P5 — the `consumers` default-deny posture is accepted** by the
  migration owner, with `["*"]` as the explicit opt-in.
- **P6 — the lean proxy conformance probes pass with broker-issued keys**
  on every replica (`flint-lean-plan.md:236-245`; `probe.rs`).

### 9.2 What the recon could not establish, ranked by design impact

| # | Unknown | Settles | Experiment or document |
|---|---|---|---|
| 1 | **What the customer's project-scoped proxy accepts on the wire** — SigV4 with keys it issues from a JWT (STS-shaped, or a credentials endpoint), or only `Authorization: Bearer` on the data path; and whether it verifies Knox JWTs via `jwks.json` or a pasted PEM | P-CRED vs P-STS vs P-BEARER; whether any exchange code is needed | one hour with the proxy team + a `curl` matrix: (a) Knox bearer on `GET /bucket/key`, (b) discover `AssumeRoleWithWebIdentity` or a `/credentials` endpoint, (c) SigV4 with issued keys; (d) present a Knox JWT for the project user minted with a DIFFERENT `aud` (or none) to the credentials endpoint — it must be refused: Knox's own verifiers accept any `aud` when `knox.token.audiences` is unset (v2.1.0 `AbstractJWTFilter.validateAudiences`), and a proxy that inherited that default accepts every Knox SSO token of that user (a human's KnoxSSO cookie, an API key), so the broker would not be the only minter; record the four responses |
| 2 | **Which Knox** — Apache 2.1.0, or a Cloudera 7.x build with IDBroker (`aws-cab`, `api/v2`); is the TSS on | K1/K2 vs K3 (the design collapses to one call) | `GET /gateway/admin/api/v1/version`; list topologies; `getTssStatus` |
| 3 | **Does Knox 2.1.0 verify a `typ`-less K8s projected token** when `knox.token.allowed.jws.types` is configured | K1 viability; the credential-less endgame | 10-minute test: a topology with `knox.token.jwks.urls=<cluster>/openid/v1/jwks`, `jwt.expected.issuer`, present a projected token to `knoxtoken/api/v1/token`; KNOX-3434 predicts failure |
| 4 | **Does `doAs` work for a service identity with `server-managed=false`**, and which provider fronts `knoxtoken` | K2 viability; how the broker authenticates to Knox | read the topology's `hadoop.proxyuser.*`; one `doAs` call from a test identity |
| 5 | **E3 — bind into a running worker's emptyDir** after container start is visible inside with `HostToContainer` | whether `flint-workers` can drop from `privileged`-labelled to `restricted` (the hostPath delivery is the v1 answer either way) | kind, 20 min |
| 6 | **E6 — the CRT's TLS trust for the broker's STS endpoint** (in-cluster CA; does the mounter image's `ca-certificates` bundle plus a mounted CA suffice, or is the loopback door required) | primary vs fallback credential delivery | build the worker on kind against a broker with a cert-manager CA |
| 7 | **E5 — `AWS_ENDPOINT_URL_STS` reaches the Rust SDK's web-identity STS client** (code-read: `web_identity_token.rs:244` → `client_config()` → `EnvServiceConfig`, `env_service_config.rs:12-30`; the exact service-id suffix scheme lives in aws-runtime) | zero-code credential delivery for `flint-sync` | a unit test in `flint-store` against a fake STS on loopback |
| 8 | **E2 — propagation self-heal**: does a fresh mount over a dead FUSE target on the host reach a running container whose csi volumeMount is `HostToContainer` | whether the lost readiness flip becomes a self-heal win | kind, 30 min: mount, kill mounter, `umount -l`, remount, re-bind, `ls` from the running container |
| 9 | **E4 — lean under retried `NodePublishVolume`**: a 10 GiB checkout across 2-min attempts with backoff to 2m2s; syncer survival; the deadline arithmetic; drain under `--grace-period=0 --force` | the start-gate and drain designs (§5) | Lima/kind measurement; compare against the derived budget |
| 10 | **E7 — pod-count capacity**: `max-pods` vs up to 2× pods per node; worker-creation latency at 100 pods/node churn | whether the per-node "mounter host" must be the fallback | load test on a kind node with `max-pods` lowered; p50/p99 `NodePublish` |
| 11 | **Does `flint-sync`'s scanner follow symlinks out of the tree** (the read-side twin of `safefs`) | not a new hazard, but unverified today and under CSI | read `barrier.rs` scan + `checkout.rs` adopt paths for `lstat`/`O_NOFOLLOW`; add a chaos leg **[graft: minimal]** |
| 12 | `--service-account-max-token-expiration` on the target cluster; CSIDriver field mutability (moot for a fresh object); the target cluster's patch level against the kubernetes#121271 fix (P2) | minimum cluster version; rotation cadence | release notes; API server flags |
| 13 | **uid/gid**: `VOLUME_MOUNT_GROUP` + `fsGroupPolicy: None` interaction; whether tenants relying on `runAsUser` defaulting today accept the CR/attribute rule | §3.6 | a 20-minute test with a pod setting `fsGroup` and no CR uid |
| 14 | Cluster-autoscaler / Karpenter treatment of bare worker pods with `safe-to-evict: true` + PDB + Node ownerReference | scale-down behaviour | CA docs + a scale-down test on a runb* cluster |
| 15 | Is P5-at-the-proxy built before or alongside | straggler-containment claims in the drill | a sequencing decision; the B12 zero-PUT gate is already specified |
| 16 | Does a Knox JWT with `knox.groups` exceed MinIO's 2048-char `WebIdentityToken` limit; does Knox's JWKS emit `x5c` | only the P-STS variants | measure one token; `curl jwks.json` |
| 17 | Whether the customer's security review accepts the node SA creating pods in `flint-workers` (namespaced Role + VAP) or requires the central controller | one more Deployment on the start path | a review meeting; the controller variant is contained |
| 18 | **Lean uid mismatch** — app `runAsUser` ≠ CR `uid`: files created `0600` by the app must be published or the publish must fail loudly (a syncer at uid U cannot read `0600` files of uid V and the scan would silently skip them); today's root sidecar over a `0777` emptyDir (`Dockerfile.sync.prebuilt:47-48`; `empty_dir.go` perm 0777) makes this moot | §3.5 step 6's uid rule; the L1 bridge's `securityContext` | kind, 20 min: a pod at uid 1001 over a CR with `uid: 1002`, one `0600` file, `.flint/publish` |
| 19 | Does mount-s3's fd mode accept `--allow-other` when the driver already set `allow_other` in the `mount(2)` data (`mounter_args` always pushes it, `inject.rs:255-259`) | §3.4 step 9's argv | one mount on kind; drop the flag if refused |

### 9.3 Milestones, each with its falsifier **[graft: minimal]**

- **M0 — settle unknowns 1-4** (documents + four `curl`s; no code).
- **M1 — lean, child-process shape, kind** (~600 lines): proves the
  `NodePublish`/`NodeUnpublish` semantics, the start gate and the drain
  before any worker-pod plumbing. Falsifier: a pod with the old label and
  no volume must start *without* a workspace.
- **M2 — worker pods + passthrough fd passing + `static` credentials**:
  proves legs S1-S8, S14-S19. Falsifier: S9's control (worker stopped ⇒
  consumer strands) — if it passes with the worker alive, the leg is
  vacuous.
- **M3 — the broker (STS façade) + web-identity delivery + rotation**
  against a stub broker that `TokenReview`s and vends 2-minute MinIO STS
  keys: proves S6-S8, S10. Falsifier: two pods must present two
  *different* pod-bound JWTs; a broker that logs one JWT twice fails.
- **M4 — the Knox step** (only if M0 says the proxy wants a Knox-issued
  subject): K1 or K2 against a real Knox; the `typ` test is the gate.
- **M5 — lean on worker pods** (the design of record for lean): S11-S13,
  S17-S18.
- **M6 — retire the webhooks** one minor after M2/M5 ship; delete two
  minors later.

### 9.4 Reusable code inventory (from `code-csi §6`, verified) and new code, by module **[graft: minimal]**

Reused verbatim: `spdk_csi_driver::csi` (`lib.rs:210-215`); the
Unix-socket serving loop (`main.rs:552-590`); the Identity service
template (`main.rs:606-655`, minus `CONTROLLER_SERVICE`);
`mount_util::{bounded_mount, bounded_umount, bounded_sync,
mountpoint_probe_says_unmount, probe_staging_liveness}`
(`mount_util.rs:20-271`); `mount_opts::merge` (`mount_opts.rs:24-165`);
`node_volume_locks` (`node_volume_locks.rs:31-82`, keyed on `volume_id`);
the ephemeral-marker pattern (`main.rs:5069-5079,5321-5324`); the
unpublish ladder (`main.rs:5104-5290`); `passthrough::spec::MountSpec::validate`
and `inject::mounter_args` (`spec.rs:39-165`; `inject.rs:251-299`);
`lean_operator::inject`'s env builder and `boundary::derived_grace_secs`;
`Dockerfile.passthrough`; the `nfs-only` node-mode template
(`node.yaml:75-163,650-683`); the e2e house rules and rigs.

| New module (hub crate `spdk-csi-driver/src/s3csi/`) | ~lines | What |
|---|---|---|
| `bin/flint_s3_csi_node.rs` | 120 | env → socket loop → Identity + Node services; node-only |
| `s3csi/node.rs` | 400 | `NodePublish`/`NodeUnpublish`/`NodeGetCapabilities` (`VOLUME_MOUNT_GROUP`, `GET_VOLUME_STATS`; `GET_VOLUME_HEALTH` optional — `VOLUME_CONDITION` is removed from the spec, §6.1); dispatch on selector; `state.json`; idempotency; teardown reuse; `statfs`-based stats |
| `s3csi/resolve.rs` | 150 | CR GET in `pod.namespace` via an informer (kube 3.0.0 is a dep), `consumers`, attribute allow-list, `validate` reuse |
| `s3csi/fuse.rs` | 180 | `open("/dev/fuse")`, `nix::mount::mount` with `fd=`, `sendmsg` + `ScmRights` (nix 0.27 is a dep, `spdk-csi-driver/Cargo.toml:173`, with features `fs`, `user` only — add the `mount` and `socket` features, which `nix::mount` and `ControlMessage::ScmRights` need), bind, `umount2` |
| `s3csi/worker.rs` | 260 | build/create/adopt/wait/delete worker pods; comm socket path under `/var/lib/kubelet/pods/<uid>/volumes/kubernetes.io~empty-dir/comm/`; startup reconcile; the lean spec reuses the env builder and grace arithmetic |
| `s3csi/creds.rs` | 300 | `CredentialSource` trait: `webIdentity` (token-file writer), `door` (per-volume cache + exchange + `creds.json`), `static` (`nodePublishSecretRef` → profile), `ambient`; the publish registration + per-volume nonce (`POST /v1/volumes`, §4.2) |
| `s3csi/tokens.rs` | 60 | parse `serviceAccount.tokens` from `volume_context` or `secrets`; expiry tracking; never logged |
| `bin/flint_s3_worker.rs` | 200 | PID 1 of every worker: passthrough — `recvmsg` fd, `exec mount-s3 … /dev/fd/3`; lean — spawn `flint-sync run`, forward SIGTERM, propagate exit; optional loopback creds door |
| `bin/flint_s3_broker.rs` + `s3csi/broker/` | 700 | STS façade (XML), `TokenReview`, CR entitlement, Knox step (K1/K2/K3), proxy step (P-CRED/P-STS), audit, metrics |
| unit tests | 500 | attribute allow-list; record round-trip; creds expiry; worker-spec parity with `inject.rs` (the "every knob stamped" test re-homed); broker refusal matrix |
| `lean/sidecar`: `flint-sync drain` | 80 | the syncer-already-gone path (§3.5) |

Changed: `passthrough/spec.rs` and `lean_operator/crd.rs` (+~40 each:
`consumers`, `identity`, `uid/gid` for lean, tombstone `image`) and both
CRDs; both `webhook.rs` (+~10: refuse label + volume during coexistence).
**`flint-store`: zero changes.** New non-Rust: `flint-s3-csi-chart/`
(CSIDriver, DaemonSet, `flint-workers` namespace + VAP + PDB, broker
Deployment/Service/RBAC, PodMonitor, NetworkPolicy; ~350 lines);
`docker/Dockerfile.s3csi.prebuilt` (ubuntu + `util-linux` + ca-certs +
two binaries, the `Dockerfile.csi.prebuilt:23-58` recipe);
`docker/Dockerfile.s3worker` (`FROM flint-passthrough-mounter:<ver>` +
`COPY flint-s3-worker`, keeping the mount-s3 pin) and — the lean worker
image, named once here and used by §3.1/§3.5 —
`docker/Dockerfile.s3worker-lean` (`FROM flint-sync:<ver>` + `COPY
flint-s3-worker`): `Dockerfile.sync.prebuilt` is alpine + `flint-sync`
only and `stage-prebuilt.sh:72` `LEAN_BINS` is `flint-sync
flint-lean-gateway`, so `flint-s3-worker` is staged under the `s3csi`
scope and copied in; the alternative (flint-sync itself as PID 1) leaves
nobody to forward SIGTERM or serve the loopback creds door; a third scope `s3csi`
in `stage-prebuilt.sh`/`publish-images.sh`/`release.sh` alongside `lean`
and `passthrough` (`code-csi §5.1`); `s3csi/e2e/`. Deleted at end state:
`passthrough/webhook.rs`, the sidecar arm of `passthrough/inject.rs`,
`bin/flint_passthrough_operator.rs`, `lean_operator/webhook.rs`, the
sidecar arm of `lean_operator/inject.rs`, `webhook_certs.rs`, both charts'
MWC/cert RBAC.

---

## 10. Migration, coexistence, and the e2e drill

### 10.1 Side by side

Both mechanisms ship together. The label opt-in and the `csi:` volume
are orthogonal; two guards make an overlap loud instead of doubled: the
still-installed webhooks refuse a labelled pod that also declares an
`s3.flint.io` volume, naming both; and the node plugin refuses to publish
for a pod whose spec already carries the `flint-passthrough`/`flint-sync`
sidecar (GCS's cross-check inverted — GCS refuses pods the webhook
*missed*, [web-fuse-csi-priorart §3]; we refuse pods it *hit*). Chart
value `delivery: webhook | csi | both`. Under `csi` the passthrough chart
ships the CRD only; the lean chart runs the reconciler with the
MWC/cert/9443 path off (an env flag on `flint-lean-operator`). Per-
namespace migration: install `flint-s3-csi-chart`; set `consumers:
{serviceAccounts: ["*"]}` on existing CRs; flip workloads from label to
volume as they roll; tighten `consumers`; switch the namespace to
`restricted`; set `delivery: csi`; delete the MWC and cert Secret.

**Lean L1 bridge (optional, only if lean must wait for the worker
plumbing) [graft: security]**: keep the lean webhook and sidecar but give
the sidecar a real `securityContext` (uid = the CR `uid`, never 65534 —
a syncer at a uid other than the app's cannot read the app's `0600`
files and silently skips them, exactly the case
`Dockerfile.sync.prebuilt:47-48` calls unsafe, §9.2 item 18; `drop:
[ALL]`, limits — the Dockerfile's "chart sets the security context"
claim is false today, `:47-50`)
and replace `envFrom` the tenant Secret with the sidecar exchanging its
own pod's projected SA token at the broker (`AWS_WEB_IDENTITY_TOKEN_FILE`
= a projected volume, `AWS_ENDPOINT_URL_STS` = the broker). The webhook
can still be spoofed into injecting a sidecar, but the sidecar cannot
obtain a credential the pod's SA is not entitled to — the webhook stops
being the security boundary before it is removed. A bridge, not the
design of record: a credential-holding process still lives in the
tenant pod.

### 10.2 The e2e drill — every leg has a failing control

House rules inherited from the passthrough drill: every leg proves its
precondition or fails; every refusal has an accepted control; readers run
as uid 1001; every read asserts content; a same-bucket-other-prefix
fixture falsifies the content legs; roster reconciliation fails the run
if a leg never ran (`run-passthrough.sh:12-21,445-451`). Rig: kind +
MinIO (`passthrough/e2e/rig.yaml`; `lean/e2e/minio.yaml`) + a stub
`flint-s3-broker` in `static` backend mode that `TokenReview`s, issues
2-minute keys from rig secrets and records every issuance, and a second in
`deny` mode; Knox legs run only against a real Knox.

| Leg | Asserts | Failing control |
|---|---|---|
| S1 | `CSIDriver s3.flint.io` registered on every node; DS Ready; **zero** flint `MutatingWebhookConfiguration`s in `csi` mode | scale the DS to 0 ⇒ a new tenant pod stays `ContainerCreating` with a `FailedMount` event (fail-closed) |
| S2 | a pod in a namespace enforcing `restricted` with the `csi:` volume is admitted and Running; app (uid 1001) reads `shard-05.txt = seeded-object-05`, 11 entries, fstype `fuse*`, `elsewhere/` invisible (A2) | the same pod with the old label is rejected in that namespace naming `privileged` (A11 inverted); a second pod on prefix `elsewhere` sees exactly one file (A2b) |
| S3 | zero `AWS_*`, zero `FLINT_SYNC_*`, no token files in the app container; no `envFrom`/Secret volumes in the pod spec; the worker's `comm/token` exists, is 0600, and is where the keys are (A4 anti-vacuity: the counter must find them somewhere) | — |
| S4 | `flint.io/mount` naming a missing CR ⇒ `FailedMount` names the CR and the namespace; pod never starts | the same pod with a real CR mounts |
| S5 | a CR present only in another namespace ⇒ refused naming the namespace; `volumeAttributes.bucket: other` refused naming `bucket`; `readOnly: true` over a RW CR yields a read-only *presentation* (write fails at the mount, nothing lands from it — the worker's keys are still RW, §2.4 T2) | the same CR name created in the pod's namespace ⇒ mounts; `readOnly: false` over a RO CR is still read-only (no widening); RW over RW: bytes land and unlink removes them (A6/A7) |
| S6 | a pod under SA `bob` (not in `consumers`) stays `ContainerCreating`, event names `bob` and `spec.consumers.serviceAccounts`; the CR **exists** | the identical pod under SA `alice` mounts and reads content |
| S7 | a captured SA token replayed to the broker after its pod is deleted is refused within 90 s (`TokenReview` `authenticated: false`); a token from worker A presented for project B is refused **[graft: security]** | the same token replayed while the pod lives, for its own project, is accepted |
| S8 | rotation: 2-minute keys; a continuous reader sees ≥ 3 issuances carrying **different** pod-bound JWTs for two pods, zero I/O errors; the SA JWT has `aud = s3.flint.io` and is rejected by the API server's TokenReview for its own audience | broker blocked at t=0 ⇒ the reader fails between 2 and 3 minutes with an auth error, not a hang, and the pod's events say so |
| S9 | DS rolled mid-`cat` of a 1 GiB object ⇒ checksum matches, zero `FailedMount`, worker pods unchanged | `crictl stop` the **worker** ⇒ reader gets `ENOTCONN` within 10 s; a plugin-authored `Warning` Event on the tenant pod within one republish — not kubelet's, whose volume-health reporting is gated off by default (§6.1) (A12 shape) |
| S10 | revocation: SA removed from `consumers` ⇒ reader fails within one key lifetime + slack | untouched sibling pod keeps reading |
| S11 | lean: a pod created after the seeder is gone finds all 200 files with correct bytes before its first instruction, **and its init container sees them too** (a NEW leg — lean A2, `run-agent.sh:187-200`, asserts nothing about init containers, and today's appended sidecar never gates them, §2.2) | a workspace with a forced-wedged checkout (proxy first-byte stall) stays `ContainerCreating`, the marker is never written, the event names the budget (C12) |
| S12 | lean: `.flint/publish` acked `ok` within 90 s, manifest seq advances (A5); gateway boundary request honoured (B34); protocol legs B1-B25/C1-C12 run against the worker pod unchanged, with every `kubectl exec -c flint-sync` step (`run-boundary.sh:214-216`; `run-verbs.sh:147-161`) re-targeted to the worker pod in `flint-workers` (§3.2) | — (they are the oracle) |
| S13 | lean drain: `kubectl delete pod` ⇒ bucket carries a `source=drain` boundary, owed ack settled, lease released, the Pod object disappears only after (B11a); **`--grace-period=0 --force` also drains — with the delete timed so the drain starts after the worker's keys would have needed a refresh** (proves the long-key rule of §5, not just the happy case) | SIGKILL the syncer then delete ⇒ `orphans.json`, `recover-staged` re-cites (B11b); today's mechanism under `--force` drains nothing |
| S14 | pod replacement ⇒ new volume id, takeover rotation observed (6 quiet polls, `seq++`) | container restart in the tenant pod ⇒ same worker, same holder id, no rotation |
| S15 | two projects, one node: two workers, two distinct `AccessKeyId`s, no cross-visibility either way | delete one worker ⇒ only its tenant strands; the other keeps reading |
| S16 | node drain: PDB blocks worker eviction until tenants unpublish — and the PDB must report `DisruptionAllowed=False` with a reason other than `SyncFailed` and no `UnmanagedPods` event (a broken PDB also blocks, vacuously — the §10.2 shape this rule forbids); drain completes; zero orphaned mounts on the node | drain with the PDB removed evicts a worker first ⇒ tenant sees `ENOTCONN` before its own eviction |
| S17 | plugin restart mid-checkout: checkout completes; the marker's mtime predates the new plugin's start | the M1 child-process variant restarts the checkout |
| S18 | quota: fill the workspace to `sizeLimitGib` ⇒ `ENOSPC`; node root disk free space unchanged (±1 %); over-budget `expectedFiles > maxFiles` ⇒ final refusal naming `maxFiles` with zero GETs under the prefix in MinIO's access log | plain-directory mode grows past the limit; an in-budget sibling checks out |
| S19 | RBAC: `kubectl auth can-i --as=system:serviceaccount:flint-system:flint-s3-csi-node` cannot `get secrets` in any namespace, cannot `create pods` outside `flint-workers`; a `delete` of a worker whose `spec.nodeName` is another node is refused by the VAP node-name rule (§3.6) **[graft: security]** | can `create pods` in `flint-workers`; can delete its own node's worker |
| S20 | coexistence: a pod carrying both the label and the `csi:` volume is refused by the webhook naming both; the plugin refuses a pod already carrying `flint-sync` | label-only pod still injected; volume-only pod still mounted |
| S21 (Knox, real Knox only) | K1 or K2 mints; a project principal revoked at Knox is refused at the next exchange and the mount goes read-refused at key expiry | a never-authorized SA is refused by Knox, not by flint; the live principal keeps refreshing |

The memory's rule applies with force: a leg that passes *because of* the
bug it tests for is the failure mode this shape exists to prevent — every
control above is chosen so that a vacuous pass is detectable.

---

## 11. Review record

**2026-09-02 — verification pass, 3 lenses (`k8s-csi-facts`,
`knox-identity`, `flint-code-fidelity`), 29 findings: 12 refuted, 15
unsupported, 2 unverifiable; 2 blockers, 12 major, 15 minor. All 29
applied; none skipped (no two verifiers contradicted each other).**
Verifier notes: scratchpad `verify/`. Rule applied: a refuted claim is
corrected to what the evidence says, never softened; an unsupported
claim is given the citation the verifier found or relabelled; an
unverifiable claim is labelled UNVERIFIED with what would settle it.

*Blockers (identity chain).* (1) `identity.projectPrincipal` was a
tenant-writable field selecting the principal the broker impersonates
under K2 — removed from the CR (§3.3); the (ns, SA) → principal binding
is now admin-owned and cluster-scoped (§4.2 step 2, §6.7). (2) K1 had no
gate distinguishing the broker from a pod presenting its own
`aud=s3.flint.io` projected token — `knox.token.client.cert.required` +
`allowed.principals` + `audiences` + `ttl` are now REQUIRED on the K1
topology (§4.3); the endgame broker keeps a pinned client certificate
rather than "no standing credential" (§4.7, §4.2, §7, §0).

*Major, refuted.* Kubelet does not surface `VolumeCondition` (gated by
`CSIVolumeHealth`, Alpha/off; `VOLUME_CONDITION` removed from the CSI
spec) — the plugin now emits its own tenant-pod Events (§6.1, §3.7,
§4.6, §9.4, S9). `PodDisruptionBudget maxUnavailable: 0` is invalid over
bare pods and blocked eviction only through the controller's `SyncFailed`
error path — replaced by integer `minAvailable` + `controller: true` on
the Node ownerReference, with an anti-vacuity assertion on S16 (§3.6,
§5). T2 "closed" was over-claimed: any process under an entitled SA can
mint the audience and call the broker — re-scoped in §2.4, `readOnly`
demoted to presentation (§3.2, S5), and a per-volume publish
registration + `AWS_ROLE_SESSION_NAME` nonce added as the one binding a
pod cannot self-mint (§4.2, §4.4, §9.4), replacing the unimplementable
"pod-uid the plugin recorded" clause. `--force` delete "still drains"
was false past key expiry (`deletionTimestamp` = delete-time + grace;
bound tokens die 60 s later; no stale-credential fallback) — corrected in
§5, §6.6, §4.6, §8, S13, with longer-lived lean keys and a SIGTERM-time
refresh as mitigations (§5 intro now admits a second lean change). The
node SA's shared `pods delete` Role let one compromised node delete every
worker in the cluster — the VAP now pins CREATE/DELETE to the caller's
`node-name` extra (§3.6, S19). `flint-sync` does not act on
`StoreError::Auth` (only `flint-store` classifies it) and a 120 s
renewal gap triggers the operator's DR takeover — §6.3 rewritten with the
lease-liveness cost stated as a design gap. The 1M-file/10 GiB budget is
≈ 56 min (~14 attempts), not 6-7 min, and needs `maxFiles` raised (§6.4).
The lean tree's uid model chowned to a uid `NodePublish` cannot
correlate with the app — tree `1777`, CR uid REQUIRED for lean, the L1
bridge's uid 65534 dropped, new §9.2 item 18 (§3.5, §3.6, §10.1). The
in-pod `flint-sync status|ctl|recover-staged` exec surface was silently
lost — now listed in §3.2 with the replacement recipe, and S12 re-targets
the exec steps.

*Major/minor, unsupported → cited or relabelled.* K0 "no broker" now
carries the per-SA `sub` condition it depends on (§4.2, §4.3, §7). T5
is "moved", not closed (§2.4). The lean sidecar is *appended* to
`initContainers` and gates app containers only (§2.2, §5, S11).
kubernetes#121271's fix is now pinned to #139045 / v1.37.0 with
backports (§9.1 P2, §9.2 item 12). Worker `Failed` phase is handled as
final, not as a timeout (§3.4 step 6, §3.5 step 7). Checkout progress
figures do not exist during checkout — the event says "in progress"
(§3.5 step 8). Only the passthrough CRD has a parity test; lean is
schemars-derived (§3.3). `nix` needs the `mount` and `socket` features
(§9.4). Path-relativity cites `lib.rs`/`uds.rs`, not `control.rs`
(§3.2). `GraceTooShort` bounds gated drains only; cadence/hybrid has no
ceiling (§5). `mounter_args` needs a target parameter and its
`--allow-other` is §9.2 item 19 (§3.4 step 9). The `runAsUser`-derived
default owner is a documented loss (§3.2, §3.6). `FLINT_SYNC_NAMESPACE`
is a downward-API ref that would mislabel worker metrics — stamped as a
literal, scrape annotations copied (§5). Syncer workers get `OnFailure`
with a repeated-`Fenced` policy; mounters keep `Never` (§6.1, §6.7).
The lean worker image is named once: `Dockerfile.s3worker-lean` (§9.4,
§3.5). P-CRED gains a wrong-`aud` refusal probe (§9.2 item 1 (d)).

*Unverifiable → labelled.* KNOX-3434's "opened 2026-09-03" is one day
after this document's date — labelled UNVERIFIED (JIRA server time) in
the header and §4.1. P-CRED remains UNKNOWN (customer-side) with the
added probe.

Still open for a second pass: the §9.2 unknowns 1-4 gate the identity
chain, 5-10 and 18-19 the mechanics; the §6.3 lease-liveness gap and the
lean key-lifetime chart value need a decision before M5.

---

## 12. Sources

### 12.1 Repo (at `a9f3facd`, via the recon reports)

- `spdk-csi-driver/src/passthrough/{mod.rs, spec.rs, inject.rs, webhook.rs}`; `spdk-csi-driver/src/webhook_certs.rs`; `spdk-csi-driver/src/bin/flint_passthrough_operator.rs`; `flint-passthrough-chart/{values.yaml, templates/rbac.yaml, templates/NOTES.txt, crds/flintpassthroughmounts.yaml}`; `spdk-csi-driver/docker/Dockerfile.passthrough`; `passthrough/e2e/{run-passthrough.sh, mounts.yaml, rig.yaml}` — cited lines: `inject.rs:5-13,15-28,30-35,37-40,52,57-85,95-111,173-202,238-244,251-299,355-498,431-441,445-461,466-476,492-495`; `spec.rs:1-10,39-165,64-67,72-74,91-93,188-241`; `webhook.rs:63-103,142-177,205-225`; `webhook_certs.rs:84-110`; `values.yaml:18-33,32-34,36-44,77-81,94-107`; `rbac.yaml:58-60`; `crds:85-93,118-126`; `run-passthrough.sh:12-21,359-368,371-442,445-451`; `mounts.yaml:1-3`.
- `spdk-csi-driver/src/lean_operator/{webhook.rs, inject.rs, crd.rs, boundary.rs, reconcile.rs}`; `lean/sidecar/src/{bin/flint_sync.rs, lease.rs, state.rs, checkout.rs, sentinel.rs, control.rs, uds.rs, inbox.rs, barrier.rs, safefs.rs, gateway.rs, gauges.rs, lib.rs}`; `spdk-csi-driver/docker/Dockerfile.sync.prebuilt`; `flint-lean-chart/templates/monitoring.yaml`; `lean/e2e/{run-agent.sh, run-verbs.sh, run-chaos.sh, run-boundary.sh}` — cited lines as given inline.
- `spdk-csi-driver/src/{main.rs, mount_util.rs, mount_opts.rs, node_volume_locks.rs, lib.rs}`; `flint-csi-driver-chart/templates/{csidriver.yaml, node.yaml, rbac.yaml}`; `flint-csi-driver-chart/values.yaml:70`; `spdk-csi-driver/docker/Dockerfile.csi.prebuilt`; `scripts/{release.sh, publish-images.sh, stage-prebuilt.sh}`.
- `crates/flint-store/src/{s3.rs:39-74,131-143, lib.rs:529-691, probe.rs:1-18}`; `crates/flint-store/Cargo.toml:26`.
- `docs/plans/flint-lean-plan.md:66-73,236-247,304-322,407-408,421,434,438-441`; `docs/plans/flint-lean-boundary-verbs-plan.md:314-317`; `docs/plans/flint-lean-chaos-drill.md:131-138`; `docs/plans/file-api-fleet-auth.md:92-104,204-242,236-238`; `docs/plans/multicluster-frontdoor-review.md:50-51`; `docs/plans/libflint-and-snapshotter-design.md:249-258`; `docs/flint-fuse-architecture.html:376`; `docs/flint-lean-for-agent-fleets.md:115-117`.
- Verified this session: `~/.cargo/registry/src/index.crates.io-*/aws-config-1.10.1/src/ecs.rs:433-484` (+ tests `:575-666`), `web_identity_token.rs:244`, `env_service_config.rs:12-30`; `flint-csi-driver-chart/templates/csidriver.yaml` (renders `Ephemeral` by default).

### 12.2 Recon reports (scratchpad `recon/`)

`code-auth.md`, `code-csi.md`, `code-lean.md`, `code-passthrough.md`, `web-fuse-csi-priorart.md`, `web-istio-precedent.md`, `web-k8s-csi-mechanics.md`, `web-knox-jwt.md` — the `[recon §n]` citations above resolve into these; each carries VERIFIED/REPORTED/UNKNOWN tags per fact.

### 12.3 External (primary unless marked)

Kubernetes: https://kubernetes.io/docs/concepts/storage/ephemeral-volumes/ ; https://kubernetes-csi.github.io/docs/ephemeral-local-volumes.html ; https://kubernetes-csi.github.io/docs/pod-info.html ; https://kubernetes-csi.github.io/docs/token-requests.html ; https://kubernetes.io/docs/reference/kubernetes-api/config-and-storage-resources/csi-driver-v1/ ; https://kubernetes.io/docs/concepts/security/pod-security-standards/ ; https://kubernetes.io/docs/concepts/storage/volumes/ ; https://kubernetes.io/docs/reference/access-authn-authz/service-accounts-admin/ ; https://kubernetes.io/docs/tasks/configure-pod-container/configure-service-account/ ; https://kubernetes.io/docs/reference/kubernetes-api/authentication-resources/token-review-v1/ ; https://github.com/kubernetes/enhancements/blob/master/keps/sig-storage/596-csi-inline-volumes/README.md ; https://github.com/kubernetes/enhancements/blob/master/keps/sig-storage/1855-csi-driver-service-account-token/README.md ; https://github.com/kubernetes/enhancements/blob/master/keps/sig-auth/2579-psp-replacement/README.md ; https://github.com/kubernetes/kubernetes/blob/master/pkg/volume/csi/{csi_plugin.go,csi_mounter.go,csi_util.go,csi_client.go} ; https://github.com/kubernetes/kubernetes/blob/master/pkg/kubelet/{kubelet.go,kubelet_pods.go,kubelet_volumes.go,pod_workers.go,status/status_manager.go,volumemanager/volume_manager.go,volumemanager/populator/desired_state_of_world_populator.go,volumemanager/reconciler/reconciler_common.go,volumemanager/cache/actual_state_of_world.go,token/token_manager.go,apis/config/v1beta1/defaults.go} ; https://github.com/kubernetes/kubernetes/blob/master/pkg/util/goroutinemap/exponentialbackoff/exponential_backoff.go ; https://github.com/kubernetes/kubernetes/blob/master/pkg/apis/{storage/validation/validation.go,authentication/validation/validation.go,authentication/v1/defaults.go,core/validation/validation.go} ; https://github.com/kubernetes/kubernetes/blob/master/pkg/features/versioned_kube_features.go ; https://github.com/kubernetes/api/blob/master/storage/v1/types.go ; https://github.com/kubernetes/mount-utils/blob/master/mount_helper_unix.go ; https://github.com/container-storage-interface/spec/blob/master/spec.md ; https://github.com/kubernetes/kubernetes/issues/121271 (fix: pull/139045, backports pull/139228, 139229, 139230) ; https://github.com/kubernetes/kubernetes/issues/96361 ; verifier evidence 2026-09-02: https://kubernetes.io/docs/tasks/run-application/configure-pdb/ (§Arbitrary workloads and arbitrary selectors) ; pkg/controller/disruption/disruption.go ; pkg/features/kube_features.go (`CSIVolumeHealth`) ; pkg/kubelet/server/stats/volume_stat_calculator.go ; pkg/kubelet/volumemanager/volumehealth/manager.go ; staging/src/k8s.io/apiserver/pkg/registry/rest/delete.go (`BeforeDelete`) ; staging/src/k8s.io/kubectl/pkg/drain/filters.go ; https://github.com/kubernetes/api/blob/master/admissionregistration/v1/types.go ; https://github.com/torvalds/linux/blob/master/Documentation/filesystems/fuse/fuse.rst ; https://man7.org/linux/man-pages/man2/mount.2.html (REPORTED).

Istio: https://istio.io/latest/docs/setup/additional-setup/cni/ ; https://istio.io/latest/docs/setup/additional-setup/sidecar-injection/ ; https://istio.io/latest/docs/setup/additional-setup/pod-security-admission/ ; https://istio.io/latest/docs/ops/best-practices/security/ ; https://istio.io/latest/docs/ops/integrations/spire/ ; https://istio.io/latest/blog/2023/native-sidecars/ ; https://istio.io/latest/news/releases/1.27.x/announcing-1.27/change-notes/ ; https://istio.io/latest/news/security/istio-security-2023-005/ ; https://raw.githubusercontent.com/istio/istio/master/cni/README.md ; https://raw.githubusercontent.com/istio/istio/master/architecture/ambient/ztunnel-cni-lifecycle.md ; https://raw.githubusercontent.com/istio/istio/master/manifests/charts/istio-cni/templates/daemonset.yaml ; https://raw.githubusercontent.com/istio/istio/master/manifests/charts/istio-cni/values.yaml ; https://github.com/istio/istio/issues/21981 ; https://github.com/istio/istio/wiki/Troubleshooting-Istio-Ambient ; https://raw.githubusercontent.com/spiffe/spiffe-csi/main/README.md ; https://raw.githubusercontent.com/spiffe/spiffe-csi/main/example/config/spiffe-csi-driver.yaml.

FUSE CSI prior art: https://github.com/awslabs/mountpoint-s3-csi-driver (README, docs/CONFIGURATION.md, docs/UPGRADING_TO_V2.md, docs/MOUNTPOINT_POD_SHARING.md, docs/ARCHITECTURE.md, deploy/kubernetes/base/csidriver.yaml, charts/…/templates/node.yaml, pkg/podmounter/mppod/creator.go, pkg/driver/node/mounter/pod_mounter.go, pkg/driver/node/credentialprovider/provider_pod.go, pkg/mountpoint/mounter/{mount_linux.go,fd.go}, pkg/mountpoint/mountoptions/mount_options.go, pkg/mountpoint/runner/foreground.go, cmd/aws-s3-csi-mounter/…, issue #504) ; https://github.com/awslabs/mountpoint-s3 (doc/CONFIGURATION.md, doc/TROUBLESHOOTING.md, CHANGELOG.md, mountpoint-s3-client/src/s3_crt_client.rs, issues #389, #927, #1203) ; https://github.com/awslabs/aws-c-auth (source/credentials_provider_{default_chain,profile,sts_web_identity,ecs}.c) ; https://github.com/GoogleCloudPlatform/gcs-fuse-csi-driver (README, docs/{installation,authentication,known-issues}.md, deploy/base/setup/csi_driver.yaml, deploy/base/node/node.yaml, pkg/csi_mounter/csi_mounter.go, pkg/sidecar_mounter/sidecar_mounter.go, pkg/webhook/{mutatingwebhook,sidecar_spec}.go, pkg/csi_driver/node.go) ; https://docs.cloud.google.com/kubernetes-engine/docs/concepts/cloud-storage-fuse-csi-driver ; https://github.com/kubernetes-sigs/blob-csi-driver (README, docs/limitations.md, deploy/blobfuse-proxy/README.md, docs/workload-identity-static-pv-mount.md) ; https://github.com/ctrox/csi-s3 ; https://github.com/yandex-cloud/k8s-csi-s3 ; https://secrets-store-csi-driver.sigs.k8s.io/ (concepts, secret-auto-rotation, best-practices, known-limitations) ; https://github.com/kubernetes-sigs/secrets-store-csi-driver/blob/main/deploy/csidriver.yaml ; https://developer.hashicorp.com/vault/docs/platform/k8s/csi ; https://github.com/aws/secrets-store-csi-driver-provider-aws/blob/main/README.md ; https://juicefs.com/docs/csi/introduction/ (REPORTED).

Knox and S3 identity: https://raw.githubusercontent.com/apache/knox/master/gateway-service-knoxtoken/src/main/java/org/apache/knox/gateway/service/knoxtoken/{TokenResource,JWKSResource}.java (and the v2.1.0 tag) ; gateway-spi `…/token/impl/JWTToken.java`, `JWTokenAttributes.java`, `TokenUtils.java` ; gateway-server `…/token/impl/DefaultTokenAuthorityService.java`, `…/config/impl/GatewayConfigImpl.java` ; gateway-provider-security-jwt `JWTFederationFilter.java`, `AbstractJWTFilter.java`, `TokenExchangeHandler.java` (master and v2.1.0) ; gateway-service-knoxidf `{TrustedOidcIssuersResource,RegisterIssuerRequest,TokenResource,DiscoveryResource,JwksResource}.java` ; gateway-provider-security-k8s `preauth/k8s/ServiceAccountValidator.java` ; https://raw.githubusercontent.com/apache/knox/master/gateway-release/home/conf/topologies/homepage.xml ; https://apache.github.io/knox/{config_knox_token,config_client_credentials,config_apikey,config_sso_cookie_provider,config_id_assertion,config_hadoop_auth_provider,config_pac4j_provider}/ ; https://knox.apache.org/books/knox-2-1-0/user-guide.html ; https://cwiki.apache.org/confluence/pages/viewpage.action?pageId=195726105 ; https://cwiki.apache.org/confluence/display/KNOX/KIP-11+Cloud+Usecases ; ASF JIRA REST for KNOX-1204, 1740, 2149, 2570, 2608, 2714, 2938, 3014, 3040, 3048, 3109, 3120, 3141, 3266, 3355, 3368, 3373, 3384, 3403, 3405, 3408, 3412, 3424, 3432, 3433, 3434 ; Cloudera (REPORTED): security_how_identity_federation_works_in_cdp.html, rm-pc-configure-idbroker.html, security-knox-token-api.html, community article 295485 ; AWS: https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_providers_create_oidc.html ; https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html ; https://raw.githubusercontent.com/awslabs/aws-sigv4-proxy/master/README.md ; MinIO: https://raw.githubusercontent.com/minio/minio/master/docs/sts/web-identity.md ; https://raw.githubusercontent.com/minio/minio/master/internal/config/identity/openid/jwt.go ; Ceph: https://docs.ceph.com/en/latest/radosgw/STS/ ; https://raw.githubusercontent.com/ceph/ceph/main/src/rgw/rgw_rest_sts.cc ; Google: https://docs.cloud.google.com/storage/docs/authentication ; https://docs.cloud.google.com/storage/docs/xml-api/overview ; Envoy: https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/jwt_authn_filter ; https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/aws_request_signing_filter.
