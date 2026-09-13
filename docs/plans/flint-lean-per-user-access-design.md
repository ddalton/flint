# flint-lean — per-user access through the mount, and a read-only posture: investigation and design

Date: 2026-09-13. Status: **INVESTIGATION + DESIGN, no code.** Written the
day after the protocol review (`flint-lean-protocol-review-2026-09-12.md`)
against `37ff97d0` (v1.51.0). Every code claim below is `file:line` at
that commit.

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
   Lean has exactly one writer per workspace: a second writer pod waits
   on the lease until the first releases it (`verbs.rs:80-97`) and
   never becomes Ready meanwhile. Readers are unlimited. So an NLX
   session is one writer plus N readers, or N workspaces, or (unbuilt)
   branches.
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
- D7 **One writer per workspace stands.** Multi-agent sessions are one
  writer plus readers, or many workspaces, until branching is built.
- D8 **The branching design's §4.5 is re-based on this document** (S4).
