# flint forge — a file API with lite's semantics: design

*Draft 2026-09-07. Status: DESIGN ONLY, no code. Companion to
`docs/plans/flint-forge-design.md` (the design of record), whose §6
identity model and §4 push path this reuses unchanged.*

---

## 0. What it is, and what it is not

**It is** the six verbs of flint lite's file API — `GET /files`,
`GET|PUT|DELETE /files/content`, `POST /files/folder`,
`POST /files/move` — served by the forge syncer against the bare
repository it already owns, with the same paths, the same query
parameters and the same `If-Match` contract. One client library then
speaks to a lite share and a forge repository without knowing which it
has.

**It is not:**

- **Not a GitHub API.** No pull requests, reviews, comments, issues,
  actions, webhooks or search. Those are a product surface; this is file
  access.
- **Not a second writer.** Every write submits a `PushRequest` on the
  same channel `proc-receive` uses and is executed by the same batch.
  It adds a *submitter*, never a writer. §4.
- **Not a way around the branch policy.** `policy.judge` runs on a
  file-API write exactly as it runs on a push (`batch.rs:471`), which
  has a consequence people will not expect. §4.3.
- **Not a merge engine.** A write that cannot be applied cleanly is
  refused with a reason. forge never merges on the API caller's behalf,
  never resolves, and never guesses. Whether to discard the user's
  input, re-prompt, or three-way it belongs to the application. §4.6.
- **Not a replacement for git.** History, blame, diff, bisect and
  atomic multi-ref work stay in the git door. This serves the caller
  who wants one file.
- **Not a low-latency surface on a cold repository.** §5.2.

---

## 0a. The deployment this serves (2026-09-07)

Recorded because it bounds every decision below.

The browser app talks to an **S3 proxy** (which holds the credentials)
and drives a file manager with S3 API calls. Agent pods mount **flint
passthrough read-only**, so **every write already goes through the
app** — a single-writer discipline at the application layer, which is
what makes a passthrough prefix safe today.

**One project has exactly one backend.** Either S3-through-the-proxy or
forge, never both. Settled 2026-09-07, to be revisited.

Three consequences:

1. **No cross-backend moves**, so the hardest question does not arise.
   There is no atomic operation across two storage systems, and a file
   crossing that boundary changes semantics rather than location — it
   loses its history or acquires one. Had projects been mixed, this
   would have had to surface in the UI as import/export, never as a
   move.
2. **Forge's prefix is the project's prefix**, whole. No carve-out for
   the proxy to express, and no chance of a file-manager delete landing
   on `git/snapshot`. Worth stating why that mattered: §17's drills
   measured that a foreign write into a forge-managed prefix is **never
   repaired** (C3), and that *"a reader with no manifest — a key and a
   GET, which is what a passthrough or lite mount is — has nothing to
   check the bytes against and still takes the foreign write"* (C4).
   One backend per project removes the second writer by construction
   rather than by policy, which is the stronger fix — the design's own
   note is that across products single-writer *"is a convention, not a
   mechanism."*
3. **The agents' access is per project too.** A forge prefix cannot be
   passthrough-mounted; the bytes are packfiles. An S3 project's agents
   mount read-only, a forge project's agents `git clone`.

**The app-side consequence.** The file manager now has two backends
behind it. If the S3 proxy exposes the same six verbs this API does,
that is one file-manager implementation with a routing decision in
front — which is the reason §1 chose lite's verb shape over GitHub's.

---

## 1. Why lite's shape and not GitHub's

Both cover the same file operations, and both fail the same one. The
choice is therefore not about capability.

**GitHub cannot express an empty directory either.** There is no
create-folder verb in the GitHub REST API, for the same reason forge
has none: git has no empty trees. Adopting GitHub semantics recovers
nothing that lite's shape loses.

What lite's shape buys, and GitHub's does not:

1. **One client for two modules.** The application already reaches lite
   shares through `flint-hub-gateway`'s six verbs. Identical verbs on
   forge make the backing module a deployment detail rather than a
   second integration.
2. **The door is already built for it.** `resolve.rs:99-104` projects
   both `FlintShare` and `FlintRepo` onto one `ShareView`. §7.
3. **Less work for identical coverage** — no base64 content envelope,
   no second pagination convention, no one-commit-per-file baked into
   the contract.

GitHub semantics would be right only if an existing client library or
tool were the consumer. It is not; the consumer is a bespoke backend.

---

## 2. Where it runs — the syncer, and why not the git container

The repository pod has two containers, and they share one volume:

- `syncer` (`forge_operator/render.rs:492`) — mounts `repo_mount` and
  `policy_mount` (`:498`).
- `git_http` (`:550`) — mounts `repo_mount` (`:563`).
- `repo_mount` is the `repo` **emptyDir** (`:290-311`); the test at
  `:773` pins that no PVC is involved. It is a cache, not storage.

So *either* container can read the tree. Only one can write:

- The batch channel lives in the syncer process. The git container
  reaches it over a Unix socket (`server.rs:29`, `uds::serve` at
  `:226`), which is how `proc-receive` submits.
- The syncer already serves REST on a TCP listener — `/status`,
  `/healthz`, `/lfs/objects/batch`, `/lfs/objects/verify`
  (`server.rs:655-748`).

**Decision: both directions in the syncer.** One authenticated surface
instead of two, no socket hop on the write path, and it reuses the
listener that already exists. The alternative — reads in `git_http`,
writes relayed over the UDS — buys nothing and doubles the surface.

### 2.1 A separate port — the `/status` hazard, restated

The syncer's Service already carries its HTTP listener: `service()`
renders `git`/8080 **and `status`/9848** (`render.rs:244-271`,
`STATUS_PORT` at `:52`). Serving the file API there needs no new
Service, and that is the trap.

`/status` is served **unauthenticated on that same listener**
(`server.rs:667`) — epoch holder, `serverId`, phase, refs, packs,
snapshot seq. Admitting the door to 9848 would delete an explicit
negative test:

