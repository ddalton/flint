# Branching on lean — design of record

Status: **DESIGN — no code.** Written 2026-09-04, after the user asked
whether a "flint-git" front end should be built on lakeFS and
`lakectl local`. The answer was to build branching on lean's own
substrate and to use lakeFS as a reference design, not as a server.
This document is that design. It builds on two shipped layouts —
`flint-lean-manifest-pointer-design.md` (an immutable generation plus
a small CAS'd pointer, `1ace7bca`) and
`flint-lean-chunked-manifest-design.md` (content-addressed chunks, a
publish that costs O(changed)) — and reverses one stated non-goal of
the boundary-verbs plan: *"No history/branching. A boundary is a
coherent point in a single linear manifest sequence, not a snapshot,
tag, or branch."*

The user's steer on scope, verbatim in spirit: refs, CAS ref updates,
three-way merge with a retained base, refuse-on-conflict with explicit
strategies, and reachability GC are all well-trodden — adopt them
(§1). Design the two things git and lakeFS cannot give lean: **where a
branch's bytes physically live under a legible layout** (§3) and **who
is allowed to move main** (§4). Everything else here exists to serve
those two sections.

## 0. What lean gains

Each item names the shipped limitation it removes, by file.

1. **Many writers on one project.** A prefix has ONE holder today. A
   second pod that wants the same workspace observes the lease cell
   for `QUIET_POLLS` heartbeats and then deposes the first
   (`lease.rs:41-118`); the rotation, fencing and deposition apparatus
   exists to make that safe. With a branch per pod there is nothing to
   contend for: every pod publishes to its own ref at its own floor,
   and the coordination cost moves from *before every write* to
   *once, at merge*, where a policy or a human can look at it. This is
   what an agent fleet on a shared project needs and lean cannot
   offer now.

2. **History and rollback.** The chunked design's §9 named this gap
   and deferred it: retained pointers "have to be enumerable, i.e. the
   generation objects the pointer reaper already keeps must become
   pointer snapshots". A commit here IS that snapshot. Undoing a bad
   publish becomes one pointer CAS. Today an agent that deletes half
   the tree and publishes it is recoverable only by hand through
   bucket versioning, one object at a time.

3. **Conflicts become visible instead of resolved by rule.**
   `manifest::merge` (`manifest.rs:941-987`) resolves modify/modify as
   *mine wins* and `preserve_conflict_copy` (`barrier.rs:307-326`)
   files the loser under `conflicts/<uuid>/` — a silent winner with a
   receipt. Right for a lone straggler racing a takeover; wrong for
   two agents who each did a day's work. A branch merge refuses by
   default and names the paths.

4. **A coherent main without gated mode's price.** Main moves only at
   merges, which are coherent points by construction, while agents
   keep cadence RPO on their own branches. Gated mode buys coherence
   by making automatic-recovery RPO "the last boundary" (CRD,
   `boundaryMode` cost 1); branching gives both at once.

5. **Diff for free.** Two pointers' chunk lists differ in exactly the
   chunks whose contents differ: "what did this branch change" is a
   `ChunkRef` set difference plus a parse of the differing chunks —
   O(changed chunks), never O(entries).

6. **Zero-copy experiments.** A fork is three small requests, no
   chunk writes, no data copies (§5.1). Nothing under `files/` moves
   until a merge says so.

7. **The legible bucket survives** — §3 is the argument.

8. **Blast radius.** A straggler, a runaway agent, a bad merge: each
   is confined to one ref and, with credential scoping (§4.5), to one
   prefix.

What it does NOT give: live shared visibility. A branch does not see
main's later edits until it pulls; two branches never see each other.
That is git's model and lean's already (sync is explicit, plan §3).

## 1. Adopted, not designed

These are settled in git and lakeFS and lean adopts them with the
parameters below. Each maps onto machinery that already exists.

| Adopted | Lean's instance | Already built |
|---|---|---|
| Refs: named, mutable pointers to immutable commits | `.flint/lean/refs/<name>` is a `Pointer` (three optional fields added: `ref`, `base`, `parent`); `main` is a ref like any other | `manifest::Pointer`, `put_pointer` |
| CAS ref updates | `If-Match` on the ref's etag; a 412 reloads and retries, bounded, as the barrier's step 5 does | `put_pointer`, `barrier.rs:718-790` |
| Commits: immutable snapshots | every ref CAS first writes its body to `commits/<seq:020>-<uuid>` with `IfNoneMatchAny` — the pointer layout's generation object, kept instead of reaped | `generation_key`, `cas_write_stamped` step 1 |
| Three-way merge with a retained base | `merge(base = the fork/pull commit, theirs = target head, mine = delta)`; the base is a commit that retention keeps for as long as the branch lives | `manifest::merge`, `LeanSubtree` |
| Refuse on conflict; explicit strategies | conflict set = paths changed on both sides since base, plus delete/modify either way; `refuse` (default) writes nothing and names the paths; `ours`/`theirs` resolve wholesale and are stamped on the commit | `LeanChunkMerge` still governs the chunk list: recomputed, never spliced |
| Reachability GC | live = every chunk / overlay object / commit reachable from a ref, a tag, or a live branch's `base`; `keepCommits` (default 5, `KEEP_GENERATIONS`'s number and reason) beyond that; the four `LeanChunkGC` rules unchanged, the reference set enlarged (§6) | `sweep_chunks`, `LeanChunkGC.tla` |
| Tags | `tags/<t>`: an `IfNoneMatchAny` PUT of a commit body; a retention pin | — |
| Diff | `ChunkRef` set difference, then parse the differing chunks | `chunk::ChunkRef` |

The cost table these give (fork O(1); pull with zero data copies;
merge O(changed) copies; diff O(changed chunks)) is in §5.7. Nothing
in this table is where the risk is. The risk is in §3 and §4.

### 1.1 Why not git itself — the alternative, recorded

