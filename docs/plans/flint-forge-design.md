# flint forge — a git server with S3 behind it: design of record

Status: **PHASES 1-5 LANDED, AND LFS** — `forge/syncer` (the `flint-forge`
crate: the syncer, both hooks, restore, the sweep, `/status`, the
branch policy, the credential helper and the legible export), the door
(`lite_gateway::git` with `Door::Git`), and the operator
(`forge_operator`: the `FlintRepo` CRD, the renderer, the one-rung
ladder, the reconciler and `flint-forge-operator`), with the chart and
the two server images, the fleet levers and the agent guide. What
remains is phase 0's measurement and the cluster drills — falsifiers 7,
8 and 9's bucket half, all of which need a cluster. Written 2026-09-04; **revised
the same day after a 15-agent review** (five lenses, one refuter per
significant finding; record in §16), and §14 records what phase 1
actually built against what it planned. Working name "flint forge" (the user's earlier
name was "flint-git"; the name is the user's call, §15). A fourth front
end, sibling to lite, lean and passthrough, and deliberately NOT lean's
branching design (`flint-lean-branching-design.md`), which stands as
lean's own answer for any-file workspaces. This one is for code.

The user's rule: *"we don't have to reinvent a lot of components and
still use battle tested components."* Forge's data model is git and
its server IS git — the same `receive-pack`, `upload-pack`, hooks,
`merge-tree` and `repack` that GitHub, GitLab and Gerrit run, served by
nginx and `git http-backend` the way every self-hosted git has been
served for fifteen years. Flint supplies exactly what git lacks in a
cluster with an S3 store: a door with pod identity, durability of the
local repo into S3 with an acknowledgement that means it, a lifecycle
that idles to zero, and an optional legible export so the rest of the
family can read what forge holds. The review cut the new code further:
the door is lite's gateway with git glue, the merge endpoint is a push,
and the export is the shipped `flint-sync` binary.

## 0. What it is, and what it is not

A repo server per `FlintRepo` CR, in lite's topology: agent pods are
stock git clients; a stateless door authenticates them and routes to
the repo's server pod; the server runs real git over a local disk that
is a CACHE; S3 is the truth, written on every push before the push is
acknowledged.

| | lite | lean | forge |
|---|---|---|---|
| agent sees | a POSIX mount over NFS | a local POSIX checkout | a git remote |
| durable unit | every write, tiered | every changed file, per floor tick | a commit |
| what is stored | any file | any file, one legible object per path | tracked files, in packs; plus an exported legible tree of chosen refs |
| long-running component | the hub, in the data path | none | the server, in the data path; idles to zero |
| who moves main | n/a | the credential | the server: hooks and policy, as every forge does it |
| the bucket, without flint | tier layout | a legible tree | **a bare git repository: `git clone` works against it read-only with the server down** (§3) |

Not in scope: a web UI, pull requests and review, code search, CI, a
POSIX volume, untracked files. An agent's `.env`, build output,
datasets and checkpoints are not durable in forge unless committed or,
in phase 6, put in LFS. Uncommitted work is not durable either, which
is git's contract; §7 adds an optional snapshot sidecar for harnesses
that want an RPO on the working tree anyway.

## 1. What is reused, and what is new — corrected by the review

| Need | Reused | From |
|---|---|---|
| serve clones, fetches, pushes | nginx + fcgiwrap + `git http-backend` (a CGI), smart HTTP; `Git-Protocol` forwarded so sessions are protocol v2 | git, nginx |
| decide and confirm a push per ref | the `proc-receive` hook (git ≥ 2.29): the server executes the ref update and reports `ok`/`ng` per ref | git; Gitee/AGit in production |
| branch protection, who may push what, who may merge | `pre-receive` with `REMOTE_USER` in the environment | git |
| server-side merge without a worktree, **as a push** | a push to `refs/for/<target>` (AGit plumbing, Gitea/Gitee) handled by the same `proc-receive`: `git merge-tree --write-tree`, `commit-tree`, `-o strategy=` via `receive.advertisePushOptions` | git ≥ 2.43 for `-X` |
| local GC and packing | `git repack -a -d -b`; `receive.unpackLimit = 1` so a push is always a pack; **`receive.autogc = false`, `gc.auto = 0`** — the syncer is the only writer of the pack directory (§10) | git |
| cheap clone storms | reachability bitmaps; bundle URIs; `--single-branch` clones (§8) | git |
| read-only DR clone from the bucket | the dumb HTTP protocol: `HEAD`, `info/refs`, `objects/info/packs` | git |
| S3 with conditional writes, listing, an epoch lease | `flint-store`: `PutCondition::{IfMatch,IfNoneMatchAny,Unconditional}`, `epoch_{read,acquire,renew,release}` | flint |
| the immutable-objects-plus-one-CAS'd-pointer pattern and its GC rules | `LeanChunkGC.tla`'s four rules over packs (§10); the lease claim loop, the lost-renew rule and the takeover rotation from `lease.rs` / `manifest.rs`, as SPEC — the code is typed on lean's `Sidecar` | flint-lean |
| **the door**: CR lookup from the request, `Decision::{Dial,Wake,Wait,Refuse}`, arming `chert.us/requested-at`, waiting on the CR (never the pod), a streaming reverse proxy over a closed verb table | `lite_gateway` (`flint-hub-gateway`): `resolve::decide_for` with a new `Door::Git` arm; `proxy::{arm_wake,wait_for_ready,send_upstream,relay}`; `route::Verb` with three git verbs | lite gateway |
| pod identity → principal | `identity_from_review` + `TokenReview` from the broker, **with a ≤ 60 s cache keyed by token hash** (§6); `Consumers` allow-list from the CSI policy block | flint-s3-csi |
| human auth | the gateway's bearer; Knox JWT per the CSI identity chain, later | flint |
| **the legible export** | `git archive <ref> \| tar -x` into a scratch tree, then the shipped **`flint-sync barrier`** publishes it as a lean workspace — lean's ordering, lean's model, no manifest code in forge (§9) | flint-lean binary |
| CR lifecycle, one idle rung, front-door wake, claim/adopt, `Refused` | the lite operator, **by copy-and-trim, not import**: `reconcile.rs` is typed on `FlintShare` and ~800 of its ~2,850 production lines are PVC/hibernate/reprovision/expand, dead under an `emptyDir` cache | lite operator |

**New code, all of it:** the syncer (§4: one process per repo owning
the writer lock, the lease heartbeat, batching, the S3 sync, repack,
export and a `/status` document); two hooks of ~25 lines each that
speak pkt-line to the syncer over a Unix socket; the door's git glue
(Basic-auth password → TokenReview, three streaming routes with
chunked bodies both ways, `Git-Protocol` and `X-Remote-User`
forwarding, a longer wake hold); the `FlintRepo` CRD and a trimmed
reconciler. No manifest format, no scanner, no merge algorithm, no
object store, no wire protocol, no CGI runner, no merge API.

Options checked and not taken (§16): Gitea/Forgejo/soft-serve/Gitaly
keep repos on local disk only; KEDA's HTTP add-on could replace routing
plus wake plus idle-to-zero with zero flint code but not the auth,
and the lite gateway already exists; JGit-S3/git-remote-s3 are dumb
remotes without a server.

## 2. The shape, and the topology decision

```
 agent pod                    flint-system                            tenant namespace
 ┌───────────────┐  HTTPS    ┌────────────────────────┐  HTTP       ┌──────────────────────────────┐
 │ git clone/push│ ────────▶ │ flint-hub-gateway,      │ ──────────▶ │ forge server pod (1 per repo) │
 │ cred helper:  │  basic    │ Door::Git               │  chunked    │  nginx+fcgiwrap+http-backend  │
 │  SA token as  │  auth     │ TokenReview (cached)    │  both ways  │  hooks → UDS → syncer         │
 │  password     │           │ Consumers; route; wake  │  X-Remote-  │  syncer: lock, lease, batch,  │
 └───────────────┘           └────────────────────────┘  User        │   S3 sync, repack, export,    │
                                                                     │   /status                     │
                                                                     └──────────────┬───────────────┘
                                                                                    │ flint-store
                                                                                    ▼
                                                                       S3  <prefix>/git/…  (§3)
```

**One server pod per `FlintRepo` in v1 — DECIDED on the review's
numbers**, with three trims to lite's shape and a hedge:

- **Trims.** A headless Service, not a ClusterIP: git carries the repo
  in the path, so the door routes to the pod and forge does not spend
  3,000 ClusterIPs (lite's fleet plan calls that 73 % of a GKE-default
  /20). Requests sized for git — 50m / 64Mi — not the hub's 100m /
  128Mi: an idle git server is ~1.5 MB RSS. One idle rung: with an
  `emptyDir` cache, Suspended already destroys the local repo, so
  Hibernated (lite: "scaled to zero and the PVC deleted") is
  meaningless and its code is dropped.
- **The hedge.** The server is written multi-repo-capable from day one
  because git already is: `GIT_PROJECT_ROOT` + `PATH_INFO` serve many
  repos from one root, `core.hooksPath` shares hooks, and the syncer
  keys its lock, lease and snapshot etag by repo path. Moving to N:1
  is then an operator change, not a server change.
- **Why 1:1 now.** Writer = reader: a fetch never consults S3 and never
  serves stale refs. One lease per actor, one prefix per credential,
  cgroup isolation per repo, and every lifecycle mechanism (idle
  ladder, wake-by-annotation, claim/adopt, prefix-overlap arbitration)
  exists for this shape and none for N:1, which needs a repo-to-replica
  assignment that agrees with the lease, an LRU that takes and releases
  leases, per-repo fairness inside one process, and a stale-read check
  — thousands of lines with new failure modes, which the lite fleet
  plan already warned against ("the blockers are per-reconcile rate
  terms, not topology; sharding adds consensus surfaces").
- **Numbers**, 10 % of repos live at once, 110 pods/node, requests as
  above, ~5 Kubernetes objects per CR:

| repos | live pods | nodes by pod slots | reserved | operator idle rate | lease heartbeat |
|---|---|---|---|---|---|
| 100 | 10 | <1 | 0.5 vCPU / 640 MiB | 0.33 rec/s | 1 PUT/s |
| 1,000 | 100 | 1 | 5 vCPU / 6.4 GiB | 3.3 rec/s | 10 PUT/s |
| 3,000 | 300 | 3 | 15 vCPU / 19 GiB | 10 rec/s (the fleet plan's baseline) | 30 PUT/s ≈ $389/mo (fleet plan's number) |
| 3,000 all awake | 3,000 | 28 | 150 vCPU / 188 GiB | — | 300 PUT/s |

- **Triggers to move to N:1**, recorded rather than argued: (a) live-
  at-once repos above ~1,000, or reserved compute past budget; (b) a
  tenant with many tiny repos touched briefly, where pod scheduling
  (seconds) dominates a restore that is itself sub-second; (c) operator
  reconcile latency ≥ 1 s at the fleet size, the fleet plan's unbounded
  regime. None fires at 100 or 1,000 repos at 10 % live.

## 3. The bucket: a bare repository, plus one CAS'd object

```
<prefix>/git/objects/pack/pack-<sha>.pack       immutable, content-named by git; unconditional PUT
<prefix>/git/objects/pack/pack-<sha>.idx
<prefix>/git/objects/pack/pack-<sha>.bitmap     with the pack it belongs to, so a restore is clone-ready (§8)
<prefix>/git/objects/pack/pack-<sha>.rev
<prefix>/git/objects/info/packs                 derived, dumb protocol — written BEFORE info/refs
<prefix>/git/info/refs                          derived, dumb protocol
<prefix>/git/HEAD                               derived, once
<prefix>/git/snapshot                           THE pointer: {seq, epoch, refs{name: oid}, packs[name], bundles[name], exported_commit}; CAS'd
<prefix>/git/epoch                              the single-writer lease (flint-store epoch cell)
<prefix>/git/claim                              the operator's project claim
<prefix>/git/bundles/<oid>.bundle               phase 5: clone bundles (§8)
<prefix>/lfs/objects/<oid>                      phase 6
<prefix>/files/<path> + <prefix>/.flint/lean/…  phase 4: the legible export, a valid lean workspace (§9)
```

- **`snapshot` is the only mutable object the server trusts.** Packs
  are immutable and content-named. Refs live in the snapshot, so a
  batch that moves thirty refs is one CAS and a reader never sees half
  of it. The derived files exist for git's dumb protocol and for
  humans; the server never reads them. `packed-refs` and `config` are
  not uploaded — a dumb clone does not read them (verified).
- **The bucket is a bare git repository.** With the server down, a
  static HTTPS view of `<prefix>/git/` is a read-only dumb remote.
  Two S3 details are UNVERIFIED and are falsifier 4's job: the bucket
  must return `info/refs` while ignoring git's `?service=` query
  string, and a dumb clone probes loose objects and must see 404 and
  not 403 for the misses (S3 answers 403 without `s3:ListBucket`).
- **Versioning is OFF for a forge prefix**, or its noncurrent
  expiration is one day. The snapshot is CAS'd by etag and nothing pins
  a version; under the batch rate of §4 a versioned prefix would mint
  a snapshot version per batch for no reader.

## 4. The push path — one syncer per repo, acknowledged means durable

The review's central finding: **`receive-pack` serialises nothing.**
One process per push, one `proc-receive` per process, and with
`receive.procReceiveRefs` set git performs neither the old-oid check
nor `receive.denyNonFastForwards` for the handed-off commands — the
hook "is responsible for updating the relevant references"
(githooks(5)). Verified on 2.50.1: two concurrent pushes ran their
hooks fully overlapped, and a push with a stale old-oid was accepted
by receive-pack once the hook said `ok`. A design that put the S3 sync
inside the hook, as the first draft did, would have (a) let two pushes
to one ref CAS the snapshot before anyone checked old == current, so
the bucket held an unacknowledged oid and lost an acknowledged one,
and (b) at fleet rates turned every self-inflicted 412 into a full
restore. It would also have run three dependent S3 round trips per
push — 300–600 ms at AWS's small-object latency — for 1.7–3.3
pushes/s against 16.7/s arriving from 1,000 agents at a 60 s cadence.

So the hook does not sync. **The syncer does**, one long-lived process
per repo, the pod's main container (its exit restarts the pod, §5),
holding the repo's writer lock for every path to S3.

**The hook** (`proc-receive`, ~25 lines of pkt-line): reads the
command list and push options, finds its own incoming pack by the
`.keep` file naming its parent `receive-pack` pid (the pack is already
in `objects/pack/` — quarantine is migrated before `proc-receive`
runs, verified), sends `{commands, options, REMOTE_USER, pack}` to the
syncer over the pod's Unix socket, waits, and relays the per-ref
report. `pre-receive` runs first with `REMOTE_USER` and the rendered
policy, and refuses before anything reaches the syncer.

**The syncer's batch**, under the writer lock:

1. **Collect** every pending push (a bounded batch; at 16.7 pushes/s
   and a ~400 ms batch, ~7 pushes per batch).
2. **Check each command**: refuse with `ng <ref> stale, fetch first`
   unless BOTH the local ref and the last-synced snapshot's `refs[ref]`
   equal the command's old-oid; apply fast-forward and policy here,
   since receive-pack no longer does. A `refs/for/<target>` command
   runs the merge (§6) and packs the objects it created with
   `pack-objects` — `merge-tree --write-tree` and `commit-tree` write
   LOOSE objects, which a pack-only upload would silently omit
   (verified; the first draft's merge endpoint had exactly this hole).
3. **Renew the lease** — one `epoch_renew` for the batch, in parallel
   with step 4. A 412 takes lean's lost-response rule first (re-read
   the cell; if it still names this holder at this epoch, adopt the
   token); anything else is the fence: `ng` to every waiting hook,
   stop serving, exit (§5).
4. **Upload** every pack the batch's accepted commands brought, with
   `.idx`, `.bitmap`, `.rev` — `Unconditional`, content-named,
   idempotent; the rewrite refreshes the age the sweep reads
   (`LeanChunkGC` rule 4). A refused push's pack is deleted locally
   once its `receive-pack` has exited and dropped the `.keep`.
5. **ONE snapshot CAS**, `If-Match` on the etag this syncer last
   synced, carrying every accepted ref and the full pack list. Under
   the lock a 412 can only mean another server: a straggler after a
   roll, or this pod after a takeover rotation. It is the fence:
   `ng` to all, stop serving, exit.
6. **Apply** all ref updates as one `git update-ref --stdin`
   transaction, THEN report `ok` to every hook — per-ref `ok` reaches
   the client as it is emitted, so a report interleaved with updates
   would acknowledge a subset the snapshot already holds in full.
7. Best effort after the report: `objects/info/packs`, then
   `info/refs` (a dumb clone that reads fresh refs against a stale
   pack list fails; the reverse serves the previous state), then the
   export (§9) and the sweep (§10) if a repack happened.

Capacity is batches per second — one two-round-trip chain per batch —
and pushes per batch grow with load, so the syncer gets faster per
push as the fleet gets busier. S3 requests per batch: one renew, two
to four per new pack, one CAS, two derived; not seven per push.

**MEASURED 2026-09-05, and the code did not match this paragraph.**
Counted against the memory store's op counter, a batch cost FIVE fixed
requests, not four: `HEAD` — "derived, once" in §3 — was re-PUT on
every batch, restating `ref: refs/heads/main` forever. It is now
published on change only, and the fixed cost is the four this
paragraph claims. Steady state fell from 8 requests per push to 7.

The "two-round-trip chain" was also optimistic in a second way: pack
siblings were uploaded one `await` at a time, so a batch's dependent
chain was two round trips PLUS one per sibling, in series. Siblings
are independent, immutable, content-named keys written unconditionally
and are now uploaded with bounded concurrency (4), which is what makes
the chain the length this paragraph says it is. The bound exists
because `put_whole` holds the body in RAM.

Three tests hold this shape — the fixed cost, the HEAD rule, and the
per-pack term — so a regression that adds a round trip to every push
is caught here rather than on a bucket's request bill.

No path acknowledges a push the bucket does not hold. A syncer crash
between steps 5 and 6 restarts the pod, which restores from the
snapshot (§5) and so cannot serve stale refs; the client saw a failed
push and retries into a ref already at the new oid.

## 5. Start, restore, idle, takeover

**Start.** Claim (`git/claim`; refuse a foreign `projectId`, lean's
rule). Acquire the lease: a clean release claims at once; after a
crash, wait out the quiet polls (6 × 10 s). **Rotate the snapshot**
on an unreleased takeover — `seq + 1`, same content, one small CAS —
so any straggler's `If-Match` is stale before the successor serves a
byte; lean's `rotate_for_takeover`, and the mutation `LeanNoRotate`
is why. Then restore: GET `snapshot`; fetch the listed packs with
their `.idx`/`.bitmap`/`.rev`; write `packed-refs` and `HEAD`; `git
fsck --connectivity-only`; open the socket; serve. A pack the
snapshot names and the bucket lacks is refused loudly — but first
re-read the snapshot and retry once, because a repack under the
holder may have moved the list while this reader was fetching
(lean's revalidate rule).

**Heartbeat.** The lease is renewed on a 10 s timer whether or not
pushes arrive — the first draft renewed only inside a push, which
left a quiet server's lease expiring and a straggler unfenced. A 412
on the timer takes the lost-response rule, else the server
**self-fences: stops serving reads as well as writes, and exits.** A
deposed server that kept answering `upload-pack` would serve stale
refs indefinitely.

**Restore time.** After a repack the repo is ONE pack, so a restore is
one object at single-stream rate (lean's fan-out parallelises across
objects, not within one). Measured on this repository: one 42.5 MB
pack, `fsck --connectivity-only` 0.11 s, sub-second on loopback. At
the EC2 rates of record (130–256 MiB/s), 1 GB is 4–8 s and 10 GB is
40–80 s; ranged parallel GET is the lever if phase 0 finds the tail
matters (`flint-store` has no ranged GET today). Uploading `.bitmap`
with the pack is what makes the restored repo clone-ready without a
`repack -b` (42 s / 125 CPU-s on a 1 GiB corpus).

**Idle.** Lite's ladder, ONE rung: no git traffic for
`suspendAfterSecs` ⇒ replicas 0 and `chert.us/idle-state`. The ladder
suspends only on a polled `/status` document (`hubstatus.rs`'s shape:
at minimum `{phase: serving, activity.idleSecs, rpoClean}`) — a poll
failure Holds forever — so the syncer serves one; `rpoClean` means
the last acknowledged push is in S3, which is always true by §4. The
next request at the door arms `chert.us/requested-at`; the door waits
on the CR, not the pod, and **holds the request for up to 180 s**, not
lite's 25 s: git clients do not retry a 503, and a wake after an
unclean death is 60 s of lease wait plus a restore. `emptyDir` in v1;
a PVC is the lever if phase 0 measures restores as too slow.

**Roll.** A new pod for a repo whose old pod is alive: the new one
waits out the quiet polls or takes a clean `preStop` release, rotates,
restores, serves; the old pod's next heartbeat or batch 412s and it
exits. The models that proved this for lean apply without change: the
actors are the same, the cell is the same, the pointer is one object.

## 6. Identity and authorization — who moves main

**Agents.** A credential helper in the agent image presents the pod's
projected ServiceAccount token (audience `forge.chert.us`) as the HTTP
basic password. The door runs `TokenReview` (the broker's
`identity_from_review`), **caches the verdict for ≤ 60 s keyed by the
token's hash** — a clone is two to four requests carrying the token,
so 1,000 clones would otherwise be 3–4,000 reviews at the apiserver —
checks the repo's `spec.consumers`, and forwards `X-Remote-User =
system:serviceaccount:<ns>:<sa>`. Agents never hold an S3 credential.

**Humans.** The gateway's bearer today; Knox JWT via the CSI identity
chain when that lands; an operator-issued per-CR token is the interim.

**The `X-Remote-User` boundary, stated.** `REMOTE_USER` is what
`pre-receive` and the syncer read as the principal, and it arrives as
the door's `X-Remote-User` — which the door sets from a verified
`TokenReview`, and which no caller can smuggle past it because the
door builds its upstream headers from an allowlist. But anything that
can reach the server pod's git port directly can set that header
itself, and both enforcers would believe it. Three things stand in the
way: the port is on a headless Service with no external address, the
repository is in its tenant's namespace, and the operator renders a
NetworkPolicy admitting only the gateway's pods — rendered only when
the operator is told where the door runs (`door.namespace` in the
chart). Where it is not rendered, **reaching the port is the
authorization**, which is a defensible posture on a single-tenant
cluster and is not one on a shared cluster. A per-repository shared
secret derived from the gateway's HMAC root (`lite_gateway::derive`'s
`Minter`, which already solves exactly this for the file API) is the
upgrade when a CNI that cannot enforce NetworkPolicy has to be
supported; it is not built.

**Policy**, rendered by the operator, read by `pre-receive` and by the
syncer's step 2:

```yaml
spec:
  consumers: { serviceAccounts: [agent-runner] }
  branches:
    protected: [main, release/*]           # no direct push; move only via refs/for or a listed pusher
    pushers:  { main: [system:serviceaccount:team-a:release-bot] }
    agentPattern: "agent/*"                # what an agent principal may create and push; its own only
    mergeInto: { main: [system:serviceaccount:team-a:agent-runner] }   # who may push refs/for/main
```

**Merge is a push.** There is no merge API. `git push origin
HEAD:refs/for/main [-o strategy=theirs]` (Gitea's and Gitee's AGit
flow; `receive.advertisePushOptions = true`). `pre-receive` checks
`mergeInto`; the syncer runs `git merge-tree --write-tree` (git ≥ 2.43
for `-X ours|theirs`), on conflict reports `ng refs/for/main
conflict: <paths>` and moves no ref — the conflicted tree's objects
are unreachable garbage the next repack drops — else `commit-tree`,
packs the new objects, and the batch CAS carries `main`; the client
sees `ok` with `option refname refs/heads/main` and the new oid.
`refs/for/` is never stored. Verified end-to-end on 2.50.1 in a
46-line hook. One path to S3, structurally, rather than by convention.

**Per-principal, on the wire (2026-09-05).** `forge/e2e/run-rights.sh`
(with `rig-kind.yaml`) puts two ServiceAccounts through one repository
on a kind cluster whose CNI is Cilium — because kind's default CNI does
not enforce a NetworkPolicy, and the boundary below is the whole point.
A reader listed only in `consumers` is refused a direct push to `main`
(`is protected: push to refs/for/main`) and a `refs/for/main` merge
(`only system:serviceaccount:agents:forge-writer may propose merges`),
while the writer named in `mergeInto` merges and both read the result.
A forged `X-Remote-User` sent to the DOOR is overridden — the door sets
it from the verified token — but sent straight to the repo pod's git
port it is believed: with the rendered NetworkPolicy in place the
reader cannot open 8080 at all (the door can, the control), and with it
deleted the same forged header pushed to 8080 MERGES into `main`. That
pair is the evidence that `X-Remote-User` means something ONLY behind
the policy. The `open` repo, carrying no `branches` block, lets the
reader push straight to `main`: the permissive default, confirmed.
17 legs, green, twice.

## 7. The agent's workflow, and the RPO question

```
git clone --single-branch -b main https://forge/<ns>/<repo>.git   # v2 ls-refs with a prefix: no 1,000-ref advertisement
git switch -c agent/$POD
… work …
git commit -am "…" && git push -u origin agent/$POD              # durable when this returns
git push origin HEAD:refs/for/main                               # propose; ok or ng with the conflicted paths
```

Nothing is mounted, nothing is privileged, no sidecar is required.
The agent's project repo is the served repo, so lean's nested-`.git`
problem does not arise. An external remote coexists as a second
remote; phase 6 adds a mirror job (`git fetch --prune
'+refs/*:refs/*'` on a schedule — `git fetch` has no `--mirror`).

Agent branches stay under `refs/heads/agent/`, and clones use
`--single-branch`: a full clone of a repo with 1,000 one-commit agent
branches costs 0.54 CPU-s instead of 0.13 and a 74 KB advertisement
per request (measured). A nightly job prunes branches whose pod is
gone. Partial clone (`--filter=blob:none`) is for agents with a sparse
working set, not a storm lever (§8).

**Uncommitted work has no RPO**, git's contract, and forge keeps it.
A harness that wants one anyway runs `spdk-csi-driver/docker/forge/wip-snapshot.sh` in
its OWN pod: `GIT_INDEX_FILE=<tmp> git add -A`, `write-tree`,
`commit-tree -p HEAD`, `push --force <c>:refs/wip/<pod>` — plumbing,
because `git commit` against a throwaway index still moves HEAD (the
first draft said "never touching the agent's branch" over a command
that did).

**It is NOT a CRD field**, and the correction is worth recording:
`spec.wipSnapshots` was in the phase-3 CRD and is gone. Forge owns
repository servers, not agent pods, and injects nothing into them —
lean's webhook was removed for the same reason — so a spec field asking
for a sidecar would have been a field the operator silently ignores,
which is worse than no field at all.

## 8. Clone storms — corrected

A thousand agents cloning one repo at once. Measured on this
repository (89 MB, 25k objects) with bitmaps: a full clone costs the
server 0.13 CPU-s and 42.9 MB of egress; on a 1 GiB corpus, 2.5 CPU-s
and 1.05 GB. **Egress binds before CPU**: 1,000 clones are 130 CPU-s
(16 s on 8 cores) but 43 GB from one pod's NIC (34 s at 10 Gbps); on
the 1 GiB corpus, 1.05 TB (14 min). Memory is not a factor (private
RSS ~2.4 MB per clone; the rest is shared page cache).

1. **Reachability bitmaps** — `repack.writeBitmaps` is already on for
   bare repos; the bitmap covers the last `-a` pack, so clones after N
   pushes walk N small packs until the next repack (§10's cadence).
   `.bitmap`/`.rev` are uploaded with the pack (§3).
2. **Bundle URIs** — the lever that moves the storm to S3. The syncer
   cuts a full bundle **hourly with a clamp**, not per merge (a bundle
   is a full-repo copy: 0.26 s on this repo, 5.8 s / 1.05 GB on the
   1 GiB corpus, then a PUT of that size), lists it in the snapshot,
   and advertises a presigned URL (`uploadpack.advertiseBundleURIs`
   + `bundle.*` — undocumented in git-config(1) at 2.50.1 but what
   upload-pack reads; a query string is fetched intact). Verified:
   the server's share of a clone fell from 42.55 MB to a 5.8 KB
   remainder. **Three conditions the first draft missed**, each
   verified: the client must opt in with `transfer.bundleURI = true`
   (default false — a stock client ignores the advertisement) or
   `--bundle-uri`; both sides ≥ 2.40; and the session must be protocol
   v2, which `http-backend` sees only if the door forwards the
   `Git-Protocol` header as `GIT_PROTOCOL` — a door that drops it
   silently degrades every clone to v0. Presigned URLs expire (≤ 7
   days), so the syncer re-signs on a schedule shorter than that.
3. **Partial clone is NOT a storm lever.** `--filter=blob:none` makes
   the clone itself cheap (2.8 MB, 0.02 CPU-s) but the agent's first
   checkout fetches every HEAD blob in one batch at 2.8× the CPU of a
   bitmapped full clone (measured: 0.37 CPU-s vs 0.13). It is for
   sparse working sets only.

## 9. The legible export — the shipped lean binary

Per repo, optional: `export: {refs: [main], prefix: …, everySecs: …}`.
After a batch that moved an exported ref, the syncer runs `git
archive <ref> | tar -x -C <scratch>` and then **`flint-sync barrier`**
— the shipped lean sidecar binary — over that tree, which publishes it
as a lean workspace with lean's own ordering (upload, CAS, deletes
LAST; the first draft's "PUT files; deletes; then the manifest" was
exactly `LeanDanglingOrder`, the mutation lean's model refutes). The
syncer records `exported_commit` in its NEXT snapshot CAS, so the
export never becomes a second snapshot writer racing pushes. No
manifest code in forge, no `LeanConfig` plumbing, ~30 lines. Lean and
passthrough readers mount `main` read-only with no forge code in
them. The export is a mirror derived from git, never a source of
truth; a foreign write to `files/` is overwritten by the next export
and the CRD says so.

## 10. GC and the sweep — the syncer is the only writer of `objects/pack/`

`receive.autogc` is OFF and `gc.auto = 0`. With every push a pack,
git's default would run a detached `maintenance run --auto` behind
every push and a full `repack -a -d --cruft` every 50 pushes — a
second, unowned writer of the pack directory that can delete a pack
the syncer is uploading, and whose consolidated pack must then be
uploaded before the next push can be acknowledged. Instead the syncer
repacks under its own lock, between batches, when the pack count
passes a threshold or on a schedule: `repack -a -d -b`, then a batch
that uploads the new pack with its `.bitmap`/`.rev` and CASes a
snapshot naming only it, then the sweep. A full repack rewrites every
byte (0.24–0.41 s on this repo, 6.4 s wall on 1 GiB), so the
threshold is the knob that trades clone cost against repack cost.

The sweep deletes packs and bundles not in the current snapshot's
lists, under `LeanChunkGC`'s four rules with "chunk" read as "pack":
list candidates first; read the snapshot after the listing and abort
if its etag moved; HEAD each candidate at delete time and require an
age past `orphanGraceSecs`, a grace that must outlive the longest
upload; a retry's re-upload refreshes the age. Pack names are content-
derived but differ across clients' delta settings, so the reference
predicate is "named by the snapshot whose etag is the If-Match",
never "same objects". `ForgeSync.tla` instantiates the module.

## 11. Failure model

| failure | effect | why |
|---|---|---|
| syncer crash mid-batch | every push in the batch fails at the client; the pod restarts and restores; retries succeed | §4 ordering; the syncer is the main process |
| node lost | restore from S3 on the replacement; clones resume | local disk is a cache |
| S3 unreachable | **pushes fail, clones and fetches keep working** until the lease TTL, then the server self-fences | the syncer refuses to acknowledge; a server that cannot renew cannot know it still holds the repo |
| door down | all git traffic fails | in the path; N stateless replicas |
| two servers for one repo | the straggler 412s at its next heartbeat or batch and exits; the successor rotated first | the single-writer fence, lean's models |
| snapshot names a pack that is gone | re-read the snapshot once; if still gone, refuse to start and name it | fail-closed, lean's `load` rule |
| a repack races the sweep | the sweep aborts on the moved etag and reruns | rule 1 |
| a stale push (old-oid ≠ current) | `ng stale, fetch first` from the syncer's step 2 | receive-pack does not check it under proc-receive |

## 12. Prior art, and the warning it carries

AWS CodeCommit was this architecture as a managed service — a git
server with packs in S3 and refs in DynamoDB — and on 2024-07-25 AWS
closed it to new customers, recommending GitHub and GitLab. It ran for
nine years at scale, so the warning is not technical. It is that a git
server competes as a FORGE — pull requests, review, integrations, an
ecosystem — and storage is not where that contest is decided.

Forge does not enter that contest. Its job is the one the hosted
forges do not do: an in-cluster repo server with pod identity as the
credential, S3 as the only durable state, idle-to-zero economics,
LAN-speed clones for a fleet that starts a thousand pods at once, a
merge policy that says which principals may move `main`, and a legible
export the rest of the family can mount. A team keeps GitHub for humans
and review; forge is where the agents work, and a mirror job connects
the two. A clone through the server is the standard smart-protocol
path and the client never touches S3; what is forge's own problem is
§8, which was never CodeCommit's.

**The buy option, recorded.** Forgejo or Gitea on a lite share needs
zero flint code: a full forge UI on a POSIX volume that tiers to S3.
Their repository root must be a local path (only LFS, attachments and
packages can use S3), so there is no per-push S3 durability, no
idle-to-zero, no pod identity, no legible export, and git over NFS is
slow in ways that matter for a fleet. Phase 0 runs it as the control.

**RUN 2026-09-04 on EC2.** Gitea 1.22, the same 40 MiB corpus at the
same commit, the same storm clients and the same NIC-counter oracle:

| | server egress, 100 clones | wall |
|---|---|---|
| forge, bundle advertised + client opt-in | **0.5 MiB** | 27 s |
| forge, opt-in off | 4,041 MiB | 58 s |
| forge, not advertised | 4,042 MiB | 56 s |
| **Gitea (the buy option)** | **4,019 MiB** | 66 s |

The bought forge lands on top of forge's CONTROL arms, within 0.5%.
That is the honest statement of what is being bought and given up: a
full UI and zero code to maintain, against every clone coming off one
pod's NIC because there is no bundle-URI mechanism to move it to the
object store. At fleet scale that is the ~8,000x.

It also says something about the earlier measurement: a real bought
forge behaves the same as forge with its lever off, which is what
those controls were claiming to simulate.

**What this run could NOT test, stated plainly.** It ran on a LOCAL
volume, not a flint POSIX one, so the "git over NFS is slow" half is
unmeasured — the flint NFS hub on this cluster refuses with
`F30 REFUSAL (exit 57): export "/mnt/volume" has neither identity
marker nor flint state` and the pNFS pods sit Pending, because trove's
manual blobstore disk-init was never run here. That is a trove
provisioning gap, not a finding about either forge. The remaining
differences — no per-push S3 durability, no idle-to-zero, no pod
identity, no legible export — are architectural and follow from the
repository root having to be a local path; they are not measurable and
do not need to be.

## 13. Falsifiers

1. **Acknowledged means durable.** Kill the syncer between step 4 and
   step 5: every push in the batch FAILS at the client; the bucket
   holds the previous snapshot; the restart restores; retries succeed;
   `git fsck` is clean. Control: sync moved after the report — the
   push is acknowledged and the restore lacks the commit.

   **RUN 2026-09-04 on EC2 — GREEN, 8 kills mid-push.** No
   fault-injection hook exists, so this is the black-box form: push a
   12 MiB payload, kill the pod at a random moment inside the window,
   check the invariant. Both sides were sampled (2 acknowledged, 6
   refused) and `fsck --strict` was clean after all 8.

   **The relation is an implication, not an equivalence, and the first
   run asserted the equivalence and "failed" twice.** Kill the pod after
   the CAS but before the response reaches the client and the client
   sees a broken connection while the bucket holds the commit. No system
   whose acknowledgement can be lost in flight can do better, and git
   push carries no idempotency token to reconcile against. What is
   checked instead is that the indeterminate outcome is BENIGN: the
   agent's natural retry returns `Everything up-to-date` with the ref
   already at its commit. Told-ok-but-absent — the direction that
   silently loses work — never occurred.
2. **Concurrent pushes to one ref.** Two clients push `L→N1` and
   `L→N2` concurrently: exactly one gets `ok`, the other `ng stale`;
   the bucket and the local ref agree. Control: with step 2's
   snapshot-side check removed, the bucket holds an oid no client was
   told about.

   **RUN 2026-09-04, both arms, on EC2** (`forge/e2e/f2-concurrent.sh`).
   Treatment green in both timings: same-batch (two pushes inside the
   400 ms window) gives one `ok` and one `ng stale info: fetch first`;
   cross-batch (staggered past the window) gives one `ok` and one
   client-side `fetch first`. In both, the bucket snapshot, the
   server's own ref and the winner's oid agree, and the loser's commit
   is on no ref.

   The control — the shipped tree with ONE line removed, the
   `eff.insert` after each accepted command — fails, and how it fails
   corrects this paragraph. Both clients are told `ng`, not `ok`:
   `update-ref --stdin` refuses a transaction carrying two updates for
   one ref, the batch goes fatal and the syncer restarts. But **the
   bucket ends up holding one of the two commits anyway**, because
   step 5 CASes the snapshot BEFORE step 6 touches a local ref, and
   step 5 folds `accepted` into a map where the last write wins. The
   restart then restores from that bucket. So a client was told its
   push was rejected while its commit became the branch.

   The lesson is about which check is load-bearing. git's own
   transaction refusal looks like a second line of defence and is not
   one: it fires after the snapshot is already durable. **The step-2
   overlay update is the only thing standing between two racing
   pushes and a bucket that disagrees with what every client was
   told.**
3. **Loose objects never leak.** A `refs/for/main` merge is
   acknowledged, the pod is killed, the restore passes `fsck`.
   Control: skip the `pack-objects` in step 2 — the restore fails.

   **RUN 2026-09-04 on EC2 — GREEN.** A `refs/for/main` merge with two
   parents was acknowledged, the snapshot already named the merge
   commit, and the pod was deleted — taking the loose objects with the
   emptyDir. After the restore: `fsck --strict` clean, `main` still at
   the merge commit, **zero loose objects**, and the merge commit
   readable from the restored packs.
4. **The fence.** Two server pods for one repo: the straggler's next
   heartbeat 412s and it exits within the heartbeat interval; a push
   routed to it fails. Control: without the rotation, a straggler's
   batch lands after the successor restored.

   **RUN 2026-09-04 on EC2 — GREEN.** A second server pod for one
   repository WAITS rather than serving — `another server holds
   drill/git (0/6 quiet polls)` — and never becomes ready. Partitioning
   the incumbent from S3 (its heartbeat logging `epoch_put: dispatch
   failure`) let the newcomer count 0→5 and take over. On healing, the
   incumbent's very next renew produced `fenced: deposed at renew:
   epoch_put: 412 PreconditionFailed` and it exited, within one
   heartbeat interval.

   The "a push routed to it fails" half turns out stronger than
   expected: a deposed pod is not ready, and a headless Service
   publishes only READY pods, so it has no endpoint at all. The door
   cannot route to it even by accident.
5. **Restore fidelity and DR.** Cold restore from S3 alone: refs equal
   the snapshot, `fsck --connectivity-only` passes, a clone is
   byte-identical; with the server scaled to zero, `git clone` over
   the dumb protocol from the bucket succeeds — this leg also settles
   the two UNVERIFIED S3 behaviours of §3.

   **RUN 2026-09-04 on EC2 — GREEN, 8 legs.** Deleting the pod destroys
   the emptyDir, so the replacement has nothing but the bucket. Restored
   refs were EXACTLY the snapshot's and equal to what preceded the kill;
   `fsck --strict` clean; a clone byte-identical (`40eea7e35cf13978…`).

   **The DR half needs the right transport, and the first attempt used
   the wrong one.** The bucket carries `info/refs` and
   `objects/info/packs` — the DUMB HTTP layout, exactly as §3 claims. A
   LOCAL clone reads neither; it reads `refs/` and `packed-refs`,
   neither of which is in the bucket, so `git clone <synced-dir>` fails
   for a reason that says nothing about the bucket. Served over HTTP,
   stock git clones it with no forge anywhere in the path and the result
   passes `fsck --strict` with identical content. For an offline
   runbook, two steps open the synced prefix directly:
   `mkdir refs && cp info/refs packed-refs`.
6. **Protected main.** An agent's push to `main` is refused by
   `pre-receive` naming the rule; its push to `agent/<pod>` lands; its
   push to `refs/for/main` merges for a listed principal and returns
   `ng` with the conflicted paths otherwise, moving no ref.

   **RUN 2026-09-04 on EC2 — GREEN, after it found a bootstrap
   defect.** The direct push is refused with `refs/heads/main is
   protected: push to refs/for/main to propose a merge`; the
   `agent/agent1` push lands. The third leg failed: on a NEW
   repository `refs/for/main` was refused with `no such merge target`,
   because `main` did not exist yet — so between the protection rule
   and the missing target, **`main` could never be created and a fresh
   repository was unusable from birth**. Every merge unit test seeded
   `main` by direct push first, which is why none of them could see
   it. A merge request into the DEFAULT branch now creates it, which
   is within the authority `mergeInto` already checked; into any other
   missing ref it is still refused, so this cannot conjure arbitrary
   refs.
7. **Idle-to-zero.** Replicas 0 after `suspendAfterSecs`; a clone
   during suspension succeeds after a wake of up to the hold; the
   restore time is reported.

   **RUN 2026-09-04 on EC2 — GREEN.** With `suspendAfterSecs: 120`,
   the repository reached `IdleSuspended` at 0 replicas with no pods.
   A `git clone` through the door then completed in **11 s**, and the
   repository came back at `Ready`, 1/1, on a pod created BY the
   request. The door held the request for the wake rather than
   answering 503 — which is the whole point, since git clients do not
   retry.
8. **The storm, on EC2 only** (kind measures its host's loopback, not
   NIC egress or S3 fan-out): 1,000 concurrent clones with bitmaps and
   a bundle URI advertised AND `transfer.bundleURI=true` on the
   clients: server egress bounded, S3 carries the bytes. Controls:
   bundle URIs off — the server's NIC saturates; the client opt-in
   off — identical to bundle URIs off, proving the advertisement alone
   does nothing.

   **RUN 2026-09-04 on EC2 — GREEN, all three arms.** 1,000 clones of a
   40 MiB repository per arm, 3,000 in total, zero failures, 256 in
   flight. The oracle is the server pod's own NIC counter, not
   wall-clock: a storm that is slower but off the server's NIC still
   passes.

   | arm | server egress | wall |
   |---|---|---|
   | advertised + `transfer.bundleURI=true` | **5.7 MiB** | 129 s |
   | advertised, client opt-in OFF | 40,409 MiB | 460 s |
   | not advertised | 40,417 MiB | 467 s |

   A **7,000x** reduction, and 3.6x faster wall-clock as a side
   effect. The controls land within 0.9% of the predicted 40,052 MiB
   (1,000 x 40 MiB), which is what says the counter is measuring the
   right thing. **The two controls differ by 0.02%** — that is the
   second control earning its place: the advertisement alone does
   nothing at all, and since `transfer.bundleURI` defaults to false,
   the do-nothing configuration is the one a fleet gets by accident.

   Two traps, each of which produced a confident wrong answer first:

   - **A global `http.extraHeader` breaks the bundle fetch.** git sends
     it to EVERY host, including the presigned S3 URL, and S3 refuses a
     presigned request that also carries an `Authorization` header. git
     reports `failed to download bundle` only under `GIT_TRACE2` and
     silently falls back to a full fetch — so the lever looks inert for
     a reason that has nothing to do with forge. The first measurement
     read 40.24 MiB treatment against 40.46 MiB control and looked like
     a clean refutation of the design. Scope the credential to the
     door's URL, which is what the shipped `flint-forge-credential`
     helper does anyway.
   - **A restore came back advertising nothing** (§8, fixed by
     `bundle::readvertise`). Until that fix every arm run after a wake
     was measuring an unadvertised repository.
9. **Export.** lean mounts the exported `main` read-only and every
   file is byte-identical to `git show main:<path>`; a push changing
   three files rewrites one or two chunks and three objects; a reader
   resolving the manifest mid-export never finds a cited object gone.

   **RUN 2026-09-04 on EC2 — GREEN, 6 legs, after finding a defect that
   made the export freeze permanently.** All 164 files byte-identical to
   `git show main:<path>`, nothing in the export the tree does not have,
   and a three-file push rewrote **3 of 164** objects.

   **THE DEFECT. lean protects a workspace by making every upload
   conditional: an etag it did not last write means PARK, not
   overwrite.** That is right for lean, whose baseline lives on the
   volume with the workspace. Forge's export has no volume — the
   baseline sat in the export tree on the pod's `emptyDir` — so the
   first restart destroyed it, every object then looked foreign, and
   every file parked. Permanently, because nothing rebuilds a baseline:
   the published workspace froze while `main` moved on. The cluster run
   found `README.md` still holding the first seed's text with 164 files
   parked and `up=0`. The baseline is now preserved to the bucket after
   each successful barrier and rehydrated at startup
   (`export::preserve_baseline` / `rehydrate_baseline`).

   Residual, stated plainly: this prevents the loss, it does not repair
   a prefix already stuck — that still needs the export prefix cleared
   so it can republish. And a restart costs one full re-upload
   (`up=164`), because the materialised tree died with the pod and
   `materialize` falls back to its full path; the barrier after it was
   `up=3`.

   Two oracle bugs in the drill, both of which read as product
   failures: the snapshot's `exported_commit` LAGS BY DESIGN (an export
   never spends a CAS of its own; the next batch carries it), so the
   last push of a drill can never see it there — ask the server's own
   record instead. And `comm` compares whole lines, so listing objects
   with `sort -k2` reported 59 changed of which 58 carried the OLD
   timestamp.
10. **The sweep.** After a repack, old packs are deleted past the
    grace; a pack in the snapshot is never deleted; the probe asserts

    the sweep fired.

    **RUN 2026-09-04 on EC2 — GREEN, 5 legs.** 26 pushes drove the
    snapshot's pack count from 5 to a peak of **25**, past the threshold
    of 24, after which it collapsed to 10 — a repack, observed directly.
    Every pack the snapshot names was present; a cold restore after the
    repack passed `fsck --strict`.

    A repack writes NO log line, so it cannot be found after the fact:
    by the time the pushes finish the consolidation has happened and the
    count has climbed again. The first version polled afterwards, saw 5,
    and reported "no repack" while one had run. Sample DURING.

    The sweep's oracle also had to change. Demanding a log line marks
    correct behaviour as failure — a run whose orphans are all younger
    than the grace sweeps nothing, correctly. The invariant is what
    holds: every pack object in the bucket is either named by the
    snapshot or younger than the grace. 231 orphans, all within it.
11. **S3 outage.** Pushes fail with a clear message; clones and fetches
    succeed until the lease TTL; the server then exits rather than
    serving what it cannot prove it still holds.

    **RUN 2026-09-04 on EC2 — GREEN, 4 legs.** With egress to S3 denied
    (in-cluster traffic and DNS left intact, so the failure is an S3
    outage and not a DNS one): clones kept serving from local packs; a
    push was refused with `the repository server is not accepting writes
    (the syncer closed the connection without a report)`; the server
    stopped being ready ~20 s later rather than holding a lease it could
    not renew; and it recovered when S3 returned.
12. **Per-principal authorization and the header boundary.** On one
    repository, a reader (in `consumers`, in no push or merge list) and
    a writer (named in `mergeInto`) — the reader refused `main` and
    `refs/for/main`, the writer merged, both read. A forged
    `X-Remote-User` to the door is overridden; the same header sent
    directly to the repo pod's git port is trusted, so the boundary is
    the NetworkPolicy the operator renders.

    **RUN 2026-09-05 on kind + Cilium — GREEN, 17 legs (rerun 17).**
    `forge/e2e/run-rights.sh`. The vacuity-breaker: with the
    NetworkPolicy deleted (operator scaled to 0) the reader's forged
    `X-Remote-User: …forge-writer`, pushed straight to `:8080`, merged
    into `main`; the operator then re-rendered the policy and the port
    was blocked again. A NetworkPolicy is inert under kind's default
    CNI, which is why this runs on Cilium.


## 14. Phases

0. **Measure before building.** (a) Forgejo on a lite share, on kind:
   clone/push latency, git-over-NFS behaviour — the buy control. (b)
   A spike: nginx + fcgiwrap + `http-backend`, a `proc-receive` that
   hands off to a syncer doing §4 against MinIO — push latency with
   durability, batch size under load, restore time, on this
   repository. The product decision is made on (b) beating (a) where
   it matters.
1. **The syncer and the server pod.** — **BUILT** in
   `forge/syncer` (crate `flint-forge`), 27 tests green, clippy clean:
   the batch of §4 in `batch.rs` (staleness against both the local ref
   and the snapshot AND against the batch's own running view, the
   fast-forward test git no longer performs, `refs/for/*` merges with
   `-o strategy=`, the packing of server-created objects, one renew,
   one CAS, one `update-ref` transaction, then the reports); the lease
   with the heartbeat, the lost-response rule and the takeover rotation
   (`lease.rs`, `snapshot.rs`); restore with the revalidate-once rule
   and `fsck` (`restore.rs`); syncer-owned repack and the four-rule
   sweep (`sweep.rs`); `/status` in `hubstatus`'s shape (`status.rs`);
   the `proc-receive` relay and its pkt-line (`bin/flint_forge_hook.rs`,
   `pktline.rs`). Falsifiers 1–5 and 10 are decided in the battery
   against the memory store; 11 (S3 outage) and the cluster-scale legs
   are not. **Still to do in this phase:** `ForgeSync.tla` in the lean
   formal gate, and the suite against MinIO rather than the double.

   Three defects the tests found, each of which the unit battery alone
   would have missed: the git-floor check ran `git -C <repo>` before
   the repository existed, so a fresh server exited before creating the
   directory it complained about; a merge with no `REMOTE_USER` — a
   deployment without a door — hit git's "empty ident name" and
   surfaced at the client as a git internal error; and a git failure
   while judging ONE command propagated as fatal, taking the
   repository server down and failing every other push in the batch.
   All three were found by the end-to-end `git push` test and none by
   the 23 unit tests, which is the design's own rule about a wire
   feature having three parties.
2. **The door and the policy.** — **BUILT.** `lite_gateway::git`
   with a `Door::Git` arm on `resolve::decide_for`: HTTP basic whose
   password is the pod's projected token, `TokenReview` behind a TTL
   cache (a `Reviewer` trait, so the cache is a wrapper and a test can
   count what reaches the apiserver), `spec.consumers`, three streaming
   routes with no length limit, `Git-Protocol` and `X-Remote-User`
   forwarded and `Authorization` not, a 180 s hold, and the upstream
   URL built from the CR's endpoint plus a `&'static str` — the
   caller's segments are a lookup key and never a path. The
   `FlintRepo` CRD (`forge_operator::crd`) carries the branch policy,
   which `BranchPolicy::render` converts into the `flint-forge` crate's
   own `Policy` type, so a field either side adds and nobody maps is a
   compile error rather than a rule that stops being enforced. Both
   enforcers now read that one document: `pre-receive` at the edge for
   the message, the syncer's step 2 for the guarantee — a repository
   whose hooks are misconfigured must not become an open one, and a
   test removes `pre-receive` and proves it does not. The credential
   helper (`flint-forge-credential`) reads the projected token on every
   invocation, so nothing is cached and a deleted pod loses access when
   its token stops being renewed. Falsifier 6 runs through the wire.

   Two defects the tests found. `warp`'s `or` never reached the
   `git-receive-pack` route, because the `git-upload-pack` handler
   checked the literal INSIDE itself and answered 404 — consuming the
   request instead of rejecting it, so every push 404'd; the literal is
   in the path filter now. And the branch policy's own module records
   what it cannot express: `agentPattern` bounds the branch NAME, not
   its owner, because the principal a pod presents is its
   ServiceAccount and many pods share one.

   **Still to do in this phase:** mounting the door in the chart, and
   the human-auth interim (§15.13).
3. **The operator.** — **BUILT**, as a slim controller rather than a
   trim: lite's `reconcile.rs` is 4,000 lines because a share has a PVC
   to create, expand, verify and delete, a hibernate that must prove a
   clean flush first, a reprovision path and four Service types. A
   repository has an `emptyDir`, one rung and one port, so what was
   COPIED is the shape — label sets, ownership, the checksum-annotation
   trick and its deliberate absence — and what is shared is the code
   that carries a lesson: `lite_operator::idle::clock` (extracted, so
   both front ends enforce one skew rule) and `hubstatus::suspendable`
   (reused outright, which is why forge's `/status` was written in that
   shape).

   `forge_operator::render` produces a ConfigMap, a headless Service, a
   Deployment of one pod (syncer + nginx/fcgiwrap/`http-backend`,
   25m/32Mi each, `Recreate`, `emptyDir`) and — when the operator is
   told where the door runs — a NetworkPolicy. `idle` is the one rung.
   `reconcile` arbitrates the bucket subtree, applies the children,
   polls the server's own `/status`, computes the phase and moves the
   ladder. `flint-forge-operator` is the controller; `crdgen -- forge`
   emits the CRD; `flint-forge-chart` installs it;
   `spdk-csi-driver/docker/Dockerfile.forge-git` and `.forge-syncer.prebuilt` build the
   two server images.

   Four things worth recording. **The policy ConfigMap is deliberately
   not in the pod's checksum annotation** — a mount updates in place and
   both enforcers re-read the document, so rolling the server to change
   who may push would drop every clone in flight for nothing; the
   syncer re-reads between batches and a test proves an edit is in force
   for the very next push. **`preStop` was removed**: it runs BEFORE
   SIGTERM, so a sleep there only delays the signal — the clean lease
   release is on SIGTERM inside the syncer, and
   `terminationGracePeriodSeconds` is its budget. **Nesting is not a
   collision**, because everything a repository owns is under
   `<prefix>/git/`; only an exact prefix match is two servers over one
   subtree, and an EXPORT prefix is a claim too, which the CRD's own CEL
   rule cannot see because it can only compare a CR against itself.
   **The `X-Remote-User` trust boundary is a NetworkPolicy**, and it is
   opt-in: see §6.

   **Still to do in this phase:** falsifier 7 on a cluster, and the
   images published.
4. **Export** via `flint-sync barrier`. — **BUILT**
   (`forge/syncer/src/export.rs`). Forge writes no manifest: it
   materialises the ref's tree and runs the shipped `flint-sync
   barrier` over it, so lean's ordering — upload, CAS, deletes LAST —
   is inherited rather than re-derived, and `LeanDanglingOrder` is not
   reachable from here.

   **`git archive | tar -x`, which §9 first described, is not what it
   does**, and the substitution is the interesting part. That pipeline
   rewrites every file, so lean's next scan reads the whole tree as
   touched and re-uploads it; and it leaves a deleted path behind, so
   the export publishes a file the ref no longer contains, forever.
   What runs instead is a two-tree `read-tree -m -u <exported> <new>`
   against an index kept beside the scratch tree — the only form that
   touches exactly what changed AND removes what is gone. The full
   path (a first export, or a restart that lost the index) clears the
   tree first, keeping lean's own `.flint-sync/` baseline.

   The defect a test found: `checkout-index` without `-u` records
   content but not stat data, so the NEXT two-tree update refuses with
   "not uptodate" and falls back to the full path. Every export would
   have re-materialised the whole tree and made lean re-upload all of
   it — O(everything) forever, with one log line to say so.

   Two ordering rules of forge's own hold: the export runs AFTER the
   report, because it is derived data and nothing about it may delay a
   push; and it never CASes the snapshot — it stashes the commit and
   the NEXT batch's single CAS carries it, so the export is never a
   second writer of the one object that has exactly one.

   `spec.export.refs` is refused at admission unless it names exactly
   ONE ref: a lean workspace is one tree, and two refs in one prefix
   would be two writers of one manifest. **Still to do:** falsifier 9's
   bucket half — a lean mount of the exported `main`, and the
   O(changed) object count — which needs a real store.
5. **Fleet levers.** — **BUILT.** Bitmap upload landed in phase 1
   (`pack_siblings`). Clone bundles are `forge/syncer/src/bundle.rs`:
   cut on a floor, uploaded beside the packs, advertised through
   `uploadpack.advertiseBundleURIs` and the `bundle.*` section, and
   **re-signed at half the URL's TTL** — signing is local computation,
   so being early costs a config write and being late costs a client a
   dead URL. The presigned GET is new in `flint-store` (`presign_get`,
   defaulted to a refusal so a backend that cannot sign says so rather
   than advertising a URL that will not resolve). Bundles are swept by
   the same four rules as packs, from the same reference set.

   The pruner is `prune.rs`, and its rule is deliberately not a clock:
   a branch is taken only when it is ALREADY CONTAINED in the default
   branch — so nothing is lost that `main` does not have — AND has been
   quiet longer than the TTL, so a merge that just landed does not
   delete the branch out from under the agent still pushing to it. Its
   deletions travel the ordinary batch, one CAS and one transaction,
   because a ref the syncer moves outside that path is a ref the bucket
   does not know about.

   Like the export, neither a bundle nor a prune ever CASes the
   snapshot: the bundle is stashed and the next batch's single CAS
   names it.

   `docs/flint-forge-for-agents.md` is the guide — `--single-branch`,
   the credential helper and its audience, the `refs/for` flow, the wip
   script, and the three conditions bundle URIs need, the first of
   which is on the agent image (`transfer.bundleURI=true`, whose
   default is false).

   **Still to do:** the storm leg on EC2 (falsifier 8), whose control
   is the client opt-in switched off — provisioned only on the user's
   go, pure spot.
6. **LFS — BUILT** (`forge/syncer/src/lfs.rs`, `spec.lfs`). The batch
   API lives in the SYNCER, not the door: it needs the bucket
   credentials, which the door deliberately has none of, and the
   objects never pass through either — the response is presigned URLs,
   so a 4 GB checkpoint goes client-to-store and the pod sees a few
   hundred bytes of JSON. nginx routes `/info/lfs/objects/{batch,verify}`
   to the syncer; the door gains two static suffixes and keeps §1's
   path invariant.

   `flint-store` gained `presign_put`, and with it a trap worth
   recording: since SDK 1.66 the default `WhenSupported` adds an
   `x-amz-checksum-crc32` header to PutObject, and a presigned URL
   signs the headers it was built with — a git-lfs client does not send
   that header, so S3 answers 403 with nothing in it about checksums.
   The presigning client sets `WhenRequired`, scoped to itself because
   every other write in flint passes its CRC-64 explicitly.

   Rules the tests pin: an object already in the bucket is offered NO
   upload action, which is the dedupe that makes LFS cheap; a missing
   object is a 404 on THAT object, not a failure of the batch; a store
   that cannot be reached is a 503 and never "absent", which would make
   a client re-upload what is already there; an oid is 64 lower-case
   hex characters and nothing else, because it becomes an S3 key; and
   `verify` exists because a presigned PUT is a grant to write, not
   evidence that the write happened — its href comes from the DOOR,
   which is the only party that knows the URL the client reached.

   **Nothing sweeps LFS objects, deliberately.** An object is
   referenced by a pointer file inside some tree of some commit, so
   deciding one is unreferenced means walking every reachable tree —
   and lean's own `sweep_chunks`, safe against one reference set and
   unsafe the moment a second appeared, is the cautionary tale. An
   unreferenced object costs storage and nothing else.

7. **Later.** Mirror jobs; the shared multi-repo server behind §2's
   triggers; HITL commits from the gateway (as `refs/for` pushes);
   Knox; ranged restore.

## 15. Decisions and open questions

1. **The server is real git behind nginx; flint writes no git
   internals and no CGI runner — DECIDED.**
2. **One syncer per repo owns every write to S3 and to
   `objects/pack/`; hooks are clients of it — DECIDED** (§4, §10).
3. **Acknowledgement after S3 durability; one snapshot CAS per batch;
   transaction then report — DECIDED.**
4. **One CAS'd snapshot; packs immutable; the bucket is a bare repo;
   versioning off — DECIDED** (§3).
5. **Merge is a push to `refs/for/<target>`; no merge API — DECIDED**
   (§6).
6. **One server pod per repo in v1, server multi-repo-capable, N:1
   behind three recorded triggers — DECIDED** (§2).
7. **The door is the lite gateway with a `Door::Git` arm — DECIDED.**
8. **Export is `git archive` + `flint-sync barrier` — DECIDED** (§9).
9. **`emptyDir` cache with restore-on-start — RECOMMENDED**; PVC if
   phase 0 measures restores as too slow; ranged GET if the 10 GB tail
   matters.
10. **Forgejo-on-lite as the phase-0 control — RECOMMENDED.**
11. **Git floors: server ≥ 2.43** (`merge-tree -X`); **client ≥ 2.40
    with `transfer.bundleURI=true`** for the storm lever, any version
    otherwise.
12. **KEDA HTTP add-on** as the door — recorded, not taken: it
    replaces routing, wake and idle-to-zero with zero flint code but
    not the auth, and the lite gateway exists. Revisit if the lite
    gateway's git glue grows past a few hundred lines.
13. **Name** — the user's call; "flint forge" here, "flint-git" the
    honest alternative. **CRD kind** `FlintRepo`, short name `fr` —
    OPEN. **Human auth interim** — OPEN.

## 16. Review record (2026-09-04, 15 agents: 5 lenses, 10 refuters)

Confirmed by an independent refuter and folded in: concurrent
`receive-pack` processes with no serialisation and no old-oid check
under `proc-receive` (§4, two lenses); server-side merges write loose
objects a pack-only sync omits (§4/§6, two lenses); bundle URIs are
client opt-in, v2-only, and the door must forward `Git-Protocol`
(§8, two lenses); the lite gateway already is the door (§1/§2); the
merge endpoint is redundant with `refs/for` pushes (§6); the idle
ladder has one rung under `emptyDir` and needs a `/status` document
(§5). Topology: one pod per repo, with the numbers and triggers of §2.

Folded in without a refuter, on the reviewer's own commands or man
pages: partial clone is not a storm lever; egress binds before CPU;
1,000 agent branches cost every clone; `receive.autogc` is a second
writer; the takeover needed rotation and a heartbeat; the export
ordering was `LeanDanglingOrder`; `git fetch --mirror` does not exist;
`git commit` moves HEAD; `merge-tree -X` is 2.43; a conflicted
`merge-tree` still writes objects; derived files must be ordered;
`TokenReview` needs a cache; the lite door's 25 s hold and 411 on
chunked bodies; lean's fan-out and `cas_write_chunked` are not
libraries; the lite reconciler is copy-and-trim.

Refuted or narrowed: nothing the refuters examined was refuted; two
reviewer-cited scripts were missing from the scratchpad and the
refuters rebuilt the experiments, which then agreed — recorded so a
future reader knows the evidence was reproduced, not trusted.

## 17. Composition — two products on one bucket (drills C1–C5, RUN 2026-09-04)

Drills: `forge/e2e/composition/`. Local rig, real binaries, MinIO
(`export::run_barrier` execs the shipped `flint-sync`, so the second
party cannot be an in-process double). First run **30 passed, 12
failed**; after the C2 and C4 fixes and the C1 detection below, **45
passed, 0 failed, 8 accepted** — the eight are the conditions recorded
at the end of this section, and the suite is green while only those are
outstanding. Every
control and precondition is green, so the failures are findings rather
than rig noise.

The rule under test is the one the whole design rests on: *one prefix
has exactly one writer, arbitrated by an epoch lease and a conditional
PUT against one pointer.* It holds **within** a product. Across
products it is a convention, not a mechanism.

### C1 — forge and lean on one prefix do not contend

The lease keys are derived independently and disagree:

| product | epoch cell | claim cell |
| --- | --- | --- |
| forge | `<prefix>/git/epoch` (`lib.rs:210`) | `<prefix>/git/claim` |
| lean | `<prefix>/.flint/lean/epoch` (`lib.rs:389`) | `<prefix>/.flint/lean/claim` |

Pointed at one prefix, both acquire at epoch 1 under different holder
ids. No 412, no fence, no log line on either side. The drill's two
controls — forge against forge, and lean against lean — both contend
on this same rig, so the absence is the products' and not the rig's.

Nothing above them closes it either: `arbitrate` reasons over
`&[FlintRepo]` (`reconcile.rs:124`), so it cannot see a lean CR at all.

**Detection added** (drill now 16/4; the four failures are unchanged,
because nothing here enforces anything). Each product probes the
other's lease cell at claim time — `flint_store::layout::neighbours`,
one exact-key read — and prints what it found: the neighbour's kind,
its cell, its holder, whether it is holding or merely released, and
which field would move it.

This is deliberately NOT enforcement. Prevention belongs to whatever
assigns prefixes; refusing here would turn a diagnostic into an outage
the first time a stale cell outlived the workspace that wrote it.
What it buys is that the condition stops being silent, which is the
property an external control needs in order to be safe to rely on: the
control still does the preventing, and you find out when it did not.

Four things the drill pins:

- **An exact-key probe, not a listing.** One request instead of a
  paginated scan, it finds a WRITER rather than the litter one leaves,
  and it cannot be confused by nesting — a repository at `t/a`
  exporting to `t/a/inner` puts lean's cell at
  `t/a/inner/.flint/lean/epoch`, which is not the key `t/a` probes.
- **A writer alone on its prefix says nothing.** Drilled, because a
  check that cries wolf on every healthy deployment is a check somebody
  switches off.
- **A released cell is still reported**, worded differently: the
  objects are still there and the holder takes the prefix back when it
  restarts.
- **A published mirror does not probe, and its publisher does instead.**
  Forge spawns a barrier per export, so probing there would be a
  recurring read — and `run_barrier` echoes only child lines containing
  "barrier" (`export.rs:389`), so the warning would have been
  discarded. A check whose output is thrown away is worse than no
  check, because it reads as coverage. The export prefix is instead
  probed once by forge at startup, which is the right frequency: a
  prefix two products both write legitimately is likelier than most to
  have a third pointed at it.

What is still true: nothing prevents the collision, and the four legs
that fail here are the same four.

**C1b, minor:** the syncer's self-collision guard is
`prefix == cfg.prefix` (`flint_forge_syncer.rs:133`) — exact string
equality on a normalised prefix. A *nested* export prefix
(`tenant/A` exporting to `tenant/A/inner`) is accepted. Blast radius is
small: forge's sweep lists only `git/objects/pack/` and `git/bundles/`,
so nothing is destroyed. It is a guard asymmetry, not a defect.

### C2 — a second writer on the EXPORT prefix wedges the REPOSITORY

Here the violation *is* arbitrated — forge's export runs the real
`flint-sync`, so both parties contend on `<B>/.flint/lean/epoch`. What
the drill measures is the cost of catching it. Three shipped facts
compose:

1. `flint-sync`'s claim loop never gives up — `Waiting` sleeps 10 s and
   retries forever (`bin/flint_sync.rs:290-317`).
2. `export::run_barrier` awaits that child with no timeout
   (`export.rs:254`).
3. `export::maybe_run` is awaited **inline** in the serving loop
   (`server.rs:288`), and the lease heartbeat is a timer on that same
   `select!` (`server.rs:158-199`).

Measured, with both observables shown live first: forge's lease on
prefix A froze at an unchanged `renewed_unix`, and the next push timed
out at 30 s having been accepted at 0 s minutes earlier. **A
misconfiguration on B takes down A.**

**FIXED** (drill now 11/0). `run_barrier` spawns with `kill_on_drop`
and waits under `FLINT_FORGE_EXPORT_TIMEOUT_SECS` — default 300 s, the
export floor's own default, on the reasoning that an export which
cannot finish within the interval between exports can never keep up
anyway. On elapse the child is killed and the error is
`ForgeError::ExportBlocked`, which names the prefix and sends the
operator to `<B>/.flint/lean/epoch` rather than to the export.

Two things the drill forced that a timeout alone would have missed:

- **A hold-off, and a growing one.** With no backoff the serving loop
  re-enters the doomed barrier on the next batch and spends the whole
  timeout again: the same outage, paced. A FLAT hold-off of one timeout
  is not enough either — the blocker is a misconfiguration that stands
  until a human clears it, so forge would be blocked one timeout in
  every two. `backoff_secs` doubles per consecutive failure, capped at
  an hour, and any export that publishes resets it.
- **The hold-off must be stamped when the barrier is ABANDONED.** The
  first version used the `now` handed to `maybe_run`, which is read
  before the barrier starts — so the timeout consumed the entire
  hold-off and the backoff was always already expired. The drill
  reported "no backoff" against code that had one; the bug was real and
  in a different place than the failing assertion pointed.

**Residual, and it is not small.** The loop is still blocked for up to
one timeout, so pushes still stall for that long and the heartbeat
still cannot tick — with the default 300 s and `QUIET_POLLS` at 60 s, a
competing server could depose this one during a single blocked export.
The bound is now finite and the cause is now logged, which is the
difference between an outage and a mystery, but the structural fix is
to move the export off the serving loop entirely.

Two properties make it quiet. `run_barrier` reads the child's stderr
only after it exits, so the child's "waiting on the standing lease"
lines sit in an unread pipe and forge logs nothing. And the status
listener is a separate task (`server.rs:90`) still answering from the
last published phase, so the operator's readiness check — which needs
"the server's own word" — is satisfied by a wedged process.

### C3 — a foreign write into the export prefix is never repaired

`export.rs:27` states: *"A foreign write into its prefix is overwritten
by the next export, and the CRD says so."* **That is false.**

The barrier computes uploads and deletes from a LOCAL scan diffed
against a LOCAL baseline (`barrier.rs:469-475`); the only remote thing
it consults is the manifest pointer's etag. A foreign write moves no
pointer and changes no local file, so it is not in the diff.

Measured: object overwritten (etag confirmed moved), then two further
commits exported — each republishing the file git changed, proving the
export ran — and the foreign bytes stood throughout. Meanwhile the
snapshot's `exported_commit` continues to name a commit whose content
the prefix does not hold.

### C4 — every reader of a diverged export takes the foreign bytes

The prediction from reading was that lean would fail closed, because
`checkout` fetches each object at the etag the manifest cites
(`checkout.rs:257`). **The drill refuted that.** The loud refusal is
guarded by `if pinned` (`checkout.rs:258`) and fires only under a gated
citation; for the cadence/hybrid manifests the export actually writes,
the next arm takes over and is explicit:

> `// S3-wins: the object moved past the manifest (a HITL write not yet`
> `// re-cited). Adopt the CURRENT version` — `checkout.rs:279-291`

Correct for lean's own workspace, where that means a human wrote newer
bytes. On an export prefix, where forge is the sole legitimate writer,
it means someone wrote who should not have. Measured: manifest-less
reader served them silently; lean adopted them (rc=0) and materialised
them into the reader's tree; only a `git clone`, which never touches
the export, was unaffected.

**FIXED** (drill now 7/1). A manifest carries `sole_writer`, set by the
installing pass from config; forge's export sets it via
`FLINT_SYNC_SOLE_WRITER` in `barrier_command`. A reader that finds an
object off its citation in such a workspace refuses instead of
adopting, with a message that names the cause and — deliberately — does
NOT give the gated lane's `recover-staged` advice, since nothing was
staged and the thing to find is the second writer.

Three properties worth keeping:

- **The flag lives in the manifest, not the reader's config.** A reader
  that must be configured to be careful is one that will eventually be
  deployed without it. The drill's reader runs a default config and
  refuses anyway, because the workspace tells it to.
- **It is cleared by `merge` and restated by the installing pass**, the
  same discipline `pinned_reads` follows. Inheriting it would let a
  workspace keep refusing forever after one mirrored publish; not
  restating it would let a mirror quietly stop being one.
- **Ordinary workspaces are untouched.** The S3-wins arm is still what
  an agent's workspace uses, guarded by its own test: with the flag
  unset, bytes past the citation are still adopted.

**What the fix does not reach, and cannot.** A reader with no manifest
— a key and a GET, which is what a passthrough or lite mount is — has
nothing to check the bytes against and still takes the foreign write.
That leg still fails, honestly. It is the argument for repairing the
divergence at the source (C3), not for more reader code. And the flag
only protects a manifest published with it: an export written before
this change stays adoptable until it publishes again.

### C5 — a foreign DELETE is refused, and never restored

The very next match arm refuses a missing cited object
(`checkout.rs:292-298`, *"refusing a silent hole (mixed-writer
bucket?)"*). Measured: lean refused (rc=1) and invented nothing. But no
later export restores the object — the export uploads what changed
locally, and a deletion behind its back changed nothing — so the hole
is permanent.

The asymmetry is the useful result: **overwrite is adopted silently,
delete is refused loudly.** The safer-looking operation is the
dangerous one.

### Accepted conditions — the decision of record, 2026-09-05

Two of the five drills were fixed (C2, C4). The rest stand, **by
decision rather than by omission**, and the drills encode that: each
of the eight outstanding legs reports `KNOWN <id>` instead of `FAIL`,
the suite exits green while only accepted conditions are outstanding,
and a leg whose accepted condition stops reproducing reports `STALE`
and fails — because a record that has quietly become wrong is also
something a human has to look at. A suite that is permanently eight-red
is a suite nobody reads, and a real regression would have hidden in it.

| id | condition | judgement |
| --- | --- | --- |
| **A1** | forge and lean on one prefix do not arbitrate: disjoint cells, both acquire, neither fences (C1, three legs) | **Not fixed.** Prevention belongs to whatever assigns prefixes. Enforcement would mean unifying the claim cell key and adding owner identity — and a migration, since the operator writes today's key. Detection shipped instead (`flint_store::layout`), so the condition is audible. |
| **A2** | a nested export prefix is admitted; the guard is string equality, not containment (C1b) | **Not fixed, and small.** The blast radius was checked: forge's sweep lists only `git/objects/pack/` and `git/bundles/`, so nothing is destroyed. Untidy, not corrupting. |
| **A3** | the export can neither see nor repair a foreign change — an overwrite stands, a delete is never restored (C3 ×2, C5) | **Not worth fixing** (owner's call). It requires a second writer on the export prefix, which is a misconfiguration, and the readers that can be protected now are. A LIST-based reconcile would close it; the cost was judged not to earn it. |
| **A4** | a reader with no manifest is served the foreign bytes with no error (C4) | **Not fixable where it is observed.** A passthrough or lite mount is a key and a GET; there is nothing to check against. It closes only if A3 closes, and A3 is declined. |

The one-writer rule therefore remains a mechanism within a product and
a convention across them, deliberately. What changed is that breaking
it is no longer silent.

**C2's residual still stands**, and is not on this list because nobody
has judged it: an export awaited inline in the same `select!` as the
heartbeat is a liveness hazard even when bounded. Moving it to its own
task — with the snapshot CAS still carrying the published commit, so
the ordering rule in section 9 is untouched — would remove the stall
rather than cap it.