```rust
assert!(
    !admits(STATUS_PORT, "flint-forge-door"),
    "the door has no business on the status port — /status is exactly what the \
     gateway design refuses to proxy"
);
```
— `render.rs:990-995`, verbatim.

Today three independent layers keep `/status` unreachable: the door
builds upstream paths from `&'static str` only (`git.rs:102-110`,
`:942`); the door's endpoint names 8080, not 9848; and the
NetworkPolicy admits the door to `GIT_PORT` and the operator — never
the door — to `STATUS_PORT` (`render.rs:1153-1214`). Putting the file
API on 9848 removes the third and leaves a route-table bug one step
from a fleet-wide disclosure.

**Decision: a third port, `FILE_PORT`, on its own listener.** The
hazard dissolves structurally instead of being defended against, the
test above stays true as written, and the door speaks HTTP to a real
server rather than through a relay. The rejected alternative — relaying
through gitcgi as LFS does — needs gitcgi to forward a *query string*
(LFS forwards none) into a hand-rolled request-line concatenation, which
is request-smuggling surface.

### 2.2 The listener must move to hyper — a memory bound, not a style choice

`serve_http` (`server.rs:655-741`) is a hand-rolled matcher, and its
response path is **fully buffered**: the body is a `Vec<u8>` whose
length becomes `Content-Length` (`:733`). There is no streaming, in
either direction.

