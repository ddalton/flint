# flint-lean — a node body cache: many read-write pods from one base

Date: 2026-10-24. Status: **PARKED (§14).** Designed, attacked by an
adversarial review (§11; the first draft's durability, single-flight,
security framing and benefit claims did not survive it), prototyped and
measured (M1, §13: it passes its gate), then parked by the user for the
workload this fleet has. Nothing of it is on main; the prototype is on the
local branch `lean-node-body-cache-prototype`. Asked by the
user: "on a node, if multiple pods need to work on the same S3 endpoint,
can the driver load the files once and each pod do copy-on-write on a
layer over its mount?" for the case of **many read-write agents starting
from one base**.

## 1. The answer in one paragraph

Yes. The driver keeps one verified copy of each file body per workspace per
node, and gives each pod its tree as **reflinks** (`FICLONE`) of those
bodies instead of downloading them again. A reflink is copy-on-write at the
block level: the pod gets an ordinary private file whose blocks are shared
with the cache until either side writes. The second and later pods on a
node then check out the base with almost no S3 requests, no network bytes
and almost no new disk. Each pod's tree stays a plain directory, so the
scan, the barrier, the model and the proofs do not change; the cache sits
underneath "fetch the cited bytes". What it does **not** buy (§6): local
time for small files, page cache, or anything for the first pod on a node,
which it charges. It is not built on the container runtime's image layers
and not on overlayfs (§10).

## 2. What happens today

- **One worker, one tree, one credential per pod.** Each pod that mounts a
  FlintLeanWorkspace gets its own syncer worker and its own tree at
  `<plugin>/volumes/<vid>/tree` (`node.rs::publish_lean`). Per-user access
  design §10 D9: two pods on one node "share the bucket prefix and nothing
  on the node". `lean/syncer/AGENTS.md` rule 4: one tree per pod.
  `s3csi/SECURITY.md` P8: "A pod cannot reach another pod's worker, door
  or state."
- **Every pod downloads every file.** Checkout GETs each cited object under
  `If-Match` on its etag, checks the CRC-64 against the manifest, and only
  then renames it into place (`checkout.rs::materialize`, whole-object and
  ranged arms). Consume and sync fetch the same way, but write
  **conditionally**: `write_file_atomic_if(…, &still_clean)` re-checks that
  the agent has not written the path since the fetch began, immediately
  before the rename (`barrier.rs:464`, `sync.rs:407`; review 2026-09-18
  H7). A node cache was deferred on purpose: `flint-lean-plan.md` §6
  ("per-NODE cache is FUSE/mount-pod territory, out of scope"); csi-node
  design §3.6 ("Lean syncers are never shared").
- **Durability is batched, not per file.** `write_file_atomic` is "FAST
  (no per-file fsync)" (`barrier.rs:2598`); a materialization is made
  durable by one `syncfs` (`safefs::sync_tree`) before the baseline is
  committed. A million fsyncs "would be the cost" (`safefs.rs:130`). Any
  cache path that adds a per-file fsync is a regression (§11, F3).
- **What a checkout costs per pod.** Measured by M1 (§13, in-memory store,
  every request paying 25 ms, 8 cores): **45 s at 200k files of 8 KiB and
  225 s at 1M files of 4 KiB**, one GET per file; 6–9 s at 200k with no
  delay. (The first draft quoted 3.9–4.8 s and 17.9–18.2 s "at 25 ms" from
  latency plan §17; that bench injects its delay only AFTER the checkout,
  so those were zero-delay numbers.) 128 fetches in flight at 25 ms is
  about 5,000 files/s. Against real S3 the request rate is the wall: AWS
  documents 5,500 GET/HEAD per second per partitioned prefix, so a
  1M-file checkout needs minutes per pod however wide the fan-out. Bytes: 3.3 s/GiB on the 0b rig
  (`flint-lean-0b-measurements.md`). Node disk scales the same way: 16
  agents on a 20 GiB base hold 320 GiB.
