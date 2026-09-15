# flint-lean — per-user access through the mount, and a read-only posture: investigation and design

Date: 2026-09-13. Status: **DESIGN; phases A-E BUILT 2026-09-14, phase F
DRILLED on EC2 2026-09-15 and on a local kind rig (OIDC STS, gVisor,
AccessIsolation) the same night** (see §10, which supersedes what it
names; §10.6 has the EC2 drill and the three defects it found, §10.7 phase
E, §10.8 the local drill; G is not built). Written the day after the
protocol review (`flint-lean-protocol-review-2026-09-12.md`) against
`37ff97d0` (v1.51.0). Every code claim in §0-§9 is `file:line` at that
commit; §10 was re-verified against `75e306c8` (after v1.53.0).

## 0. The question, verbatim

> A user logs in using either OAuth2 or a JWT (SSO flow) and can perform
> operations from the UI. As part of this they might launch an NLX
> session (natural-language interface) that can end up spawning multiple
> agents that run in a sandboxed environment such as gVisor. The agent
> pod will need to interact with the S3 workspace using lean. The file
> access needs to use the permissions of the logged-in user, so there
> needs to be a mechanism to allow this. From the UI the user could have
> a few roles that allow either read-only or full access. If a user only
> has read-only access, how can this be enforced through lean?

### 0.1 The answer in eight lines

1. **Read-only cannot be enforced through lean today.** The syncer has
   no reader mode: `run` claims the writer's lease before its first
   read (`lean/syncer/src/bin/flint_sync.rs:441`), the node plugin
   binds a lean tree read-write whatever the pod asked
   (`spdk-csi-driver/src/s3csi/node.rs:908,1129`), and the broker mints
   one access level per workspace with no notion of read or write
   (`spdk-csi-driver/src/s3csi/broker.rs:263-330`). A read-only user's
   agent gets a writable tree and a syncer that publishes what it writes.
2. **Where enforcement has to live:** the S3 credential the syncer
   holds. The agent container never holds one (the worker pod does,
   behind its loopback door), so the agent's only path to the bucket is
   its tree, and the syncer is the only thing that moves bytes out of
   it. Scope the syncer's credential and everything above it is
   presentation.
3. **The design is one posture, `access: read | readWrite`, carried
   end to end:** the CR names which ServiceAccounts are read-only
   (`consumers.readOnlyServiceAccounts`), the pod asks with the
   standard `csi.readOnly: true`, the plugin binds the tenant read-only
   and launches the syncer as a reader, the broker mints a read-only
   session policy (or asks the customer's REST door for read-only
   keys), and the syncer's reader mode takes no lease, runs no barrier,
   and answers `publish` with `refused-read-only`.
4. **The user's identity reaches the credential through the pod.** In
   phase 1 the orchestrator that verified the JWT maps the role to a
   ServiceAccount and the pod's `readOnly` flag; the user's `sub` rides
   along as an audited attribute. With the broker's existing `rest`
   backend the customer's own backend is the policy decision point at
   every mint and re-mint (every ~11 minutes), so a credential dies
   with the session. Phase 2 lets the broker verify the user's JWT
   itself, with the verifier the lite gateway already has
   (`spdk-csi-driver/src/lite_gateway/jwt.rs`).
5. **There is no shared syncer.** One worker pod, one syncer, one local
   tree and one credential per *tenant pod* (`worker.rs:68`, csi-node
   design §3.6: "lean syncers are never shared"). Two users' agent
   pods on one workspace are two syncers that meet only in the bucket,
   so each is enforced on its own: A's syncer holds a read-only key
   and takes no lease; B's holds a read-write key and the lease (§4.8).
   ~~Lean has exactly one writer per workspace: a second writer pod waits
   on the lease until the first releases it (`verbs.rs:80-97`) and
   never becomes Ready meanwhile.~~ **Superseded (v1.52.0):** several
   writer pods share a workspace and take turns at a per-barrier fence;
   none waits on another's lifetime. Readers never touch the fence. So
   an NLX session's agents can be any mix of writers and readers on one
   workspace, access decided per pod (§10).
6. **The UI path is separate and simpler:** the backend uses the
   `flint-lean-gateway` crate with its own credential, so the role
   check is the backend's; the crate should offer a read-only handle
   so a read-only session cannot reach a write verb by mistake.
7. **Six findings** (§2), two of them defects at HEAD: lean ignores the
   pod's `readOnly` flag, and the branching design of record rests on a
   broker capability ("RO keys for an RO CR") that does not exist.
8. **gVisor changes nothing in the trust argument** (the tree is a host
   bind mount either way) and two things in the runtime that must be
   measured, not assumed: write coherence between the sandbox and the
   syncer's scan, and the UDS door (host Unix sockets are off by
   default under `runsc`).

## 1. Ground truth — what the code does today

### 1.1 Identity is per workspace, and the level is whatever the backend gives

- A pod names a workspace in an inline CSI volume
  (`chert.us/workspace: <cr>`; the only attributes are `mount`,
  `workspace`, `uid`, `gid` — `spdk-csi-driver/src/s3csi/attrs.rs:18-21`).
- The plugin fetches the CR in the pod's namespace and checks the
  pod's ServiceAccount against `spec.consumers.serviceAccounts`
  (`node.rs:318-323`, `resolve.rs:141-155`). Absent = deny; `"*"` is
  the explicit wildcard (`policy.rs:19-31`). The list is a set of
  names; it carries no level.
- The credential arm (`creds.rs:1-24`): `broker` (default) exchanges
  the kubelet-minted, pod-bound SA token at the broker for short-lived
  keys, written host-side into the worker's memory-backed comm dir and
  served on the worker's loopback door `127.0.0.1:9911`; `webIdentity`;
  `static` (a Secret); `ambient`.
- The broker's chain (`broker.rs:1-31`, `decide` at `:193-228`):
  TokenReview (online, so a deleted pod is refused within 60 s) →
  the `RoleSessionName` must be the nonce of a live registration the
  plugin made for this pod and CR → the CR must list the SA → mint.
  The registration carries `volume_id, pod_uid, namespace, pod,
  service_account, cr, mode, nonce, node` (`creds.rs:281-293`) —
  **no access level, no user.**
- `mint` (`broker.rs:263-330`): `static` hands out one key set for
  every workspace; `sts` forwards the pod token to an upstream STS
  with one optional `RoleArn` for the whole broker and **no session
  `Policy`**; `rest` POSTs `{namespace, serviceAccount, podUid, cr,
  mode, durationSeconds}` to the customer's door and takes whatever
  keys it returns. **The scope of the credential is the backend's
  decision, and the broker gives it nothing to decide by except the
  workspace name.** Prefix isolation between workspaces today is the
  syncer's config (`FLINT_SYNC_PREFIX`), not the credential — the
  csi-node design's "proxy-scoped keys" put that job on the proxy.

### 1.2 Lean ignores the pod's `readOnly`

`publish_passthrough` honours it — `let read_only = req.readonly ||
spec.read_only` (`node.rs:430`), binds read-only and adds
`--read-only` to mount-s3 (`:527`). `publish_lean` never reads
`req.readonly`: the state is stamped `read_only: false` (`:908`) and the
tenant bind is `bind_mount(src, target, false)` (`:1129`). The lean CRD
has no `readOnly` field at all (`lean_operator/crd.rs`). The plugin's
own drill table recorded the truth for passthrough: "`readOnly: true`
over a RW CR yields a read-only *presentation*; the worker's keys are
still RW" (`csi-node-mount-design.md:2189`). For lean there is not even
the presentation.

### 1.3 The syncer always claims; a reader would need no write at all

`run` is claim → checkout → barrier loop (`flint_sync.rs:29`, `:441`).
The verbs already know the distinction: `checkout` "takes NO lease: it
installs nothing in the bucket" (`flint_sync.rs:7-8`), and
`run_verb` routes any step that `installs_nothing_in_the_bucket()`
through `read_only_then` (`verbs.rs:62-68`, `:111-120`). A count of
store writes per module: `barrier.rs` 2, `gated.rs` 4, `inbox.rs` 1,
`manifest.rs` 5, `lease.rs` (epoch acquire/renew/release) — and
**`checkout.rs` 0, `sync.rs` 0**. A syncer that checks out, refreshes
`.flint/remote.seq` and honours `.flint/sync` needs `GetObject`,
`GetObjectVersion` and `ListBucket` on the prefix and nothing else.
There is no such mode: the conformance probes run before the claim
(`flint_sync.rs:311`), and `run` cannot skip the claim.

### 1.4 A second writer waits forever

**Superseded 2026-09-13/14:** the lease is held per barrier and `run`
claims nothing before checkout; see §10.1.

`claim` loops on `ClaimOutcome::Waiting`, polling every 10 s
(`verbs.rs:80-97`; the outcomes are `Claimed | Waiting`,
`lease.rs:26-33`). It never refuses, never times out, and checkout
comes after it, so a second writer pod on the same workspace sits in
`ContainerCreating` until the first releases. **One writer per
workspace is a lease, not a convention.**

