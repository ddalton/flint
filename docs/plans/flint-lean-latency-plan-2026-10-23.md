# flint lean: low-latency publish and sync, and what can still be simpler

**Date:** 2026-10-23. **Status:** plan. Levers A and C are built and
tested on branch `lean-latency-ac`, uncommitted (§5, §6). The others are
not started. Nothing here changes a rule of `lean/formal/LeanP1.tla`, so no
claim of `lean/PROTOCOL.md` moves and no proof is re-run.

**Read with:** `lean/PROTOCOL.md` (the protocol as it ships),
`lean/e2e/perf/results/barrier-steps-2026-10-23.md` (where a barrier's time
goes, step by step, the same day), and
`docs/plans/flint-lean-simplification-analysis-2026-09-24.md` (the
simplification that produced today's shape).

## 0. The questions, verbatim

User, 2026-10-23: "can you check if the lean protocol can be made simpler
and also allowing low latency publish and sync speeds?" Then: "write it up
in docs/plans and start with A and C. What is the FIFO fairness and is it
needed?"

### 0.1 The answers, short

1. **The protocol is already the simple one.** Simplification step 5
   (2026-09-25) removed the inbox entries, the writer queue, the citation
   repairs and the journal's installed document. What remains is a
   pointer CAS, a derived owed set, a per-barrier lease and a retire age.
   One large protocol-level removal is left, the lease, and §7 says why it
   should stay.
2. **The latency is in the implementation, not the protocol.** Two kinds
   of cost dominate. On S3, chains of sequential requests: every manifest
   load fetched every chunk one at a time, and every publish ran three
   LISTs and four GETs of garbage collection under the lease. At large
   workspaces, CPU and local I/O in proportion to the workspace: the
   baseline file parsed twice and rewritten, the whole document merged and
   serialised under the lease, the tree walked. Neither needs a protocol
   rule changed.
3. **FIFO fairness** is the queue in the lease cell that hands the cell to
   writers in the order they asked, so W writers wait at most W commit
   sections. It is needed for exactly as long as the lease is, and the
   lease is load-bearing for garbage collection, not for the merge (§7).
   Keep both.

## 1. Where the time goes today

Two measurements, one per kind of cost.

**Requests, in-memory store** (the 2026-09-25 barrier bench,
`tests_bench`, one changed file; S3 adds tens of milliseconds to each
sequential request):

| files | idle tick | one-file publish | a peer's one-file change arriving | requests per publish |
|---:|---:|---:|---:|---:|
| 20k | 86 ms | 0.5 s | 0.2 s | 24 |
| 200k | 0.9 s | 2.3 s | 1.7 s | ~65 |
| 1M | 5.1 s | 10 s | 15 s | ~240 |

**Steps, loopback MinIO** (`barrier-steps-2026-10-23.md`): a one-file
publish took 195 ms, 835 ms and 8.5 s at 20k, 100k and 1M files. At 1M:

| bucket | share |
|---|---:|
| the baseline file: parsed in the consume, parsed again before the scan, rewritten after the CAS | 38% |
| the whole document under the lease: load, merge, CAS, retire set, free | 38% |
| walking the tree | 20% |
| per-publish garbage collection | 0.8% |
| the lease's own requests | ≈0 |

Loopback requests cost well under a millisecond, so that table is CPU. The
request column is what S3 multiplies: the load step alone was 220
sequential chunk GETs at 1M. The 2026-09-14 contention drill on real S3
puts a request at roughly 25 ms (claim-to-merge 138 ms for about five
requests, merge-to-CAS 62 ms for two or three PUTs).

**Visibility is bound by the floor, not by any of this.** The pointer and
the request cell are read only inside a barrier, and a barrier runs at the
floor tick (`floorSecs`, 60 s) unless the agent publishes. The 1 s poll
reads the agent's local `.flint/` files only. So a peer's or a UI's change
reaches an idle agent's tree in up to 60 s, and the gateway's "please
publish" waits for the same tick. Until this plan, `lean/gateway/README.md`
said that request was honoured "at its next poll (about a second)". It is
corrected on this branch.

## 2. The levers

None changes a LeanP1 rule. "Model" says what the formal side needs.

| | lever | what it removes | model | status |
|---|---|---|---|---|
| **A** | a manifest chunk cache, verified by address, misses fetched in parallel; the gateway judges and commits against the document it holds | per load: N sequential chunk GETs become one LIST plus the changed chunks. Per UI save: two whole-document loads become two pointer reads | none: a load returns the document at the pointer it read, as before | **built** (§5) |
| **C** | the collection pass (retire ledger, reap, orphan, generation and chunk sweeps) runs on a cadence, not on every publish | per publish: 3 LISTs and 4 GETs under the lease | none: the reap and the sweeps are steps a commit section may take or not | **built** (§6) |
| B | an adaptive news poll: read the pointer at 1 s while the agent is active, back to the floor when idle; the request cell rides the same read | visibility from up to 60 s to about 1 s | none: more barriers, each the same steps | **built** (§10). Cost: 2 GETs/s per active writer, 1 per active reader |
| D | the baseline in memory, loaded once per process, persisted incrementally | at 1M, the baseline file is 38% of a 10 s publish: two parses (about 27%, removed by loading it once) and one full rewrite (about 9%, mostly removed by persisting incrementally); and most of the idle tick | none | **built** (§11) |
| E | the walk in parallel: workers take directories from one queue, and each entry is stat'ed relative to its directory. Not inotify (§9, item 8) | at 1M, most of the scan's 2 s (walk and classify) in every barrier, the idle tick included | none: the walk returns what it returned (§13) | **built** (§13) |
| F | policy: `sentinelMinIntervalSecs` 5 → 1, and a bytes-only publish budget | a sustained ceiling of one declared publish a minute | none | not started; a user decision |
| G | the document under the lease merged and serialised incrementally, by chunk | at 1M, 3.8 s of a 10 s publish (38%) | none: the same document, chunk list and retire set | **built**, the commit side (§12); the consume side is G2 |
| G2 | the consume derives only where the document moved since its last derive: a chunk-list diff | nothing measurable on the shipped allocator (§14) | a new rule and claim (`ddoc`, `Inv_DiffSound`) | **dropped** (user, 2026-10-23); archived on the local branch `lean-latency-g2-dropped` |
| GW | the gateway's UI commits by path: an overlay re-chunked by chunk, the installed document held, mimalloc | at 1M, about 3.7 s of every UI save, rename and delete (§16) | none: the same document, chunk list and retire set | **built** (§16) |
| RH | the post-checkout re-hash: a checkout measures its racy flags at the agent's gate, after waiting out the last window when that is many files | the whole tree hashed by the first barrier after a checkout: 107 s at 1M, 21.6 s at 200k (§17) | LeanScanRacy gains the gate (`MeasureAtGate`); re-proved, 38 obligations; the wait is ticks before the gate | **built** (§17) |

**What the two measurements say about order.** On S3 and at 20k–200k
files, requests dominate, so A, C and B come first. At 1M, CPU dominates,
so D, G and E come first; A also matters there, because 220 sequential
GETs at 25 ms is 5.5 s per load before any CPU.

## 3. What can be simpler, and what should not be

| | change | protocol? | verdict |
|---|---|---|---|
| S1 | delete the legacy single-object manifest layout and its migration (about 50 code sites: `Entries::Single`, `REFUSAL_DOC`, `KEEP_GENERATIONS`, `sweep_generations`, the legacy HEAD arms) | no | **do it**: nothing is deployed on that layout, and its sweep was one of the per-publish LISTs |
| S2 | three collection mechanisms (retire reap, orphan sweep, chunk sweep) folded into one pass | no | done in shape by C; one LIST of `files/` and one of `chunks/` per pass could share a fence |
| S3 | the reader's `reader.json` memo folded into the baseline's `derived_etag` | no | small; do with D |
| S4 | the lease, its FIFO, deposal and fencing replaced by a lock-free pointer CAS | yes: DP1, DP2, `Inv_OneHolder`, Depose, Fence | **do not**, §7 |
| S5 | the tombstone map dropped (one GET per load) | yes: AN5 restated and re-proved | low value; with A the tombstones object is cached like a chunk |
| keep | rescope, the two-scan delete rule, racy-clean, content convergence, the cheap path, the ack rules | — | each is what publishing a mutable POSIX tree honestly costs |

## 4. The measurement method for every lever

`tests_bench::barrier_cost_against_workspace_size` (in-memory store), with
two knobs added by this branch:

- `FLINT_BENCH_DELAY_MS`: every request pays this round trip once the
  second writer has checked out (the seed and the checkout run at memory
  speed). A delay on EVERY request, not only ranged GETs and PUTs as the
  store's older knobs did: whole GETs, HEADs, LISTs, DELETEs and the lease
  cell's reads and writes. 25 ms is the drill's real-S3 figure.
- `FLINT_BENCH_ARM=control`: the shape before A and C from the same
  binary: no chunk cache, chunk fetches one at a time, the collection pass
  at every commit.

Arms are interleaved within each repetition, three repetitions at 20k and
200k, one at 1M. Ranges, never means. The raw logs stay on the box
(`/mnt/nvme/latency-bench/`): they are generated output.

## 5. Lever A, as built

**The cache.** `lean/syncer/src/chunkcache.rs`: a `ChunkCache` trait with
two implementations.

- `DiskChunkCache`, the syncer's: one file per chunk under
  `.flint-sync/chunk-cache/`, in the state dir, so it survives a container
  restart and costs no memory between barriers.
- `MemoryChunkCache`, the gateway's: one per workspace, held beside the
  gateway's existing manifest cache (`ManifestCache`).

Both keep only the chunks of the document loaded last, so either holds
about one manifest: ~277 bytes per entry, 66 MiB at 250k files. The disk
cache counts toward the tree's node disk.

**The load** (`manifest::load_from`, `fetch_chunks`):

1. The pointer, read fresh, exactly as before.
2. Each chunk it names from the cache, used only if the body hashes to its
   address. A body that does not is dropped and fetched.
3. The misses from the bucket, `chunk_fetch_parallelism` (16) at a time.
   They were one at a time.
4. Beside those, ONE LIST of the chunk prefix when anything came from the
   cache. A body proves content, never that the object is still in the
   bucket.
5. A chunk the pointer names that the bucket does not hold, by 404 or by
   absence from the LIST, is decided exactly as before: the pointer moved
   (a sweep took a superseded generation) restarts the load; an unchanged
   pointer is a hole and refuses. A document is never served short.
6. A LIST the credential may not make, or one that fails, sends the cached
   chunks to the bucket instead. The existence check is paid the old way,
   never skipped. The broker's read grant includes a prefix-bounded LIST,
   so readers take the fast path.

Every body is hashed once, whichever way it arrived:
`chunk::assemble_verified` skips the re-hash `assemble` does, and the
loader checks each body on arrival so it can tell a damaged cache entry
from a bad fetch.

**The write side.** `cas_write_chunked` keeps each chunk it uploads, so a
writer's next load of its own document fetches nothing. The tombstones
object is cached like a chunk.

**The gateway.** `put_file` judges its precondition against `view()`, the
document at the pointer just read: from memory while the pointer has not
moved. Every UI commit (save, delete, rename, promote) makes its first CAS
attempt against the same view (`manifest::commit_edit_from`). The CAS is
If-Match on that document's etag, so a stale view loses and the retry
loads. `view()` no longer reads the pointer twice. A UI save on a
workspace the gateway holds is now two pointer GETs, plus one LIST when
the pointer moved since the gateway last read it, plus its writes. Before,
it was two whole-document loads, each the pointer and every chunk.
`Workspace::without_chunk_cache` turns the memory cache off for an
embedder that would rather pay requests than memory.

**Why it is safe, in one place.**

- *Content.* A cached body is used only after it hashes to the 128-bit
  address the pointer names. A planted body needs a preimage.
- *Existence.* A load that used the cache still proves, once, that the
  bucket holds everything the pointer names. The two tests that pin the
  fail-closed load run on both paths:
  `a_reader_swept_mid_read_restarts_onto_the_current_generation`, its
  sweep landing at the GET uncached and at the LIST cached, and
  `a_missing_chunk_refuses_cached_or_not`.
- *Containment.* The state dir sits in the app-writable tree. An address
  becomes a file name only if it is 32 lowercase hex digits
  (`chunk::is_chunk_address`), so no pointer names `../x`. No link is
  followed at the file (`safefs::open_beneath_nofollow`). A FIFO is a miss,
  not a wait. Writes go through `safefs::write_via_tmp_fast`. The cache
  never recreates a missing state dir.
- *The model.* A load returns the document at the pointer it read, as an
  uncached load did. The cache changes which requests produce the bytes,
  never which bytes. No step of `LeanP1.tla` reads a load's request
  pattern (its `RLoad` is one atomic read). `LeanChunkGC.tla` does model
  a reader chunk by chunk: `RdrFetch` takes any unread chunk at any
  instant, and a missing one restarts the read under a moved pointer and
  is a hole under an unchanged one. A cached load whose LIST shows its
  chunks present is those reads, taken at the LIST's instant, an order
  the model already has; a missing one meets the same rule. The two
  assumptions this rests on (an address names its bytes; a LIST shows
  what the bucket holds) are in `lean/SAFETY.md` §2.

**Knobs.** `FLINT_SYNC_CHUNK_CACHE=off`, `FLINT_SYNC_CHUNK_FETCH_PARALLELISM`
(default 16; 1 = one at a time). `LeanConfig::chunk_cache`,
`LeanConfig::chunk_fetch_parallelism`.

**What it does not do.** It does not remove the CPU of a load: the
document is still parsed whole. That is lever G. And a load that needs
chunks still pays one LIST, which on S3 is slower than a GET. §9 Q1 asks
whether to drop it.

## 6. Lever C, as built

**The pass.** Step 6 of the barrier runs the collection pass (the retire
ledger, the reap of what is due, the orphan sweep when it is due too, the
generation sweep, the chunk sweep) only when `collect_every_secs` has
passed since this writer last ran it. The clock is
`.flint-sync/collect-at`, so a restart does not reset it. It is written
only when the ledger was read: a pass that read nothing is retried at the
next commit.

**The cadence.** Half the retire age by default: 300 s at the shipped
G = 600 s. A replaced version waits at most about 1.5 × G before it is
collected, instead of G. `FLINT_SYNC_COLLECT_EVERY_SECS=0` collects at
every commit, the shape before. A drill that sets the retire age to 0 gets
a cadence of 0, so it collects at every commit as it always did.

**The ledger in parallel.** A pass every few minutes meets every retire
log written since the last one, under the lease. The new ones are now read
`chunk_fetch_parallelism` at a time; they were read one at a time.

**Why it is safe.** The pass still runs inside the commit section, under
the cell (DP2). Only when it runs changes. The model's `Reap` and `Sweep`
are steps a commit section may take or not, so a schedule that takes them
less often is a subset of the model's behaviours. Skipping a pass costs
storage for the interval and never a citation: nothing is deleted while it
is skipped. The one collection step the model requires of a commit,
`Collect` (before `Finish`), still runs at every commit: under the retire
age it takes nothing, and what the commit retired is logged with its CAS
(`manifest::cas_write_retiring`), outside the gate. The trace converter
(`formal/trace/ndjson2core.py`) ignores generation and chunk sweep
events, and maps a reap one step per key, so a pass that reaps a batch
replays as before.

**Checked against the model at the shipped cadence.** The conformance
scenarios collect at every commit (`cfg_for`), so the code-trace check's
83/83 never saw a commit that skips the pass. With
`FLINT_TEST_COLLECT_EVERY_SECS=300` they run at the shipped cadence: 28 of
the 31 write their traces, 21 of those traces' 34 commits skip the pass
(sweeps of generations and chunks 34 → 13, reaps 25 → 12, `Collect` 24 at
both), and LeanP1 accepts all 28 (tla-mc 0.1.2, 2026-10-23). The other
three cannot run at 300 s, and say so in their fixtures: two crash the
barrier at the first DELETE between the CAS and step 7, which is the
pass's, and one asserts a reap. The mutation set needs one of those
traces, so it was not generated from the 300 s set; at every commit it
was, and 83/83 hold.

**Tests.** The battery's config keeps `collect_every_secs = 0`, so every
existing collection test keeps its meaning.
`a_publish_inside_the_collection_interval_collects_nothing_and_the_next_pass_collects_it_all`
pins the cadence: inside the interval no pass step runs and nothing is
deleted, every request is still charged to a step, and the next pass
collects the versions the skipped publishes replaced.

## 7. FIFO fairness: what it is and whether it is needed

**What it is.** The lease cell (`.flint/lean/epoch`) carries, beside the
holder and its epoch, a list of waiters (`EpochState::waiters`). A writer
that finds the cell held appends itself once, by a CAS on the cell
(`epoch_enqueue`). The holder's release hands the cell to the head of that
list (`epoch_handoff`). A cell released for someone else is not taken by
anyone else. The head polls every 200 ms and the rest every second. The
result is the property the lease module states: **W writers wait at most
W commit sections.** Two timers keep a dead party from wedging the queue.
A reservation nobody claims across two spaced observations is abandoned.
A holder whose cell stands still for six observations ten seconds apart
(a minute) is deposed, and the manifest rotates so its CAS fails.

**Why it exists.** The user's requirement of 2026-09-13: "if two read-write
users are concurrently working on the workspace, one should not face
starvation." The lease was then held for a pod's whole life, and a second
writer waited the first writer's lifetime in `ContainerCreating`. The fix
held the lease for one barrier's commit section only, and added the queue
so that a writer behind busy peers is served in order, not by luck. The
6-writer contention drill on real S3 (2026-09-14, after its fixes)
measured a claim wait p50 of 0 and p90 of 0.7–1.1 s, a 0.40 s hold, an
ack p50 of 1.86 s, and 0 of 679 acks timed out.

**Is it needed?** Separate the queue from the lease.

- **Not for safety.** The pointer CAS (If-Match), the uploads'
  If-None-Match and the commit's re-read of its own uploads carry every
  safety claim. A false deposal costs one barrier, never a write.
- **The lease itself is load-bearing for collection.** The reap, the
  orphan sweep and the chunk sweep delete only under it (DP2). The sweep's
  safety rests on no other writer standing between its re-read of its
  uploads and its CAS while the sweeper deletes: `LeanP1SweepFree`, the
  sweep off the lease, breaks `Inv_CitationsLive`. Removing the lease
  needs a lease-free collection design first, for example deletions
  decided against pointer generations every writer has passed. That is
  new protocol and new proofs (DP1, DP2, `Inv_OneHolder`, Depose, Fence).
- **The queue is load-bearing only for bounded waiting.** Without it the
  lease is a plain mutex: a released cell goes to whichever waiter polls
  first. Starvation becomes unlikely instead of impossible. Handoffs get
  slower too, because nobody is reserved: either every waiter polls at
  200 ms, five times the GETs, or the cell idles up to a second. What
  dropping the queue would remove is small: the waiter list, the
  reservation and its abandonment timer, and one bug class (the drill's
  lost handoff).
- **What the queue does not cover.** A writer against the gateway: a UI
  save never waits for the lease (G1), so a writer can lose its CAS to
  saves. It backs off and retries, 12 attempts, then the barrier is
  retried at the next floor. That pressure is by design (G2). And a writer
  behind a dead holder waits about a minute for the deposal.
- **What the lease costs.** About four requests per publish: an epoch
  read and an acquire CAS to claim, a fence read after the load, and the
  handoff CAS. On loopback that measured 17–37 ms, 9% of a 20k-file
  publish and 0.4% at 1M. On S3 it is about 100 ms. Lever C shortens the
  hold, which is what waiters pay.

**Verdict: keep both.** The queue is the cheap half and buys the property
the user asked for. The lease is the expensive half to replace, and
replacing it buys about four requests per publish. Revisit only if the
collection is redesigned for its own reasons.

## 8. Results

The bench of §4 on the Linux build box, 2026-10-23, one test binary
(sha256 prefix `021f508e705cc86f`). At 20k and 200k files the two trees
are on the box's `/tmp` (tmpfs), three repetitions per cell. At 1M, one
repetition per arm at 25 ms, the trees on the NVMe (ext4): `/tmp` holds
1,048,576 inodes, two 1M-file trees do not fit, and the first attempt
failed there in both arms at B's checkout.

Wall time per barrier at 25 ms a request, the range over three repetitions:

| barrier | 20k control | 20k shipped | 200k control | 200k shipped |
|---|---:|---:|---:|---:|
| one-file publish, collection not due | 866–868 ms | 568–572 ms | 3600–3606 ms | 2178–2191 ms |
| one-file publish, collection due | 874–878 ms | 767–768 ms | 3601–3648 ms | 2375–2411 ms |
| the same, orphan sweep due too | 900–906 ms | 811–812 ms | 3756–3763 ms | 2562–2572 ms |
| taking a peer's one-file change | 349–350 ms | 255–259 ms | 2712–2729 ms | 1513–1548 ms |
| idle tick | 138 ms | 138–139 ms | 994–1012 ms | 986–1017 ms |

Requests per barrier, the same in every repetition and at every delay
(control's collection runs at every commit, so its two publish rows differ
only by one GET, which a writer's first commit makes in both arms):

| barrier | 20k control | 20k shipped | 200k control | 200k shipped |
|---|---|---|---|---|
| one-file publish, collection not due | 24: get 14, head 1, put 6, list 3 | 13: get 5, head 1, put 6, list 1 | 66: get 56, head 1, put 6, list 3 | 13: get 5, head 1, put 6, list 1 |
| one-file publish, collection due | 25: get 15, head 1, put 6, list 3 | 21: get 10, head 1, put 6, list 4 | 67: get 57, head 1, put 6, list 3 | 21: get 10, head 1, put 6, list 4 |
| the same, orphan sweep due too | 25: get 14, head 1, put 6, list 4 | 24: get 12, head 1, put 6, list 5 | 67: get 56, head 1, put 6, list 4 | 24: get 12, head 1, put 6, list 5 |
| taking a peer's one-file change | 8 GETs | 5: get 4, list 1 | 50 GETs | 5: get 4, list 1 |
| idle tick | 2 GETs | 2 GETs | 2 GETs | 2 GETs |

What the numbers say:

1. **A takes the manifest load out of the round trips.** At 200k the
   publish's `load` step is 1443 ms in control and 248 ms shipped (the
   pointer, one LIST, and hashing the cached chunks); a peer's change
   arrives in 1.5 s instead of 2.7 s. At 20k, where the document is a few
   chunks, A saves about 100 ms a publish.
2. **C takes about 200 ms out of every publish inside the interval** at
   25 ms a request (shipped, 20k: 568–572 ms not due against 767–768 ms
   due). The pass is 7 requests, 4 GETs and 3 LISTs; the due row's eighth
   is the first commit's extra GET. A publish that collects costs what it
   did, plus A's LIST.
3. **At memory speed the two shapes time the same** (0 ms, 200k: 1811–1840
   ms shipped against 1817–1835 ms control for a one-file publish; every
   row within the noise, logs on the box). The gain is round trips, and
   verifying every cached body costs no more than fetching it did.
4. **The idle tick is unchanged**: 2 GETs, and at 200k about 1 s of local
   work: scan 357 ms, consume 318 ms, baseline load 248 ms. `consume`
   opens with its own `load_baseline` (barrier.rs `consume_owed`), so two
   baseline parses are about half the tick. Neither lever touches it; D
   (one parse per process) and E (the scan) do.
5. **Not A or C, found on the way: the re-hash after a checkout.** The
   first two barriers after B's checkout re-hash every file written inside
   the racy window: at 200k 21.5 s, then 11.6–12.4 s, in both arms and
   every repetition (2.1 s twice at 20k); A's first publish after its seed
   does the same (22.2–24.0 s). This is the post-checkout re-hash the step
   timings found at 111 s for 1M. No lever of §2 covers it.

At 1M files, 25 ms a request, one repetition per arm (trees on the NVMe):

| barrier | control | shipped |
|---|---:|---:|
| one-file publish, collection not due | 16.2 s, 238 requests (get 228, list 3) | 10.3 s, 13 requests |
| one-file publish, collection due | 16.3 s, 239 requests | 10.6 s, 21 requests |
| the same, orphan sweep due too | 17.1 s, 239 requests | 11.4 s, 24 requests |
| taking a peer's one-file change | 14.3 s, 222 GETs | 8.6 s, 5 requests |
| idle tick | 5.2 s, 2 GETs | 5.2 s, 2 GETs |

The publish's `load` step goes from 6.8 s to 1.1 s; what is left of a
shipped one-file publish at 1M is local work, by step: scan 2.0 s, consume
1.5 s (its own baseline parse, item 4), baseline load 1.3 s, cas 1.25 s,
load 1.1 s, merge 0.9 s, baseline save 0.9 s. D takes the two parses and
most of the save (about 3.5 s), G the load, merge and cas (about 3.2 s),
E the scan. In the idle tick, the two parses are 2.9 of 5.2 s.

## 9. Decisions

1. **The per-load LIST (A): KEPT** (user, 2026-10-23). Every cached load
   still proves the chunks it used are in the bucket, so a live hole is
   refused on every load, as before the cache. Dropping it would have saved
   one request per load that needs chunks.
2. **The collection cadence (C): 300 s** (user, 2026-10-23): half the
   retire age, so replaced versions stay up to 1.5 × G.
3. **D: built by flint-c2** (assigned by the user, 2026-10-23; §11), on
   `lean-latency-d` off the A+C commit. B: built (item 5, §10). Still open:
   G (the document under the lease, 38% of a 1M publish; D and G together
   are 76%), the post-checkout re-hash (§8, item 5), and E, the hardest
   and the last.
4. **The models: no change, no re-proof** (asked by the user, 2026-10-23).
   A and C stay inside behaviours LeanP1 and LeanChunkGC already have
   (§5, §6); the code-trace check at the shipped cadence is the evidence
   for C (§6).
5. **B: news runs the floor's barrier early** (user, 2026-10-23), at most
   one such barrier per 5 s. Not "ticker only" (the agent told and left to
   pull), and not a background sync, which would take a peer's change
   without publishing but reverse `sync.rs`'s "never background" stance.
   The cost the user accepted: an active agent's unpublished edits publish
   when news arrives, not only at the floor.

6. **D: no model change, no re-proof.** A save is still all or nothing,
   the disk still holds what the last successful save held, and a restart
   reads the disk, which is what LeanP1's `Restart` assumes (§11). S3,
   folding `reader.json` into the baseline, is NOT done: it changes the
   reader's skip rule, which the model checks, to save one small write per
   pull. **The journal: KEPT** (user, 2026-10-23), after the comparison in
   §11 found it about two-thirds of D's code for about a tenth of a
   publish: it changes no rule the model checks. What the model assumes of
   a save (all or nothing), the journal's own code now provides, where
   temp+rename did before; that is tested (§11's battery and controls), not
   proved.
7. **G: a lazy per-chunk view, the commit side first** (user,
   2026-10-23). No document is held between barriers (D already holds the
   baseline); a commit parses only the chunks its paths fall in. The
   consume side (deriving what is owed from a chunk-list diff) is G2, after
   the commit side lands, because it is a new shortcut whose soundness
   belongs in LeanP1 as an invariant (§12). **The order after it** (user,
   2026-10-23): G2 next, then the gateway's commits (UI saves, deletes,
   renames and promotes edit the whole document today), the latter after a
   measurement of what a UI save costs at 200k and 1M. **G2 is dropped**
   (user, 2026-10-23: "if G2 makes no difference we should drop it"; §14).
   **The benches measured the wrong allocator** (§15): from here on a
   lever is measured on flint-sync's own (musl + mimalloc).

8. **E: the walk in parallel, not inotify** (user, 2026-10-23; built by
   flint-c2, §13). A declared publish includes every completed write
   (`lean/syncer/AGENTS.md`: "everything on disk when the syncer picks the
   file up is part of the boundary"), and inotify can miss one. A write
   through a shared mapping raises no event, a full queue drops events
   (`IN_Q_OVERFLOW`), and files made in a new directory before its watch
   is added are never reported. The watch limit is also per uid on the
   node, shared by every pod that runs as that uid. Each of these needs a
   walk to recover, so inotify could only be a hint, and a publish would
   still walk the tree. The walk stays the record, and E makes it cheaper.
   No model change (§13).

## 10. Lever B, as built

**What it does.** `news.rs`. A third arm of the syncer loop, on its own
interval like the other two. While the workspace is active it reads, every
`news_poll_secs` (1 s), what an idle tick's first two steps read: a
writer's cell, then the manifest pointer. A gateway boundary request in the
cell becomes the pending publish the sentinel arm honours within its next
second, under its usual minimum interval and hourly budget. A pointer whose
seq is past the news ticker's `observed_seq` is news: the ticker moves at
once, so an agent reading `.flint/remote.seq` sees it within a second, and
the floor's barrier runs early, at most once per `news_min_interval_secs`
(5 s). A reader's early tick is its pull. Before its first poll the arm
primes the ticker with the document the tree holds (`news_start`): a
checkout writes no ticker, and a fresh tree's first poll would read its
own checkout as news.

**Active** means a publish, a consume, an ack, a request at the control
socket, or news, within `news_active_secs` (300 s); a syncer starts active.
Past the window the arm sends nothing and only the floor reads the bucket,
as before. The first news after a quiet spell therefore waits for the floor
(up to 60 s), and the workspace is active again from it.

**What it never does.**
- Honour a pending publish or sync. The early barrier is the floor's
  cadence barrier alone (`cadence_floor`, split out of `floor_tick`), never
  `floor_tick`, whose deferred honours would then run every 5 s and spend
  past the hourly budget. A request the barrier finds standing it carries,
  as every barrier does (H10); the ack stays the sentinel arm's.
- Run a barrier for the tree's own install: the ticker already names it.
- Move the floor. Resetting the floor's interval on news would let a busy
  peer starve the floor's deferred honours, the starvation the independent
  intervals exist to prevent (D3/D12). This one lives in the loop of
  `bin/flint_sync.rs`, which no unit test reaches: it rests on reading the
  loop, which never touches `floor_iv` outside its own arm.
- Move the ticker's `integrated_seq`: nothing is integrated until a
  barrier takes the news, so the poll writes only `observed_seq` and
  `observed_etag` (`touch_remote_seq`), not through `ticker_from`.

**Cost.** Two GETs a second per active writer, one per active reader:
about 173k GETs a day, $0.07 on S3 Standard, for a writer busy all day;
nothing for an idle one. Each early barrier costs what a floor barrier
costs: an idle tick's local work (0.14 s at 20k files, 1 s at 200k, 5 s at
1M; §8), the consume of what is new, and a publish when the tree has
unpublished edits.

**The bound on what news can make a syncer do.** Whoever moves the pointer
(a peer, or the gateway on a UI save) causes at most one early barrier per
5 s at each active syncer: 720 an hour. The sentinel budget (60 work units
an hour) bounds declared publishes only, as before.

**The model.** Nothing new. A poll is two reads. Folding a gateway request
between barriers is `Touch`, which needs only an idle writer, as the
sentinel arm already does every second. The early barrier is a barrier,
and LeanP1 does not time barriers.

**Knobs.** `FLINT_SYNC_NEWS_POLL_SECS` (1; 0 = off, the shape before),
`FLINT_SYNC_NEWS_ACTIVE_SECS` (300), `FLINT_SYNC_NEWS_MIN_INTERVAL_SECS`
(5); `LeanConfig::news_*`.

**Tests.** `tests_news.rs`: a peer's publish is news the ticker shows before
the early barrier takes it; a writer's own publish is not news, nor is a
fresh checkout once the ticker is primed (with an unprimed control); a poll is
two GETs for a writer and one for a reader, with or without news; a gateway
boundary request is pending one poll later; the early barrier leaves a
pending publish to the floor, which then honours it; a reader takes news
and writes nothing; the arm on a made-up clock takes news at once, then at
most once an interval, and sends nothing once idle; the clock's own rules.
`tests_conformance.rs` gains a scenario through the arm, so the code-trace
check replays a poll's fold and an early barrier against LeanP1.

**Results.** Two `flint-sync run` writers, A and B, on one MinIO prefix on
the build box (loopback; binary sha256 prefix `a70618bb0f1a77ea`, B on top
of D), 2026-10-23. A publishes a one-file change through its publish
sentinel; measured is the time from A's ack to the bytes in B's tree.

| arm | runs | A's ack → B's tree |
|---|---:|---:|
| B (news poll 1 s) | 12 | 421–445 ms |
| B, after 330 s idle | 1 | 28.4 s |
| control (`FLINT_SYNC_NEWS_POLL_SECS=0`) | 8 | 48.5–52.5 s |

The narrow B range is the two syncers' one-second ticks running in a fixed
phase; in general expect up to a second plus the barrier. The control's
runs sit near the floor's worst case because each started 6–12 s after
the previous one arrived, just after B's floor tick: a change at a random
moment waits 0–60 s, 30 s on average. The idle run is the floor too, as
designed. Loopback MinIO answers in about a millisecond; on S3 add the
poll's and the barrier's requests at about 25 ms each, and the consume
(§8: 0.26 s at 20k files, 1.5 s at 200k; 5.5 s at 1M with D, §11).

Verification, B on top of D: syncer 361 pass and 1 ignored (debug, CI's
command); gateway 66; the binaries build; mutation controls N1–N8 each
fail their test (the poll treats the tree's own install as news; the
early barrier honours pendings; no minimum interval; an idle workspace
polls; a writer's poll skips the cell; news does not move the ticker; a
barrier leaves news pending; the ticker is never primed); the code-trace
check 84/84 on tla-mc, 32 traces accepted, the news scenario among them.

## 11. Lever D, as built

flint-c2, 2026-10-23, branch `lean-latency-d` off the A+C commit
`1528f5d5`.

**What it removes.** Every barrier parsed `.flint-sync/baseline.json`
twice (once in `consume_owed`, once before the scan) and rewrote it whole
at the end of every publish. The deployed floor tick parsed it twice more
in `ticker_from`, for `seq` and `derived_etag`; the bench does not run
that path, so it under-counts what a deployed tick paid. At 1M files a
parse is about 1.3 s and the rewrite 0.85 s.

**On disk.** `baseline.json` keeps its format: the whole snapshot,
written temp+rename, plus one new field, `journal`, the snapshot's id,
minted at each whole write and serialised LAST so the e2e plant's
`sed` anchor still finds the baseline's own `"entries":{` first
(`run-verbs.sh`). Beside it, `baseline.log`: the first line names the
snapshot id it belongs to, and each further line is one save:
`<crc64 hex> <json>` of the scalars (`seq`, `manifest_etag`,
`derived_etag`) and the members it put or removed in `entries`,
`skipped` and `prev_scan`. A load applies every whole, verified line of
the journal that names its snapshot and stops at the first that is not
(a crash mid-line, a flipped bit). So a torn save is a save that never
happened, as a torn temp file was before. A save writes at the verified
length, never at the file's end, so the bytes of one that failed half-way
are overwritten, not followed. A whole write (the first save, a large
change, a journal past a quarter of its snapshot) writes the snapshot
with a new id and then a fresh journal; a death between the two leaves
the old journal naming the old id, and the next load ignores it.

**In memory.** The state directory's flock already made the syncer its
only writer, so the parsed baseline cannot change under the process
holding it. `SyncerState` holds it:

- `take_baseline` / `commit_baseline`: the hot paths (consume, the
  barrier's scan and commit, sync, checkout) take it and hand it back,
  with no copy. A path that errors out drops it, and the next load reads
  the disk, which still holds the last successful save.
- `with_baseline`: per-tick readers (the ticker, the publish ack, the
  reader, the drain) read it in place.
- `load_baseline` / `save_baseline`: copies, for the rare paths
  (rescope) and the battery.

`TrackedMap` and `TrackedSet` (`tracked.rs`) record the keys a save
changed. They have no `DerefMut`, so a mutation the journal would miss
does not compile. What they cannot record is a collection replaced
whole, or a copy older than the last save; each collection carries the
lineage it was loaded under, and a save of anything not of the disk's
current lineage writes the whole document, which is exactly what every
save did before D. A save that fails drops the held copy.
`TrackedSet::sync_to` walks the old and new members once and copies only
what came or went, so `prev_scan` no longer allocates a million strings
per barrier.

After a whole write the store holds the bytes it just wrote, parsed, not
the map it wrote (the old one freed first). The first cut held the map,
and the bench below found every walk of a large structure slower under it:
a map a checkout built by inserting in arrival order is scattered across
the heap, and it now lived for the whole process instead of one barrier.
A parse allocates in key order. Whole writes are rare (a checkout, a
compaction, a large change), so that is one parse each.

**Readers outside the process, and tests that fake a restart.**
`flint-sync status` read `baseline.json` raw for `baseline_seq`; it now
reads snapshot and journal through `state::read_baseline_at`, which never
cuts a torn line (it may be the owner's write in progress). In the
battery, one test read `baseline.json` raw and two helpers faked a crash
by copying it aside (three tests failed through one of them); they now
read through `read_baseline_at`, or copy `baseline.log` with it, because
the durable baseline is both files. The e2e plant in `run-verbs.sh` edits
the snapshot with `sed` while no syncer runs; the id survives the edit, so
the journal still applies (`a_snapshot_edited_in_place_keeps_its_journal`).
The e2e itself was not re-run.

**Not done.** S3 (the reader's `reader.json` memo folded into
`derived_etag`): it changes the reader's skip rule, which the model
checks (`RPullRead`, `LeanP1ReaderOwedIgnored`), to save one small write
per pull.

**Evidence.**

- The lean battery on the code as committed: 351 pass (the base's 340
  and D's 11), 1 ignored (the bench).
  The gateway's tests pass against it.
- `tests_baseline.rs`: a small save appends and reads back; a torn line;
  a corrupt line; a half-written line followed by a save; a journal for
  another snapshot; a snapshot edited in place keeps its journal; a stale
  copy and a collection replaced whole are written whole; `sync_to`; a
  seeded 400-step walk of every mutation, re-read from disk at every
  step, with stale copies and compactions; and a steady barrier parses
  the baseline zero times, with a control (forget it once: one parse).
- Six mutations, each failing its test and nothing passing vacuously:
  the lineage check ignored, the torn tail not cut, the save appended at
  the end, the journal applied to any snapshot, the checksum not checked,
  no cache.

**Results.** The bench of §4, on the box, in-memory store; the A+C
commit `1528f5d5` (before) against D, two binaries interleaved within
each repetition. 20k and 200k: three repetitions, ranges. 1M: one run.
Logs on the box (`/mnt/nvme/d-bench/`, `/mnt/nvme/d-reparse/`,
`/mnt/nvme/d-final/`).

The first cut (no re-parse), at 0 ms and at 25 ms a request:

| barrier | 20k before | 20k D | 200k before | 200k D | 1M before | 1M D |
|---|---:|---:|---:|---:|---:|---:|
| idle tick, 0 ms | 81–82 ms | 39–40 ms | 948–967 ms | 476–482 ms | 5.1 s | 2.5 s |
| idle tick, 25 ms | 142–145 ms | 106–107 ms | 996–1000 ms | 541–547 ms | 5.1 s | 2.6 s |
| taking a peer's one-file change, 25 ms | 267–274 ms | 221–223 ms | 1542–1548 ms | 1126–1130 ms | 8.4 s | 5.5 s |
| one-file publish, collection not due, 25 ms | 589–593 ms | 528–541 ms | 2210–2217 ms | 1791–1796 ms | 10.0 s | 8.0 s |
| one-file publish, collection due, 25 ms | 794–801 ms | 728–738 ms | 2431–2444 ms | 2003–2021 ms | 10.3 s | 8.3 s |

What went, per step at 1M and 25 ms (one-file publish): `consume` 1519 to
151 ms, `baseline_load` 1308 to 0, the save 843 to 65. What came back:
`scan`, `load`, `merge` and `retire_set` each walk a large structure, and
each got slower, 1.6 s together at 1M (`retire_set` 418 to 961 ms), 260 ms
at 200k. `retire_set` never touches the baseline, so it is the heap, not
the map's own walk.

The re-parse, at 0 ms (200k: three repetitions, the three binaries
interleaved; 1M: one run):

| barrier | 200k before | 200k first cut | 200k D | 1M before | 1M first cut | 1M D |
|---|---:|---:|---:|---:|---:|---:|
| idle tick | 962–987 ms | 479–487 ms | 445–450 ms | 5.1 s | 2.5 s | 2.4 s |
| taking a peer's one-file change | 1444–1445 ms | 989–1009 ms | 881–888 ms | 8.3 s | 5.3 s | 5.1 s |
| one-file publish, collection not due | 1844–1855 ms | 1423–1438 ms | 1223–1235 ms | 9.7 s | 7.6 s | 6.9 s |
| that publish's `merge` | 165–167 ms | 225–235 ms | 167–168 ms | 866 ms | 1177 ms | 989 ms |
| that publish's `retire_set` | 81–84 ms | 181–184 ms | 100–104 ms | 418 ms | 961 ms | 718 ms |

At 200k the re-parse takes `load` and `merge` back to where they were and
most of `retire_set`; `scan` stays 40–70 ms slower. At 1M it takes back
about half of what the first cut lost; the rest (`retire_set` still 300 ms
over) is not understood. D as committed (the re-parse, the old map freed
first), one run at 1M, 0 ms: idle tick 2.5 s, taking a peer's change
5.1 s, one-file publish 6.9 s (7.0 s with the collection due, 7.6 s with
the orphan sweep due too).

**The cost: memory.** The baseline now stays resident between barriers.
Peak RSS of the whole bench process, which also holds the in-memory store
and a second writer: 20k 139–141 to 142–144 MiB, 200k 1242–1253 to
1314–1365 MiB, 1M 5929 MiB to 6364 MiB as committed (6158 for the first
cut, 6349 for the re-parse). The bench process holds two writers, each
with its own resident baseline, so this is roughly 200 MiB a writer at 1M
files, from single runs. Freeing the written map before the re-parse did
not move the peak. The bench does not measure the syncer alone.

**The journal against the cache alone.** Is the journal worth its code?
The same bench, D as committed against "cache-only": D with the journal
off, so every save is a whole write of `baseline.json` as before D, a save
that changed nothing writes nothing, and the re-parse runs only after a
large change. The two binaries were interleaved, at 0 ms; 20k and 200k
three repetitions, 1M two. Logs in `/mnt/nvme/d-cacheonly/`.

| barrier | 20k cache-only | 20k D | 200k cache-only | 200k D | 1M cache-only | 1M D |
|---|---:|---:|---:|---:|---:|---:|
| idle tick | 41–42 ms | 41 ms | 489–490 ms | 486–490 ms | 2.6 s | 2.6 s |
| taking a peer's one-file change | 93–97 ms | 82 ms | 1049–1078 ms | 930–943 ms | 6.4 s | 5.1–5.2 s |
| one-file publish, collection not due | 134–138 ms | 122–123 ms | 1399–1411 ms | 1261–1262 ms | 7.5–7.6 s | 6.8–6.9 s |
| peak RSS, whole bench process | 146–147 MiB | 145–148 MiB | 1380–1382 MiB | 1367–1369 MiB | 6554–6560 MiB | 6319 MiB |

All of the difference is the save step (1M: 715–735 ms cache-only, 59–76
ms with the journal). So the cache alone carries about three-quarters of
D's publish gain at every size and all of its idle-tick gain. The journal
adds about a tenth of a publish, 12–24% of taking a peer's change (a
consume that takes one also saves), nothing on the idle tick, and about
235 MiB less peak memory at 1M (no whole-document serialisation per save).
It is about 780 of D's ~1,140 lines of code and tests. Kept (§9, item 6).

**Model.** None changes (§9, item 6): a save is still all or nothing,
the disk still holds what the last successful save held, and a restart
reads the disk, as LeanP1's `Restart` assumes.

## 12. Lever G, the commit side: design

**Where the time goes** (1M files, D as built, 0 ms, a one-file publish of
6.9 s): `cas` 1.16 s, `load` 1.09 s, `merge` 0.90 s, `retire_set` 0.68 s,
`free` 0.32 s, 4.1 s in all, every one O(entries) for a change of one
path. The rest is the scan (2.5 s, E's).

| step | the whole-document work | what G does instead |
|---|---|---|
| load | fetch, hash and parse every chunk | the pointer, the tombstones object, one LIST, and only the chunks the commit's paths fall in |
| merge | `merge_base` copies the baseline; `merge_onto` and `merge` each clone the document; `foreign` and `gone` diff all of it; `path_clashes` range-scans under every cited path | per changed path, against the baseline in place; the result is an overlay (path → entry or deleted) on the view; `foreign`/`gone` only when the event trace is on (the trace check reads their counts; nothing else does) |
| path clashes | every cited path | per upsert: anything under `u/`, and each ancestor of `u` cited as a file |
| adds nothing | compare both entry maps | compare the overlay with the paths it names |
| cas | split and hash the whole merged document | re-chunk from the first changed key until a new cut lands on an old one after the last change, then reuse the old refs |
| retire set (twice: the barrier and the retire log) | every key of both documents | theirs' keys at the changed paths that the merged document no longer cites anywhere |
| free | drop two or three full maps | nothing large was built |

**Exactness, the two claims G rests on.**
1. *Re-chunking.* The chunker's only state is the run length since the
   last cut, which is 0 at every cut. Starting at the chunk that holds the
   first change (its start is a cut in both chunkings) and stopping at the
   first new cut that falls on an old cut after the last change, the rest
   of the key stream is identical and so is its chunking. So the chunk list
   G writes is `split(merged)`, byte for byte: the rule `LeanChunkMerge.tla`
   checks (recompute from the merged entries, never splice two writers'
   lists) holds, because the list is recomputed; only its unchanged stretch
   is reused, and only against the one document the merge started from.
   Property test: random key sets and edits, the re-chunk against
   `chunk::split_with` of the applied set, at small `target`/`min`/`max` so
   cascades and forced cuts happen.
2. *Who else cites a handle.* A handle key is minted `files/<path>@<flush>`.
   Call an entry an ALIAS when its key does not parse to its own path (a
   rename's destination, or a key of any other shape). Every entry citing
   key `k` is then either at `path_of(k)` or an alias, so "is `k` cited
   anywhere" is one point lookup plus the aliases, for any key format: a
   misparse only makes more entries aliases. Each chunk's aliases are
   computed when it is parsed and kept by address (chunks are immutable),
   so a process parses every chunk once, on its first commit, as D parses
   the baseline once.

**What a missing chunk does.** As a full load: one LIST of `chunks/`
proves every chunk the pointer names exists, used or not, so a hole is
refused (an unchanged pointer) or restarts the load (a moved one), and the
new pointer never re-cites a chunk the bucket lost. The LIST is the one A
already pays.

**Unchanged.** The collection pass (C: the reap, the orphan and generation
sweeps) still reads the whole installed document, assembled from the view
on the commits that run it, every 300 s. The single-object layout keeps
the full path. The gateway's commits are G's next step, not this one.

**Verification.** A shadow mode for the battery (`LeanConfig::g_shadow`,
on in the tests' config): every commit also runs the full path and asserts
the same merged entries, tombstones, chunk list, retire set, clashes and
merge outcomes, so the 361 existing tests check G against the code it
replaces. Then the code-trace check (the merge event's counts come from
the full path under the trace), mutation controls, and the bench at 20k,
200k and 1M.

**The model.** None changes: G computes the same document, chunk list and
retire set as the full path (claims 1 and 2, and the shadow mode). G2, the
consume side, is the one with a model obligation.

**As built** (`lean-latency-g`). `docview.rs`: `DocView` (open, lazy and
verified chunk reads, `first_under`, `rechunk`, `retired`, `assemble`) and
`CommitDocs`, which carries what a commit installed to the steps after its
CAS. `chunk::rechunk_with` and `chunk::parse_chunk`, which a full load's
`assemble` now parses through too. `manifest::merge_changes`, which
`manifest::merge` is now built on, and `manifest::cas_write_view` over a
publish split out of the whole-document write. In the barrier, the commit
loop opens a view (the whole-document path stays, verbatim, for the
single-object layout, a refused LIST and `FLINT_SYNC_CHUNK_VIEW=off`), and
so does the pull-only path; the clash withholding and the carrier record
are shared by both paths.

Where it differs from the design above:
- *The alias cache is the workspace's* (`LeanConfig::alias_cache`), not
  the process's: two workspaces with one prefix (the tests' many syncers)
  must not prune each other. It is filled by every full load and every
  whole-document write, which hold all the entries anyway, and by each
  chunk a re-chunk encodes; so a writer parses a chunk for its aliases
  only when someone else wrote it and this process has not loaded it.
  `aliases()` reads what the cache lacks from the view's own parse, never
  from the cache a second time, where a concurrent prune could have
  dropped it.
- *A chunk gone at a lazy read after the LIST* is refused, not restarted
  onto a moved pointer. Live chunks are never swept and a retired one
  outlives its retirement by the retire age, so this is a bucket that lost
  data, and the barrier retries either way; the view never returns a
  document missing entries (`a_view_refuses_a_hole_and_never_serves_a_short_document`).
- *The merge's `foreign` count* (`BarrierReport::foreign_queued`) is
  counted only under the event trace or the shadow check, where the whole
  document is read anyway; it is 0 otherwise. Nothing acts on it.

**Tests.** The shadow check across the battery: 147 tests reach the view's
write path and 16 its pull-only path. `tests_rechunk.rs`: the re-chunk is
`split_with` over 1,600 random documents and edits and every edge of the
key stream, and a one-path change reads 1.12 chunks on average of 162.
`tests_docview.rs`: a hole refused as a full load refuses it; a one-file
publish's commit section sends 3 GETs through the view against 58 through
the whole document (56 chunks, cache off); a handle survives an edit at its
own path while an alias cites it, and an edit at an alias while its own
path cites it, and is collected once nothing does; and `clashes_view`
answers `path_clashes` over 300 random documents.

**Results.** The bench (`tests_bench.rs`, release build, the in-memory store
at 0 or 25 ms a request). The control is `FLINT_BENCH_ARM=noview`, the
whole-document path, in the same binary. There were 3 reps at 20k and
200k and 1 at 1M; a range spans the reps. At 25 ms a request:

| barrier | 20k noview | 20k G | 200k noview | 200k G | 1M noview | 1M G |
|---|---:|---:|---:|---:|---:|---:|
| one-file publish, collection not due | 510–516 ms | 411–421 ms | 1,514–1,555 ms | 864–872 ms | 7,045 ms | 3,099 ms |
| one-file publish, collection due | 697–701 ms | 628–637 ms | 1,745–1,784 ms | 1,338–1,347 ms | 7,392 ms | 5,069 ms |
| one-file publish, orphan sweep due too | 739–744 ms | 664–667 ms | 1,842–1,899 ms | 1,479–1,491 ms | 8,103 ms | 5,832 ms |
| taking a peer's one-file change | 211–212 ms | 212–213 ms | 975–1,024 ms | 1,020–1,032 ms | 5,319 ms | 5,400 ms |
| idle tick | 104–105 ms | 103–105 ms | 520–532 ms | 532–544 ms | 2,621 ms | 2,658 ms |

At 0 ms, which leaves CPU only, the not-due publish went from 107–108 to
49–51 ms at 20k and from 1,184–1,185 to 482–488 ms at 200k. Both arms
sent the same requests in every row.

The commit's own steps at 1M, for the not-due publish:

| step | noview | G |
|---|---:|---:|
| load | 1,221 ms | 53 ms |
| merge | 899 ms | under 20 ms |
| CAS | 1,037 ms | 96 ms |
| retire set | 556 ms | under 20 ms |
| free | 196 ms | under 20 ms |
| release | 144 ms | 27 ms |
| **total** | **4.1 s** | **0.3 s** |

Most of the 3.1 s that remains is the walk (2.5 s, lever E). Taking a
peer's change is unchanged, as designed: its consume (2.6–2.7 s at 1M) is
G2. The bench process's peak RSS, seeding included, fell from 5.96 to
5.19 GB at 1M and from 1.32 to 1.08 GB at 200k.

Verification on the box:
- The syncer's tests: 369 passed, 1 ignored. The gateway's: 66 passed.
- The binaries build.
- The code-trace check: 84/84 on tla-mc.
- Mutation controls V1–V9 each fail their tests. V9's first form was
  vacuous: the full load also remembered the aliases. Its second form
  stops every path from remembering them, and is caught.

## 13. Lever E, as built

flint-c2, 2026-10-23, branch `lean-latency-e`, on flint-05's L7 commit
`dbf616d5` (main after B, plus the chmod fix).

**What it removes.** The walk read one directory at a time and `lstat`ed
every entry by its full path, so the kernel resolved every component of
every path again. At 1M files it was 2.0 s of a 10 s publish (§8,
item 5), and the same 2.0 s of every idle tick.

**What it is.** `scan::scan_with(root, threads)`. Workers take directories
from one queue (a mutex and a condition variable), so a tree that is one
huge directory and a tree of thousands of small ones both spread. Each
entry is stat'ed relative to its directory (`DirEntry::metadata`: `statx`
or `fstatat` on the directory's descriptor, `AT_SYMLINK_NOFOLLOW`), not by
its full path. One worker walks on the calling thread, and the calling
thread makes every key the map keeps ("The fix", below). The results
are sorted and built into the map in bulk, not by a million inserts.
`LeanConfig::scan_threads` defaults to the CPUs the process may use
(`available_parallelism`, which counts the cgroup's quota and the
affinity mask), at most 8; `FLINT_SYNC_SCAN_THREADS` sets it, and 1
walks on the calling thread. The barrier's scan, the sync's two
scans and checkout's rescan all use it.

**What it returns is what the one-thread walk returned**: the same entries
under the same exclusions (the state and control directories at the root
only, the syncer's temporaries at any depth, symlinks and non-regular
files skipped), with the same size, mtime and mode. A directory any worker
cannot list fails the whole walk, as before (flint-05's fail-closed rule,
`c24beeb7`): the first error is kept in the queue, the other workers stop
taking directories, and the walk returns it.

**Found on the way: two names for one path.** The walk turns a `\` in a
name into `/`, so on Linux a file named `a\b` and the file `b` in
directory `a` give one relative path, and the walk keeps one of them. The
one-thread walk kept the later in `read_dir` order, the same choice from
one scan to the next. Workers would keep whichever they happened to read
later, a different file from scan to scan: each flip a stat change, so a
re-hash of `a/b` per barrier. The upload reads `root.join(rel)`, the real
`a/b`, either way. So the order is now a rule of the tree (by path, then
by size, mtime and mode, and the last kept). That a `\` name is published
under another path at all is older than E and is not changed here.

**Steps.** The `scan` step of `BarrierReport::steps` is now two: `walk`
(the tree) and `classify` (against the baseline, the racy recheck, the
absence confirmation).

**Model.** None changes (§9, item 8). The walk was never atomic: agents
write under it, and each path is observed at some instant within the walk.
LeanScanRacy's `Scan` is a per-path version oracle, and the two-scan and
racy-clean rules (L-138) are what make that safe. E changes the order in
which paths are observed, never what one observation sees, and nothing
in the model or the code depends on that order.

**Evidence.**

- The lean battery as a normal user (uid 1000, so the fail-closed tests
  run, not skip): 373 passed, 1 ignored (flint-05's 369 with E's 4). The
  gateway's tests pass against it.
- `tests_walk.rs`, with the walk before E kept verbatim as the oracle: an
  awkward tree (both exclusions at the root and at depth, temporaries,
  symlinks to a file and to a directory, a FIFO, empty directories, a `\`
  name, a 0755 file) at 1, 2 and 8 threads; six seeded random shapes (one
  directory, deep chains, 200 directories) at 1, 3 and 8; an unlistable
  directory deep in a busy tree fails the walk at 1, 2 and 8; and two
  names for one path keep the same one in 20 walks at each of 1, 2 and 8.
- Four mutations, each failing its tests: a worker that drops its error
  (fails E's test and both of flint-05's fail-closed tests), temporaries
  not skipped, the root's exclusions applied at every depth, the
  tie-break reversed.

**Results.** The bench of §4 on the box, with the in-memory store at 0 ms,
on glibc's malloc, which is not what ships: the numbers as shipped
follow this table.
Three arms from two binaries, interleaved within each repetition: `old`,
E's tree with the walk before E put back; `t1`, E with one worker; and
`tN`, E at the default (8 on the box's 8 cores). There were three
repetitions at 20k and 200k (ranges below) and one run at 1M; a second
1M round was dropped (user, to free the box). Logs are on the box, in
`/mnt/nvme/e-bench/`.

| barrier | 20k old | 20k tN | 200k old | 200k t1 | 200k tN | 1M old | 1M t1 | 1M tN |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| idle tick | 40–41 ms | 18 ms | 462–482 ms | 330–331 ms | 184–188 ms | 2.5 s | 1.7 s | 1.0 s |
| taking a peer's one-file change | 81 ms | 53–54 ms | 916–951 ms | 708–751 ms | 534–571 ms | 5.0 s | 3.6 s | 2.7 s |
| one-file publish, collection not due | 119–122 ms | 95–101 ms | 1258–1283 ms | 1074–1082 ms | 922–938 ms | 6.8 s | 5.7 s | 4.7 s |
| that publish's `walk` | 30–32 ms | 11 ms | 296–298 ms | 205–206 ms | 65–66 ms | 1551 ms | 1106 ms | 363 ms |
| that publish's `classify` | 10 ms | 10 ms | 139–143 ms | 111–113 ms | 107–110 ms | 815 ms | 589 ms | 595 ms |
| peak RSS, whole bench process | 145–146 MiB | 150–151 MiB | 1317–1379 MiB | 1414–1439 MiB | 1400–1419 MiB | 6390 MiB | 6510 MiB | 6550 MiB |

- **One worker already saves about a third of the walk** (`t1`: 1.55 to
  1.1 s at 1M), because each stat is relative to its directory and the
  kernel no longer resolves the full path. Eight workers take the rest
  to about a quarter of `t1` (363 ms).
- **`classify` is faster too** (815 to 595 ms at 1M), at any thread
  count, and so are `retire_set` (200k: 121–122 to 98–111 ms; 1M: 664 to
  455 ms) and, at 1M, `load` (1080 to 923 ms); `merge`, `cas` and `free`
  are unchanged. The old walk built its map by one insert per entry, in
  `read_dir` order, which scatters it across the heap; E builds it in
  bulk from sorted input. This is the effect §11 measured for the
  baseline: a scattered map slows every later walk, of itself and of what
  is allocated around it.
- **`consume`: glibc's, not the syncer's.** On the old arm every
  barrier's consume took 16–40 ms at 200k and about 200 ms at 1M, idle
  ticks included, for the same single GET; on E's arms it falls to 0 by
  the third barrier after the checkout. `skipped` was empty (flint-e4
  read both baselines). It is glibc parking the old walk's million freed
  path strings in its fastbins and merging them at the next large
  allocation, which is in the next barrier's consume: with
  `GLIBC_TUNABLES=glibc.malloc.mxfast=0` it is gone (flint-e4's
  allocator comparison, `/mnt/nvme/alloc-bench/`). E's strings come from
  the workers' arenas and are reused there, never merged. mimalloc, which
  the syncer ships with, has no such cost.
- **The first barrier after a checkout is still the re-hash** (§8, item
  5): 103 s of its 107 s at 1M is `classify` hashing every file. E does
  not touch it; it is still unassigned.
- **The cost: memory**, up to about 100 MiB at 200k and 160 MiB at 1M
  at the peak of the whole bench process (two writers and the store;
  the 1M figure is from single runs). Likely part of it, not measured
  apart: the walk's results are held twice for a moment, the workers'
  lists and then the sorted one. `t1` costs about the same as `tN`, so
  the worker count is not what drives it.

**On the allocator the syncer ships with.** Every table above is glibc's
malloc: `flint-sync` sets mimalloc as its global allocator in
`bin/flint_sync.rs` only, so the library's test binary, where every
bench of this plan ran, never had it (found by flint-e4, 2026-10-23). The
same three arms as first built (`f974132a`), as shipped: static musl
with mimalloc, the test binary given the syncer's allocator by one
`lib.rs` line in the bench's copies only (flint-e4 commits it on G2's
branch). Both binaries were
checked static, and mimalloc reports its start in both; the battery
passes on that build, 373. 200k three repetitions, 1M one run. Logs in
`/mnt/nvme/e-musl/`.

| barrier | 200k old | 200k t1 | 200k tN | 1M old | 1M t1 | 1M tN |
|---|---:|---:|---:|---:|---:|---:|
| idle tick | 415–417 ms | 400–405 ms | 219–223 ms | 2132 ms | 2095 ms | 1141 ms |
| taking a peer's one-file change | 746–748 ms | 755–764 ms | 566–579 ms | 3815 ms | 3943 ms | 2985 ms |
| one-file publish, collection not due | 1235–1236 ms | 1252–1285 ms | 1057–1091 ms | 6278 ms | 6567 ms | 5606 ms |
| that publish's `walk` | 322–324 ms | 274–278 ms | 90–96 ms | 1644 ms | 1458 ms | 497 ms |
| that publish's `classify` | 91–92 ms | 116 ms | 116–117 ms | 474 ms | 606 ms | 604 ms |
| that publish's `load`, `merge`, `cas`, `retire_set` | 789–795 ms | 836–869 ms | 822–852 ms | 4045 ms | 4392 ms | 4395 ms |
| peak RSS, whole bench process | 1267–1380 MiB | 1245–1374 MiB | 1361–1374 MiB | 5941 MiB | 5930 MiB | 5927 MiB |

- **The walk's gain held**: 1644 to 497 ms at 1M, as on glibc.
- **But the main thread's later steps got slower**, at any thread
  count: at 1M `classify` +130 ms, `load` +100, `merge` +147, `cas` +45,
  `retire_set` +55, about 480 ms, which took back about 40% of the walk's
  gain in a publish, and with one CPU a publish was slower than before
  (6.6 s against 6.3). On glibc the same steps got faster. The cause:
  the walk's million keys were allocated in the workers' heaps, and
  mimalloc returns a freed block to the heap that allocated it, so the
  main thread's next large allocations (the documents of `load` and
  `merge`) no longer reused the blocks the last walk freed and took cold
  pages instead.

**The fix (user, 2026-10-23: "do 1 first").** One worker walks on the
calling thread, with nothing spawned. Several workers each write their
files' relative paths end to end into one buffer, a few large
allocations per worker, and the calling thread makes every key the map
keeps. The same bench as shipped, the old arm's binary byte-identical
to the first cut's; logs in `/mnt/nvme/e-fix/`. Battery 373 on both the
gnu and the musl build, five controls (the fifth: a key's start in the
buffer not advanced).

| barrier | 200k old | 200k t1 | 200k tN | 1M old | 1M t1 | 1M tN |
|---|---:|---:|---:|---:|---:|---:|
| idle tick | 419–420 ms | 374–378 ms | 197–198 ms | 2145 ms | 1956 ms | 1038 ms |
| taking a peer's one-file change | 724–745 ms | 690–701 ms | 526–527 ms | 3854 ms | 3645 ms | 2736 ms |
| one-file publish, collection not due | 1205–1234 ms | 1142–1196 ms | 941–996 ms | 6446 ms | 6167 ms | 5232 ms |
| that publish's `walk` | 313–322 ms | 275–287 ms | 94–96 ms | 1705 ms | 1474 ms | 534 ms |
| that publish's `classify` | 90–91 ms | 91–92 ms | 92–99 ms | 474 ms | 475 ms | 478 ms |
| that publish's `load`, `merge`, `cas`, `retire_set` | 764–802 ms | 745–795 ms | 730–779 ms | 4151 ms | 4069 ms | 4076 ms |
| peak RSS, whole bench process | 1376–1378 MiB | 1309–1374 MiB | 1258–1373 MiB | 5892 MiB | 5876 MiB | 5903 MiB |

- **The later steps are back where the old walk left them** (at 1M,
  `classify` 474 to 478 ms, the commit's four big steps 4151 to 4076).
- **So, as shipped:** with 8 CPUs the idle tick drops by about half
  (200k: 419 to 197 ms; 1M: 2.1 to 1.0 s) and a one-file publish by
  about a fifth (200k: about 1220 to 970 ms; 1M: 6.4 to 5.2 s). With one
  CPU, the relative stat alone: the idle tick 10% and a publish 4% at
  1M. No memory cost.
- **Still open (user to decide):** on one thread the walk is 20–25%
  slower under musl than under glibc (flint-e4's comparison, 2.15
  against 1.69 s at 1M). If a profile puts that in musl's `readdir`
  and `fstatat` wrappers, the walk could call `getdents64` and `statx`
  directly (`rustix`, already in the lockfile).

**What the bench does not measure.** The tree was just written, so its
directories and inodes were in the kernel's cache, and the walk was CPU,
not I/O. A cold cache (after a node restart, or memory pressure) makes
the walk wait on the disk. More workers keep more reads in flight, which
an NVMe serves in parallel, but that is not measured here. A pod with a
CPU limit of 1 gets one worker (`available_parallelism` counts the
quota), and with it `t1`'s gain only.

## 14. Lever G2, the consume side: explored and dropped

(§13 is lever E's, on its own branch.)

**What it was.** A consume that found the document moved derived only
where it had moved since its last derive. It diffed the two chunk lists,
whose content addresses make an unchanged chunk provably unchanged, and
looked at the paths it had marked. Its target was the 2.7 s that G's bench
charged to the consume when a peer's change arrived at 1M files.

**What the model found first, and what to reuse if this returns.** A diff
against the derived document alone is unsound. Take a path this tree
publishes over a document that moved since its derive, which the UI then
deletes. Neither document cites it, so the tree keeps a file nothing
cites, forever. The fix: such a commit marks its own paths for the next
derive.

In LeanP1 (archived), `CommitRechecksOwn = FALSE` breaks `Inv_DiffSound`
at depth 12 on exactly that route. The fact that makes the diff sound,
`Inv_DiffRecord`, never reads the document: on what the tree holds, the
record and the baseline differ only where it marked. The full M0–M10 proof
was never re-run; `ddoc` changes the writer record, so it would need to be.

**Why it was dropped.** Built and verified (syncer 381, gateway 66,
mutations D1–D5 caught, 69 battery tests through the diff, trace 84/84).
Then it was measured on flint-sync's own allocator (§15). Taking a peer's
one-file change, at 25 ms a request:

| | without G2 | with G2 |
|---|---:|---:|
| 200k, musl + mimalloc (0 ms) | 28.3 ms | 27.9 ms |
| 1M, musl + mimalloc | 239 ms | 169 ms |
| 1M, gnu + mimalloc | 212 ms | 218 ms |
| 1M, glibc | 455 ms | 441 ms |

One run each. The barrier around it was 2.0–2.9 s, almost all of it the
walk. Whether 70 ms of a 2.4 s barrier is real is uncertain, since the
gnu runs show none. The 2.7 s G2 targeted was not the derive: it was
glibc's (§15).

## 15. The allocator: what the benches measured, and what ships

**The finding.** flint-sync ships as a static musl binary with mimalloc.
`fastalloc` is a default feature since 2026-09-12, after musl's
single-lock malloc tripled a checkout's CPU. But `#[global_allocator]` is
declared only in `bin/flint_sync.rs`. Every latency bench in this plan
(A, C, B, D, G, E) and the battery ran the library's test binary, which
never had it: they measured the box's glibc malloc, which nothing ships
with.

`lib.rs` now declares mimalloc for the test binary under `fastalloc`, so
`cargo test` runs the shipped allocator.

**glibc moved cost between steps.** glibc keeps small freed chunks in
fastbins and merges them at the next large allocation. A barrier's walk
frees about a path string per file at its end, so the next barrier's
first step paid the merge. That step is the consume:
- on glibc it cost 25–33 ms per barrier at 200k and 116–256 ms at 1M,
  idle ticks included;
- with fastbins off (`GLIBC_TUNABLES=glibc.malloc.mxfast=0`) it fell to 0;
- on mimalloc it was 0 at 200k, and at 1M only its one request.

G's 401–424 ms consume at 200k, and its 2.7 s at 1M, were mostly this.
(flint-c2 proposed the mechanism; this comparison confirmed it.)

**The comparison** (`/mnt/nvme/alloc-bench` on the box, one run per cell;
the same G2 tree; ms; 200k at 0 ms a request, 1M at 25 ms):

| allocator | 200k idle | 200k publish | 1M idle | 1M publish | 1M walk |
|---|---:|---:|---:|---:|---:|
| musl + mimalloc (**ships**) | 424 | 473 | 2,254 | 2,600 | 2,149 |
| glibc (**what the benches measured**) | 482 | 516 | 2,601 | 3,162 | 2,293–2,359 |
| glibc, no fastbins | 465 | 509 | — | — | — |
| gnu + mimalloc | 337 | 350 | 1,794 | 2,220 | 1,694 |
| musl's own malloc | 485 | 511 | — | — | — |

"Publish" is a one-file publish with collection not due.

- **What ships is 10–20% faster than what we measured**, and the
  difference sits in the walk and in the consume's merge.
- **A gnu build with mimalloc is another 20–25% faster**, almost all in the
  walk (1.69 s against 2.15 s at 1M). That points at musl's libc (string
  and syscall wrappers), not the allocator.
- **Decision (user, 2026-10-23): keep shipping musl + mimalloc.** glibc is
  avoided for its CVE exposure. Its large surface (NSS, iconv, locale,
  `GLIBC_TUNABLES`) is mostly unreachable from a static sidecar, but it
  still lands in every image scan. The walk's gap is to be won inside musl
  instead. Profile both walks at 1M first; if the libc wrappers are the
  cost, call `getdents64`/`statx` directly (rustix's raw-Linux backend; the
  build already pulls in `linux-raw-sys`), which makes the walk
  independent of the libc. That is lever E's code (§13): handed to it. The
  cost of the choice: a static binary carries its own musl, so a musl CVE
  needs a `flint-sync` rebuild, and image scanners may not see it.
- **Every per-step figure above §15 is glibc's.** The relative gains of A,
  C, D and G come from whole steps going away (requests, parses, document
  clones), so they should hold. G's 1M publish (7.0 → 3.1 s) is worth
  re-measuring on the shipped allocator before it is quoted.

**How to build what ships, on the box.** Plain cargo cannot build
mimalloc's C for musl there. zig 0.13 at `/mnt/nvme/zig-linux-x86_64-0.13.0`
does it, behind two wrappers in `~/musl-cc`:
- `x86_64-linux-musl-gcc` runs `zig cc -target x86_64-linux-musl`, and drops
  the `--target=<rust triple>` that cc-rs adds;
- `x86_64-linux-musl-ar` runs `zig ar`.

Then `PATH=~/musl-cc:$PATH cargo test --release --lib --target
x86_64-unknown-linux-musl`. The wrappers come from `~/musl-mi-build.sh`.

**One oddity, left for the post-checkout re-hash lever.** At 200k the
first barrier after a checkout took 21 s on every allocator but musl's
own, which took 2.2 s. It looks like timing, not CPU: the slower checkout
leaves fewer files inside the racy window, so fewer need re-hashing.

## 16. The gateway's commits (lever G on the UI's side)

**Measured first** (user, §9 item 7). The gateway bench
(`lean/gateway/tests/latency_bench.rs`, release, static musl as shipped,
the memory store) put every UI verb at seconds at 1M:

| 1M, 25 ms a request | musl malloc (shipped) | mimalloc |
|---|---:|---:|
| save | 3,711 ms | 2,902 ms |
| rename | 3,669 ms | 2,883 ms |
| delete | 3,685 ms | 2,866 ms |
| overwrite after a read | 2,199 ms | 1,686 ms |

The requests were the same in both arms (2 GETs, 4 PUTs, 1 LIST), so the
cost was CPU, and it was linear in the document (68 ms at 20k, 664 ms at
200k). Each verb's cost had two parts:
- **The reload, about 1.5 s.** The gateway's own last commit moved the
  pointer, so its held copy was stale, and the next verb re-assembled the
  whole document from cached chunks. An overwrite right after a read
  skipped this, which is why it was cheaper.
- **The commit, about 2.0 s.** It cloned the whole document, edited it,
  re-split and re-hashed all of it, and diffed it whole for the retire
  set.

**The lever (user, 2026-10-23: all three parts).**
1. **Edit by path.** `manifest::commit_edit_paths`. An edit writes an
   overlay (`DocEdit`), whose `entries` answer `get`, `contains_key`,
   `insert` and `remove` as the edited map would, so the verbs' judging
   code is unchanged. The commit re-chunks only from the first chunk an
   overlay path falls in until the chunking lines up again
   (`chunk::rechunk_with`, G's), loading each old chunk from the held
   document. It answers the retire set by G's rule from the overlay and
   the chunks' aliases (`docview::retired_over`). It publishes only the new
   chunks. A document that isn't chunked, or none, is edited whole as
   before.
2. **Hold what it installed.** The gateway takes its held document out of
   the cache for the commit (`take_view`). The commit makes the installed
   document from it in place (`Arc::make_mut`: no copy while nothing else
   holds it) and hands it back under the pointer it wrote. A refusal
   hands back what it read. The alias cache moves into the shared
   `ManifestCache`, because a server builds a `Workspace` per request.
3. **mimalloc**, default on (`fastalloc`, declared only in the binary).

**What does not change: the model.** The gateway installs the same
document, chunk list and retire set as before; holding its own commit is
`GCas`'s view of it. In every debug build (so in all the gateway's tests),
each per-path commit also edits, splits and diffs the whole document, and
asserts the same chunk list, chunk bytes and retire set (`shadow_edit`).
`Workspace::without_chunk_view()` is the shape before: whole commits,
reloaded after, the bench's control arm.

**Results.** The gateway bench, as shipped (static musl with mimalloc).
The control arm is `FLINT_BENCH_ARM=whole` (`without_chunk_view`) in the
same binary; one run per cell. At 1M and 25 ms a request:

| verb | whole (before) | by path (after) |
|---|---:|---:|
| save | 2,874–2,883 ms | 169 ms |
| rename | 2,836 ms | 143 ms |
| delete | 2,833 ms | 143 ms |
| overwrite | 1,792 ms | 169 ms |

At 0 ms a request, every verb after the first save took 4 ms at 1M,
against 2.6 s (1.6 s for an overwrite). At 200k at 25 ms: a save 715–738
→ 210–231 ms, a rename 716 → 146 ms. What remains is the requests: a save
is 2 GETs, 4 PUTs and a retire log; a rename or delete is one GET fewer.
No later verb reloads the document or LISTs. The first verb after start
still loads it once (1.2 s at 1M at 0 ms).

Against what shipped before (musl's malloc, whole commits), a UI save at
1M and 25 ms went from 3.7 s to 0.17 s.

**Found by the bench, fixed before the numbers above.** `put_file` and
`promote_draft` held their own reference to the document through the
commit, so `Arc::make_mut` found it shared and copied 1M entries: 126–174
ms per save at 1M against 4 ms for a rename. Both now let go before
committing.

**Verification.**
- The gateway: 67 passed, every UI commit shadow-checked. The syncer:
  377 passed.
- Both binaries build, and the code-trace check passes 84/84.
- Mutation controls GW1–GW5 each fail their test (`~/gw-mutate.py`):
  the retire set ignoring the overlay's citations, the held document not
  edited, nothing held, a chunk losing its first key, and the held
  document keeping its old etag.
- The mutation runner now touches what it copies. A restored file keeps
  its old mtime, so a target that last built a mutation served it to the
  pristine run: GW5's build failed M0.

**Lever G as shipped** (plan §15). The syncer's bench on musl with
mimalloc, shipped against `noview`, one-file publish with collection not
due, 25 ms a request:
- 1M: 7,309 → 2,732 ms (the commit's steps 4,769 → 205 ms).
- 200k: 1,694 → 857 ms (1,061 → 204 ms).

The glibc figures were 7.0 → 3.1 s and 1.5 → 0.87 s.

## 17. The post-checkout re-hash

**What it was.** The racy-clean rule (L-138) says a baseline entry whose
stat was taken within 2 s of the file's mtime cannot vouch for the bytes.
A same-size write in the same coarse tick leaves the stat identical, so
the next scan hashes the file (`scan::confirm_racy`). A checkout stats
each file the moment it writes it (`barrier::racy_stat`), so every entry
was racy, and the first barrier after it hashed the whole tree: 21.6 s at
200k files and 107 s at 1M, against about 0.5 s and 2.2 s for any later
idle tick.

**Why most of it was never needed.** No one but the checkout writes the
tree before its marker (`checkout-complete`) exists. The CSI node plugin
binds the tree into the agent's pod only after the marker
(`node.rs::resume_lean`), and a checkout returns at the marker without
touching a live tree. So an entry's stat cannot go stale before that gate,
and a write after it has a later mtime than any file written a window
before it.

**The gate.** The checkout measures each flag at the gate: after the
tree's `syncfs`, just before its baseline and marker. The sync must come
first anyway, and its time counts toward the window for free. The measure
is taken before the marker, so earlier than the gate: the safe direction.
Only what the checkout wrote in its last window stays racy.

**The wait.** On a fast disk that last window is a large part of the
tree. When at least `racy_wait_min` files are still racy at the gate
(10,000 by default), the checkout waits until the newest of them is a
window old, at most 2 s, and measures again: none is left. The threshold
is about where hashing them costs the first scan more than the wait costs
the agent's start (the box hashes a checked-out file in about 150 µs, so
2 s is about 13,000 files). Waiting is ticks before `Open`, which the
model already allows, so nothing in it changes. An mtime in the future,
or a clock that steps back, leaves those entries racy: the safe direction.

**The model.** `LeanScanRacy.tla` gains the gate: a `gate` variable, shut
at the checkout. `Open` writes the marker and, under `MeasureAtGate`,
clears the entry's flag if its mtime is a window old. The agent's `Write`
waits for the gate (`WritesWaitForGate`, the assumption).

TLC (tla-mc 0.1.2):
- the shipped rule HOLDS at 3,452 distinct and at 156,072 (Wide);
- `MeasureAtGate = FALSE`, the shape before, HOLDS;
- the old mutations still lose a change (NoRule, MeasureLate,
  ClearEarly);
- **`WritesWaitForGate = FALSE` loses one** (NoGate, 56 distinct): an
  agent that wrote before the marker could be hidden by the gate's clear.
- `ProbeGateClears` fires.

The proof (`LeanScanRacyProof.tla`) gains `GateOK`: before the gate, the
file is exactly what the checkout recorded. **All 38 obligations proved**
(35 before the gate; tlapm bfa9468, 0.87 s). Each control fails its own
step: `RacyClean` 3/38, `MeasureAtRead` 1/38, `ClearOutsideWindow` 1/38,
and `WritesWaitForGate` 1/38, at `Write`, which breaks `GateOK`
(`lean/formal/results/2026-10-23-racy-gate/RESULTS.txt`).

**The code.** In `checkout.rs`, after `sync_tree` and before the baseline
commit and the marker: the wait, then each entry's `racy` re-measured with
`scan::racy` against the time now (an unknown nanosecond reads as the
latest in its second, the safe direction). The rescope's widen is not
gated, because the agent runs then, and it keeps the flags as written.
- `LeanConfig::racy_at_gate`, `FLINT_SYNC_RACY_AT_GATE=off`: the shape
  before (bench arm `nogate`).
- `LeanConfig::racy_wait_min`, `FLINT_SYNC_RACY_WAIT_MIN` (10,000; 0
  never waits): the gate without the wait (bench arm `nowait`).

**Tests.** `tests_rehash.rs` has five arms over a 20-file checkout:
- the gate held a window past the writes: no entry is racy, and an
  agent's same-size rewrite right after the gate is still published;
- the gate at once, no wait: all 20 racy;
- the switch off: all 20 racy;
- the wait at 20 files: none racy, and the rewrite is still published;
- the wait at 21 files, one more than are racy: all 20 racy.

Mutation controls each fail it: R1 the gate settles nothing, R2 the gate
read past every write, R3 the wait never re-reads the gate, R4 the wait
ignores its threshold, R5 the wait cut short. The syncer: 378 passed;
the gateway: 67; both binaries build (the one warning is the drain arm's,
as before); the code-trace check passes 84/84.

**Results.** The bench (musl with mimalloc), a quiet box, the store at
25 ms, stopped after the first idle scan (`FLINT_BENCH_STOP=idle0`):

| | the shape before (`nogate`) | the gate alone (`nowait`) | the gate and the wait |
|---|---|---|---|
| 200k: racy after the checkout | 200,000 | 87,141 | **0** |
| 200k: the first idle scan | 21.6 s | 10.0 s | **0.51 s** |
| 200k: the checkout | 3.9 s | 3.9 s | 4.8 s |
| 1M: racy after the checkout | 1,000,000 | not run | **0** |
| 1M: the first idle scan | 107.8 s | not run | **2.4 s** |
| 1M: the checkout | 17.9 s | not run | 18.2 s |

The wait costs the checkout what is left of the window (0.9 s at 200k,
0.3 s at 1M) and saves the first barrier the re-hash (21 s and 105 s). With the wait, no run left an entry racy:
ten more on a loaded box (200k and 1M, the store at 0 and 25 ms), first
scan 0.42–0.77 s at 200k. The gate alone left 30,000–32,000 of 200k on
the loaded box at 0 ms.

**The arms, corrected.** The runs behind 8778646f's numbers compared
nothing: `bench_arm()` sent every arm it did not list to `shipped`, so
`nogate` and `nowait` ran the shipped configuration, and the differences
were run-to-run noise. It now lists both and refuses an unknown arm, and
each result line records the arm the binary printed. Every number above
is from runs whose log says `arm=nogate`, `arm=nowait` or `arm=shipped`.