The syncer's container is sized **25m/32Mi**
(`render.rs:277-285`, rationale at `render.rs:18-20`: *"An idle `git
http-backend` is about 1.5 MB RSS"*). Lite measured what buffering
costs on its own API: a 512 MiB request took `VmHWM` from 30 MB to
541 MiB, and under a 256Mi limit the GET was **OOM-killed, taking the
NFS export down with it because one process serves both**
(`fileapi/mod.rs:69-77`). The forge syncer is that same shape — one
process serving the door and holding the lease — at **one eighth** the
memory.

**But the consumer decides the bound, and this consumer is a browser
file manager.** It browses a tree and edits text. It does not stream
video and it does not download checkpoints — the agents do that, with
a real git client, through the git door that already exists.

So the API is **capped, and the cap is enforceable before any byte is
read**: `ls-tree` yields an entry's size without materialising the blob (git
reads the object header, never the content — though the object must be
present, which a control attempt discovered), and an LFS-tracked path
never enters the pod at all (§5.3). A `GET` above
the cap is a `413` with a message a file manager can render — *"too
large to open here; clone the repository"* — which is the right UI
behaviour regardless of how the server is built.

**Decision: keep the existing listener; cap instead of stream.** With
no request or response able to exceed the cap, buffering is bounded and
safe at 32Mi, and the hyper rewrite — the single largest line item in
this design — is **not needed**. What is still needed is a small query
parser and percent-decoder, because the current matcher has neither
(`/healthz?x` is a 404 today) and hyper would not have supplied them:
it frames HTTP, it does not parse query strings. That helper is ~80
lines with tests, written once, and `route.rs:18-24`'s argument about
never hand-rolling on a caller-facing path is met by testing it
properly rather than by adopting a framework that does not cover it.

**Revisit if** the file API ever grows a consumer that needs unbounded
objects. At that point the streaming rewrite is the right answer and
this decision is the thing to undo — recorded here so that it is undone
deliberately.

**Sizing:** with a cap of C, peak is roughly C per in-flight request.
The 32Mi request must be checked against the chosen cap and the
concurrency the door allows; unmeasured (§11 Q5).

## 3. The six verbs against a git tree

| lite verb | forge implementation | verdict |
|---|---|---|
| `GET /files?path=&recursive=&cursor=&limit=` | `ls-tree`, `-r` when recursive | clean |
| `GET /files/content?path=` | `cat-file blob <ref>:<path>` | clean; `Range` and `304` work — the blob is on local disk |
| `PUT /files/content?path=` | blob → rebuild trees → commit → batch | clean |
| `DELETE /files/content?path=` | same, minus the entry | clean |
| `POST /files/move {from,to}` | one tree edit, one commit | **stronger than lite's** |
| `POST /files/folder {path}` | — | **cannot exist** |

### 3.1 `folder` — the one that cannot exist

Git has no empty directories: a directory exists because a file is
under it. The two options are a `501`, or writing a `.gitkeep` the
caller never asked for and which then appears in their next
`GET /files`.

**Decision: `501 Not Implemented`**, with a body naming the reason.
Fabricating a file to satisfy a verb makes the listing lie, and the
listing is the verb the caller trusts most.

**This is UI-visible, and the UI can absorb it.** A file manager has a
"New Folder" button, and against a git backend that button cannot be a
server call. The established answer — it is what GitHub's own web
editor does — is to make the new folder **client-side state**: the user
names it, the UI shows it, and it becomes real on the server when the
first file is written into it, as `PUT /files/content?path=/new/f.txt`.
A folder with nothing in it simply never existed.

So the frontend needs one behaviour it would not need against lite: an
empty folder is local until it holds a file, and navigating away
discards it. Worth deciding before the UI is built rather than after
the `501` is discovered.

### 3.2 `move` — the one that is better here, and by how much

Lite's `move` is a compound, and `fileapi/mod.rs:32-47` records that a
compound is not atomic. In forge a rename is one tree edit in one
commit: it either lands or it does not.

**It is also far cheaper than the same operation on S3, and that is
measured, not argued.** S3 has no rename — a move is `COPY` + `DELETE`,
billed and paced by the object's SIZE, and a folder rename is that for
every object under it. Forge is content-addressed, so a rename moves
names: every blob keeps its oid and only the trees above it change.

Both arms below run `pack_new_objects(tips, ^excludes)` — exactly what
the batch uploads — and differ in one dimension, whether the content
changed:

| arm | uploaded |
|---|---|
| rename a directory holding a 1 MiB file | **226 B** |
| edit that same file (control) | **1,049,173 B** |

~4,600x, with the control arm confirming the machinery does carry
content when content moves. Pinned by
`a_rename_uploads_no_content_and_an_edit_uploads_all_of_it`.

An earlier version of that test compared blob oids before and after.
It could not discriminate: git is content-addressed, so an
implementation that rebuilt the blobs would produce identical oids and
pass. Pack bytes cannot be faked that way — which is why the test
measures the thing that is actually billed.

**Consequence for the API.** A directory rename is allowed, and bounded
by entry COUNT (`MAX_RENAME_ENTRIES`, 10,000) rather than by bytes,
because each file costs one index line and no content at all. It was
briefly refused during implementation by analogy with delete; that was
wrong — a delete destroys, a rename reuses every subtree oid verbatim.

### 3.3 ETag is the blob oid

Lite's ETag is synthetic (`"<fileid_hex>-<change_hex>"`). Forge's is
the blob oid — content-addressed, stable, and meaningful outside the
API. `If-Match` on it is a precondition on the tree entry, which is
exactly the semantics lite intends and forge can state exactly.

This is not a cosmetic improvement. Lite's validator has a documented
**false-positive mode**: the change attribute is floored by ctime, and
the tier rewrites the local inode on both eviction and hydration, so a
file going cold moves its ETag with no user-visible change and a caller
holding one across that boundary gets a `412` for no reason it can see
(`hubfs.rs:219-232`). It fails closed, and the proper fix *"means a
counter-only validator plus a per-boot nonce, which is more machinery
than the annoyance has so far justified."*

A git object store already **is** a content-addressed validator. Forge
inherits none of this, and must not reproduce it out of a misplaced
wish for parity: the ETag's *format* differs, its *contract* is
strictly stronger, and the client sees an opaque string either way.

### 3.4 The verb lite cannot have

Because the unit of a write is a ref move, `POST /files/batch
{changes:[...]}` is nearly free once §4 exists, and it gives atomic
multi-file change — which lite structurally cannot offer. It is the one
real reason to prefer forge as a backing store, so it ships in the same
phase as the write path rather than being deferred.

---

### 3.5 Contract parity — matched, and deliberately diverged

Lite's contract was read out of the code verb by verb. Forge matches it
where a client could tell the difference, and diverges where lite's own
source records the shape as a wart.

**Matched exactly:**

- **Auth runs before path matching.** Lite gates with `warp::any()`
  *after* auth (`fileapi/mod.rs:465-502`), so an unauthenticated request
  to any path is `401`, never `404`, and the readiness `503` — which
  names the phase — cannot be probed anonymously. The argument is at
  `mod.rs:481-492`; forge copies the ordering.
- Status codes verb by verb, including `DELETE` → **`200` with a body**
  (not `204`).
- `limit` default **1000**, silently clamped to `[1, 10_000]`.
- `If-Match` conditions the **destination** on `PUT` and the **source**
  on `move` (`mod.rs:1344-1347`). One header, two meanings; reproduced
  because clients will be written against it.

**Deliberately diverged** — each is lite's own source naming its
shortfall:

| lite | forge | why |
|---|---|---|
| `405`, `411`, `415` and route-miss `404` escape as **`text/plain`** (`mod.rs:1363-1391`) | **every** error is `{error, reason, message}` JSON | a client cannot parse errors uniformly today |
| error field is `nfs_status` — snake_case in a camelCase API, carrying a Rust `Debug` string | `reason`, from a documented enumeration (§4.6) | forge has no NFS statuses to leak |
| no body cap on `POST /files/folder` or `/files/move` | capped | an unbounded JSON body into a 32Mi container |
| `GET /files` has **no** conditional support — no listing ETag | the **tree oid** is the listing's ETag | a polling file manager pays full price on every tick today; this one is nearly free here |
| upload `ETag` is best-effort — a failed post-write stat ships `201` with no validator (`mod.rs:1292-1298`) | the blob oid is known before the write | forge computes the validator, it does not observe it |

**Out of scope for this consumer, and therefore not diverged from —
just absent:** `Range` (and with it lite's no-`416` behaviour, where a
malformed range silently serves the whole file, `mod.rs:783-786`),
recursive-listing pagination and its unsigned `"{cookie}.{cookieverf}"`
cursor (`hubfs.rs:426-437`), and `recursive`'s strict `true`/`false`
parsing. If forge ever adds them, note that a git tree is **immutable
at a commit**, so forge's cursor could be `(tree oid, offset)` and its
recursive listings could page — which lite's structurally cannot.

**Inherited whole, with no improvement available:** a `304` counts as
activity against the idle ladder. Lite names the trap — *"Revalidating
on a timer pins a project awake precisely as re-downloading on a timer
does"* (`mod.rs:89-95`). Forge has the same ladder. §11 Q4.

## 4. The write path — a submitter, not a writer

### 4.1 What a `PUT` becomes

1. `hash-object -w --stdin` — the blob.
2. Rebuild the trees along the path (approach in §4.2).
3. `commit_tree` (`gitcmd.rs:832`) — parent is the current tip.
4. A `RefUpdate { name, old_oid: tip, new_oid: commit }`.
5. A `PushRequest` (`batch.rs:36-54`) on the **same mpsc channel
   `proc-receive` uses** (`server.rs:513-540`), awaiting the same
   report.

Steps 1–3 create *loose* objects. That path already exists and is
already correct: the batch packs server-created loose objects before
upload, because server-side merges create them
(`batch.rs:148-151`, `gitcmd.rs:390`). **A file-API write is
structurally identical to a `refs/for/` merge** — the server builds
objects, the batch packs and uploads them, one snapshot CAS lands them.

### 4.2 The tree rebuild — a temporary index, measured

Three approaches were run against real bare repositories rather than
reasoned about. The recommendation is the **per-request temporary
index**, driven entirely by `update-index --index-info -z`.

```
parent = ref_oid("refs/heads/<branch>")            # read ONCE
GIT_INDEX_FILE=<scratch>  git read-tree <parent>^{tree}   # skip if unborn
                          git update-index -z --index-info   # create+update+delete+rename, one call
                          git write-tree
                          git commit-tree <tree> [-p <parent>]
```

**Why not `mktree`.** It is faster and size-independent (113 ms vs
274 ms at 50k files), but it validates almost nothing. Measured: a tree
entry named `.git` is **accepted**, and

```
git fsck --strict            → error: hasDotgit          exit 1
git fsck --connectivity-only → (nothing)                 exit 0
```

`--connectivity-only` is forge's own restore proof
(`gitcmd.rs:529-551`). A `mktree`-built `.git/hooks/pre-commit` would
pass it, get packed, reach the bucket, and land in every clone.
`receive.fsckObjects` does not help — a REST write never touches
`receive-pack`. The temp index gets git's `verify_path` for free:
`/abs`, `a/../b`, `.git/config`, `.GIT/config`, `git~1/x` and
file/directory collisions are all refused at exit 128.

**Why not `fast-import`.** Fastest of the three (51 ms), but it writes
a **pack** above 100 objects (`fastimport.unpackLimit`, which forge
does not set) — straight into `objects/pack/` outside forge's upload
accounting — and on a bad path it dies and drops a
`fast_import_crash_<pid>` file in the repository. Keep it for a future
bulk-import endpoint where those stop being liabilities.

#### Five rules, each earned from an observation

1. **A bytes-oriented runner is required.** `Output.stdout` is
   `String::from_utf8_lossy` (`gitcmd.rs:90`, `:141`). Measured: a
   1 KiB blob containing all 256 byte values comes back with **512
   U+FFFD replacement characters**. This is not a shipped defect —
   every current caller reads oids and ref names — but every image,
   PDF and binary served through the existing runners would be silently
   corrupted. `hash-object` stdin needs the same treatment.

2. **One `parent` oid threaded through all four uses.** The oid you
   read, the `<parent>^{tree}` you `read-tree`, the `-p` you pass to
   `commit-tree`, and the `old` you give `update-ref` must be the same
   value. Measured: building a tree from a pre-B base and re-parenting
   it onto B's commit **loses B's file while the CAS passes** — the
   parent was correct, the tree was not. This is the subtlest failure
   in the design and §10.7 exists to pin it.

3. **Check every `update-index` exit status.** Measured: after a
   refused `--cacheinfo`, `write-tree` **exits 0 and returns the
   unchanged tree**. A handler that skips the check commits nothing and
   answers `200`.

4. **The mode is an input, not an inheritance.** Measured:
   `--cacheinfo` silently demoted `100755` to `100644`. Read the
   existing mode from `ls-files -s` (the index is already loaded) and
   default new paths to `100644`. A content write must never move the
   executable bit.

5. **Delete with `--index-info`, never `--remove`.** Measured: both
   `--remove` and `--force-remove` are `fatal: this operation must be
   run in a work tree` in a bare repo. Mode `0` in an `--index-info`
   line is the delete, and it is also how rename is expressed —
   delete + add in one call.

Use `-z` throughout: git **accepts** a path with an embedded newline,
which breaks line-oriented `--index-info`. The API should reject those
itself as well.

#### Reads

`ls-tree --format=...` is the type-and-existence oracle, not
`cat-file`. Measured traps: `cat-file --batch-check` **exits 0** on a
missing path and reports ` missing` as text; `ls-tree <ref> -- <path>`
**exits 0 with empty output** for a path that does not exist; and an
absent submodule commit is indistinguishable from an absent path
through `--batch-check`. A persistent `cat-file --batch-command` child
removes the per-read spawn entirely.

`ls-tree -l` also gives the blob size **without reading the blob** —
which is what makes §2.2's cap enforceable before allocation.

#### The empty repository

Measured: `read-tree HEAD` and `ls-tree HEAD` both exit 128 on a
commitless repo. Gate on `ref_oid` returning `None`, `commit-tree` with
no `-p`, and `update_refs` with an all-zero `old_oid` — which
`gitcmd.rs:284-288` already spells as `create`, and which correctly
fails if a concurrent first write got there first. Reading
`${parent_tree:-4b825dc642cb6eb9a060e54bf8d69288fbee4904}` (git's
intrinsic empty tree) removes even that branch from the read side.

#### Cost

Measured on macOS with a warm cache — **not Linux**, and the spawn
floor there (28-44 ms per `git` process) dominates everything:

| repo | per write |
|---|---|
| 200 files | ~253 ms |
| 50,000 files | ~274-288 ms |

Subtracting ~140-220 ms of process spawn, the tree-size term is
**unmeasurable below 5,000 files** and is ~70-155 ms at 50,000. Two
cheap wins: `index.skipHash=true` on the scratch index (155 → 128 ms,
no correctness cost — it is deleted immediately), and `--index-info`
batching, which makes an N-path write cost the same as a one-path
write.

### 4.3 Where it runs — off the loop, then onto it

Object construction is concurrent and safe off the batch loop: it
writes only **unreachable loose objects**. Each request needs **its own
index file** — measured, sharing one gives either a hard
`index.lock` exit 128 or a phantom write in which six concurrent
requests all commit the same six-file tree.

The `RefUpdate` then goes **onto** the loop. The template already
exists: the pruner builds a synthetic `PushRequest` and calls
`run_batch` (`server.rs:412-421`), with the comment *"Through the
ordinary batch: one CAS, one transaction. A ref this process moved
outside that path would be a ref the bucket does not know about."*

Forge's serialisation is structural, not lock-based — one
`tokio::select!` loop owns `sc: Syncer` by value (`server.rs:253`) and
every mutating git call happens inside that `&mut` window. A handler
that called `update_refs` itself would be a second writer.

### 4.4 Why this does not violate single-writer

1. The handler runs **inside the syncer process**, which holds the
   epoch lease.
2. It writes only the **local bare repo**.
3. **It never touches S3.** Objects reach the bucket only through the
   batch: pack uploads, then one snapshot CAS.
4. It submits on the same channel and awaits the same report, so
   *acknowledged means durable* is unchanged.
5. `policy.judge` applies unchanged, because judgement lives in the
   batch and not in the door (`batch.rs:465-472`).
6. The `If-Match` precondition becomes `RefUpdate.old_oid`, checked by
   the batch's existing staleness test.

### 4.4a Many users behind one credential — authorship

The browser app serves **many people through one service credential**.
That is the gateway's stated model — it *"has no opinion about who the
end user is: the project service authenticates people and audits them,
and calls this with one service credential"*
(`lite_gateway/mod.rs:39-43`).

For a filesystem that is fine. For a **git repository it is not**: every
commit would be authored by the application, and the history — the
thing a repository is for — would record nothing about who edited what.

**Decision: `X-Flint-Author` carries the end user, and git's own
author/committer split absorbs it.**

- **author** = the end user, from `X-Flint-Author`. Untrusted, supplied
  by the app, recorded in history — exactly like a `git config user.name`
  on a laptop, which is also unverified.
- **committer** = the door-verified principal (the app's identity), and
  it is **what `policy.judge` judges**. Trust and authorship stay
  separate, which is what git's data model is already built for.

`commit_tree` already takes a principal and sets `GIT_AUTHOR_*` from it
while pinning `GIT_COMMITTER_*` to `flint-forge`/`forge@chert.us`
(`gitcmd.rs:847-868`). The change is to pass two identities instead of
one, not to invent a mechanism.

The header spelling is lean's (`lean/sidecar/src/gateway.rs:208`), so
one app-side convention covers both modules.

**Two consequences of many writers, both already designed for but now
load-bearing rather than defensive:**

- **`ref-contended` is the common case, not the edge.** Two people
  editing different files in one repository collide on the ref every
  time. §4.6's bounded retry stops being optional.
- **The parent-oid invariant (§4.2 rule 2) is the failure that will
  actually happen.** With one writer it is theoretical; with a dozen it
  is Tuesday. §10.7 is the test that matters most in this design.

### 4.5 The branch policy — decide this deliberately

`policy.judge(&push.principal, &cmd.name, &cmd.new_oid)` runs
**before mechanics** (`batch.rs:465-472`), deliberately, so that a
principal who may not touch `main` is told that rather than told their
old-oid is stale.

**If `main` is protected, the file API is a 403 machine.** This is the
decision most likely to be discovered late. Three options:

- **(a) The API writes to `refs/heads/<configured-branch>`**, not
  `main`. Honest, and it composes with the existing proposal flow.
- **(b) The API's principal is exempted in the policy document.**
  Simple, and it silently removes the protection `main` was given.
- **(c) The API writes `refs/for/<target>`**, i.e. it *proposes* and
  the server merges. This is the existing AGit flow and needs no new
  policy story — but a `PUT` then returns "proposed", not "written",
  which is not lite's contract.

**Recommendation: (a),** with the branch named in `FlintRepo.spec`. It
keeps `main` protected, keeps the API's contract honest ("your write
landed"), and leaves promotion to `main` in the review flow that
already exists. (c) is the tempting answer and it breaks contract
parity, which is the whole point of §1.

### 4.6 Write failures are errors, never merges

**The file API never merges.** A write that cannot be applied is
refused with a reason the application can render. Resolving it — and
whether to discard what the user typed — is the application's decision.

That keeps the contract small and makes the **error taxonomy the real
specification**. Five distinct failures, five distinct answers:

| what happened | code | `reason` | what the caller does |
|---|---|---|---|
| the file changed under the caller (`If-Match` on the blob oid failed) | `412` | `file-changed` | re-read the file; the user's edit is against a stale version |
| an existing path was written with no `If-Match` | `428` | `precondition-required` | read first, then write with the ETag |
| the policy refuses this principal on this ref | `403` | `policy-refused`, plus the policy's own message | nothing — it is not permitted |
| the ref moved because a **different** path was written | `409` | `ref-contended` | retry as-is; this file is not in conflict |
| local repo and bucket disagree about the ref | `503` + `Retry-After` | `reconciling` | retry; the server reconciles on restart |

Every body carries `{error, reason, message}` so the application can
branch on `reason` and show `message`. `message` is written for a
person: *"the file changed since you read it"*, not *"stale info: fetch
first"*.

**The one that must not be called a conflict.** Verified in the code
rather than assumed — `batch.rs:487-492`:

```rust
let base = eff.get(&cmd.name).cloned().unwrap_or_default();
let old = norm(Some(&cmd.old_oid));
if old != base {
    return Ok(Judged::Refused { reason: "stale info: fetch first".into() });
}
```

**The precondition is on the ref, not on the file.** Two clients
writing `/a.txt` and `/b.txt` concurrently both build a commit on tip
`T`; the first lands, and the second is refused although the files do
not overlap. Lite accepts both.

Telling that second user *"conflict — refresh your file"* is wrong
twice: their file has not changed, and refreshing shows them nothing
different. They would retry the identical write and it would succeed.
So `ref-contended` is a genuinely different failure from
`file-changed`, and collapsing them would make the API lie about whose
edit was stale.

**Recommendation: retry `ref-contended` internally, bounded; never
retry `file-changed`.** Re-read the tip, rebuild on the new parent,
resubmit, N attempts, then surface `409 ref-contended`. In the common
case the caller never sees it; when contention is real, the error is
honest.

The retry is safe here in a way a blind retry would not be: the
caller's `If-Match` is on the **blob oid**, so a retry that finds the
file itself changed still fails with `412`. The retry absorbs *ref*
contention and never *file* contention — and that is the mutation
falsifier §10.2 exists to pin.

If you would rather have no retry at all, the design still works: drop
the loop and return `409 ref-contended` immediately. The cost is that
unrelated concurrent writers see spurious failures, and the
application has to implement the same retry itself.

## 5. Reads

### 5.1 Which ref

Lite has no ref concept — it is one live filesystem. Forge must choose.

**Decision: the configured branch is implicit; `?ref=` is accepted on
reads only.** Reads gain a capability lite does not have without
changing the shape of any request lite already sends; writes stay
single-target so the contract stays identical.

**Deferred.** §12 drops `?ref=` from the first cut — a file manager
browses one branch. The decision is recorded so that adding it later
does not reopen the question, and because it costs one query parameter
on a read path that already resolves a ref internally.

### 5.2 A cold repository is not a cache hit

The bare repo lives in the pod's emptyDir, and the idle rung destroys
that pod (`RepoIdle.suspendAfterSecs` → `replicas: 0`). A read against
a suspended repository is a wake, a lease wait and a restore. Drill M7
measured the reap at 68 s and proved the restored content complete.

The door already holds git requests longer for this reason
(`lite_gateway/git.rs`, "A longer hold"). The file door needs the same
treatment, and the API must state a cold-start latency rather than
implying a filesystem.

### 5.3 LFS pointers — a `GET` must not return the pointer

`lfs.rs:1-20`: LFS keeps a **pointer file** in git and puts the bytes
at `<prefix>/lfs/objects/<oid>`, and *"the bytes never cross the
server"* — the batch response hands out a presigned URL precisely so a
4 GB checkpoint never enters the pod.

So on an LFS-tracked path, `cat-file blob` returns **a few hundred
bytes of pointer text**, which is certainly not what the caller wants,
and naively resolving it would put the bytes through the pod and undo
the property LFS exists to provide.

**Decision: detect a pointer and `302` to a presigned URL**, reusing
`lfs::batch`'s machinery, which already lives in this process because
it is the one holding the bucket credentials. Bytes still never cross
the server. A caller that wants the pointer itself can ask with a
query flag.

---

## 6. Large writes

The same rule cuts the other way on `PUT`: a large body *does* cross
the pod. Two options —

- **(a) Cap it.** `413` above a configured size. Simple, honest, and it
  matches lean's gateway, which caps whole-object PUTs at 64 MiB.
- **(b) Two-step presigned upload**, then commit the pointer. Preserves
  the LFS property for writes, at the cost of a non-lite verb.

**Decision: (a)**, and §2.2 now leans on it for more than cost — the
cap is what makes the existing buffered listener safe, and what lets
the hyper rewrite be dropped from scope. (b) is the follow-up if a
consumer ever needs unbounded objects, and §2.2 records that as the
thing to undo deliberately.

The cap applies in both directions, and on `GET` it is enforceable
before the content is materialised: `ls-tree` yields the size from the
object's header, and an LFS path never enters the pod (§5.3). Falsifier §10.6 exists to pin exactly
that.

---

## 7. The door

### 7.1 What is reusable, and what is not

**`route.rs` is reusable verbatim.** The `Verb` enum (`route.rs:44-57`)
and its tables — `upstream_path`, `method`, `is_mutation`,
`query_keys`, `request_headers`, `filter_query`, `upstream_url` — name
no share, no project and no CR kind. `upstream_url` takes an opaque
`&str` endpoint. This is the cheapest part of the job and it is the
part the contract-parity argument rests on.

**`proxy.rs::serve` is not reusable at all.** It is `FlintShare`-typed
at the store (`proxy.rs:89-93`), at the addressing (`scope()` at
`:250`), at the door (`Door::FileApi`) and at the credential
(`gw.minter.token_for(...)` at `:734`). A repo file door mirrors
`git.rs`'s scaffolding instead — repository addressing, the reviewer,
`consumer_allows`, `wait_for_ready` — while calling `route::*`
unchanged.

### 7.2 My "one `decide_for` serves every door" claim was half wrong

I asserted this from a doc comment (`resolve.rs:99-104`). Checked
against the code, the **phase ladder is genuinely shared and genuinely
correct** for a `FlintRepo`: rows 1-8 of `decide_for`
(`resolve.rs:375-428`) route on `ShareView` alone,
`RepoPhase::as_share_phase` is total (`crd.rs:478-489`), and the forge
reconciler deliberately publishes `serverPhase` in the spelling
`hub_phase_blocks` expects (`reconcile.rs:388`).

**But the last step is not shared.** Row 9c — `Door::FileApi` at
`resolve.rs:468-493` — switches on an `ApiEndpointPublished` condition
whose reasons (`NotConfigured`, `NameCollision`) are produced by the
*lite* operator (`lite_operator/reconcile.rs:2656`, `:2689`). A
`FlintRepo` has `conditions: None` hardcoded (`reconcile.rs:396`) and
`FlintRepoStatus` has no `apiEndpoint` field at all
(`crd.rs:334-380`).

So a `Ready` `FlintRepo` asked at `Door::FileApi` today returns
`503 NoApiEndpoint`, `Retry-After: 10`, *"no file-API endpoint
published for this share: no ApiEndpointPublished condition"* — a
retryable error that will never stop being returned, naming a condition
type the forge operator does not emit.

**A third door needs its own `Door::RepoFileApi` branch**, not a reuse
of `Door::FileApi`. The phase ladder above it is free.

### 7.3 Identity: TokenReview, not a derived bearer

The two existing doors authenticate differently. Lite's file API takes
a **derived bearer** — `HMAC(root, endpoint:bucket:keyPrefix:version)`,
no secrets RBAC anywhere (`derive.rs:8-16`). Forge's git door takes
**TokenReview**: HTTP Basic whose password is the pod's projected SA
token, `TokenReview` behind a TTL cache, then
`consumer_allows(spec.consumers, ...)`, then `X-Remote-User` upstream
(`git.rs:212-305`, `:463-481`, `:986`).

**Recommendation: TokenReview.** A file door sitting beside the git
door, on the same repositories, governed by the same `spec.consumers`
list, that authenticated *differently* would give one repository two
authorization models — and the file door would be the one that could
not say who wrote a file. TokenReview also keeps `spec.consumers`
enforceable, needs no per-repo Secret, and the door's ClusterRole
already grants `tokenreviews: create` (`door.yaml`).

**Cost of that choice:** the syncer must actually read `X-Remote-User`
on the file routes. It does not read it on the LFS routes today
(`server.rs:698-720`), though gitcgi forwards it
(`flint_forge_gitcgi.rs:333`). That principal is also what §4.5's
policy check and §11 Q3's commit author need, so it is required work
regardless.

### 7.4 A pre-existing gap this makes worse

The lite chart's gateway ClusterRole grants only `flintshares`
(`flint-lite-operator-chart/templates/gateway.yaml:62-66`) — **neither
`flintrepos` nor `tokenreviews`** — and `gateway.yaml` renders no
`--git` flag. The combined "one gateway, two route tables" deployment
that `door.yaml`'s own header calls *"still the right deployment"* is
therefore **not renderable from either chart today**. A third door does
not create this gap, but it does make it unavoidable to fix.

## 8. What the formal model needs

`ForgeSync.tla` models a push as an **environment event with no
origin**: `PushSend(p, s)` (`:564`) moves a push from `new` to `sent`
and the comment above `Fairness` (`:1191`) states that *"crashes,
hangups and pushes are the environment"*.

A file-API write that produces an identical `PushRequest` and runs the
same batch therefore introduces **no new protocol step**, and the
expectation is **zero new actions**.

Two things must be checked rather than assumed:

1. `PushSend`/`IdxLand` model a client pack transfer and git's `.idx`
   rename. A file-API write has neither — its objects are already on
   disk. Whether the batch's precondition on `localPacks` is satisfied
   by that path is a real question, not a formality.
2. The §4.4 retry submits a *second* push. In the model that is another
   environment event, which is fine — but it should be exercised, not
   argued.

---

## 9. Failure model

| failure | behaviour |
|---|---|
| repository suspended | wake + restore; request held (§5.2) |
| lease lost mid-write | the batch fences; the write is refused, never half-applied |
| ref moved because another path was written | `409 ref-contended` after a bounded retry (§4.6) |
| file changed under the caller | `412 file-changed`, never retried |
| existing path written with no `If-Match` | `428 precondition-required` |
| repo and bucket disagree about the ref | `503 reconciling` + `Retry-After` |
| policy refuses the ref | `403 policy-refused` with the policy's message |
| path is a directory / symlink / submodule | `409`, as lite does for non-regular files |
| body over the cap | `413`, refused before allocation (§10.6) |
| `update-index` refuses the path | `400`; never reaches `write-tree` (§4.2 rule 3) |
| path is a symlink (`120000`) or submodule (`160000`) | `409` with the kind named; a symlink's content is its target |
| LFS-tracked path on `GET` | `302` to presigned (§5.3) |
| empty repository | first write commits with no parent |

---

## 10. Falsifiers

Pre-registered, in the house style — what would show this design wrong.

1. **The retry does not converge.** If N concurrent writers to distinct
   paths cannot all land within the retry bound, the ref-level
   precondition is too coarse and the design needs a different write
   primitive. *Test: N writers, distinct paths, count landed vs
   refused. Predict: all land, N <= 8.*
2. **The retry masks a real conflict.** Mutate the file under the
   caller between its read and its write; the write must still fail
   `412 file-changed`. If it succeeds, the blob-oid `If-Match` is not
   load-bearing and the retry is unsafe. **This is the mutation that
   must fail**, and a positive control has to run through the retry
   path itself, not around it.
3. **The two failures are indistinguishable in practice.** If a client
   cannot tell `file-changed` from `ref-contended` — because the API
   collapses them, or because `ref-contended` fires so often it is
   treated as noise — the taxonomy in §4.4 is decoration. *Test: drive
   both, assert distinct `reason` values and distinct rates.*
4. **The tree rebuild is too expensive.** If a write against a large
   repository costs more than a push of the same content, the plumbing
   choice in §4.2 is wrong.
5. **The phase ladder is not kind-agnostic either.** §7.2 confirmed
   rows 1-8 are shared and row 9c is not. If a phase row also misfires
   for a repository — `of_repo` has **no direct unit test**, and a field
   added to `ShareView` arrives as `Default` for repos silently — the
   door work is larger than §7 says. *Test: every phase x every door
   against `of_repo`, plus a pin that no field arrives by `Default`.*
6. **The cap is not enforceable before the read.** §2.2 rests on
   `ls-tree` giving a size without touching the blob. If any path
   reaches a buffered read without a size known first, the 32Mi
   container is one click from an OOM that takes the writer down.
   *Test: request an oversized blob and assert 413 with no allocation
   spike. This is the mutation that must fail.*
7. **The parent-oid invariant is violated somewhere.** §4.2 rule 2: a
   tree built from one base and committed onto another loses data with
   the CAS reporting success. *Test: force the interleaving — read
   parent, let a second write land, then commit. The first write must
   be refused, not silently drop the second's file.*
8. **Binary round-trip is lossy.** *Test: PUT and GET a blob
   containing all 256 byte values; assert byte-identical. Measured to
   fail through the existing runners.*
6. **The model does need a new action.** If §8's two checks fail, the
   cost estimate is wrong by the size of a formal-model pass.

---

## 11. Decisions and open questions

**Decided here:** the syncer serves both directions (§2); `folder` is
`501` (§3.1); ETag is the blob oid (§3.3); `/files/batch` ships with
the write path (§3.4); writes go to a configured branch, not `main`
(§4.3); **no merges — a failed write is a described error** (§4.6);
bounded internal retry on ref contention only (§4.6); `?ref=` on
reads only (§5.1); `302` for LFS pointers (§5.3); size cap for v1
(§6).

**Open:**

- **Q1.** Does this reopen `batch.rs:522-525` — *"no merge API, no
  second authenticated surface, and exactly one path to the bucket"*?
  It adds a second authenticated surface. That is a deliberate
  reversal and needs to be recorded as one, not routed around.
- **Q2.** Authentication: derived bearer (lite's model) or
  TokenReview (forge's git door model)? Contract parity argues for the
  first; forge's existing identity story argues for the second.
- **Q3.** Commit message and author for an API write. Principal is
  available and trustworthy (`PushRequest.principal` is *"never the
  client's git config"*), but the message is synthetic.
- **Q4.** Does the file API count as activity for the idle ladder? It
  must, or a repository parks under active use. Note lite's trap
  (§3.5): a `304` is activity too, so a polling file manager pins a
  repository awake. Forge cannot avoid this; it can only document it.
- **Q5.** What is the cap, and does `25m/32Mi` survive it? The request
  was sized for `git http-backend` (`render.rs:18-20`). §2.2's whole
  argument depends on the answer. Unmeasured.
- **Q6.** `of_repo` has one call site and **no direct unit test**
  (`git.rs:378`). Before a third door depends on it, it needs the
  phase x door matrix — otherwise every future `ShareView` field
  silently arrives as `Default` for repositories.

---

## 12. Scope — the smallest forge change that serves a file manager

The consumer is a browser file manager with simple text editing. The
agents keep using a real git client against the git door, which is
already built. That asymmetry is the point, and it bounds this work.

**All three tiers are in scope**: browse, view, create, edit, delete,
rename/move. Together they are a complete file manager —
`PUT /files/content` creates as well as edits (`If-None-Match: *` is
create-if-absent), and `POST /files/move` covers rename and move
alike, since a rename is a move within one directory. The tiers below
are a **shipping order**, not a menu.

**Dropped for this consumer** — each was in the draft and each is now
out: streaming (§2.2), `Range`, recursive-listing pagination,
`/files/batch`, `?ref=`. They are recorded in §3.5 and §3.4 so that
adding one later is a decision rather than a rediscovery.

### Tier A — browse and view

`GET /files` (one directory page), `GET /files/content` (capped, with
an ETag). **No commit plumbing, no batch, no policy check, no retry, no
write taxonomy.** Two git wrappers (`ls-tree`, `cat-file`), the query
helper, the new port and its NetworkPolicy rule, `status.apiEndpoint`,
and the door's `Door::RepoFileApi` branch.

This is most of a file manager, and it cannot break the writer — it
never submits anything.

*~650 production, ~520 test.*

### Tier B — edit

`PUT /files/content`, `DELETE /files/content`. This is where §4 lands:
the tree rebuild, `commit_tree`, the `PushRequest`, the branch
decision (§4.5), and the error taxonomy (§4.6).

**Most of this is irreducible.** A write to a git repository *is* a
commit; there is no cheaper representation. The one avoidable cost is
the retry loop, and §4.4 records what dropping it means.

*~450 production, ~400 test.*

### Tier C — rename and move

`POST /files/move`, `folder` → `501` (§3.1), and the LFS `302`. Same
machinery as Tier B — a rename is one tree edit in one commit — so it
is cheap once B exists, and it is the verb a file manager is asked for
most after editing.

*~250 production, ~200 test.*

### Cost

| | production | test |
|---|---|---|
| Tier A | ~650 | ~520 |
| Tier B | ~450 | ~400 |
| Tier C | ~250 | ~200 |
| **A+B+C** | **~1,350** | **~1,120** |

Against the ~1,500/~1,000 the full-parity design implied, the saving is
almost entirely §2.2's listener rewrite. **Tier A alone is ~1,170
lines** and delivers browse-and-view.

Roughly half of Tier A is door and operator work that B and C then
reuse, so the tiers are not independent estimates — shipping A first
costs nothing extra against shipping all three together, and it is the
increment that proves the door, the port and the wake path before any
write machinery exists.

**Two decisions become urgent once editing is in scope**, and both are
cheap to settle now and expensive to discover late:

- **§4.5, the branch.** If the API writes to a protected `main`, every
  save is a `403`. Recommendation there is a configured branch.
- **§11 Q5, the cap.** §2.2's whole argument — no streaming, existing
  listener — rests on a cap the `25m/32Mi` container can hold. It is
  unmeasured.

### The zero-forge-code alternative, for the read half

`export.rs` already publishes a ref's tree to a separate prefix **as a
lean workspace**, and lean's gateway already serves REST file verbs
against it. Browse-and-view with **no forge change at all**.

Why it is not the recommendation: it is one ref (and `render.rs:403`
silently exports only `refs[0]` of a list — a defect worth fixing
regardless), it is floored rather than live so the UI shows a lagging
tree, and it is a **mirror that must never be written** — the C3 drill
measured that a foreign write into the export prefix stands and is
never repaired (`export.rs:27-46`). It buys the cheap half of a
file manager and forecloses the other half.

**With editing in scope this is not an option**, only a stopgap: the
export is read-only by construction, and writing into it is the trap
above. Recorded because it can serve browse-and-view on day one while
Tier B is built.

---

## 13. Defects found in shipped code

Both are in the **existing git door**, independent of anything proposed
here, and both were verified in the source rather than inferred.

**D1 — `of_repo` never reads the token-version annotation, and the
default is wrong.** `ShareView::of_repo` (`resolve.rs:172-194`) ends
`..Default::default()`, so `token_version` is **`0`**. `derive.rs:86`
documents the field as *"the `chert.us/api-token-version` annotation,
**1 when absent**"*, and `ShareView::of` reads it that way for a share
(`resolve.rs:222-227`).

`binding()` requires only a non-empty bucket and a `Some` key_prefix
(`resolve.rs:273-284`) — both of which a repository always has — so
`of_repo(...).binding()` succeeds today and derives over `...:0`, while
a provisioner following the documented contract derives over `...:1`.
Every request would `401`. Worse, `Binding::previous()` returns `None`
at version 0 (`derive.rs:108`), so the rotation retry never fires: the
failure would be silent and permanent.

**Latent, not live** — nothing calls `binding()` on a repo view today,
because the git door uses TokenReview and never touches `derive`. It
goes live the moment anything derives a token from a repository. The
fix is ~5 lines and should land whether or not this design proceeds.

**D2 — a refused repository's reason is never surfaced.** A refused
repository records why in `status.refused` (`crd.rs:372-374`, written
at `reconcile.rs:395`) — a foreign claim, an unrestorable snapshot, a
git below the floor. `of_repo` never reads it, and `conflict_with` is
share-only, so `decide_for` row 3 answers:

> `409 Failed: "this share is Failed — see its conditions"`

pointing an operator at a field that is unconditionally `None` for a
repository, and calling it a "share". `status.refused` is read nowhere
in `lite_gateway/`.

---