### 1.5 The gateway path has one bearer and an asserted author

The gateway binary demands one shared bearer for every workspace
(`lean/gateway/src/http.rs:96-97`, `:248-257`) and takes the author
from an `x-flint-author` header the caller chooses (`:268`); the crate
records it verbatim in `InboxEntry.author` / `Removal.author`
(`lean/syncer/src/inbox.rs:29`, `:73`), defaulting to `"ui"`
(`workspace.rs:655`, `:929`). Drafts are keyed by a `user` path segment
the caller chooses (`drafts.rs:175`). None of this is a defect — the
crate is a library and its caller is the policy point — but it means
**the backend is the only place a role is checked on the UI path**, and
the crate offers no type that keeps a read-only session away from
`put_file`.

### 1.6 The agent never holds a credential, and gVisor does not change that

The worker is non-root, every capability dropped, `RuntimeDefault`
seccomp, read-only root (`worker.rs:233-271`); it reaches the tree by a
hostPath under the plugin's own volumes dir (`worker.rs:167-170`) and
a ValidatingAdmissionPolicy restricts who may create one
(`flint-s3-csi-chart/values.yaml:270-300`). The credential door is
loopback inside the worker's network namespace. The tenant pod gets a
bind mount of the tree and nothing else. Under `runsc` that bind mount
is served through the gofer like any host directory; the trust
argument is unchanged: **the agent's only way to move a byte into the
bucket is to write it into the tree and let the syncer publish.**

### 1.7 A design of record cites a capability that is not there

`flint-lean-branching-design.md:474-478`: "The broker … already issues
RO keys for an RO CR (csi-node design §6). Branch scoping is the same
idea one level down." `grep -n 'read_only\|readOnly\|Policy'
spdk-csi-driver/src/s3csi/broker.rs` returns nothing; `Registration`
has no such field; the `rest` body carries none. The csi-node design
*intended* it (`:987`) and its drill honestly recorded the opposite
(`:2189`). The branching design's §4.5 credential table therefore
starts from zero, not from "one level down".

## 2. Findings

| # | severity | finding | where |
|---|---|---|---|
| S1 | **HIGH** (defect) | A lean volume declared `readOnly: true` is published read-write, silently: the pod author's stated posture is not honoured and not refused. | `node.rs:908`, `:1129` vs `:430` |
| S2 | **HIGH** (gap) | No read-only credential exists in any broker backend; `sts` forwards with no session policy; `rest` is not told the level. Every syncer of a workspace holds the same rights. | `broker.rs:263-330`, `creds.rs:281-293` |
| S3 | **HIGH** (gap) | No reader mode in the syncer: `run` cannot skip the claim, so a read-only credential would fail at the first write (the lease) and the pod would never become Ready. | `flint_sync.rs:441`, `verbs.rs:70` |
| S4 | MEDIUM (doc) | The branching design of record cites "RO keys for an RO CR" as shipped. It is not. | `flint-lean-branching-design.md:474` |
| S5 | MEDIUM (design) | One writer per workspace is a lease with an unbounded wait; an orchestrator that launches two writer agents on one workspace gets one that never starts, with no event naming why beyond the worker's log. | `verbs.rs:80-97` |
| S6 | LOW (hygiene) | The UI path's `author` and the draft `user` are asserted by the caller; nothing in the guide tells an integrator to fill them from the verified `sub`. The agent-fleets guide has no `automountServiceAccountToken: false` for agent pods. | `http.rs:268`, `docs/flint-lean-for-agent-fleets.md` |

None of these is exploitable from inside the sandbox today: an agent
cannot widen its own rights, because it has none. They are all "the
system cannot express the narrower right", which is exactly the
question asked.

## 3. The threat model for this use case

Principals, and what each can do at HEAD:

| principal | trusted for | can do today |
|---|---|---|
| the SSO issuer | who the user is, what roles | — |
| the UI backend (Rust, `flint-lean-gateway` crate) | mapping role → verbs; it holds a bucket credential | every verb on every workspace it is configured for |
| the NLX orchestrator (creates agent pods) | mapping user → pod spec: SA, workspace, `readOnly`; recording which pod acts for whom | chooses any SA in the namespace, any CR in the namespace |
| the agent (LLM-driven, prompt-injectable, in `runsc`) | nothing | read and write its tree; touch `.flint/publish` and `.flint/sync` |
| the node plugin + broker | the chain in §1.1 | mint for any (SA, CR) the CR allows |
| the worker/syncer | moving the tree's bytes | everything its credential allows, on the prefix |

What read-only must defend: **an agent acting for a read-only user
must not be able to land a byte in the bucket**, including through a
bug or misconfiguration in anything above the credential (a
misrouted `readOnly` flag, a syncer defect, a future verb). That is
why the credential is the enforcement and the rest is presentation:

1. **credential** — the syncer's keys cannot write the prefix
   (enforced by S3/STS; survives every bug in flint);
2. **syncer** — a reader takes no lease and runs no barrier (so a
   read-only credential is *sufficient* for a working pod, and a
   `publish` touch is answered, not ignored — "nothing is silent");
3. **mount** — the tenant bind is read-only (so the agent learns at
   `write(2)` rather than at pod end that its edits go nowhere).

Each layer alone is insufficient: (3) without (1) is what passthrough
has now — a presentation; (1) without (2) is a crash-looping pod; (2)
without (1) is a promise.

What this does **not** defend, stated plainly: an agent for a
read-only user still reads *everything* under the prefix, and a
partially-scoped reader (scoped checkout) still reads every **name**
in the manifest (`manifest.rs:25-41` — key, size, etag, CRC, mode,
mtime for every entry). Path-level read restriction is a bucket-policy
matter and leaks names; it is out of scope here and noted in §6.

## 4. The design: `access` as a first-class posture

### 4.1 The CRD: who is read-only

`s3csi::policy::Consumers` gains a second list. Both CRDs share the
type (`policy.rs:6-10`), so passthrough gets it for free and its
`readOnly` presentation becomes an enforcement too.

```yaml
spec:
  consumers:
    serviceAccounts: [nlx-agent-rw]            # unchanged: may mount read-write
    readOnlyServiceAccounts: [nlx-agent-ro]    # NEW: may mount read-only, never wider
```

Two lists, not a list of objects: a string-or-object union is a
schema junctor, which Kubernetes refuses for a structural CRD and
`crd_is_structural` (`crd.rs`) guards against. An existing CR is
byte-identical in behaviour.

**Refined when built (§10.5, D13):** the function below gives a
wildcard read-write entry precedence over a named read-only one. As
built, the most specific entry decides, and at equal specificity the
narrower one.

The decision, one pure function beside `Consumers::allows`:

```
access(sa, req.readonly) =
  if sa ∈ serviceAccounts          { if req.readonly { Read } else { ReadWrite } }
  else if sa ∈ readOnlyServiceAccounts { Read }        // readOnly:false never widens
  else                             { Deny }
```

The pod asks with the **standard** CSI field — no new attribute:

```yaml
volumes:
  - name: ws
    csi:
      driver: s3.csi.chert.us
      readOnly: true                      # the orchestrator sets this for a read-only role
      volumeAttributes: { chert.us/workspace: proj1 }