- **Facts the design rests on:**
  - A cited object is immutable: "A cited HANDLE is immutable: nothing in
    the protocol writes one twice" (`checkout.rs`, the 412 arm). So
    `(key, etag)` names one sequence of bytes forever. `LeanEntry.key` is
    the FULL object key, prefix included (`lib.rs:650–661`).
  - The resume arm already materializes from LOCAL bytes with no bucket
    request: size and CRC-64 of the file already in the tree, then the
    baseline entry (`checkout.rs`, "The check is local-only … No bucket
    request either way"). A node cache hit is the same step with the bytes
    coming from the node instead of this tree's previous incarnation. The
    precedent is weaker than it looks: resume adopts only bytes THIS tree's
    own incarnation fetched under `If-Match`; the cache extends that to
    bytes another process fetched (§5).
  - Lever A's manifest chunk cache (`chunkcache.rs`) is the precedent for
    a local cache of immutable remote bodies, verified before use — and
    its rules carry over: content before trust, pid-suffixed temp names,
    remote names never become local paths unchecked.

## 3. Design

### 3.1 What the cache holds

One directory per workspace, named
`hex(sha256(CR UID ‖ endpoint ‖ bucket ‖ prefix ‖ uid))`, so a re-created
CR never inherits and an **edited** CR (the CRD has no immutability rules;
bucket, endpoint, prefix and uid are all editable in place) gets a fresh
directory rather than a stale one owned by the old uid. Inside, file bodies
named `hex(sha256(key ‖ 0 ‖ etag))`. The name is derived, never taken from
the bucket as a path. A body is the exact bytes of one cited object; size
and CRC-64 come from the manifest entry at lookup, so the cache keeps no
metadata of its own. **A body's mode is the cache's (0600), never the
entry's**: a cited 0000 must not make the body unreadable to the next
worker. Bodies are written to a pid-and-random-suffixed temp name, renamed
into place, never modified after, and made durable by the checkout's
existing `sync_tree` (same superblock, measured §11), not by an fsync.

Per workspace, not per node: two workspaces never share a body, even with
identical bytes. Cross-workspace dedup would need a content hash the
manifest does not carry (CRC-64 is forgeable, §5).

### 3.2 Hit: verify, clone, rename

At each fetch site, after the existing resume check and before any request:

1. **No CRC, no hit.** An entry with an empty `crc64_b64` (consume and sync
   fall back to the backend's attestation today, `barrier.rs:435`,
   `sync.rs:373`) has nothing to verify a body against; it fetches as
   today.
2. Open the cache body for `(key, etag)`. Absent, or gone between the
   lookup and the open (eviction, §3.6): go to §3.3.
3. **Verify every body before any clone, the filler's own included**: size
   against the entry and CRC-64 against the manifest, exactly as a fresh
   fetch is checked. Mismatch: unlink the body (a miss) and go to §3.3.
   This keeps the manifest's claim, "every reader verifies a fresh fetch
   against it before the file becomes visible", true for cached bytes too.
   The verify reads every byte through the shipped slice-by-8 table CRC
   (`flint-store/src/lib.rs:61`); for 20 GiB of large files that is CPU
   time the M1 bench must show (§7).
4. Create the tree's temp file with the existing safe-create discipline
   (`safefs`, the tree is tenant-writable), `FICLONE` the body into it
   (fallback §3.7), set the entry's mode, rename into place. **At the
   consume and sync sites the rename stays conditional**: the same
   `still_clean` re-check as `write_file_atomic_if`, run immediately before
   the rename, so an agent write since the pass began still wins (H7). A
   clone gets a fresh mtime exactly as a written file does (measured §11),
   so the racy-clean gate (latency plan §17) treats it the same.
5. Record the baseline entry as the fetch arm does today.

The fetch sites: `checkout.rs::materialize` (ranged and whole-object arms),
the barrier's consume (`barrier.rs`, the `get_whole` at the consume step)
and `sync.rs`. Rescope and resume reach `materialize`, so they inherit it.
**What a miss means stays per site**: checkout refuses on 412 and 404;
consume and sync mark the path owed (`left = true`). The cache sits below
the fetch and changes none of that.

### 3.3 Miss: fetch into the cache, then clone

The fetch is today's, under `If-Match`. Only the destination of the bytes
changes: the whole-object arm writes the verified body to a cache temp
name, the ranged arm writes its ranges into a cache temp file instead of
beside the target. Rename to the body name, then §3.2 steps 3–5 (verify the
body you just wrote: a torn or raced body is caught here, §11 F1). The
bytes reach disk once, as today, plus the clone's metadata.

**Any cache failure falls back to today's path.** ENOSPC, EACCES, a temp
evicted mid-write, a rename that finds its directory gone, a clone the
filesystem refuses: log it, count it, and write the tree directly as today.
The cache is best-effort at every step; a checkout never fails because of
it.

The cache is **filled only from the store**, never from a tenant's tree.
A publisher reading the tree could be racing the agent's next write, and a
CRC-64 match after the fact proves nothing against a deliberate rewrite
(§5). So a pod's own publishes do not warm the cache in v1; the first peer
on the node that consumes them fills it.

### 3.4 Pods that start together

Many agents from one base usually start at once, and twenty simultaneous
misses would make twenty GETs per file. Single-flight per body:

1. Open-or-create `<body>.filling` (`O_CREAT`, not `O_EXCL`).
2. Try `flock(LOCK_EX | LOCK_NB)`. **Got it: you are the filler.** Check
   for the body first (someone may have finished); fetch and rename it;
   unlink the marker while still holding the lock; release.
3. **Did not get it: wait with a bound.** `flock(LOCK_SH)` on the blocking
   pool (never on a fan-out task; the code already learned this for
   `pwrite`, `checkout.rs:103–110`), with a timeout of twice the fetch's
   own retry budget. When it returns or times out, re-check for the body;
   absent, go back to step 1. A filler that died (the kernel drops its
   lock) or hung (the timeout) therefore hands over, and a stale marker is
   just a marker whose next opener becomes the filler; no "who may unlink
   it" rule is needed.
4. Waiters hold **no fetch-budget permit** while waiting; the permit is
   acquired after the hit-or-miss decision, so pods 2..N do not serialize
   behind pod 1's budget, and pod 1's death is a handover, not a stampede.

Markers are inodes only while a body is in flight or until the next opener
cleans up. `flock` works across containers because every worker locks the
same inode on the host filesystem.

### 3.5 Placement and mounts

- **Same filesystem as the trees.** A reflink needs source and destination
  on one filesystem: the cache lives beside the plugin's volumes directory
  (`<plugin>/body-cache/`), on the node's root filesystem (XFS on AL2023,
  where trees live by default: `flint-lean-loop-image-perf-2026-09-15.md`).
  **Not** under `workers.cacheHostPath`: that is the passthrough block
  cache's root, possibly another device, and the plugin sweeps unnamed
  `s3w-*` directories there.
- **Across two hostPaths: measured, works.** The worker sees the tree and
  the cache as two bind mounts of one filesystem. Box 2026-10-24 (kernel
  6.12.0, XFS `reflink=1`, two bind mounts of one filesystem): `FICLONE`
  across the mounts produced shared extents (fiemap flag `0x2000`); a write
  to the clone changed the clone and left the source intact; a clone from a
  body already **unlinked** (the eviction race) works; `syncfs` on the tree
  mount flushed the cache mount's dirty data (one superblock). Repeat on
  the target AMI's kernel and confirm `reflink=1` on its root XFS before
  P2 (§8).