The user asked it twice, in sharpening forms: *why re-invent this —
can lean not `git clone` the bucket and manage files locally with the
existing tool, reimplementing only the sync?* and then: *the only
lean-specific thing is the mapping between POSIX and S3; store that in
S3 and manage it with git.* The instinct is right about what is
lean-specific, and the table above IS git's data model — commits,
refs, trees, a three-way merge over a retained base. What does not
survive is git's **storage format** and git's **index**, and "manage
it with git" means both.

1. **Git's format needs a server to be efficient over a dumb store.**
   A manifest as a git tree at 1M files is more than a million
   objects. A fresh pod's fetch from a bucket is O(files) GETs, unless
   the objects are packed — and packs need a repacker, and a repacker
   over a shared bucket with concurrent pushers needs a pointer, a
   CAS, a grace and a reference set, which is the chunk layout. lakeFS
   hit exactly this and built ranges; lean's content-defined chunks
   (~250 objects at 1M entries, O(changed) per publish) are the same
   answer. Git's smart protocol avoids it by having a server compute
   the pack. There is no server here by design.
2. **Git's index cannot track the agent's own repository.** Verified
   on git 2.50.1: a nested repo is added as a gitlink (mode `160000`),
   `git add proj/.git/HEAD` fails with *"is in submodule"*, and
   `update-index --cacheinfo` on the same path fails with *"Invalid
   path"* — the index rejects any `.git` component outright. Lean's
   workload is agents working on git repos, and the 0b rig asserts a
   repo round-trips `fsck --strict` clean
   (`flint-lean-0b-measurements.md`). Git as the workspace tracker
   would silently publish none of that. So the scanner stays, and the
   local side is not "nothing lean-specific" either. The other local
   hazards are of the same kind: an agent's `.gitignore` hides files
   from the tracker; `autocrlf`, modes and symlinks become places
   where git's tree and the disk disagree, and a disagreement reads
   as a deleted file.
3. **Git without a server has no authorization.** Refs in a bucket
   are keys. Branch protection, hooks and "who may push main" live in
   GitHub's, GitLab's or lakeFS's server. On a bucket they are the
   credential, which is §4 unchanged.
4. **The mapping-in-git version does not buy per-line merge.** Git
   merging manifest entries merges per file, as lean does today.
   Per-line merge needs the data in git's object database, which is a
   compressed second copy of every file beside the working tree —
   the 20 GiB `sizeLimitGib` becomes 40 GiB per pod across the fleet.
   The two properties are mutually exclusive in git.

What git leaves undecided is exactly §3 and §4: git has no human paths
in its store, so it cannot say where a branch's bytes live at a
legible path, and it has no server, so it cannot say who moves main.

