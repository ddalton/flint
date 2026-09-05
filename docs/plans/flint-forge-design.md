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
A harness that wants one anyway runs `docker/forge/wip-snapshot.sh` in
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

## 13. Falsifiers

1. **Acknowledged means durable.** Kill the syncer between step 4 and
   step 5: every push in the batch FAILS at the client; the bucket
   holds the previous snapshot; the restart restores; retries succeed;
   `git fsck` is clean. Control: sync moved after the report — the
   push is acknowledged and the restore lacks the commit.
2. **Concurrent pushes to one ref.** Two clients push `L→N1` and
   `L→N2` concurrently: exactly one gets `ok`, the other `ng stale`;
   the bucket and the local ref agree. Control: with step 2's
   snapshot-side check removed, both get `ok` and the bucket holds an
   oid no client was told about.
3. **Loose objects never leak.** A `refs/for/main` merge is
   acknowledged, the pod is killed, the restore passes `fsck`.
   Control: skip the `pack-objects` in step 2 — the restore fails.
4. **The fence.** Two server pods for one repo: the straggler's next
   heartbeat 412s and it exits within the heartbeat interval; a push
   routed to it fails. Control: without the rotation, a straggler's
   batch lands after the successor restored.
5. **Restore fidelity and DR.** Cold restore from S3 alone: refs equal
   the snapshot, `fsck --connectivity-only` passes, a clone is
   byte-identical; with the server scaled to zero, `git clone` over
   the dumb protocol from the bucket succeeds — this leg also settles
   the two UNVERIFIED S3 behaviours of §3.
6. **Protected main.** An agent's push to `main` is refused by
   `pre-receive` naming the rule; its push to `agent/<pod>` lands; its
   push to `refs/for/main` merges for a listed principal and returns
   `ng` with the conflicted paths otherwise, moving no ref.
7. **Idle-to-zero.** Replicas 0 after `suspendAfterSecs`; a clone
   during suspension succeeds after a wake of up to the hold; the
   restore time is reported.
8. **The storm, on EC2 only** (kind measures its host's loopback, not
   NIC egress or S3 fan-out): 1,000 concurrent clones with bitmaps and
   a bundle URI advertised AND `transfer.bundleURI=true` on the
   clients: server egress bounded, S3 carries the bytes. Controls:
   bundle URIs off — the server's NIC saturates; the client opt-in
   off — identical to bundle URIs off, proving the advertisement alone
   does nothing.
9. **Export.** lean mounts the exported `main` read-only and every
   file is byte-identical to `git show main:<path>`; a push changing
   three files rewrites one or two chunks and three objects; a reader
   resolving the manifest mid-export never finds a cited object gone.
10. **The sweep.** After a repack, old packs are deleted past the
    grace; a pack in the snapshot is never deleted; the probe asserts
    the sweep fired.
11. **S3 outage.** Pushes fail with a clear message; clones and fetches
    succeed until the lease TTL; the server then exits rather than
    serving what it cannot prove it still holds.

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
   `docker/Dockerfile.forge-git` and `.forge-syncer.prebuilt` build the
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