- **The copy-on-write hint.** The plugin sets XFS's `cowextsize` hint to
  one block on each tree's root before the checkout (`xfs_io -c
  "cowextsize 4096"`; files created under it inherit it). With the
  default hint, a clone's later overwrites stay on the slow copy-on-write
  path for the CoW-preallocation lifetime (300 s on the box's kernel):
  repeated 100-byte edits ran 4.2× slower; with the hint, at parity (§13).
  Inheritance by new subdirectories, and the hint's effect on large-file
  fragmentation, are to be confirmed in P1.
- **Who mounts what.** The plugin creates the workspace's cache directory
  (owner = the workspace uid, mode 0700) and adds it as a second hostPath,
  read-write, to **read-write workers only** (§5). Read-only workers and
  tenant pods never get it.
- **Admission and the pod builder.** The workers-namespace
  ValidatingAdmissionPolicy is a prefix check with no count
  (`workers-policy.yaml:102`), so a second hostPath under a new root needs
  the policy's `hostPathPrefix` list extended; `worker.rs:6–7` ("exactly
  one") and its test (`worker.rs:846`, `hp.len() == 1`) both change.
  Startup adoption ignores directories without `state.json`
  (`state.rs:227–239`), so nothing today would remove the cache; its whole
  lifetime is new code (§3.6).

### 3.6 Lifetime, budget, eviction

- **Per workspace, under a per-workspace lock.** The plugin's lock today is
  per volume id (`node.rs:305`); the cache needs a per-workspace one, so
  that "grace expired, remove the cache" never runs between a volume's
  `create_dir` and kubelet's mount, or under a live worker (which would
  leave the worker an empty read-only path: every fill fails, every lookup
  misses, silently).
- A workspace's cache lives while any volume of that workspace is on the
  node, plus a grace (`workers.leanBodyCache.graceSecs`, default 1 h: agent
  fleets have gaps). The plugin removes it after that, and at startup
  removes caches no live volume names once their grace has passed.
- **A node byte budget** (`workers.leanBodyCache.maxGiB`), enforced by the
  plugin, which alone sees the node total (a worker sees only its own
  bind mount). Over it, the plugin **evicts whole workspaces, coldest
  first by last volume publish**, never single bodies: bodies are never
  modified after rename, so per-body mtime is fill time, and per-body
  "oldest first" would evict the hot base before cold peer-change bodies.
- **Eviction is always safe.** A clone holds its own reference to the
  shared blocks: deleting a cache body never changes a tree, and a worker
  cloning from a body it already opened finishes on the open inode
  (measured §11). The hit path handles ENOENT after the lookup as a miss.
- Temp files older than the fill timeout are swept with the workspace
  (`chunkcache.rs` has the same `retain`-style sweep).
- Accounting: shared blocks are stored once. `du` on a tree still reports
  its full size, and freeing the cache does not free blocks that trees
  still share. **Page cache is not shared**: each clone is its own inode,
  and 16 agents reading their clones hold 16 copies of the hot set in
  memory (measured §11).

### 3.7 Fallbacks

- **No reflink** (ext4, or XFS without `reflink=1`): a real read/write copy
  (`copy_file_range` on a reflink filesystem IS a reflink, so it is not a
  fallback and must not be the probe). The copy saves every S3 request and
  the network bytes; it does not save disk, and it writes the bytes twice.
  On a gp3 root disk (125 MiB/s) that can be **slower than S3** for large
  files (`values.yaml`: "6 GiB through a 768Mi cache read 5× slower than S3
  on a gp3 root"). So the copy mode is **off by default**; the plugin
  probes once at startup (clone a scratch file from the cache root into the
  volumes directory, check the extent flags) and logs which mode it is in.
  Counters on the checkout report (`cloned`, `copied`, `fetched`, `hits`),
  because, as `CheckoutReport::ranged` already notes, a feature that never
  fires looks exactly like one that does not help.
- **Quota on** (`workers.quota: true`, default off): the tree is a
  loop-mounted ext4 image, another filesystem, so it is always the copy
  mode, i.e. off unless turned on. Moving the quota to XFS project quotas
  would keep reflinks but is a node-image change (AL2023's root lacks
  `prjquota` by default, loop-image perf note §As deployed).

### 3.8 What a reflinked file does differently (POSIX)

To POSIX a clone is an ordinary, independent regular file: its own inode,
owner, mode and times, link count 1. `read`, `write`, `mmap`, `truncate`,
`rename`, `link`, `unlink`, `chmod`, `chown` and `stat` behave as on any
file, and a write to a clone never reaches the cache or another pod's copy
(measured, §3.5). A clone's mtime is the clone time (measured). Nothing in
lean or the plugin reads block counts (grep 2026-10-24); the scan compares
size, mtime and mode.

**The one difference POSIX can see: an overwrite in place needs free
space.** The first write to a shared block allocates a private copy, so on
a full filesystem a write that succeeds on an ordinary file fails.
Measured 2026-10-24 (kernel 6.12.0, XFS `reflink=1`, a 512 MiB image
filled to 0 MiB free; each file 64 MiB, 1 MiB overwritten at offset 0):

| file | overwrite in place, disk full |
|---|---|
| ordinary file (control) | ok |
| clone | **ENOSPC** |
| clone, `fallocate(mode 0)` over its range beforehand | **ENOSPC** |
| clone, `fallocate(FALLOC_FL_UNSHARE_RANGE)` beforehand | ok (the unshare took its 64 MiB when it ran: free 285 → 221 MiB) |
| clone written through a `MAP_SHARED` mapping | **SIGBUS** (the process is killed) |

And freeing space works differently: unlinking a clone whose source still
exists freed **0 MiB**, against 64 MiB for the ordinary control.

What that means here:

- **Who it bites.** Programs that write in place into existing files: a
  SQLite or other database file in the tree, anything that preallocates
  with `fallocate` and relies on later writes not failing (the guarantee
  `fallocate` gives is gone for shared blocks), and `mmap` writers (SIGBUS
  rather than an error return). Editors and git write a new file and
  rename it over the old one, which never touches a shared block.
- **Only on a full node.** Below full, the difference is cost (§13: the
  first write to each shared block pays for the copy), not behaviour. On
  Kubernetes the kubelet's hard eviction (`nodefs.available < 10%` by
  default) normally evicts pods before the root filesystem fills, which
  is itself disruptive for an agent.
- **Eviction cannot rescue it.** Evicting cache bodies frees nothing while
  trees share their blocks (§3.6). The space a node needs to stay out of
  ENOSPC is the bytes pods may still rewrite, which in the worst case is
  every pod's whole copy.
- **The guard is a free-space floor in the plugin** (proposed: below 20%
  free on the trees' filesystem, stop cloning and write new trees and
  fills plainly, and raise a node event), plus the node budget (§3.6).
  It bounds new sharing; it does not unshare what is already shared.
  `FALLOC_FL_UNSHARE_RANGE` is how a program that needs the old guarantee
  can get it, at the cost of the space. §9 item 6.

Also visible, not semantic: `du` and `st_blocks` count every clone at full
size (`df` is right); `FIEMAP` shows the shared flag; files rewritten at
random offsets fragment (§13); page cache is not shared (§3.6); one bad
block damages every file sharing it (a hit's CRC check catches it in the
cache, not in trees already cloned); `swapon` refuses a swap file with
shared blocks; reflink does not work on DAX mounts.

## 4. What changes in the protocol

Nothing in the document, the manifest, the barrier, the lease, the scan or
the racy-clean gate. A hit materializes exactly the cited bytes, verified
against the cited CRC-64 before they are visible, with a fresh mtime, as a
fetch does; consume and sync keep their conditional rename. `LeanP1`,
`LeanScanRacy` and their proofs are unaffected so far as the design goes;
before P1, confirm in `lean/formal` that no model's fetch action carries
the store check as a guard.

**The one difference: a hit makes no store request.** Two things depend on
a fetch's error today:

- **A moved pointer.** Checkout re-reads the pointer only when some fetch
  errored (`checkout.rs:868`); the common 404 is in-protocol — the
  collector took a handle after the pointer moved — and triggers the
  re-resolve. A hit-only checkout would complete against a stale document.
  So **a checkout that served any hit re-reads the pointer before it
  commits the baseline** (one GET per checkout) and re-resolves exactly as
  a 404 would have. `Inv_ReaderFetches`' row in `lean/SAFETY.md` says what
  bounds what a reader can materialize; it must say this.
- **An out-of-protocol overwrite (412) or delete.** A pod served from the
  cache does not notice it. The first pod on the node that fetched it, and
  every pod on other nodes, still refuses. This is observability, not
  safety: SAFETY.md's fetch row says so, and the `hits` counter makes it
  visible. A HEAD per hit would bring back one request per file, which is
  the cost this design removes.

## 5. Security

What the first draft said — "it could already publish arbitrary bytes" —
was the wrong equivalence (§11, F4). Plainly:

- **The cache is a new write channel between pods of one workspace on one
  node, and it is unaudited.** A publish is recorded (manifest generation,
  epoch, `conflicts.jsonl`, traces) and seen by every peer; a cache fill is
  recorded nowhere and reaches only same-node peers. It needs no write
  credential: a read-write worker whose credential was refused or expired
  can still fill. CRC-64 does not stop a deliberate forgery (it is linear:
  any 8 chosen bytes can force any CRC). `s3csi/SECURITY.md` P8 ("a pod
  cannot reach another pod's … state") and per-user design D9 ("nothing on
  the node") must be reworded, not footnoted: with the cache on, same-node
  read-write pods of one workspace share verified-by-CRC-only bytes.
- **Read-only pods do not mount the cache at all in v1.** A reader's tree
  today holds only cited, `If-Match`-fetched bytes; a cache hit would hand
  it bytes no committed fetch ever carried, from a channel a writer
  controls. The stated case (many read-write agents) loses nothing.
- **The way to let readers in, and to close the channel for writers, is a
  cryptographic hash in the manifest**: a `sha256` field on `LeanEntry`
  (serde default, additive), computed by whoever moves the bytes as the
  CRC-64 is today, verified on every hit. The cache then becomes "content,
  not trust" exactly as the chunk cache is (a forged body needs a
  preimage). Cost: a manifest field, and sha256 CPU on publish and on hit
  (about 1.5 GB/s per core against the table CRC's few GB/s). This is P3's
  first item, and the gate for `readers: true`.
- **Tenants cannot reach the cache.** Only workers mount it; a tenant sees
  clones in its own tree, and a tenant's write to a clone changes only its
  own copy (measured). A tenant can tell a file is reflinked (`FIEMAP`),
  which tells it a same-workspace peer is on the node: negligible.
- **Authorization is per workspace, not per object**, so a hit never serves
  bytes the pod's credential could not have fetched. If per-object
  authorization is ever added, the cache must be scoped with it.
- **Revocation.** A pod whose refresh was refused (P6) can still
  materialize cited bodies from the cache, where today each GET would 403;
  it cannot load a new manifest without a credential, so this is bounded
  by the document it already holds and by the workspace's lifetime on the
  node. P6's "bounded by the credential" stops being the whole bound for
  lean bytes; say so.

## 6. What it buys and what it does not

Buys, for the second and later read-write pods of one workspace on one
node: checkout with close to zero S3 requests and network bytes, free of the
request-rate wall; and close to one copy of the base on disk instead of one
per pod. Measured (§13): at 200k files and 25 ms per request, pod 2 checks
out in 6.6 s against 45.4 s, with 45 GETs against 200,045 and 233 MiB of new
disk against 1.8 GiB; at 1M, 74.5 s against 227 s (before the buffer fix). Peers' changes that another pod on the node already consumed are
local too.

Does **not** buy:

- **Local time for small files.** With durability batched as today, a
  plain write of an 8 KiB file and a clone of it cost about the same
  (37.8k vs 44.9k files/s single-threaded, §11; with no request delay a
  hit checkout took 7.0 s against 6.3–6.7 s for a fetch from an in-memory
  store, §13): for small files the win is requests and disk, not local
  speed. For large files the clone saves the bytes too.
- **Free writes.** The first write into each shared block pays for its copy
  (§13): 1.8× for an in-place edit of a small file, 3–7× for random 4 KiB
  writes into a large one, 1.7× for a sequential overwrite. Editors and git
  (write a new file and rename it) and truncate-and-rewrite pay nothing.
- **Page cache.** 16 clones read are 16 copies in memory (§3.6). This is
  the one thing overlayfs does better (§10).
- **The first pod on a node is charged, not neutral.** Per file it adds an
  inode, a create, a rename, a `FICLONE` and refcount-btree updates. M1
  (§13): +8% at 200k files and 25 ms, under the 10% gate, but its CPU is
  about 2.5× today's (196 s against 77 s, mostly system time), which the
  request latency hides and a fast store or a busy node would not.
- Anything for pods on different nodes, or for steady-state publish and
  sync latency (that is the walk and classify, about 0.45 s each at 1M,
  latency plan).

**The benefit needs co-scheduling.** Nothing in the chart or the CSI path
puts two pods of one workspace on one node; a 16-agent fan-out over 16
nodes is 16 first pods, zero hits, and the first-pod charge everywhere. The
fleet needs `podAffinity` on a workspace label (or a scheduler hint); the
chart's NOTES must say so, and the `hits` counter shows whether it is
happening.

## 7. Measurements before code

The user's rule: build it if it pays and is not disruptive.

- **M0 (done 2026-10-24, §11).** Reflink across two bind mounts; clone
  mtime; clone from an unlinked body; page cache not shared; per-file
  costs with and without per-file fsync.
- **M1: DONE 2026-10-24, results in §13.** Library bench on the box (XFS image, shipped allocator, arms that
  print themselves): two syncers sharing one cache check out the same
  manifest against the 25 ms in-memory store. Arms: cache off; **one
  syncer, cache on** (the first-pod charge); second syncer, cache on
  (reflink); second syncer, copy mode. Measured: checkout time, request
  count, disk use, CPU (the verify's CRC time). Shapes: 200k small files,
  1M small files, 50 × 200 MiB. Trees compared by digest across arms.
- **M2, one real node** (AL2023, XFS root with `reflink=1` confirmed, real
  S3): 1, 4 and 16 pods on one node under `podAffinity`, per-pod checkout,
  GETs and node disk, cache off and on. Needs a cluster; asked for
  separately, with a plan and cost.
- **Ship gate:** for an unchanged base, pods 2..N make about zero GETs; the
  first pod's checkout is within 10% of cache-off; trees are byte-identical
  across arms; and every control below fails its test:
  - C1: a hit that skips the CRC check (a corrupted body must be refused);
  - C2: a key without the etag (an older version must not be served);
  - C3: a read-only worker given the mount (it must not be in the pod spec);
  - C4: a fill from the tree (a rewrite between upload and clone must not
    reach the cache);
  - C5: a filler killed mid-fill, and one hung past the timeout (the waiter
    must take over, not hang or stampede);
  - C6: two fillers of one body (the torn-body race: the verify must catch
    it);
  - C7: a body evicted between lookup and clone (a miss, not a failure);
  - C8: an entry with no CRC (fetched, never a hit);
  - C9: a consume whose path the agent wrote during the pass (the clone
    must not land: H7);
  - C10: a hit-only checkout under a moved pointer (it must re-resolve).

## 8. Phases

- **P0:** M0 (done), M1. Repeat M0 on the target AMI's kernel.
- **P1, the `flint-lean` crate only:** `bodycache.rs` (lookup, verify,
  fill, single-flight, fallback-to-direct-write, counters), wired into the
  fetch sites of §3.2 with the per-site conditional rename kept; the
  pointer re-read after hits; `FLINT_SYNC_BODY_CACHE` names the directory,
  and unset is today's code path unchanged. Tests and the C1–C10 controls.
  No Kubernetes.
- **P2, the s3csi plugin and chart:** the per-workspace directory and lock,
  the second hostPath for read-write workers, the policy's prefix list,
  `worker.rs` and its test, grace, budget and whole-workspace eviction, the
  startup reflink probe, chart values `workers.leanBodyCache.{enabled,
  maxGiB, graceSecs, copyMode}`, NOTES on `podAffinity`, `lean/SAFETY.md`
  (§4), `s3csi/SECURITY.md` P8 (§5), per-user design D9, CHANGELOG.
- **P3:** `sha256` on `LeanEntry` and `readers: true` (§5); share lever A's
  manifest chunk cache per node the same way (its bodies are already named
  by a content hash); when branching lands
  (`flint-lean-branching-design.md`, design only), branches cite their
  base's objects by version, so one cache scoped to the base serves every
  branch forked from it.

## 9. Decisions for the user

1. Default: off until M2, then on for read-write pods on reflink
   filesystems only?
2. The node budget's default (a fixed GiB, or a share of the root
   filesystem), and the grace (proposed 1 h).
3. Accept P8/D9 reworded as §5 states for v1, with readers excluded, and
   `sha256` in the manifest as the P3 item that lets readers in?
4. A hit makes no store request, with one pointer re-read per checkout
   that served hits (§4). Recommended; the alternative, a HEAD per hit,
   gives back most of the gain.
5. Is co-scheduling by workspace (`podAffinity`) acceptable for the fleet?
   Without it the design has no case.
6. The free-space floor (§3.8): below it the plugin stops cloning. 20% of
   the trees' filesystem proposed; is a workload that writes databases in
   place into the tree expected (then the floor must be higher, or the
   cache off for that workspace)?

## 10. Rejected

- **The container runtime's image layers.** A CSI volume never passes
  through the runtime's snapshotter. Kubernetes' `image` volume is
  read-only, and a container's writable layer lives in the runtime's
  private storage, where the syncer cannot see the changes to publish them.
- **overlayfs (a shared base, a writable layer per pod).** It works across
  filesystems and it would share the page cache (the one thing it does
  better). But in lean: the base goes stale at the first consume (peers'
  changes land in each pod's layer) and cannot be swapped under a running
  pod; agents meet overlay semantics (renaming a base directory fails
  unless `redirect_dir`, hard links break, inode numbers change); a scoped
  checkout needs per-pod whiteouts. The first draft also charged it a whole
  -file copy-up on first write: on the reflink filesystem this design
  requires, overlayfs copy-up clones first (`ovl_copy_up_data` tries
  `vfs_clone_file_range`), so that cost was wrong and is withdrawn. Its one
  unique gain, scanning only the writable layer, would be a new change
  detector with new model and proof work. Revisit for that, or if page
  cache becomes the bound.
- **Hard links into the cache.** One inode in every tree: a `chmod` or an
  in-place write by one agent would change every pod's file and the cache.
- **One shared read-write tree.** Breaks one tree per pod: agents would see
  each other's live writes, not boundaries.

## 11. The adversarial review (2026-10-24)

Before any bench, the user asked for the approach to be attacked. Two
passes: the author's own against the code and the box, and an independent
reader given the doc and the code but none of the author's conclusions.
Eighteen findings, ranked by the reviewer; every code-level claim was
checked against the tree (`5131f18e`) before it was accepted, and all of
them held. Each entry: what the first draft said, what undermines it, and
where the revision answers it.

**F1. A torn body could be renamed into the cache and cloned unverified.**
Draft §3.3 wrote ranges "into a tmp file in the cache directory" and
cloned it without the hit-path verify. `RangedTmp::create` derives the
temp name from the target (`checkout.rs:120`), so two fillers of one body
share a name; `safefs::create_exclusive` unlinks on `EEXIST` and recreates
(`safefs.rs:166–180`), so filler 1 keeps writing an unlinked inode while
filler 2 writes a fresh one; `RangedTmp::commit` checks its own `written`
counter then renames **by name** (`safefs.rs:272–274`), so filler 2's
half-written inode lands under the body name. `chunkcache.rs:182–184`
names this hazard and pid-suffixes its temps; the draft cited that file as
precedent and did not carry the rule. → §3.1 (pid-and-random-suffixed
temps), §3.3 (verify the filler's own body), C6.

**F2. The clone dropped H7 at the consume and sync sites.** Draft §3.2 gave
one unconditional sequence (clone, mode, rename) for all three sites.
Consume and sync write conditionally: `write_file_atomic_if(…,
&still_clean)` (`barrier.rs:462–464`, `sync.rs:405–407`) re-runs
`local_dirty` immediately before the rename, because "a whole-tree sync
fetches for minutes" and an agent write since then must win (review
2026-09-18 H7; `consume_window`, `barrier.rs:2632`). Also, "the same
refusals on 412 and 404" is checkout's behaviour only: consume and sync
mark the path owed (`barrier.rs:425–428`, `sync.rs:356–360`). → §3.2 step
4 (conditional rename kept per site), §3.2 (what a miss means stays per
site), C9.

**F3. Per-file fsync on hit and miss, where today there is none.** Draft
§3.1 "tmp + fsync + rename", §3.2 "fsync, rename", §3.3 "the bytes reach
disk once, as today." `write_file_atomic` is "FAST (no per-file fsync)"
(`barrier.rs:2584–2600`); `safefs.rs:130`: "a million fsyncs would be the
cost and one `sync_tree` … is the equivalent"; durability is one `syncfs`
(`safefs.rs:296`) before the baseline, which covers the cache directory
(same superblock). The draft quoted a stale comment in `checkout.rs`. The
hit-time verify makes an unsynced torn body harmless (a miss), so the
fsync bought nothing. Box (20k × 8 KiB, one thread): with per-file fsync,
today's write 635 files/s, hit 974, **miss 329** — the first pod 2× slower;
with batched durability, today 37.8k, hit 44.9k, miss 15.6k. → §2 (the
fact), §3.1, §3.3 (no fsync), §6 (benefit reframed).

**F4. P8 and D9 are weakened more than the draft said; the cache is a new
unaudited write channel.** Draft §5: "what the cache adds is that the bad
bytes would reach same-node peers without a publish", as if equivalent to
publishing. `SECURITY.md:114–115` P8: "A pod cannot reach another pod's
worker, door or state"; per-user design line 493: "two workers, two trees,
two keys … nothing on the node." A publish is recorded (generation, epoch,
`conflicts.jsonl`, traces) and seen by every peer; a fill is recorded
nowhere, reaches only same-node peers, and needs no write credential (a
worker whose refresh was refused, P6 `SECURITY.md:82`, can still fill).
Read-only pods would receive bytes no committed manifest carried. →
§5 rewritten; readers excluded in v1; `sha256` on `LeanEntry` as P3; P8 and
D9 to be reworded in P2.

**F5. The marker protocol had four holes.** (a) Between `open(O_EXCL)` and
`flock(LOCK_EX)` a waiter can take `LOCK_SH` first, see no body, and fetch
itself: two fillers (feeds F1). (b) Death between marker create and unlink
leaves a marker no one owns: every later worker gets `EEXIST`, takes
`LOCK_SH` instantly, fetches itself — single-flight permanently off for
that body, and an inode at rest, contradicting "cost no inodes at rest";
no rule said who may unlink it. (c) A hung filler (S3 retries,
`checkout.rs:134–150`) blocked every waiter with no timeout; C5 covered
"killed", not "hung". (d) `flock` blocks; on a fan-out task it stalls the
driver (the shape the code fixed for `pwrite`, `checkout.rs:103–110`), and
waiters holding fetch-budget permits (`acquire_many_owned`) serialize pods
2..N behind pod 1. → §3.4 redesigned (O_CREAT; EX non-blocking or SH with
a timeout; blocking pool; no permit while waiting), C5.

**F6. Lifetime and eviction.** (a) The plugin's lock is per volume id
(`node.rs:305`); nothing serialized "grace expired, remove the cache"
against a new volume's publish — between `create_dir` and kubelet's mount
the pod fails (`type: Directory`, `worker.rs:209`); after the mount, the
worker is left an empty read-only path and every fill fails silently
(rmdir-under-a-foreign-bind semantics not verified). (b) "evicts the oldest
bodies": bodies are never modified, so mtime is fill time and atime under
`relatime` moves daily — FIFO evicts the hot base first. (c) "workers stop
filling": a worker sees only its bind mount, never the node total. (d) What
a failed fill does to a checkout was unspecified. → §3.6 (per-workspace
lock, whole-workspace eviction by the plugin), §3.3 (fallback to today's
direct write on any cache failure).

**F7. An entry with an empty `crc64_b64` has nothing to verify a hit
against.** `barrier.rs:435` and `sync.rs:373` fall back to the backend's
`meta.crc64_b64` when the entry carries none; a hit has no `meta`. Checkout
already refuses such entries (`checkout.rs:621–633`), consume and sync do
not. → §3.2 step 1 (no CRC, no hit), C8.

**F8. §4 misattributed the 404 and leaned on the resume arm.** The common
404 is in-protocol: the collector took a handle after the pointer moved,
and checkout re-resolves (`pointer_moved_past`, `reresolved`,
`checkout.rs:735–870`) — but only when some fetch errored
(`checkout.rs:868`: `results.iter().any(|r| r.is_err()) &&
pointer_moved_past`). A hit-only checkout completes against a document
older than the retire age G (`lean/SAFETY.md`, `Inv_ReaderFetches`). It
converges at the next consume, so not a safety break, but the SAFETY row
must say the reader-fetch guarantee no longer bounds what a reader can
materialize. And resume (`checkout.rs:434–460`) adopts only bytes this
tree's own incarnation fetched under `If-Match`; the cache extends
no-request materialization to bytes another process fetched. → §4
(pointer re-read after any hit; SAFETY row), §2 (the precedent qualified),
C10.

**F9. The benefit depends on unstated deployment facts.** At least two pods
of one workspace on one node: nothing in the chart or the CSI path
co-locates by CR, so a 16-agent fan-out over 16 nodes is 16 first pods and
zero hits, plus F3's charge everywhere. Read-only workers never fill, so a
node whose first arrival is a reader has no cache. `reflink=1` on the AMI's
root XFS was not verified (the box test used a hand-made image). With
`workers.quota=true` every tree is a loop ext4 image and the design is the
copy mode; the "project quotas" option is a node-image change (AL2023's
root lacks `prjquota`, loop-image perf note). → §6 (co-scheduling), §3.5,
§3.7, §9 item 5.

**F10. "A re-created CR never inherits" — but an edited CR does.** The CRD
(`flint-lean-chart/crds/flintleanworkspaces.yaml`) has no
`x-kubernetes-validations`/`oldSelf` rules; bucket, endpoint, prefix and
uid are editable in place. Keys carry the prefix (`lib.rs:661`), so a
prefix change cannot collide; a bucket/endpoint change with the same prefix
presents the same `(key, etag)` and is caught only by size+CRC (forgeable);
a `spec.uid` change leaves the directory owned by the old uid, so every
fill fails. → §3.1 (directory scoped by CR UID and a hash of endpoint,
bucket, prefix, uid).

**F11. Admission and the pod builder.** "Needs no new rule" was true for the
policy as a prefix check with no count (`workers-policy.yaml:102`), but
`worker.rs:6–7` ("no hostPath … or exactly one") and the test at
`worker.rs:846` (`assert_eq!(hp.len(), 1)`) pin exactly one; and placing
the cache under `workers.cacheHostPath` contradicts the same-filesystem
requirement (it is the passthrough block cache's root, possibly another
device, swept by `sweep_cache_root`, `node.rs:345`). "Startup adoption must
learn to recognise the cache" was moot: `VolumeState::list`
(`state.rs:227–239`) ignores directories without `state.json`, so nothing
today would remove it; the lifetime is all new code. → §3.5.

**F12. The first pod is not "not bought"; it is charged.** Per file: one
inode, one create, one `FICLONE`, one rename, XFS refcount-btree updates
(and, as drafted, F3's fsyncs). Box, single thread, batched durability:
miss 15.6k files/s against today's 37.8k. M1 as drafted measured only "the
second syncer's checkout". → §6, §7 (one-syncer cache-on arm; gate ≤10%).

**F13. "Local clone time" is a local re-read plus CRC with a table CRC.**
`flint-store/src/lib.rs:61–69`: hand-rolled slice-by-8, no PCLMUL. The hit
verify reads every byte through the CPU before the clone; "close to zero
requests and bytes" is network-only. → §3.2 step 3, §7 (CPU measured in the
50 × 200 MiB shape).

**F14. Reflinks share disk, not page cache.** Each clone is its own inode;
16 agents hold 16 copies of the hot set in memory, and the verify pulls each
body through once more. Box: reading a 512 MiB clone after its source added
another 512 MiB to `Cached`. The one thing overlayfs does better; §10 had
not weighed it. → §3.6, §6, §10.

**F15. overlayfs was partly rejected for a wrong cost.** "The first write to
a large file copies all of it up": on the reflink filesystem this design
requires, overlayfs copy-up clones first (`ovl_copy_up_data` tries
`vfs_clone_file_range`). The other reasons stand. → §10 corrected.

**F16. The body's mode.** `RangedTmp::create` applies `mode` to the temp
(`safefs.rs:~230`); reusing it with `Some(entry.mode)` would make a cited
0000 or 0200 body unreadable to every later same-uid worker — a permanent
miss. → §3.1 (a body's mode is the cache's, never the entry's).

**F17. Process-death leftovers.** Death after the cache temp is written and
before its rename leaves the temp; eviction by "oldest bodies" never swept
temps (`chunkcache.rs:198` has a `retain`-style sweep). Death after the body
rename and before the marker unlink: F5(b). Death after `FICLONE` into the
tree temp and before its rename: `.flint-sync-tmp` is skipped by the scan
(`scan.rs:360`) and unlinked by the next `create_exclusive` — fine. → §3.6
(temps swept with the workspace), §3.4.

**F18. Revocation.** A pod whose refresh was refused (P6) still materializes
cited bodies from the cache, where today each GET would 403. Bounded by key
lifetime (per-user design line 556) and by the pointer GET still needing a
credential — low — but P6's "bounded by the credential" stops being the
whole bound for lean bytes. → §5 (stated).

**Box facts** (2026-10-24, kernel 6.12.0-061200-generic, XFS image
`reflink=1`, cache and tree as two bind mounts of one filesystem; scripts
`reflink-test.sh`, `reflink-adv*.sh`, not kept):
- `FICLONE` and `copy_file_range` across the two mounts both produce shared
  extents (fiemap flag `0x2000`); the control clone inside one mount is the
  same. So `copy_file_range` is a reflink there, not a copy fallback.
- A write to the clone changes the clone and leaves the source intact.
- A clone's mtime is the clone time (source 2020-01-01 → clone now).
- `FICLONE` from a body unlinked after open succeeds with the right bytes.
- Reading a 512 MiB clone after reading its source adds +512 MiB to
  `Cached`: no shared page cache.
- `syncfs` on the tree mount flushes the cache mount's dirty pages (Dirty
  200 → 40 KiB).
- Per-file, 20k × 8 KiB, one thread, per-file fsync: write+fsync+rename 635
  files/s; clone+fsync+rename 974; clone+rename 39.4k; fill cache (with
  fsync) then clone+fsync 329. Batched (no per-file fsync, one `syncfs` at
  the end): write 37.8k (syncfs 0.24 s); clone 44.9k (0.02 s); fill then
  clone 15.6k (0.04 s).

**What the reviewer could not verify:** the shared-extents result (it had
no box access; measured by the author above); `reflink=1` on the target
AMI's root; the rmdir-under-a-foreign-bind semantics in F6(a).

**Verdict:** the core — one verified body per workspace per node, cloned
into private trees, below the fetch — stands. The first draft's
implementation sketch did not: it would have halved the first pod's
checkout, reintroduced a fixed overwrite, let a torn body through, left
single-flight dead after one crash, and undersold the security change.
Proceed to M1 with §3–§7 as revised, with the first-pod arm and controls
C6–C10 in the gate, and with §9's decisions taken first.

## 13. M1 results (2026-10-24)

**Rig.** The Linux box (8 cores, 30 GiB, kernel 6.12.0). Trees and cache on
an XFS `reflink=1` image loop-mounted on the box's ext4 NVMe; the seed tree
on the ext4 itself. The in-memory store, every request paying the injected
delay through both checkouts. The shipped allocator (musl + mimalloc,
release). Each run: A publishes the seed, pod 1 checks the workspace out,
then pod 2 does, sequentially (no single-flight, §3.4, was exercised). Every
arm's result line carries the arm the binary printed; every run compared
both pods' trees with the seed by SHA-256 over every data file, and every
run matched. Logs: box `/mnt/nvme/m1*`, `/mnt/nvme/cow*`, not kept in the
repo.

**The prototype** (local branch `lean-node-body-cache-prototype`, not
pushed): `LeanConfig::{body_cache, body_cache_copy}`, a
hit/miss arm in `checkout.rs::materialize` (`body_cache_materialize`:
verify every body by size and CRC-64, clone or copy into the tree, fall
through to the plain fetch on any failure, counted), `safefs::
{clone,copy}_via_tmp_fast`, and two ignored tests in `tests_bench.rs`:
`body_cache_checkout_cost` and
`body_cache_refuses_a_body_that_does_not_hash_to_its_citation`. Checkout
only: no consume or sync, no single-flight. With the cache off the lib
suite is unchanged: 393 passed, 0 failed (4 ignored: the 2 new tests).

**Controls.** Pod 1 fills the cache; one body has a byte flipped in place,
another is cut short; pod 2 must refuse both and still hold the seed's
bytes. Passed on the clone and the copy arms (48 hits, 2 refused, digest
equal). Positive controls: with the hit's CRC check disabled (K1, the clone
path; K2, the copy path; the size check left on), the control FAILED as
required (49 hits, 1 refused).

**A bug found and fixed on the way.** The first matrix allocated a fresh
1 MiB read buffer per file in every verify, copy and fill: 200k × 1 MiB of
zeroing and page faults. With no request delay it made a hit checkout 2×
slower than a fetch (14–15 s against 6–9 s). Buffers are now sized to the
file (`checkout::read_buf_len`). Today's resume check
(`checkout::local_crc64_b64`) has the same pattern on main; the fix is on
the prototype branch only and would be its own change. Tables below are the fixed build unless marked.

**200k files × 8 KiB, 25 ms per request.**

| arm | pod 1 | pod 2 | pod 2 GETs | pod 2 new disk |
|---|---|---|---|---|
| off (today) | 44.8 s | 45.4 s | 200,045 | 1.8 GiB |
| clone | 48.3 s (**+8%**) | **6.6 s** | **45** | **233 MiB** |
| copy (fallback) | 55.1 s † | 10.6 s † | 45 | 1.8 GiB |

Quiet-box pair (load < 1 at start). † The run before it, whose baseline
started at load 10 after a build (off 48.8/52.1 s, clone 50.1/11.6 s).
Before the fix, three rounds: off 45.1–45.4 s; clone 51.3–51.5 s (+14%)
and 13.8–16.7 s; copy 49.7–55.0 s and 9.7–9.9 s.

**200k files × 8 KiB, no request delay** (local cost only, fixed build):
off 6.7 / 6.3 s; clone 11.4 / **7.0 s**; copy 14.7 / 9.6 s. A hit now costs
about what a fetch from an in-memory store does.

**24 files × 256 MiB, 25 ms** (before the fix; the buffer was right-sized
for these): off 8.6–42.6 s per pod (noisy writeback); clone pod 1 7.1–7.8 s,
pod 2 **1.9–2.0 s**, 2 GETs, 0 MiB new disk; copy pod 2 37.3–42.3 s, a local
copy slower than the fetch.

**1M files × 4 KiB, 25 ms, one round** (before the fix): off 224.8 / 227.1 s,
1,000,219 GETs, 5 GiB each; clone 267.1 s (+19%) / **74.5 s**, 219 GETs,
1.1 GiB; copy 248.8 s (+11%) / 86.7 s. Not re-run after the fix.

**Writes into a clone** (fio and a Python loop, XFS on a direct-I/O loop
device, one round; ordinary file vs clone):

| workload | ordinary | clone | |
|---|---|---|---|
| 256 MiB, random 4 KiB writes over 64 MiB, buffered + fsync | 1016 MiB/s | 144 MiB/s | 7× slower, first touch |
| same offsets again | 1143 MiB/s | 955 MiB/s | about parity |
| random 4 KiB, `O_DIRECT` | 69.4k IOPS, 14 µs | 20.8k IOPS, 48 µs | 3.3× slower, first touch |
| extents after the random writes | 1 | 21,859 | fragmented |
| sequential 1 MiB read after (`O_DIRECT`) | 1191 MiB/s | 1231 MiB/s | no penalty on NVMe |
| sequential 1 MiB overwrite (`O_DIRECT`) | 3048 MiB/s | 1778 MiB/s | 1.7× slower |
| append 64 MiB (control) | 1939 MiB/s | 2560 MiB/s | no copy-on-write |
| 20k × 8 KiB, 100-byte in-place edit | 1.04 s | 1.85 s | 1.8× (the copy at writeback) |
| the same edit again, and a third time | 1.05 s | 4.41 s | 4.2× while CoW-preallocated |
| ... with `cowextsize` = 1 block on the tree | 1.53 s | 1.17 s | parity (§3.5) |
| truncate + rewrite 8 KiB | 0.82 s | 0.99 s | parity |
| write new + rename (editors, git) | 0.95 s | 0.94 s | parity |

**What M1 decides.**

- **The gate passes:** pods 2..N make about zero GETs for an unchanged
  base (45 of 200,045); the first pod's checkout is +8%, under 10%; trees
  are byte-identical in every arm; C1 holds and its positive controls
  fire. C2–C10 belong to P1 (they test code M1 did not build).
- **Clone everywhere it is available; copy only as the no-reflink
  fallback.** The first matrix suggested copying small files (copy 9.7 s
  against clone 13.8 s); that was the buffer bug. Fixed, clone is faster at
  every size measured (7.0 s against 9.6 s with no delay) and saves the
  disk. Copy stays off by default (§3.7): for large files it was slower
  than the fetch.
- **Set `cowextsize` to one block on every tree** (§3.5).
- **Writes are the cost to state, not hide.** Code-editing workloads pay
  nothing; a database or large file written in place pays 3–7× on each
  block's first write and fragments; a full node turns those writes into
  ENOSPC or SIGBUS (§3.8).
- **Still open:** the first pod's CPU (2.5× today's, mostly system time);
  pods that start together (single-flight, §3.4, never ran); the 1M shape
  on the fixed build; consume and sync; and M2 on a real node against real
  S3.

## 12. Recommendation: the reflink cache, not overlayfs — on three conditions

**Proceed with the body cache as revised; do not build on overlayfs.** The
reasons, in the order they decide it:

1. **A layer model fits an immutable base; a lean tree is not one.** Every
   floor, consume pulls peers' changes into the tree. Under overlayfs those
   land in each pod's upper layer, the lower layer is fixed at mount time
   and cannot be swapped under a running pod, so on an active workspace
   every pod's upper diverges from the base by the whole set of peer
   changes and the shared part shrinks to nothing. The reflink cache has no
   such decay: a body consumed by one pod on the node is cloned by the
   next, so sharing tracks the live document, not the mount time.
2. **overlayfs cannot verify at read time; the cache verifies every hit.**
   The kernel serves a lower file directly. Whoever filled the base (a
   worker, with the fill-trust problem of §5) poisons every pod mounted on
   it, with no CRC or hash check in the path, ever. The cache checks each
   body against the manifest's CRC-64 on every hit today and against a
   `sha256` once P3 lands; that is the whole difference between a cache
   and a trust.
3. **Zero protocol change versus a new change detector.** Each pod's tree
   stays a plain directory: the scan, the barrier, the model and the proofs
   are untouched (§4). overlayfs's one unique gain, scanning only the upper
   layer, means reading overlay's on-disk format (whiteouts as 0/0 char
   devices, opaque dirs, `redirect_dir` xattrs — kernel-internal, not an
   API), a different observation for the model
   (`feedback_model_the_observation`), and new proofs. The steady-state
   win it promises has a cheaper path that the latency assessment already
   named: an inotify/fanotify-fed scan with a full-walk fallback.
4. **What overlayfs does better, and why it does not decide it.** Shared
   page cache (F14), and working across filesystems (so with the loop-image
   quota on). Page cache matters for a hot read set larger than the node's
   memory divided by the pod count — a model-weights workload, not an
   agent's code base; if that is the fleet's shape, revisit. The quota is
   off by default and on XFS can move to project quotas (§3.7).

**The three conditions**, from the review; without them the design has no
case and should not be built:

- **Co-scheduling.** The fleet puts the pods of one workspace on one node
  (`podAffinity` on a workspace label). Otherwise every pod is a first pod
  and pays the charge (F9, F12).
- **Reflink-capable trees.** XFS with `reflink=1` under the plugin's
  volumes directory (AL2023's root; verify the AMI). On ext4 the design
  saves S3 requests only and must stay in request-bound shapes (§3.7).
- **A base worth sharing.** The win scales with (pods per node) × (files
  and bytes in the base). Below about 200k files or a few GiB, a checkout
  is seconds and this is not worth its plugin code; at 1M files and 20 GiB
  with 16 agents a node, it is minutes per pod and 300 GiB per node.

**Order:** take §9's decisions; then P1 behind the flag, with controls
C2–C10 and the open items of §13. M1 (§13) passed its gate: the second pod
6.9× faster with 45 GETs instead of 200,045, the first pod +8%.
Pull `sha256` on `LeanEntry` forward to **before the default turns on**,
not P3: it is additive (serde default), it closes the unaudited channel of
§5 for writers and readers alike, and it is the only thing that makes the
cache "content, not trust" rather than "verified by a forgeable checksum".

## 14. Decision: parked (2026-10-24)

The user parked the cache: "park it, commit the doc as the record." The
fleet's typical workspace is **about 10,000 files, mostly small, with some
around 300 MB**.

For that shape the cache has little to give. At 25 ms per request and 128
fetches in flight (about 5,000 files/s, §2) the files cost about 2 s of a
pod's checkout; each 300 MB file adds about 0.7 s at the ~420 MiB/s a node
pulls from S3 (`flint-s3-csi-chart/values.yaml`). So a pod's checkout is a
few seconds, and the cache could save each extra co-located pod of the
same workspace only that, plus about 300 MB of disk per large file. Against
it stand the plugin code of §8 P2, the security change of §5, the
semantics of §3.8, the co-scheduling of §6 and the first pod's charge of
§13. By the user's rule (build it if it pays and is not disruptive) it does
not pay here.

**Revisit when any of these holds**, with this doc and the prototype branch
as the starting point (P1, §8):

1. **Large bases, co-located:** several pods of one workspace per node on a
   base of about 100k files or more, or several GiB (monorepos, datasets,
   model weights). M1 measured 45 s → 6.6 s per extra pod at 200k files.
2. **Burst starts throttled by S3:** many pods of one workspace starting
   together on one prefix (100 pods × 50k files is 5M GETs, about 15 min
   at 5,500 GET/s); the cache divides the GETs by the pods per node.
   S3 `503 SlowDown` during fleet starts is the signal.
3. **Node disk pressure from duplicate bases:** N pods each holding a full
   copy of the same large files.

Before building: §9's six decisions, `sha256` on `LeanEntry` (§5), and
§13's open items (the first pod's CPU, single-flight, consume and sync, M2
on a real node).