Kept from git: the data model (this table) and, if per-line merge is
wanted, its merge *algorithm* — a three-way text merge library
(`gix-merge`, libgit2's file merge) inside §5.3 step 5, producing
conflict markers for text and refusing for binaries. That is the part
of git worth having, and it drops into the design as written.

## 2. Layout

```
<prefix>/files/<path>                                   main's tree — UNCHANGED, one object per file
<prefix>/branches/<b>/files/<path>                      branch b's bytes: an overlay (light) or a full tree (§3.3)

<prefix>/.flint/lean/current                            main's pointer today; after opt-in: REFUSAL_DOC (§8)
<prefix>/.flint/lean/refs/main                          main's pointer, after opt-in
<prefix>/.flint/lean/refs/<b>                           branch b's pointer
<prefix>/.flint/lean/chunks/<addr>                      main's chunks — read by every branch, written only by main-capable principals
<prefix>/.flint/lean/commits/<seq>-<uuid>               main's commits
<prefix>/.flint/lean/tags/<t>
<prefix>/.flint/lean/merges/<id>                        approvals and results — main-capable writers only (§4.4)
<prefix>/.flint/lean/{epoch,claim,inbox,conflicts/}     main's cells — UNCHANGED

<prefix>/.flint/lean/branches/<b>/chunks/<addr>         b's OWN chunks (§3.6)
<prefix>/.flint/lean/branches/<b>/commits/<seq>-<uuid>  b's commits
<prefix>/.flint/lean/branches/<b>/{epoch,inbox,conflicts/,merge-request}
```

The shape to notice: **a branch owns exactly two prefixes and one
key** — `<prefix>/branches/<b>/`, `<prefix>/.flint/lean/branches/<b>/`
and `.flint/lean/refs/<b>` — and reads everything else. That is not a
tidiness choice; it is what makes §4.5's credential policy three
allow-patterns long and auditable by eye.

Branch names are one key component: `[a-z0-9][a-z0-9._-]{0,62}`, not
`main`, no `/` in v1 (a hierarchical name makes `x` a listing prefix
of `x/y`, and the fence in §6 lists refs).

## 3. Where a branch's bytes live

This is the one place branching and the legible bucket pull against
each other. Git and lakeFS do not have this problem because neither
promises a human-readable object at a human path; lean does, and it is
the differentiator.

### 3.1 The legibility contract, as invariants over the bucket

Stated so a foreign reader — `aws s3 cp`, an import tool, a person
with `mc ls` — knows what it can rely on without the manifest.

- **L1 — `files/` is main.** For every path main's head cites, the
  CURRENT version of `<prefix>/files/<path>` is the cited bytes, and
  every current object under `files/` is cited by main's head.
- **L2 — a branch's overlay is its diff.** For every path branch b's
  head cites with an *own* entry, the current version of
  `<prefix>/branches/b/files/<path>` is the cited bytes, and nothing
  else is under that prefix. Listing it is the branch's added-and-
  modified set.
- **L3 — nothing opaque.** No object under `<prefix>/files/` or
  `<prefix>/branches/` is named by anything but a workspace path.
  Content addressing stops at `.flint/lean/`.
- **L4 — a branch's tree is closed.** Everything b cites lives under
  `files/` or `branches/b/files/` — never under another branch's
  overlay. Deleting branch c can therefore never dangle branch b.

Known exposures, each already documented elsewhere and not new here:
L1's second half is violated transiently by a merge that dies between
its copies and its CAS (§3.4) and, on a gated main, by staging; both
are the "raw-key visibility" cost the gated CRD text carries. L2 does
not show DELETIONS — a light branch that deletes an inherited path has
nothing in its overlay to show for it. A tombstone object would make
`mc ls` show a file that "exists", which misleads in the other
direction; the deletes are in the manifest and in `flint-sync diff`,
and L2 says "added-and-modified" on purpose.

### 3.2 The three options

**(a) Everything through versions.** A branch writes `files/<path>`
as main does; each ref pins its own version ids. No new namespace.
**Rejected**: the current version of `files/<path>` becomes "whoever
wrote last", so L1 fails permanently rather than transiently. Gated
mode accepts that exposure for the seconds of a staging window and
names it as a cost; branching would make it the steady state.

**(b) Copy on fork.** Every branch gets a full private tree under
`branches/<b>/files/`. No shared objects, no pins, no cross-ref GC.
**Rejected as the only mode**, on arithmetic: a fork is O(entries)
copies — a 100k-file project forked by a thousand agents is 10⁸
requests — and storage multiplies per branch. Kept as an opt-in
branch kind (§3.3), because for a few long-lived branches its
properties are exactly right.

**(c) DECIDED — a sparse overlay with pinned inheritance.** A
branch's pointer cites two kinds of entry, told apart by the entry's
existing `key` field:

- *inherited*: `key = files/<path>`, `version_id` = the version main
  cited at the fork or last pull. Read by version id, never by
  etag-with-S3-wins.
- *own*: `key = branches/<b>/files/<path>`, written by this branch.

`files/` stays main. A merge moves own entries into `files/` as new
versions (§5.3), which is the only time branch bytes reach main's
tree. L1–L4 hold by construction; the exposures are the two named
above.

### 3.3 Two branch kinds

| | `light` (default) | `full` |
|---|---|---|
| fork | 3 requests, 0 copies | O(entries) server-side copies into the overlay |
| reads main's `files/` after fork | yes, by pinned version | **never** — its credential needs no read on `files/` (§4.5) |
| depends on main's versions surviving | yes — the TTL rule, §6.3 | no |
| legible full tree without the manifest | no (overlay + manifest) | **yes** — `branches/<b>/files/` is the whole tree |
| storage | O(changed) | O(entries) per branch |
| merge, pull, diff, delete | identical | identical |
| for | agents; many, short-lived | release/staging trees; a contractor's isolated copy; few, long-lived |

`full` needs server-side `CopyObject` in `flint-store`, which does not
exist yet (`preserve_conflict_copy` is GET + PUT and names CopyObject
as "the designed v2 optimization"). A full fork through the syncer's
memory is not an option at 20 GiB. So `full` ships in phase 6 behind
that lever, and v1 is `light` only.

One subtlety `full` introduces: `CopyObject` of a multipart-uploaded
object yields a different ETag. A full branch's inherited entries
therefore do not share main's etags for large files, and a delta
computed as "etag differs from main's fork commit" would report every
large file changed. The base for a full branch is a PAIR — main's
commit at fork (for "did main change it") and the branch's own initial
commit (for "did the branch change it") — and identical-bytes-on-both-
sides is judged by `crc64_b64` where present, etag otherwise. For a
light branch the two commits are the same object.

### 3.4 Versioning is a precondition, and main's reads become pinned

An inherited entry MUST be pinned. Without a version id a branch
reading `files/<path>` after main rewrote it takes the shipped 412 →
S3-wins-adopt arm (`checkout.rs:257-296`) and silently materialises
main's newer bytes into a checkout that was supposed to be a snapshot.
So: a branching workspace requires versioning; the fork refuses any
entry lacking a version id (the one-time backfill is the gated lane's
`version_index`, `gated.rs:653`, one `ListObjectVersions` walk keyed
on `(key, etag)`); every branch pointer carries `pinned_reads: true`.
The conformance probe (`gated.rs:1545`) is reused unchanged, including
its refusal of a proxy that strips the version header.

Main is pinned too, and for a different reason. A merge copies own
entries to `files/<path>` BEFORE its pointer CAS (§5.3). A crash
between the two leaves new current versions main's pointer does not
name; under etag-with-S3-wins a main checkout would adopt them and
half a branch would leak into main through the arm that exists to
trust the bucket. Under pinned reads they are invisible to every
manifest-resolving reader until the merge is retried or the strays are
reclaimed, and `surface_orphans` names them. **Enabling branching on
a workspace flips main's read semantics to version-pinned** — the
gated lane's posture — and the CRD doc-comment says so. On an
unversioned bucket a crashed merge would have overwritten main's
bytes with no version to fall back to, which is the second reason
versioning is a precondition for every branch kind, not only `light`.

### 3.5 A branch delete never touches `files/` — the trap

Step 6 of the barrier deletes `file_key(path)` for every path the new
manifest no longer cites, HEAD-guarded on the recognised etag
(`barrier.rs:800-829`). On a branch, "no longer cites" includes
inherited paths the branch deleted — whose objects are MAIN's. The
guard passes (the etag is the one the branch recognises) and the
delete lands on main. With versioning that is a delete marker,
recoverable and wrong; at the model's atomicity floor it is data loss.
The rule: a branch's step 6 deletes only keys under its own overlay,
and a delete of an inherited path is a citation drop and nothing
else. `LeanBranchDeletesInherited` is the mutation (§10) and it is the
first the model must catch, because the shipped code does the wrong
thing by default. §4.5's credential makes it impossible as well as
forbidden; the code rule stays because `static` keys cannot be scoped.

### 3.6 Per-branch chunks and commits

A branch's own chunks go under `.flint/lean/branches/<b>/chunks/`,
not `.flint/lean/chunks/`. `ChunkRef` gains an optional `ns` (absent
= main's namespace), and `chunk_key` resolves it. Sharing is
preserved where it matters — a branch's untouched chunks reference
main's objects by address and are never rewritten — and a branch's
credential never writes to a namespace main reads.

Why this is worth a field: `cas_write_chunked` writes chunks
UNCONDITIONALLY (adoption must rewrite what it adopts, rule 4). With a
shared namespace, a branch key could PUT `chunks/<addr>` with a body
that does not hash to `addr`; `assemble` would refuse (the address
check), so main's readers would fail closed rather than read garbage
— but every main reader would fail. A buggy or hostile branch could
take main down by touching one shared chunk. Separate namespaces make
that a write the credential does not permit (§4.5), and the merge —
run by a main-capable principal — recomputes main's chunks from the
merged entries under main's namespace, which it was going to do
anyway (`LeanChunkMerge`: never spliced).

It also decomposes GC (§6): main's sweep unions every ref's and
commit's references to `chunks/` (branches reference main's chunks);
branch b's sweep of its own namespace needs only b's refs and commits,
because L4 holds for chunks too.

### 3.7 The `file_key` audit

`checkout.rs` reads `entry.key`. Everything else recomputes
`file_key(path)` from the path — fifteen sites across `barrier.rs`,
`sync.rs`, `gated.rs` and `gateway.rs`
(`grep -n 'file_key(' lean/sidecar/src`). On a branch, WRITES resolve
to the overlay and READS resolve to the entry's key, and those are
different functions. Every site is classified as read or write and
routed accordingly, with a test per site. The three that carry data-
loss weight: `sync.rs:165` (a HEAD that decides dirt), step 6 above,
and `preserve_conflict_copy` (a GET of a path that may be inherited).

### 3.8 Fork from a branch

Deferred. Forking b from c would inherit c's *own* entries, which cite
`branches/c/files/` and violate L4: deleting c dangles b. The fix L4
demands is a copy of c's delta into b's overlay at fork — O(c's
changes), server-side — which is the same `CopyObject` lever `full`
waits on. v1 forks from main only.

## 4. Who is allowed to move main

"Move main" means any write of `refs/main` — publish, merge, reset,
rotation — and any write under `files/`, since L1 makes `files/`
main's tree and a stray there moves what a non-manifest reader sees.

### 4.1 Three threats, three mechanisms

Lean today has two of the three and relies on proxy tenancy for the
third at project granularity.

| threat | mechanism | status |
|---|---|---|
| two LEGITIMATE writers race | the pointer CAS serialises them; the loser merges and retries | shipped |
| a DEPOSED writer (straggler) lands late | the epoch cell, rotation, per-request epoch validation at the proxy/gateway; `Inv_NoStragglerInstall`, `LeanEpochOnlyHolds` | shipped |
| a STRANGER writes — a principal that should never move main | the credential. Today: project-granular at the proxy; within a project "gateway-validated + CAS-cooperative", i.e. anyone holding the project key can write any key (plan §3, §9 Q6: "within-project residual accepted for v1; versioning recovers") | **branch-granular after this design** |

The CAS is not authorization and the epoch is not authorization;
both assume the writer was entitled to try. Branching introduces, for
the first time, principals inside a project that are entitled to write
SOME of it and not the rest. The answer has to be the credential, with
the code refusing as defence in depth for backends that cannot scope.

### 4.2 The principals

| principal | may move main? | credential |
|---|---|---|
| **P1 — main's holder**: a workspace pod bound to `branch: main` under a CR that allows direct publish to main | yes: publish, rotation | main-scoped (RW on `files/`, `refs/main`, main's cells and namespaces) |
| **P2 — the gateway**, on a human's behalf: HITL PUT today; merge, reset, approve tomorrow | yes, under the bearer, the CR policy and the window discipline | the project key it already holds |
| **P3 — the operator**: sweeps, tags, retention, branch deletion, the opt-in migration | yes | the operator principal (plan §2.4 principal split) |
| **P4 — a pod on a branch** | **never** | branch-scoped (§4.5) — or cooperative-only under `static`, surfaced as a condition |