```

Kubernetes carries it to `NodePublishVolumeRequest.readonly`, the
field passthrough already reads. **S1 is fixed by the same change:** a
lean pod asking `readOnly: true` gets exactly that.

### 4.2 The plugin: bind, launch, register

- `VolumeState.read_only` = the decided access; the tenant bind uses it
  (`:1129` takes `st.read_only` like the rebind at `:287` already does).
  The worker's own view of the tree stays read-write: a reader's
  syncer still writes `sync` results and `.flint/` acks *into* the
  tree, and the two binds have independent flags.
- The launch list gains `FLINT_SYNC_ACCESS=read|readWrite`
  (`lean_operator/sync_env.rs:39` is the one place the list is built;
  the plugin appends it beside `FLINT_SYNC_REGION`).
- `Registration` gains `access` and an optional `on_behalf_of` taken
  from a new volume attribute `chert.us/on-behalf-of` (the pod
  author's assertion — the orchestrator's — useful for the audit line
  and for the REST door to cross-check; **never** an input to the
  decision in phase 1).
- The broker's audit line gains both: `issued ns=… sa=… cr=… access=read
  on_behalf_of=…`.

### 4.3 The broker: mint by access

`mint(id, mode, cr, access, on_behalf_of, …)`:

- **`sts`** — attach a session `Policy` to `AssumeRoleWithWebIdentity`
  (AWS and MinIO document the parameter; Ceph RGW to be confirmed;
  Apache Ozone has no STS and takes the `static` or `rest` road):

  read:
  ```json
  {"Version":"2012-10-17","Statement":[
    {"Effect":"Allow","Action":["s3:GetObject","s3:GetObjectVersion","s3:GetObjectAttributes"],
     "Resource":"arn:aws:s3:::<bucket>/<prefix>/*"},
    {"Effect":"Allow","Action":["s3:ListBucket","s3:ListBucketVersions"],
     "Resource":"arn:aws:s3:::<bucket>","Condition":{"StringLike":{"s3:prefix":["<prefix>/*"]}}}]}
  ```
  readWrite: the read statements plus `s3:PutObject`, `s3:DeleteObject`,
  `s3:DeleteObjectVersion`, `s3:AbortMultipartUpload`,
  `s3:ListMultipartUploadParts` on `<bucket>/<prefix>/*` — today's
  within-project posture, now also prefix-bounded by the credential
  rather than by the syncer's config. A session policy can only narrow
  the role, which is the right failure direction: a broker
  misconfigured with a too-wide role still hands out bounded keys.
- **`rest`** — the body gains `"access": "read"|"readWrite"` and
  `"onBehalfOf"`. The customer's door decides and scopes; the broker
  records which it answered.
- **`static`** — cannot scope. The CR surfaces `AccessIsolation:
  Cooperative` (the branching design's `BranchIsolation` shape,
  §4.5) so a fleet on a static key knows its read-only is a promise
  the syncer keeps, not one the bucket enforces.

### 4.4 The syncer: reader mode

`FLINT_SYNC_ACCESS=read` makes `run` a reader:

- **No claim, no heartbeat, no drain.** `run` = conformance probes that
  read only (the copy probe is skipped and the marker says so) →
  `checkout`/`checkout_scoped` → capabilities → the reader loop.
- **The reader loop** each tick: refresh `.flint/remote.seq` (one GET of
  `current`), honour `.flint/sync` (scoped or whole, the existing
  `sync_scoped`), answer `.flint/publish` with `{"status":
  "refused-read-only", "reason": "this workspace is mounted read-only
  for this pod"}`. The UDS door's `POST /v1/boundary` answers 403 with
  the same body; `POST /v1/sync` works.
- **`capabilities.json`** gains `"access": "read"` and advertises
  `verbs: ["sync", "remote-seq"]`; the guard test that every
  advertised verb is named in the guide covers it.
- **`.flint/AGENTS.md`** gains a section the marker points at:
  *"This workspace is read-only for you. Your tree is a read-only mount;
  writes fail with EROFS. Use a scratch directory outside the
  workspace for build output. `sync` works; `publish` is refused."*
- **Local writes, optional, opt-in** (`spec.readers.localWrites: true`,
  default off): the tenant bind is read-write but the reader still
  never publishes, and the contract line becomes *"your writes are
  local only and are gone when the pod ends"*. For agents that must
  build or test inside the tree. Off by default because a silent
  discard is the thing the contract exists to prevent, and EROFS is
  the honest answer.
- **`sync` onto a read-only tree**: the worker's view is read-write
  (§4.2), so foreign changes still land; "your modified version always
  wins" has nothing to win against.
- Operator status: a reader has no lease heartbeat, so `observed_*`
  stays the writer's. A `readers` count from the plugin is a later
  nicety.

What a reader with a *read-only credential* touches, checked against
§1.3: `current` (GET), manifest generations (GET), `files/*` (GET,
`If-Match` or by version), `claim` (GET, `verify_claim`). Nothing else.

### 4.5 The user's identity: how the role reaches the credential

**Phase 1 — the orchestrator is the mapper, the pod is the carrier.**
The orchestrator has the verified JWT. It launches the agent pod with
`serviceAccountName: nlx-agent-ro` (read-only role) or `nlx-agent-rw`,
`csi.readOnly` accordingly, and `chert.us/on-behalf-of: <sub>`. The CR
lists both SAs at their levels. The chain enforces:

- the CR: which SA at which level (the workspace owner's policy);
- the plugin: the bind and the syncer mode;
- the broker: the credential at that level;
- the orchestrator: which user gets which SA — **the one trust this
  phase adds**, and the same trust the cluster already places in
  whoever may create pods in the namespace (`csi-node-mount-design.md
  §8`, "the namespace is the project boundary"). Tighten it with the
  VAP that design recommends: pods declaring an `s3.csi.chert.us`
  volume may use only the two agent SAs, and only the orchestrator's
  SA may create them.

**Phase 1b — the customer's backend as the policy decision point at
every mint.** The broker's `rest` backend already POSTs `podUid` to a
door of the customer's choosing. The UI backend can *be* that door:
`podUid → NLX session → user → role`, cross-checked against
`onBehalfOf`, returning keys from its own `AssumeRole` with the session
policy of §4.3 — or 403. The plugin re-exchanges every ~11 minutes
(within three periods of a 900 s key's expiry, `creds.rs:11-13`), so a
refused re-mint removes the credential file and the syncer fails at
expiry: **the credential's lifetime is bounded by the session's, with
no flint change beyond §4.1-4.4.** A role narrowed mid-session yields
read-only keys to a writer-mode syncer, whose next lease renew and
upload fail closed (nothing lands); the orchestrator replaces the pod.

**Phase 2 — the broker verifies the person.** `Consumers` gains
`principals: [jwt:user:alice@…, jwt:group:…]` at each level, the
orchestrator registers `podUid → user token` with the broker (a
verb authenticated as the orchestrator's SA, mirroring how the plugin
registers publishes), and at exchange the broker verifies the token
with `JwtReviewer` (`lite_gateway/jwt.rs:202`, exact `iss`, `aud`,
JWKS with a refetch floor) and matches the principal. `Identity`
already carries `Vouched::Issuer` for exactly this (`broker.rs:118-160`).
What it buys over 1b: the *workspace owner* names people, not the
orchestrator's SAs, and the broker enforces `exp` independently of the
orchestrator. What it costs: the forge JWT design's whole §5-§6
(discriminating the two RS256 issuers, audience, key material) and a
token-registration verb. Recommended only when the workspace owner and
the orchestrator's operator are different parties.

### 4.6 The UI path: the crate

- `flint_store::ReadOnly<S>` — an `ObjectStore` wrapper whose every
  write returns `StoreError::Forbidden`; the test suite's `Hooked`
  wrapper already shows the shape.
- `Workspace::read_only(store, prefix)` — the same verbs; every writer
  (`put_file`, `remove_*`, `rename_*`, `withdraw_removal`,
  `request_boundary`, `promote_draft`, `put_draft`, `delete_draft`) answers
  `VerbError::ReadOnly` (403 on the wire, `read-only`) before touching
  the store. Two layers again: the typed refusal is the honest error;
  the store wrapper is the backstop.
- The backend builds **one client per (bucket, role)** — two, not one
  per user: a read-only client from a read-only session and a
  read-write one — and constructs the `Workspace` for a request from
  the caller's role. Per-user clients are neither needed nor cheap
  at "hundreds to thousands of workspaces".
- `author` and the draft `user` are filled from the verified `sub`,
  never from the request body; the README says so.
- The gateway *binary*'s one shared bearer is unchanged (it is the
  cluster-internal door, "deferred" per `http.rs:71-73`); a fleet that
  fronts it with people must front it with the backend.

### 4.7 Multiple agents in one session

**Superseded (v1.52.0):** several writers share a workspace, so the
orchestrator's first option is simply N agents on one workspace, each
pod read-write or read-only by its own role (§10.3). The list below is
the 2026-09-13 reasoning, kept for the record.

The lease decides: **one writer per workspace.** The orchestrator's
options, in order of availability:

1. **One writer, N readers** — available after §4.4. Readers see each
   boundary through `sync`; the writer's `publish` is the coherent
   point. Fits "agents that research, one that edits".
2. **N workspaces** — one CR per agent (a prefix each), the backend
   merges through the crate. Available today; costs N checkouts and a
   merge the backend owns.
3. **Branches** — `flint-lean-branching-design.md`: a branch per agent,
   proposals merged by main's executor. Designed, unbuilt, and its
   credential story (§4.5 there) starts from this document's §4.3.

S5's operational fix is cheap and separate: the plugin should surface
"waiting on the standing lease held by pod X" as a Kubernetes event on
the tenant pod, so an orchestrator that launched two writers sees why
the second is stuck.

### 4.8 Worked scenario: two users, two sessions, one workspace

User A logs in read-only, user B with full access. Each starts an NLX
session; each session spawns agent pods against the same workspace
`proj1`.

**Is it the same syncer process?** No. The plugin creates one worker
pod per published volume — `s3w-<hash(volume_id)>` (`worker.rs:68`) —
with its own tree under `<plugin>/volumes/<vid>/tree`, its own
memory-backed comm dir, its own credential exchanged with *that* pod's
bound token, and its own bind into *that* tenant (csi-node design
§3.6: "Sharing: none in v1 … lean syncers are never shared"). A's pod
and B's pod on the same node are still two workers, two trees, two
keys. They share the bucket prefix and nothing on the node.

What each component does, per pod, after this design:

| | A's agent pod (read-only) | B's agent pod (full) |
|---|---|---|
| orchestrator sets | `serviceAccountName: nlx-agent-ro`, `csi.readOnly: true`, `on-behalf-of: A` | `serviceAccountName: nlx-agent-rw`, `csi.readOnly: false`, `on-behalf-of: B` |
| CR `consumers` | `readOnlyServiceAccounts: [nlx-agent-ro]` ⇒ `Read` | `serviceAccounts: [nlx-agent-rw]` ⇒ `ReadWrite` |
| plugin | tenant bind **ro**; `FLINT_SYNC_ACCESS=read`; registration `access=read` | tenant bind rw; `FLINT_SYNC_ACCESS=readWrite`; registration `access=readWrite` |
| broker → keys | session policy: Get/List on `proj1/*` only (or the REST door's read-only keys) | Get/List/Put/Delete on `proj1/*` |
| syncer | checkout; **no lease**; loop = `remote.seq` + `sync`; `publish` ⇒ `refused-read-only` | claim (epoch N); checkout; barrier loop; `publish` ⇒ `ok` |
| what A's agent sees of B | B's boundaries, when A's agent touches `sync` (or at pod start) | — |
| what B's agent sees of A | nothing: A publishes nothing, and A's tree is A's | — |
| if the wrong SA is used | a pod with `nlx-agent-ro` asking `readOnly: false` is **still** read-only (§4.1, F9) | a pod with `nlx-agent-rw` asking `readOnly: true` is narrowed to read (F1) |
| if the orchestrator is wrong | A launched with `nlx-agent-rw`: full access — phase 1's one trust (§4.5); 1b's REST door refuses the mint because `podUid → A → read-only`; phase 2's broker refuses because `jwt:user:A` is not a read-write principal | — |

Two more agents in A's session: two more readers, each its own
worker, all Ready in checkout time. Two more agents in B's session on
the same workspace: one holds the lease, the others wait
(`verbs.rs:80-97`) — B's orchestrator gives them separate workspaces
or waits for branching (§4.7).

**At HEAD, for contrast:** both SAs must be in `serviceAccounts`; both
syncers get the same keys; both claim the lease, so whichever pod
starts second sits in `ContainerCreating` until the first ends; and
while A's pod holds the lease, A's agent publishes at every cadence
tick under full access. The scenario is not expressible today, which
is finding S2 + S3 stated as a user story.

## 5. gVisor: what changes, what must be measured

Unchanged: the trust argument (§1.6); the read-only bind (an OCI
read-only mount is honoured by `runsc`); mtime precision (the host
kernel stamps the host FD; the syncer scans host-side).

To measure, not assume — each is a falsifier in §7:

- **Write coherence.** The contract says a `publish` touch includes
  every completed write. Under `runsc` a write passes through the
  sentry before the host sees it; for non-root mounts `runsc`'s
  default file access is *shared* (revalidated against the host), which
  should make a completed write visible to the syncer's scan at once.
  "Should" is not a measurement: F7 writes, touches `publish`, and
  compares the cited bytes.
- **The UDS door.** `runsc` does not expose host Unix sockets unless
  `--host-uds` is enabled; `.flint-sync/ctl.sock` will be unreachable
  from the sandbox by default. The door is opt-in and the file protocol
  is the guaranteed interface (`crd.rs`, `uds_door`), so nothing breaks
  silently; the guide should say it.
- **`automountServiceAccountToken: false`** on agent pods, sandbox or
  not: the CSI token the broker consumes is minted for the plugin and
  never enters the pod, but the pod's *own* SA token would, and an
  agent has no business holding one.

## 6. What this does not solve

- **Path-level read restriction** — the manifest names every path; a
  reader scoped to `inputs/` still learns that `secrets/plan.md` exists,
  its size and its mtime. Real path-level read isolation needs the
  manifest split by scope, which is the branching design's overlay
  question in another costume. Not here.
- **`static` keys** — cooperative read-only only; surfaced as a
  condition, never silently.
- **Revocation latency** — bounded by the key lifetime (≤ 15 min at the
  default; the broker refuses within 60 s of pod deletion, but keys
  already issued live to `Expiration`). Same as today.
- **Phase 1 trusts the orchestrator's mapping.** §4.5 says where that
  trust sits and how to narrow it.
- **The UI backend's own credential** is read-write on every
  workspace it serves. That is the backend's threat model, not lean's.

## 7. Falsifiers — each with the control that must fail

| # | claim | test | control |
|---|---|---|---|
| F1 | a lean pod with `csi.readOnly: true` gets EROFS on write | write in the tenant; expect EROFS | same pod with `readOnly: false`: the write lands |
| F2 | a reader takes no lease | mount a read-only pod beside a writer; the writer keeps epoch N; the reader is Ready in checkout time | a read-write pod beside the writer waits (today's behaviour) |
| F3 | a reader's credential cannot write | from the worker, `PutObject` under the prefix with the issued keys ⇒ 403 | the writer's keys: 200 |
| F4 | `publish` on a reader is answered `refused-read-only`, never silently dropped | touch, read the ack | on a writer: `ok` |
| F5 | `sync` on a reader applies a foreign change onto the read-only tree | HITL write through the crate, touch `sync`, read the file in the tenant | same with no `sync`: unchanged |
| F6 | a `rest` door that returns 403 at re-mint takes the credential away | flip the door; within one key lifetime the syncer reports a credential refusal, nothing lands | door returns keys: the barrier continues |
| F7 | under `runsc`, a write then an immediate `publish` cites the written bytes | 1000 files, touch, compare manifest sizes/CRCs against the tenant's view | the same on `runc` |
| F8 | the session policy narrows, never widens | broker role deliberately bucket-wide; issued keys refuse a `GetObject` outside the prefix | no `Policy`: it succeeds |
| F9 | `readOnly: false` on an SA in `readOnlyServiceAccounts` does not widen | mount, write ⇒ EROFS; keys ⇒ 403 on PUT | the SA moved to `serviceAccounts`: both succeed |
| F10 | a read-only `Workspace` cannot reach the store's writers | `Workspace::read_only(...).put_file` ⇒ `ReadOnly`; and with the typed check deleted, the `ReadOnly` store still refuses | `Workspace::new`: the put lands |

The mutation discipline of the protocol review applies: each fix
lands with its test written first against the unfixed tree, and F10's
two layers are each shown load-bearing by deleting the other.

## 8. Phases and cost

| phase | content | size |
|---|---|---|
| A | S1: honour `req.readonly` for lean (bind + state) — a defect fix, ships alone | ~20 lines + F1 |
| B | `readOnlyServiceAccounts`, the decision function, `FLINT_SYNC_ACCESS`, registration `access`/`on_behalf_of`, broker audit line, `AccessIsolation` condition | ~250 lines, both CRDs regenerated, render fixtures |
| C | syncer reader mode + `refused-read-only` + capabilities + guide section + UDS 403 | ~300 lines + F2, F4, F5 |
| D | broker `sts` session policy + `rest` body fields | ~120 lines + F3, F6, F8, F9 |
| E | crate `ReadOnly` store + `Workspace::read_only` + README | ~150 lines + F10 |
| F | live drill on a cluster with `runsc` nodes (F1-F9 as deployed, F7 in particular) | a drill script; ASK before provisioning |
| G | phase 2 (people at the broker) | its own design, after forge's JWT phase settles |

A, B, C, D ship as one release; E can ship as a crate minor first,
since the backend is the first consumer.

## 9. Decisions

- D1 **The credential is the enforcement; the mount and the syncer
  mode are presentation.** No layer may be the only one.
- D2 **`access` is binary in v1: `read | readWrite`.** Path-level read
  is out of scope (§6).
- D3 **The pod asks with the standard `csi.readOnly`;** no new attribute
  for the level. `chert.us/on-behalf-of` is audit only.
- D4 **Two consumer lists, not a list of objects** (structural schema).
- D5 **A reader's tree is read-only by default;** `localWrites` is an
  explicit opt-in with a contract line.
- D6 **The user's identity reaches the credential through the pod in
  phase 1 and through the customer's REST door at every mint in 1b;**
  the broker verifies people only in phase 2.
- D7 ~~**One writer per workspace stands.**~~ **Superseded (v1.52.0):**
  several writers per workspace; access is per pod, so one workspace
  carries any mix of writer and reader agents.
- D8 **The branching design's §4.5 is re-based on this document** (S4).

## Status note, 2026-09-13 (later the same day)

The per-barrier publish fence was built
(`flint-lean-writer-lease-and-gated-assessment.md` §4, §10), which
changes two statements above without changing the design's conclusion:

- **S3 / §4.7 "one writer per workspace is a lease, not a convention."**
  The lease is now held per barrier: several writer pods share a
  workspace and merge at the manifest, and none waits on another's
  lifetime. A read-only credential therefore no longer fails at a claim
  before the first barrier — `run` claims nothing before checkout — so
  the pod becomes Ready; it fails at its first heartbeat PUT and at
  every barrier's uploads instead, logged and retried each floor. That
  is still not a reader mode: the enforcement is the credential, the
  syncer keeps attempting writes it cannot make, and §4.4's reader loop
  (no heartbeat, no barrier, `refused-read-only`) remains the build.
- **§4.8's worked scenario** stands, with B's "holds the lease" now
  reading "claims the fence for each of its commit sections".

## 10. Refreshed 2026-09-14 against `75e306c8`, and what was built

### 10.1 Ground truth that moved since §1

- **No claim before checkout, no heartbeat.** `run_loop`
  (`lean/syncer/src/bin/flint_sync.rs`) is `verify_claim` (a GET) →
  `warn_if_prefix_is_shared` (a HEAD) → `release_stale_own` → checkout
  → a loop of two timers (floor, sentinel poll), the UDS door and the
  SIGTERM drain. The writer heartbeat was removed on 2026-09-14. Each
  barrier claims the fence after its uploads, for its commit section.
- **Store writes by module** (call sites of every `ObjectStore` write
  verb, `tests.rs` excluded; control: all 14 write verbs are counted in
  the memory double's impl): `barrier.rs` 7, `lease.rs` 6,
  `manifest.rs` 7, `inbox.rs` 1 — and `checkout.rs` 0, `sync.rs` 0,
  `bin/flint_sync.rs` 0 (its probes call flint-store's). A per-file
  count is not a call graph; the reader's actual proof is the battery's
  write census (§10.4), which counts every request a reader sends.
- **v1.53.0's pull-only boundary** takes no fence and writes nothing
  when a barrier has nothing of its own to publish. Useful, but not a
  reader mode: a writer with local changes still uploads.
- **Writers integrate foreign changes at every boundary** (AGENTS.md,
  "Foreign changes at a boundary, and other writers"), onto paths the
  agent has not modified. §4.4 had the reader integrate only on a
  `.flint/sync` touch; that would leave a reader's copy further behind
  than every writer's. Corrected in §10.2.
- **S1 still held at HEAD** before this change: `publish_lean` stamped
  `read_only: false` and bound the tenant with `false`
  (`spdk-csi-driver/src/s3csi/node.rs`); the rebind path already honoured
  `st.read_only`.

### 10.2 What was built (phases A and C)

**The syncer (`FLINT_SYNC_ACCESS=read`, `LeanConfig.access`,
`lean/syncer/src/reader.rs`):**

- **The floor tick pulls.** The inbox cell and the manifest pointer —
  the two GETs an idle writer makes — and a whole-tree `sync` only when
  either document's etag moved since the last pull (remembered in
  `.flint-sync/reader.json`; `sync` deliberately does not advance
  `baseline.manifest_etag`, so the baseline cannot answer "moved").
  `remote.seq` and `gauges.json` move every tick. `sync` makes no store
  writes, applies only onto paths the scan finds clean, and records a
  `sync-dirty` conflict for every change it declines.
- **Nothing publishes.** `barrier_inner` refuses a reader before its
  first request (every publishing path ends there); `run_verb` refuses
  `barrier` and `rescope` (the latter claims the fence); the binary
  refuses `probe-copy` and `probe-conditional` (both PUT); `run` skips
  `release_stale_own` (a reader never claims, and releasing is a write).
- **Nothing is silent.** A `.flint/publish` touch is answered
  `status: "refused-read-only"` with a `reason`, and retired; so is one
  owed at SIGTERM. The UDS door's `boundary` answers
  `{"status":"refused-read-only"}`; its `sync` works. The drain runs no
  barrier.
- **The marker and the guide.** `capabilities.json` gains `access`
  (`"read"` | `"readWrite"`; an old marker parses as read-write); a
  reader advertises `verbs: ["sync", "remote-seq"]`. `.flint/AGENTS.md`
  gains a "Read access" section.

**The plugin (`s3csi/node.rs`):** `publish_lean` stamps
`read_only: req.readonly`, binds the tenant with it, and adds
`FLINT_SYNC_ACCESS=read` to the saved launch list
(`lean_access_env`), so a relaunched worker keeps its access. The
worker's own hostPath view of the tree stays read-write: a reader
writes what it pulls.

**Not built here:** B (`readOnlyServiceAccounts`, registration
`access`/`on_behalf_of`, the audit line, `AccessIsolation`), D (the
broker's read-only credential), E (the crate's `ReadOnly` store and
`Workspace::read_only`), F (the live drill). **Until D, read-only is a
promise the syncer and the mount keep, not one the bucket enforces**
(D1): the reader's credential is still read-write.

### 10.3 Decisions taken while building

- **D9 Access is per POD, not per workspace.** One workspace carries any
  mix of read-write and read-only agents at once. Each pod has its own
  worker, syncer, tree and credential; a reader never touches the fence,
  so it never delays a writer, and a writer never learns a reader exists.
  The battery's `a_reader_follows_writers_and_the_inbox_without_one_store_write`
  is exactly that shape: one writer and one reader on one workspace.
- **D10 A reader integrates at every floor**, not only on `sync` — the
  convergence a writer gets from its boundaries. Idle cost: two GETs per
  floor per reader, the same as an idle writer.
- **D11 No nested read-write bind of `.flint/` in v1.** A read-only tenant
  bind covers `.flint/` too, so an agent on a read-only mount cannot
  create `.flint/sync` (EROFS) and relies on the floor. A second bind of
  `tree/.flint` read-write over the read-only tree would restore
  sync-on-demand, but it adds a mount to the publish, rebind and unpublish
  paths (and to their crash matrix) that only a node can verify; it
  belongs with phase F. The syncer still honours `sync` and answers
  `publish` wherever `.flint/` IS writable, and the guide says both.
- **D12 A reader's local change is kept, never published.** If a
  reader's tree is writable (no bind flag, or a future `localWrites`), a
  local edit wins over a foreign change exactly as `sync` rules, is named
  in `conflicts.jsonl`, and is gone with the pod.

### 10.4 Falsifiers, where they stand

| # | status |
|---|---|
| F1 EROFS on a read-only lean mount | **measured, §10.6**: EROFS for a pod that asks `readOnly: true`; the plugin's bind was `rw` on the host (F-1, fixed) and cannot reach the container for a pod asking `rw` (F-2, now refused) |
| F2 a reader takes no fence and is Ready in checkout time | **unit, the fence half**: the write census allows only GET/HEAD/LIST/`epoch_read`; `one_shot_verbs_that_publish_or_fence_are_refused_on_a_reader`. Readiness beside a committing writer is phase F |
| F3 a reader's credential cannot write | **live on AWS** (§10.6) and **on MinIO's OIDC STS with the broker as deployed** (§10.8, O3); first **live on MinIO** (§10.5): PUT and DELETE under the prefix denied on keys the broker's policy narrowed, the parent user's allowed; a read-write barrier on them 403s and moves nothing. AWS STS on a cluster is phase F |
| F4 `publish` answered `refused-read-only` | **unit**: `a_publish_touch_on_a_reader_is_answered_refused_read_only` (control: the writer's `ok`), `a_reader_drain_answers_what_it_owes_and_writes_nothing` |
| F5 `sync` applies a foreign change onto the reader | **unit**, and at every floor: `a_reader_follows_writers_and_the_inbox_without_one_store_write` (a writer's edit, delete, add and a UI inbox write) |
| F6 a refused re-mint takes the credential away | unchanged code path (`republish` removes `creds.json` on a refusal); phase F |
| F7 write coherence under `runsc` | **live on kind, arm64, runsc systrap** (§10.8, G1): 1000 files then an immediate publish, every file cited at its size, a runc reader byte-identical; the same from a runc writer as the control. Not measured on amd64 or under KVM |
| F8 the session policy narrows, never widens | **on MinIO's OIDC STS** (§10.8, O0/O3): the same role without a policy writes and reads another prefix; the policy the broker sent, read off the wire, holds the reader to reads of its prefix. First **live on MinIO** (§10.5): the parent user's own policy is bucket-wide, and the narrowed keys are denied GET and LIST of another prefix |
| F9 `readOnly: false` on a read-only SA does not widen | **unit, both ends**: `a_read_only_consumer_is_granted_read_whatever_the_pod_asks` (plugin), `a_grant_is_the_registration_narrowed_by_the_cr_never_widened` (broker); the bind as deployed is phase F |
| F10 a read-only `Workspace` cannot reach the store's writers | **unit** (§10.7): every writing verb answers `ReadOnly` with nothing sent; a write through `store()` is refused by the `ReadOnly` wrapper; removing the verb guard or the wrapper each fails its own test, and the other layer still refuses |

Mutation-checked: nine mutations (floor tick without the reader branch;
that plus the barrier guard removed; the guard alone; the publish
refusal off; the drain running a barrier; `rescope` not refused; the pull
memo never matching; the memo ignoring the inbox; a reader advertising
`publish`) each fail the test meant for it. With both the reader branch
and the guard removed, the writer-and-reader test catches the reader's
uploads, so the no-write property is not resting on the guard alone.

### 10.5 Phases B and D, as built (2026-09-14)

**B — who is read-only, carried end to end.**

- `s3csi::policy::MountConsumers { serviceAccounts, readOnlyServiceAccounts }`
  is the consumers type of both mount CRDs. `FlintRepo` keeps
  `Consumers`: forge's door has no read-only posture, and a field it
  would accept and ignore is the kind a reader trusts. The lean CRD is
  regenerated (`crdgen lean`; the diff is the new property and three
  descriptions); the hand-written passthrough CRD declares it, and its
  drift guard now compares `spec.consumers`' keys too, because a pruned
  read-only list DENIES the ServiceAccounts it names.
- `MountConsumers::access(sa, readOnly) -> Option<Access>` and
  `resolve::authorize` return the grant. `node.rs` decides it once per
  publish and everything follows that value: `VolumeState.read_only`
  (the bind), mount-s3's `--read-only` (passthrough; the CR's own
  `readOnly` still narrows), `FLINT_SYNC_ACCESS=read` (lean), and
  `Registration.access`.
- `Registration` gains `access` (default `readWrite` on the wire, which
  the broker narrows by the CR anyway) and `on_behalf_of`, from a new
  pod-authored volume attribute `chert.us/on-behalf-of` (1-256 bytes, no
  control characters, since it lands in log lines). It is kept in the
  volume state, so a re-registration after a broker restart carries it.
  It decides nothing.
- The lean operator reports `AccessIsolation` on every workspace with
  `consumers` (a pod on a read-write SA with `csi.readOnly` is a reader
  too): `False/Cooperative` under `identity.mode` `static` or `ambient`,
  `Unknown/DecidedByBroker` under `broker` or `webIdentity`.

**D — the broker mints by access.**

- `decide` returns a `Grant { mode, cr, access }`: the registration's
  access narrowed by the CR's lists. A rig without registration gets the
  CR's access for the SA.
- `sts`: a read grant carries `Policy` = `read_session_policy(partition,
  bucket, prefix)` — `s3:GetObject`, `s3:GetObjectVersion` on
  `<bucket>/<prefix>/*` (no `s3:GetObjectAttributes`, which §4.3 had:
  nothing calls it, and Ceph RGW Squid refuses a policy naming it, found
  2026-09-14 reading RGW's parser), and `s3:ListBucket`,
  `s3:ListBucketVersions` on the bucket conditioned `StringLike s3:prefix
  [<prefix>, <prefix>/*]` (no condition for a root workspace). mount-s3's
  configuration guide says the same for a prefix mount: object actions
  scoped by resource, `s3:ListBucket` by the `s3:prefix` condition key.
  `FLINT_S3B_ARN_PARTITION` (chart `broker.sts.arnPartition`) for
  `aws-cn`/`aws-us-gov`.
- `rest`: the body gains `"access"` and `"onBehalfOf"`.
- `static`: optional `FLINT_S3B_STATIC_READ_*` (chart
  `broker.static.readSecretRef`), both halves or neither (half a key set
  is a startup error).
- `issued` gains `access`, `enforcement` (`none` for a write grant;
  `sessionPolicy`, `restDoor`, `readKey` or `cooperative` for a read
  grant) and `on_behalf_of`; `registered` gains `access` and
  `on_behalf_of`; `/v1/status` gains `readEnforcement`; a `static`
  broker without a read key warns once at start.

**Decisions taken while building.**

- **D13 The most specific consumer entry decides; at equal specificity
  the narrower.** §4.1's function put `serviceAccounts: ["*"]` ahead of a
  named read-only SA, so "everyone writes except agent-ro" would have
  let agent-ro write. A name beats the wildcard; the read-only list
  beats the read-write one; an SA named in both reads.
- **D14 A read-write grant keeps the role's own scope.** §4.3 also
  prefix-bounded writers with a session policy. That changes every
  existing `sts` writer at upgrade, and an action it misses (a multipart
  verb, a conformance probe) fails every writer at once — a hardening
  with its own rollout, not part of read-only. A reader's policy has no
  such risk: readers are new.
- **D15 A `static` broker without a read key still serves readers, and
  says so.** Refusing them would break every existing read-only
  passthrough mount on a static broker (the chart's default backend).
  The `issued` line, `/v1/status` and a start-up warning say
  `cooperative`, and `broker.static.readSecretRef` makes it enforced.
- **D16 `AccessIsolation` is `Unknown` under the broker.** The operator
  cannot see the broker's backend, and the chart's default backend is
  `static` without a read key, so `True` would be false on a default
  install. The broker reports its own `readEnforcement`.

**Verified.**

- Unit: 307 passing across `s3csi`, `passthrough`, `lean_operator`,
  `forge_operator` and `lite_gateway`. New: the precedence table
  (13 rows), the old-CR and old-registration wire shapes, authorization
  returning the grant, the registration narrowed by the CR in both
  directions, the session policy's exact JSON with a check that every
  action is a `Get` or `List`, `sts`/`rest`/`static` mints against
  capturing servers (the `Policy` form field present for a read grant and
  absent for a write grant), `readEnforcement`, the static read-key
  config, `on-behalf-of` bounds, the registration's wire spelling, and
  `AccessIsolation` per mode.
- Mutations, each failing the test meant for it: no `Policy` on a read
  grant; the static read key handed to writers; `rest` always told
  `readWrite`; a read-only volume registering `readWrite`; the control
  character check dropped; `static` reported `Unknown`; the passthrough
  CRD's read-only list misspelled; `decide` ignoring the registration;
  `authorize` ignoring `csi.readOnly`; `s3:PutObject` in the policy; the
  named read-only rule dropped; the wildcard read-only rule dropped.
- **Live, against MinIO RELEASE.2025-09-07** (`lean/e2e/access/read-grant-minio.sh`,
  13/13 on two consecutive runs). A parent user whose own policy is
  bucket-wide `readwrite` (the too-wide role) calls `AssumeRole` with the
  exact policy the broker builds (written by its unit test):
  - a `FLINT_SYNC_ACCESS=read` `run` on the narrowed keys checks out,
    follows a writer's edit, add and delete, and is denied nothing it
    asks for;
  - the narrowed keys are denied PUT and DELETE under the prefix, and GET
    and LIST of another prefix; the parent's keys are allowed all four;
  - a READ-WRITE `barrier` on the narrowed keys fails with
    `store: not authorized: put_whole: 403 AccessDenied`, and the manifest
    pointer's etag is unchanged — the credential holds when flint's own
    mode is wrong (D1).
  - The script's own positive control: with `s3:PutObject` added to the
    policy, B1, D1 and D2 fail; with the prefix bound removed, C1 and C2
    fail.
  - One earlier run (bucket named `ws`, then fixed) logged three
    `dispatch failure`s from a reader's floor pull between successful
    ticks; three later runs logged none. Not reproduced, recorded.
- **Not covered:** the `authorize` call's `req.readonly` argument and
  `read_only: access.is_read()` in `publish_lean`/`publish_passthrough`
  (one line each at a publish only a node exercises); AWS STS's and Ceph
  RGW's evaluation of the policy; mount-s3 under a read grant; the
  `on-behalf-of` attribute through kubelet. All phase F.

**Found on the way, fixed 2026-09-14:** `flint-forge-chart/crds/flintrepos.yaml`
was stale — `crdgen forge` emits a `spec.packs` block (commit `baf11c7b`,
v1.48.0) the checked-in copy lacked. The forge operator applies its
compiled-in CRD at start, so only a fresh `helm install` was affected.
`release.sh chart` checked the share CRD and (skipping silently when crdgen
failed) the lean one, never forge's; one `refuse_stale_crd` now checks all
three and refuses on a crdgen failure.

### 10.6 Phase F: the live drill on EC2 (2026-09-14/15)

**Rig.** trove cluster `acc`, all spot (control plane + 2 workers,
i4i.large, us-west-1, kernel 6.18, kubeadm, containerd); a versioned
private bucket; three IAM users and one role, made and deleted by
`s3csi/e2e/aws-access-iam.sh`. The drill is `s3csi/e2e/aws-access.sh`, run
after `run-s3csi.sh setup`. Three broker arms, one helm upgrade each:

- **A, sessionPolicy.** Backend `sts`, answered by `s3csi/e2e/sts-shim.py`: AWS
  STS cannot verify this cluster's pod tokens (its issuer is not
  published), so the stand-in calls AWS `AssumeRole` on a role whose own
  policy is bucket-wide and forwards the broker's `Policy` form field
  unchanged. It logs which exchanges carried a policy, independently of the
  broker. What stays real: the policy flint builds, AWS's evaluation of it,
  and the keys the syncer and mount-s3 use. What it replaces: AWS verifying
  the JWT, which the broker has already TokenReviewed.
- **B, readKey.** Backend `static` with `broker.static.readSecretRef` (a
  read-only IAM user).
- **C, cooperative.** Backend `static` without one, the chart's default.

**Three runs.** Evidence is under `s3csi/e2e/results/access-2026-09-14-run{1,2,3}/`:
the drill log, broker and stand-in logs per arm, and each syncer's log.

| run | image | result | what it found |
|---|---|---|---|
| 1 | `access-2dd1428b` (phases A-D as committed) | 75 ok / 17 bad | the read-only bind defect; the broker's secret key in its log; two rig timing defects |
| 2 | `access-robind2` (+ staged ro bind, + redaction) | 98 ok / 8 bad | the host copy is now `ro`, but the container's copy is `rw`: the runtime remounts it |
| 3 | `access-robind3` (+ lean refusal) | 90 ok / 1 void, then arms B+C 16 ok / 0 | clean through A0-A13 (A1b the refusal; every reader's container copy `ro`); a spot reclaim took worker `acc-aws-1` eight minutes after A13, voiding A12's finish and the first B/C, rerun on the surviving worker (`-run3-bc`) |

**Finding F-1 (defect in phase A, fixed): a read-only lean bind was `rw` on
the host.** `fuse::bind_mount` bound the tree into the target and then
remounted the target `ro`. The plugin's `/var/lib/kubelet` is
Bidirectional, and propagation carries mount EVENTS, not a later
`MS_REMOUNT` of one instance's flags. The plugin's instance read
`ro,nosuid,nodev`; the host copy kubelet hands the runtime read `rw`. The
unit tests could not see this and the drill's first cut read only the
container: for `r-flag` (`csi.readOnly: true`) kubelet's own readOnly made
the container read-only, which hid the rw bind. `r-sa` (read-only by its
ServiceAccount, `readOnly: false`) wrote its tree. Measured on the node,
in a namespace sharing the plugin's propagation and then inside the plugin
container itself:
- bind-then-remount leaves the host copy `rw,noatime`, and a host write lands;
- util-linux 2.39's one-call `mount --bind -o ro` gives the same result;
- binding the target FROM a bind that is already `ro` gives a host copy
  `ro,nosuid,nodev,noatime`, the host write is refused, and the stage's
  unmount propagates away (0 left on the host).

The fix does that last shape: the stage is `<vid>/ro-stage`, and every
removal path unmounts it.

**Finding F-2 (design, decided): the plugin cannot make a lean container's
view read-only; only the pod's `readOnly` can.** Run 2 on the fixed bind:
`r-sa`'s host copy was `ro`, its container's `/workspace` was `rw`, and the
write landed. The runtime binds the volume into the container and
remounts it with the pod's `rw`/`ro`. The lean tree's superblock is
writable, because the syncer writes it, so nothing on the plugin's side
survives that remount. A passthrough reader is different: `pr` got EROFS
with `readOnly: false`, because mount-s3's FUSE superblock is mounted
read-only.
- **D17 A lean read grant must be asked for with `readOnly: true`, or the
  publish is refused** (`resolve::lean_read_needs_read_only_volume`,
  PermissionDenied, naming the fix). This supersedes §4.1's "`readOnly:
  false` never widens" for lean, which was narrowing with no container-side
  effect. The alternative, a published tree whose writes go nowhere, is
  the silent discard D5 exists to prevent. Passthrough keeps narrowing
  (§4.1), because there the narrowing reaches the container.
- **D18 The host-side `ro` bind stays** (F-1's fix) although kubelet decides
  the container's flags. It is correct for anything that reads the host
  path, and gVisor's gofer is the case that matters here. Not measured:
  this rig has no `runsc`.

**Finding F-3 (security, pre-existing since v1.45.0, fixed): the broker
logged its static secret access key at INFO.** The start-up line printed
`backend = ?cfg.backend`, and `Backend` derived `Debug`. Found when run 1's
evidence was scanned for the drill's own secrets before commit; it was
redacted from the committed log. `Backend` and `StaticKeys` now implement
`Debug` by hand, and a test plus a mutation cover it. Run 2's broker log
reads `secret: <redacted>` for both key sets. Rotate any static broker key
used under v1.45.0-v1.53.0.

**Rig defects, found and fixed in the drill:**
- `rollout status` returns while the outgoing broker pod is still
  terminating and answering. Run 1's pods on the second node were minted
  by the OLD static broker: unscoped keys, never seen by the stand-in. The
  drill now waits for one broker pod, then 20 s for the service proxies.
- A denied `aws s3 cp` answers `403 Forbidden` from its HeadObject, not
  `AccessDenied`.
- A lean delete publishes after two scans, so the drill runs two boundaries.
- The rig's `minio/mc` image no longer pulls from Docker Hub; it now uses
  `quay.io/minio/mc`.

A12 (the narrowed writer) passed in runs 1 and 2; nothing between run 2 and
run 3 touches its path (the lean refusal acts at a new publish, and a
running writer is only re-registered), and its run-3 finish is void.

**What held on real nodes and AWS, in every run** (run 3's count above):
- **Grants and keys:** each pod's `issued` line carried the access and
  enforcement its CR and volume imply; `on_behalf_of` reached the audit line.
  Every reader's exchange carried a policy and no writer's did, per the
  stand-in's own log. The policy AWS received was byte-identical to
  `read_session_policy`.
- **AWS evaluation:** the keys a reader actually HELD were denied PUT and
  DELETE under the prefix, and GET and LIST of another prefix; they read
  their own prefix. A writer's keys (the bucket-wide role) did all of it.
- **Convergence and publishing:** two writers' publishes reached both
  readers and each other, and a delete reached the readers. Readers never
  held the fence, never ran a barrier, and were denied nothing on their
  read grants.
- **Passthrough:** a reader read a writer's object through mount-s3 on a
  read grant (the `s3:prefix`-conditioned `ListBucket` is enough for
  mount-s3), ran `--read-only`, got EROFS, and its keys were denied PUT.
- **Precedence and refusal:** `serviceAccounts: ["*"]` with a named
  read-only viewer gave that viewer a read grant; an SA in neither list was
  refused with both lists named.
- **Narrowing a live writer:** moving it to the read-only list minted it a
  read grant at its next refresh (692 s in run 1). After its old key
  expired its next publish was not acked, nothing landed, and its syncer
  logged `REFUSED reason=auth … 403 AccessDenied`. Its tree stayed writable:
  the bind is decided at publish, only the key changes.
- **Static backends:** `readKey` handed a reader the read user's key, which
  could not write; `cooperative` said so on `/v1/status` and at start, and
  the reader's key could write. That is the documented limit.
- **Plain directory (run 2 on):** a plain-directory tree (`sizeLimitGib: 0`,
  no loop device) behaved the same as the loop-image tree.

**Not covered:**
- gVisor (`runsc`), including whether its gofer honours the host copy's `ro` (D18);
- Ceph RGW's evaluation of the session policy;
- AWS STS verifying a real cluster token;
- the lean operator's `AccessIsolation` condition on a cluster (no operator
  was deployed; unit-tested only).

### 10.7 Phase E, as built (2026-09-14)

**The store wrapper.** `flint_store::ReadOnly<S: ?Sized>`
(`crates/flint-store/src/readonly.rs`), re-exported at the crate root and
by the gateway. It implements every `ObjectStore` method by name:
- **15 writes, refused with nothing sent:** `put_whole`, `copy_object`,
  `compose_generation`, `delete`, `delete_if_match`, `delete_version`,
  `presign_put`, `ensure_noncurrent_retention`, `abort_upload`, `bootstrap`,
  and the five epoch writes (`epoch_acquire`, `epoch_renew`,
  `epoch_release`, `epoch_enqueue`, `epoch_handoff`). Each answers
  `StoreError::Auth("read-only store: <verb> <key>")`.
  - `presign_put` is a write: the URL is a write credential handed onward.
  - `bootstrap` is a write: S3's writes lifecycle rules and a probe object.
- **16 reads, forwarded:** `head`, `get_whole`, `get_range`,
  `get_range_segments`, `list`, `head_version`, `get_version`,
  `list_versions`, `presign_get`, `lifecycle_rules`, `list_uploads`,
  `epoch_read`, plus `min_part_size`, `max_parts`, `upload_gate` and
  `request_counts`, which describe the inner store and send nothing.
  - The eight defaulted ones are forwarded explicitly. A default left in
    place runs on the wrapper, so S3's streaming `get_range_segments` would
    fall back to `get_range`'s copy, and `get_version` would answer "this
    backend has no version-scoped GET".

**The workspace.** `Workspace::read_only(store, prefix)` wraps the store
and sets a `read_only` flag. `is_read_only()` reports it.
`VerbError::ReadOnly` is 403 `read-only` and not retryable. The flag is a
`bool`, not `flint_lean::Access`: the published flint-lean 0.6.0 that the
gateway builds against has no `Access`. A crate-private `writable()` is the
first statement of every writer, before path checks and before any request:
- `put_file`;
- `remove_files` (and `remove_file` through it), `rename_files`
  (`rename_file`), `withdraw_removal`;
- `request_verb` (`request_boundary`, `request_sync`);
- `open_window`, `clear_window`, `drop_inbox`, `cas_manifest`;
- `put_draft`, `delete_draft`, `promote_draft`.

The readers are `get_file`, `snapshot`, `status`, `wait_cited`,
`get_draft` and `list_drafts`. The router and `GatewayCore` are unchanged:
the binary still builds writable workspaces behind its one bearer.

**Corrections to §4.6.**
- There is no `StoreError::Forbidden`. The wrapper uses the existing
  `StoreError::Auth` (401/403, "not allowed"). A new variant would be a
  breaking change to flint-store, whose `StoreError` is not
  `#[non_exhaustive]`.
- §4.6's writer list missed five verbs: `request_sync` (it writes the
  inbox cell) and the four syncer-facing verbs.
- The store wrapper is more than a backstop. `Workspace::store()` is
  public, so for a caller who writes around the verbs the wrapper is the
  only layer in this crate that refuses.
- Size: §8 said about 150 lines. As built, the non-test code is 205 lines
  without comments or blank lines: 165 in the wrapper, mostly the trait's
  31 signatures, and 40 in the gateway. With comments it is 244 and 63.
  The tests are about 630 lines.

**F10, unit-tested.** Each test was checked against a mutation of the layer
it pins:
- flint-store:
  - `a_read_only_store_refuses_every_write_unsent_and_forwards_every_read`
    calls all 31 methods through a double that records each call by method
    name. `MemoryStore` cannot see a `get_range_segments` left to its
    default, because its op counts read `get_range` either way.
  - `every_trait_method_is_filed_and_every_one_is_implemented_by_name` is
    a census. It parses the trait, the wrapper's impl and the double's
    impl, and fails when the trait gains a method nobody filed.
- gateway (`tests/verbs.rs`):
  - `a_read_only_workspace_refuses_every_writer_before_it_touches_the_store`
    (zero requests; it asserts "no store write" before the typed answer);
  - `a_read_only_workspace_store_refuses_a_write_that_goes_around_the_verbs`;
  - `every_writer_writes_on_a_writable_workspace`, the control on
    `Workspace::new`;
  - `a_read_only_workspace_reads_what_writers_wrote_and_sends_only_reads`;
  - `every_public_verb_is_filed_as_a_reader_or_a_writer`, the census of
    `pub async fn` in `workspace.rs` and `drafts.rs`;
  - the `ReadOnly` row of the wire table in `http.rs`, and a README doctest.

**Mutations.** Each applied only where its text occurred exactly once,
restored, and checked byte-identical afterwards.

| mutation | failed | what it showed |
|---|---|---|
| M1 `writable()` removed from `put_file` | `a_read_only_workspace_refuses_every_writer…` | answered `Store(Auth("read-only store: put_whole tenant/proj1/files/w.txt"))`; the no-store-write assert before it held, so the wrapper refused with the typed check gone |
| M2 `writable()` removed from `put_draft` | same test | `Store(Auth("read-only store: put_whole …/drafts/u/body/d.txt"))`; the wrapper still refused |
| M3 `read_only` skips the wrapper | `a_read_only_workspace_store_refuses_a_write_that_goes_around_the_verbs` | the bypassing PUT landed; the typed test still passed, so each layer is load-bearing on its own |
| M4 `ReadOnly::put_whole` forwards | `a_read_only_store_refuses_every_write_unsent_and_forwards_every_read` | `put_whole … answered Ok(()), not Auth` |
| M5 `ReadOnly::get_version` removed (trait default) | both flint-store tests | the census names the missing method; the behavioural test got `Other("this backend has no version-scoped GET")` |
| M6 `get_range_segments` present but running the default's body | the behavioural test only | "did not reach the inner store as itself"; the census passed, so only the recording double catches this |

**Not covered.** A read-only workspace on a store whose credential really
cannot write (D1) is not run here; phase F measured that credential
separately. The gateway binary has no read-only door. Filling `author`
and the draft `user` from a verified `sub` is the embedder's job (§4.6).

### 10.8 The local drill: a real OIDC STS, gVisor, AccessIsolation (2026-09-14)

Three things §10.6 left uncovered, run on one kind node on the Mac (M1,
arm64; Docker 4 GiB), images and the operator built at `135335b9`:
`s3csi/e2e/local-access.sh` (`setup`, then the legs), fixtures
`local-access-tenants.yaml`, evidence
`s3csi/e2e/results/local-access-2026-09-14/` (`run.out`: **53 ok, 0 bad**).

**O — the broker's `sts` backend against a real OIDC STS.** MinIO's
OpenID provider is the cluster's own service-account issuer
(`https://kubernetes.default.svc.cluster.local`), with client id
`s3.csi.chert.us` and a `readwrite` role policy; the discovery document and
JWKS are reachable in-cluster once `system:service-account-issuer-discovery`
is bound to unauthenticated (rig only). No stand-in signs anything:
`sts-tap.py` forwards the broker's POST byte for byte and logs the
`Policy` it carried.
- O0: MinIO refuses a token for another audience (`azp must match
  configured OpenID Client ID`) and exchanges one for `s3.csi.chert.us`;
  without a session policy those keys write under the prefix and read
  another (the control).
- O2/O2t: a writer and a reader on one workspace; the broker says
  `readWrite none` and `read sessionPolicy`, and the tap independently saw
  one exchange with a policy and three without, all 200. The policy on the
  wire: `GetObject`, `GetObjectVersion` on `s3bucket/access/lean/*`,
  `ListBucket`, `ListBucketVersions` on the bucket; no
  `GetObjectAttributes`.
- O3: the reader's HELD keys: PUT and DELETE under the prefix and GET of
  another prefix denied, GET under it allowed; the writer's PUT.
- O5: passthrough: mount-s3 on the read grant reads the writer's file with
  that policy (so dropping `GetObjectAttributes`, `135335b9`, costs
  mount-s3 nothing on this read path) and refuses a write.

This settles §10.6's open question for MinIO only: community MinIO
accepts a Kubernetes SA token as a web identity and applies the session
policy. AWS STS with a public issuer, and Ceph RGW, are still not run.

**G — gVisor (`runsc` release-20260907.0, systrap, RuntimeClass `gvisor`).**
Installed into the kind node by `setup` (the aarch64 tarball, sha512
checked; a containerd `runsc` handler with the systemd cgroup driver).
Inside the sandbox the volume is a 9p mount in `directfs` mode.
- G1 (F7): a runsc writer wrote 1000 files (17..1016 bytes) and touched
  `publish` at once; the ack said `ok`, the manifest cited all 1000 at the
  sizes written, and a runc reader reached the same digest. It did not have
  the directory before the publish. G1c: the same from a runc writer.
- G2: a runsc reader gets EROFS in the tree and in `.flint/`, and reads the
  writer's 1000 files byte for byte.
- G3 (D18): the runsc reader's host bind is `ro,nosuid,nodev`, and **the
  gofer serving its container's `/workspace` mounts it `ro`** (read from
  the gofer's own `/proc/<pid>/mountinfo`, matched to the pod through
  `crictl inspect`); the runsc writer's are `rw`. The host-side read-only
  bind is what a gVisor gofer serves from.
- G4 (§5): the UDS door is bound in each tenant's own tree (host side) and
  the socket is visible inside the sandbox, but a runsc tenant's connect is
  refused (`ECONNREFUSED`); a runc tenant on the same workspace connects.
  The file protocol is the interface under gVisor, as §5 said.

**I — AccessIsolation.** The lean operator (built from the same tree,
chart `flint-lean-chart`) on MinIO: `static` and `ambient` → `False` /
`Cooperative`; `broker`, `webIdentity` and no identity → `Unknown` /
`DecidedByBroker`; no consumers → no condition, on a workspace that
carries `SpecAccepted` and `SyncerObserved` (so the operator did reconcile
it). Patching `static` to `broker` moved the condition, with a new
`lastTransitionTime`.

**Two runs were killed before this one, and not by a defect:** the leg
that first exported the read policy by running the broker's unit test
compiled `spdk-csi-driver`'s test binary, and with the kind node up the
Mac ran out of memory twice (macOS killed the drill). The policy now comes
off the wire through the tap, which is also the stronger evidence.

**Not covered, still:** amd64 nodes and runsc under KVM; AWS STS against a
public issuer; Ceph RGW (the `GetObjectAttributes` finding is from RGW's
source); the gateway binary's lack of a read-only door (§10.7).