The decision that follows from the table: **branches propose; main
integrates.** A branch never CASes `refs/main` and never writes
`files/`. That keeps the invariant every model in `lean/formal/` was
proved under — only main's epoch holder, or the gateway under the
window, ever installs main's pointer — exactly as it is.

### 4.3 The executor: one per ref, decided by the epoch cell

Who runs a merge into main depends on whether main has a holder, and
the epoch cell already answers that.

- **Main has a holder (P1).** Its barrier gains a step before the
  inbox consume: *consume approved merge requests*. One LIST of
  `branches/*/merge-request` per barrier (a thousand branches is one
  request), then, for each approved request, the merge of §5.3 under
  main's lease. Results are written back to the request. The gateway
  APPROVES but does not execute while a holder exists — it would be a
  second writer racing the first for no reason.
- **Main has no holder** — the cell is absent or released, which is
  the case for a *protected* main (§4.6) and for any workspace with no
  pod on main. The gateway executes on approval, as a HITL-class write
  under the same discipline `handle_manifest_cas` uses today. The same
  rule generalises to HITL PUTs on a holderless main: today they wait
  in the inbox for a pod that may never come; under this rule the
  gateway installs them.

Exactly one executor per ref at any moment, chosen by the mechanism
that already chooses who publishes. A straggler executor (a main
holder deposed mid-merge) is fenced by the epoch as a straggler
publisher is.

### 4.4 Proposals, approvals, results

A merge request is a CAS'd cell in the BRANCH's namespace,
`.flint/lean/branches/<b>/merge-request`:

```json
{ "from": "agent-7", "into": "main", "at_commit": "…/branches/agent-7/commits/…",
  "strategy": "refuse", "proposer": "…", "proposed_unix": 0,
  "state": "proposed", "result": null }
```

States: `proposed` → `approved` → `merged` | `refused{conflicts}`;
`proposed` → `withdrawn`. Idempotent state, not a queue, exactly as
`VerbRequest` is: a re-proposal before execution collapses to the
newest, so no rate limit or exactly-once protocol is needed.

The branch's key can write its own cell, so the cell cannot attest
its own approval. Approval lives where a branch cannot write:

- **by policy** — the CR's `podBranches.mergeInto` names refs a pod
  branch may merge into without a human; the executor reads the CR;
- **by a person** — `POST /lean/v1/<ws>/merges/<id>/approve` under the
  bearer writes `.flint/lean/merges/<id>` (main-capable only). The
  executor merges only requests with a matching approval object or a
  matching policy.

The result — the new main commit, or the conflict list — is written
by the executor into the request AND into `merges/<id>`, so the branch
sees it (its syncer relays it to `.flint/merge.ack`) and the record
survives the branch's deletion. Latency for an agent: at most one
main floor plus one branch floor.

The agent-facing sentinel is `.flint/merge` carrying `{into,
strategy}`. The branch syncer checks it against `mergeInto`, publishes
first so `at_commit` is the head, writes the proposal, and acks
`proposed`; the ack is rewritten when the result lands. Same budget
and coalescing rules as `.flint/publish`; a merge charges by copied
bytes.

### 4.5 The credential — what a branch key can and cannot do

The broker (`s3csi/broker.rs`) already turns a pod-bound token into
per-project keys through three backends, and already issues RO keys
for an RO CR (csi-node design §6). Branch scoping is the same idea one
level down. The node plugin stamps the branch on the registration; the
broker shapes the grant:

| action | a branch key may act on |
|---|---|
| `GetObject`, `GetObjectVersion`, `HeadObject`, `ListBucket` | `<prefix>/*` — a light branch reads main's files by version and main's chunks by address |
| `PutObject` | `<prefix>/branches/<b>/files/*`, `<prefix>/.flint/lean/branches/<b>/*`, `<prefix>/.flint/lean/refs/<b>` |
| `DeleteObject`, `DeleteObjectVersion` | the same three patterns |
| everything else | denied — in particular every write under `files/`, `refs/main`, `chunks/`, `commits/`, `tags/`, `merges/`, and main's cells |

Per backend:

- **`sts`** — `AssumeRoleWithWebIdentity` accepts a session `Policy`
  on AWS and on MinIO; the broker attaches the three patterns. Real
  enforcement.
- **`rest`** — the customer's API receives the branch in the body; it
  may scope or not. The broker records which.
- **`static`** — one key per project; cannot scope. Cooperative only.

The CR surfaces the outcome as a condition,
`BranchIsolation = Enforced | Cooperative`, the way
`SentinelVerbsActive` surfaces a posture an operator would otherwise
have to infer. A fleet on `static` keys is running pod branches on
trust, and the status says so.

What a branch key can still do, stated so nobody mistakes the policy
for more than it is: read every object in the project, including
other branches' overlays (read isolation between branches is a
non-goal — a branch must read main); fill its own two prefixes; and
propose merges, which the executor rate-limits by construction (one
CAS'd cell per branch). A `full` branch (§3.3) can be narrower —
after its fork it needs no read on `files/` at all — which is the
"contractor's copy" property, phase 6.

The syncer refuses in code what the credential refuses in policy: it
will not CAS a ref it does not hold, will not delete outside its
overlay, and will not write `files/`. Under `static` keys that is the
only enforcement there is, and it is exactly today's within-project
posture, no weaker.

### 4.6 Protection, and the other main moves

- **`branching.protected: true`** — main accepts only merges: the
  operator refuses any CR or pod binding `branch: main` for writing,
  main has no holder, and the gateway is the executor (§4.3). Derived
  default: protected whenever `podBranches.allow` is set, because a
  fleet's main should move only by merge; overridable for a workspace
  that wants one privileged pod on main.
- **Reset** (a ref CAS to a retained commit, `seq` bumped so it is a
  rotation and every outstanding handle goes stale) and **tag** (a
  retention pin, i.e. a cost) are P2/P3 operations. A branch may reset
  its own ref.
- **Delete a branch**: its own holder, or P3. An unmerged delta refuses
  without `--force`, naming how many paths it drops.
- **Fork**: any branch key creates its own ref and nothing else; the
  policy's `PutObject` on `refs/<b>` is what permits it.
- **The opt-in migration** (§8): P3, once per workspace.

## 5. Operations

Each is stated in terms of §3 and §4; none introduces a mechanism
those sections do not.

### 5.1 Fork

`flint-sync fork --from main --as agent-7`, or implicitly at a
branch's first claim when its ref does not exist.

1. GET `refs/main`. Refuse unless every entry carries a version id
   (§3.4; the backfill is one-time per workspace, never per branch).
2. PUT `branches/agent-7/commits/<0>-<uuid>` with main's body.
3. PUT `refs/agent-7`, `IfNoneMatchAny`: main's chunk list verbatim,
   `ref: agent-7`, `base: <main's head commit>`, `parent: <step 2>`,
   `pinned_reads: true`, `seq: 0`, `epoch: 0`.

Three requests, zero chunk writes, zero copies. A lost race at step 3
means the branch exists; claim it instead. A `full` fork adds the
O(entries) copy between steps 1 and 2 and rewrites every entry's key.

### 5.2 Publish on a branch

The seven-step barrier of today with the substitutions of §3: uploads
to the overlay, own chunks to the branch namespace, step 6 scoped to
the overlay, the CAS on `refs/<b>`, the lease on the branch's cell,
the inbox the branch's. Foreign entries — a HITL write to the branch,
a pull that landed while the pod ran — flow through the unchanged
merge → inbox → consume path.

### 5.3 Merge (executed by main's executor, §4.3)

1. Load `B` (branch head), `F` (the commit `B.base` names; for a
   `full` branch the pair of §3.3), `M` (main head, with etag).
2. **Delta** = `B` against `F`: upserts where the entry differs or is
   new; deletes where `F` has a path `B` lacks. O(differing chunks).
3. **Conflicts** (§1's rule). Under `refuse`, a non-empty set writes
   the list into the request and stops. Nothing under `files/` or
   `refs/main` is touched.
4. **Place the bytes**: each own upsert is copied to `files/<path>` as
   a NEW VERSION and its entry's `key`/`etag`/`version_id` rewritten to
   the copy's. O(changed) copies; client-side until `CopyObject`.
   Inherited upserts (a pull brought them) need no copy.
5. `merge(base = F, theirs = M, mine = delta)` — safe now precisely
   because step 3 emptied what it would resolve silently. Chunks
   recomputed into main's namespace, never spliced.
6. Commit, then CAS `refs/main` If-Match `M`'s etag; on 412 reload `M`
   and return to step 3 with the same `F`, bounded.
7. Write the result; optionally advance `B.base` to the new main
   commit ("merged, up to date"), which lets the overlay be swept.

A local pending-merge record drives the retry after a crash between
steps 4 and 6, as the intent journal drives a barrier's; the copies
are idempotent bytes.

### 5.4 Pull (main → branch, executed by the branch's holder)

The same algorithm with roles swapped: base `F`, theirs = `B`, mine =
main's delta since `F`. Conflicts refuse. On success the branch's new
pointer carries main's entries for everything main changed — keys
`files/<path>`, main's CURRENT version ids — so a pull writes only the
differing chunks and **zero data**. `B.base` becomes main's head
commit, and `pinned_unix` resets: a pull is what refreshes a light
branch's pins (§6.3). Materialising into the live tree is the existing
sync verb: locally-dirty wins, every skip surfaced.

### 5.5 Checkout at a commit, reset, tag, diff

`checkout --at` is `load` against a commit key; `reset` and `tag` are
§4.6; `diff` is §1. One or two small requests each; no chunk writes.

### 5.6 Delete a branch

CAS-delete `refs/<b>` (If-Match, so a concurrent publish loses rather
than being lost), delete its cells, and let the sweeps collect what
only its commits named: its chunks, its overlay. Authority per §4.6.

### 5.7 Cost

| operation | requests | chunk writes | data copies |
|---|---|---|---|
| fork (light) | 3 | 0 | 0 |
| fork (full) | 3 + entries | 0 | entries (server-side) |
| branch publish, k files | as today | O(k) expected | k, as today |
| pull, main changed j paths | O(differing chunks) | O(j) | **0** |
| merge, branch changed k paths | O(differing chunks) + k | O(k) | k |
| diff | O(differing chunks) | 0 | 0 |
| checkout-at / reset / tag | 1–2 | 0 | 0 |

## 6. Garbage collection

Reachability is adopted (§1). What is lean-specific is the
decomposition and the version pins.

### 6.1 Decomposed by namespace

Main's sweep of `chunks/` and `commits/`: reference set = the union
over every ref, every tag, every live branch's `base`, and the last
`keepCommits` commits per ref — branches reference main's chunks by
address, so they are in the union. The fence widens from one pointer's
etag to "no ref moved": refs are a listable prefix and `ListObjects`
returns etags, so it is two LISTs per thousand refs. Branch b's sweep
of its own namespace: reference set = b's ref and b's retained commits
only (L4 for chunks). The four `LeanChunkGC` rules — list first,
HEAD-at-delete, the grace, adoption rewrites — carry over unchanged.

Sweeps of main's namespaces need a main-capable credential and move
to the operator's cadence (`lastVerifiedUnix`), as the MPU sweep did;
at 100k entries a commit is ~2 KB, so a thousand branches × 5 commits
is ~10 MB of pointer reads per sweep. A branch sweeps its own
namespace at its own barrier, as today.

The shipped `sweep_chunks` — reference set = main's pointer only — is
UNSAFE once a second ref references main's chunks, which is why §8
locks old binaries out and why `LeanBranchGCMainOnly` (§10) is the
current code.

### 6.2 Overlay objects

Garbage once no ref or retained commit cites them (merged and
advanced, superseded on the branch, or the branch is gone). Same
union, keyed on `(key, version_id)`. A branch's own superseded overlay
versions are its own `base_version_id`s and it reclaims them itself,
as the gated lane does.

### 6.3 Versions under `files/` — the honest limit

A light branch's inherited pin names a version of `files/<path>` that
main may since have superseded. Two things can destroy it:

- **main's exact version reclaim.** `reclaim_superseded`
  (`gated.rs:752`) deletes the `base_version_id` its own stage
  superseded — the previously CITED version, which is exactly what a
  branch forked before that citation pins. On a branching workspace
  this pass reclaims nothing; its own rule is already "if we cannot
  name it we reclaim nothing", and here we cannot know it is unpinned.
  The lane-level reclaim of superseded UNCITED versions stays: no
  branch can pin a version that was never cited.
- **the lifecycle backstop.** `NoncurrentVersionExpiration` at
  `noncurrentRetentionDays` runs its clock on every superseded version
  regardless of who pins it.

v1 bounds the second rather than fighting it. A branch pointer carries
`pinned_unix` (fork or last pull). Checkout, publish and merge refuse
a light branch whose pins are older than
`noncurrentRetentionDays − visibilityLagBoundSecs`, naming the fix:
pull, or delete. A cadence main's cited version is current at the
pull, so the clock starts there; a gated main's may already be
noncurrent by at most the lag bound, hence the subtraction. A timing
assumption, stated as one, like the chunk grace. `full` branches are
exempt: they pin nothing.

The v2 lever is the union reaper for versions — flint's exact GC as
the only GC over `files/` and the lifecycle rule dropped, lakeFS's
posture — triggered by a workspace whose light branches outlive the
TTL.

## 7. The lease

Untouched in mechanism: claim loop, `QUIET_POLLS`, rotation on an
unreleased-foreign takeover (a CAS on `refs/<b>`, O(1)), renew with
the echo, self-fence on 412. Each ref has its own cell. With a branch
per pod the cell is uncontended and does the one job it was proven
for — fencing a replaced pod's straggler. Two pods on one branch
behave as two pods on one workspace do today, and the drill keeps that
as its control.

## 8. Migration, fail-closed — the same hazard, a fourth road

Both shipped migrations make an old reader REFUSE rather than conclude
the project is empty. Branching needs the same, and for a sharper
reason: an old binary's `sweep_chunks` on a branching workspace
DELETES chunks that only branches name (§6.1).

Branching is opt-in per workspace. On opt-in, P3:

1. writes main's pointer body to `commits/<seq>-<uuid>`;
2. PUTs `refs/main` (`IfNoneMatchAny`) with `ref: main`,
   `pinned_reads: true`;
3. overwrites `.flint/lean/current`, conditional on the etag it read,
   with `{"moved":".flint/lean/refs/main","note":"this workspace has branching enabled; upgrade flint-sync"}`.

`Pointer` has no serde default for `seq` or `epoch`, so an old
`load_pointer` fails to parse it, `load` returns `LeanError::State`,
and the old binary refuses — no checkout, no publish, no sweep. A
branching-aware binary reads `refs/main` first and falls back to
`current` only when `refs/main` is absent, which is every
non-branching workspace, unchanged. A workspace that never opts in
never migrates.

## 9. Surface

```yaml
spec:
  branch: main                       # which ref this CR's pods check out and publish
  branching:
    enabled: false                   # opt-in; migrates (§8); REQUIRES versioning (§3.4)
    protected: <podBranches.allow>   # main moves only by merge; the gateway is the executor (§4.6)
    keepCommits: 5
    podBranches:
      allow: false                   # may a pod name its own branch via chert.us/branch?
      pattern: "agent-*"
      kind: light                    # light | full (full: phase 6)
      mergeInto: []                  # refs a pod branch may merge into WITHOUT a human; [] = none
status:
  conditions:
    - type: BranchIsolation          # Enforced | Cooperative (§4.5)
```

**CSI.** `volumeAttributes: chert.us/branch: agent-7` — honoured only
under `podBranches.allow` with a matching `pattern`; the plugin stamps
`FLINT_SYNC_BRANCH` and registers the branch with the broker, which
shapes the credential (§4.5). Volume attributes stay pod-author input
and the CR stays the policy (csi-node design §3, §7). A pod can only
ever write the branch it named.

**Verbs.** `flint-sync {fork,merge,pull,diff,tag,reset,branch-delete}`;
gateway routes under `/lean/v1/<ws>/refs/<b>/…` for branch-scoped
forms of today's verbs, `POST /lean/v1/<ws>/merges` and
`…/merges/<id>/approve`; the `.flint/merge` sentinel (§4.4).

## 10. Formal models first

| Module | Invariant | Mutations that must violate it |
|---|---|---|
| `LeanBranchMerge` | the executor publishes exactly the whole-document three-way result over base `F`; the reported conflict set is exactly the paths changed on both sides | `LeanBranchMergeNoBase` (base = main's head — the amputation shape); `LeanBranchSilentWinner` (`merge()` with a non-empty conflict set); `LeanBranchSpliceLists` |
| `LeanBranchAuthority` — a branch actor with §4.5's grant, honest or hostile | `Inv_MainMovesOnlyByExecutor`: `refs/main` and every current version under `files/` change only by P1/P2/P3 actions; `Inv_MainReadable`: main's readers never fail on a chunk a branch wrote | `LeanBranchSharedChunks` (one chunk namespace — the clobber); `LeanBranchPushes` (a branch CASes main); `LeanBranchStaticKey` (no scoping — must violate, and the probe must show the code refusal catching the honest actor) |
| `LeanBranchExecutor` | exactly one executor per ref at a time; a deposed holder's merge never lands | `LeanBranchGatewayRaces` (gateway executes while a holder exists); `LeanBranchNoEpochOnMerge` |
| `LeanBranchGC` — `LeanChunkGC` with N refs, commits, two namespaces | `Inv_RefsComplete`: every object any ref, tag or retained commit names is present | `LeanBranchGCMainOnly` (**the shipped reaper**); `LeanBranchGCSingleFence`; `LeanBranchGCBaseUnretained`; `LeanBranchDeletesInherited` (§3.5) |
| `LeanBranchIsolation` | a branch checkout after main rewrites an inherited path yields the fork-time bytes | `LeanBranchS3Wins` (the etag arm on a branch); `LeanBranchReclaimsPinned` |
| `LeanSubtree` + `Merge` | `Inv_MergeDurable`: a merged delta is integrated by a live main syncer, not preserved one barrier deep | the `LeanDirectMergeInsufficient` shape, multi-path |

Plus probes, as the gate requires: a merge actually refused; a
branch key actually denied; the gateway actually executed on a
holderless main; a dead branch's chunk actually swept. A strict run
green over a merge that never conflicted proves nothing —
`LeanChunkMergeProbeBoth` already encodes the lesson.

## 11. Falsifiers

Each names the arm that would pass if the feature were broken.

1. **Fork is O(1).** ≤ 3 requests, 0 chunk PUTs, 0 data GETs on a
   200k-entry fixture. Control: the first branch publish writes one or
   two chunks — the branch is real.
2. **Two pods, two branches, no contention.** Neither ever observes
   `Waiting`; neither deposes. Control: two pods on ONE branch — the
   second waits `QUIET_POLLS` and deposes, as today.
3. **Isolation.** Main rewrites `a.txt` after the fork; the branch's
   checkout yields fork-time bytes. Control: with the pin disabled it
   leaks — the pin is what holds.
4. **A branch key cannot move main.** With `sts` scoping, a PUT of
   `refs/main` and a PUT under `files/` from the branch pod are
   AccessDenied; a PUT to its own overlay succeeds. Control: the same
   PUTs under a main key succeed. Under `static`, the code refusal
   fires and `BranchIsolation=Cooperative` is set.
5. **A branch cannot clobber main's chunks.** A PUT of
   `chunks/<addr>` from a branch key is denied; main's checkout
   proceeds. Control: the same PUT with a wrong body under a main key
   makes `assemble` refuse — the address check is real.
6. **Refusal writes nothing.** A conflicting proposal ends `refused`
   naming the path; `files/` and `refs/main` etags unchanged. Control:
   the no-conflict proposal lands as ONE `refs/main` CAS and copies
   exactly the branch's changed files — counted, not deduped.
7. **One executor.** With a holder on main, the gateway approves and
   does not execute (main's next barrier does). With no holder, the
   gateway executes. Control: a deposed holder's in-flight merge is
   fenced.
8. **A crashed merge leaks nothing to manifest readers.** Kill between
   copies and CAS; main's checkout byte-identical; the retry lands;
   `surface_orphans` names the strays.
9. **The union sweep.** Three branches; no chunk any ref names is
   deleted; a chunk only a deleted branch's commits named IS.
10. **A branch delete of an inherited path leaves main's object**; of
    an overlay path removes it.
11. **Old binaries refuse** a branching workspace and sweep nothing.
12. **TTL.** A light branch past the bound refuses, naming the fix;
    after a pull it checks out.
13. **Nothing regressed** on a workspace with `branching.enabled:
    false`: the full lean suite and the chunked design's falsifiers
    1–6, unchanged.

## 12. What it costs, stated plainly

- **Versioning becomes a precondition**, with gated mode's probe and
  refusals; main's reads become version-pinned; a crashed merge
  exposes stray current versions to non-manifest readers until
  retried.
- **Version reclamation narrows** on a branching workspace and light
  branches carry a TTL bound to `noncurrentRetentionDays`.
- **GC unions** over refs and commits, fences on every ref, and moves
  to the operator's cadence for main's namespaces.
- **A format change**: overlay and per-branch namespaces, three
  pointer fields, a `ChunkRef` field, refs, commits, the request and
  approval cells. Opt-in locks old binaries out of that workspace,
  fail-closed.
- **The broker learns branches**; on `static` keys isolation is
  cooperative and says so.
- **Fifteen `file_key` sites** to classify and route.
- **Merge copies are client-side** until `CopyObject`; `full` branches
  and fork-from-branch wait on it.
- **The formal surface grows** by five modules and one action, though
  per-branch lease contention disappears.
- **No live shared view**, restated so nobody reads branching as one.

What it does NOT cost: a server, a KV store, a second source of truth,
a CLI to shell out to, a generated-path bucket, or a licence to read.
Every one of those was the price of the alternative.

## 13. Phases

0. **Models** (§10) and the decisions in §14. No code before the
   modules run in `lean/formal/check.sh` with their mutations red.
1. **History alone.** Commits, `refs/main`, the opt-in migration,
   `keepCommits`, the union sweep over retained commits under one ref,
   `checkout --at`, `reset`, `tag`. Ships the §9 gap on its own and
   de-risks the GC change under one ref before N.
2. **Branches.** Fork, the overlay, pinned inheritance, per-branch
   namespaces, the `file_key` audit with a test per site, per-branch
   cells, delete, the TTL. The syncer's code refusals (§4.5, last
   paragraph).
3. **Authority.** The broker's branch grant on `sts`, the registration
   field, `BranchIsolation`. The request/approval cells, the executor
   in main's barrier, the gateway executor on a holderless main,
   `mergeInto`, `protected`.
4. **Merge and pull.** Delta, conflicts, strategies, the CLI, the
   gateway verbs, the sentinel, the pending-merge record.
5. **Surface and drill.** `spec.branch`, `spec.branching`,
   `chert.us/branch`; §11 on kind; the two-pod fleet leg and the
   `sts`-scoping leg on EC2 (the kind multi rig has never completed
   setup — memory of record).
6. **v2 levers**, each with its trigger: `CopyObject` (merge copy cost
   measured on a real bucket) → `full` branches and fork-from-branch;
   the union version reaper (light branches outliving the TTL); a
   second chunk-list level (10M entries, chunked design §4).

## 14. Decisions and open questions

1. **Sparse overlay with pinned inheritance for a branch's bytes —
   DECIDED** (§3.2); `full` as an opt-in kind behind `CopyObject`.
2. **Per-branch chunk and commit namespaces — DECIDED** (§3.6): a
   branch owns two prefixes and one key.
3. **Branches propose, main integrates — DECIDED** (§4.2); a branch
   never CASes `refs/main` or writes `files/`.
4. **Executor chosen by the epoch cell — DECIDED** (§4.3).
5. **Approval lives outside the branch's write scope — DECIDED**
   (§4.4): policy or `merges/<id>`.
6. **Merge refuses on conflict by default — DECIDED**; strategies
   explicit and stamped.
7. **Main's reads become version-pinned on opt-in — DECIDED as a
   consequence**, to be stated in the CRD.
8. **`protected` defaults to `podBranches.allow` — RECOMMENDED**;
   overridable.
9. **Read isolation between branches — NON-GOAL** in v1; `full`
   branches can have it in phase 6.
10. **Pull on start** — RECOMMEND opt-in (`pullOnStart: false`): a
    checkout must be reproducible.
11. **`rest` backends** — OPEN whether the broker should REFUSE
    `podBranches.allow` when the customer's API reports it cannot
    scope, or merely set `Cooperative`. Recommend the condition; the
    operator decides.
12. **Retention numbers** (`keepCommits: 5`, the TTL bound) — argued
    from existing constants, to be re-derived from the drill.
13. **Name.** The user's working name is "flint-git". This is
    branching on lean; nothing here is a fourth front end.
