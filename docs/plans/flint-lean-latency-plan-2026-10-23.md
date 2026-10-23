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
| E | change tracking (inotify) in place of the walk, full walk on overflow; inotify on `.flint/` in place of the 1 s poll | at 1M, 2 s per barrier and the idle tick | none, if the fallback is the walk | not started; hardest (watch limits, uid) |
| F | policy: `sentinelMinIntervalSecs` 5 → 1, and a bytes-only publish budget | a sustained ceiling of one declared publish a minute | none | not started; a user decision |
| G | the document under the lease merged and serialised incrementally, by chunk | at 1M, 3.8 s of a 10 s publish (38%) | none | not started; needs A's per-chunk view |

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
   pull.

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

**Model.** None changes (§9, item 6): a save is still all or nothing,
the disk still holds what the last successful save held, and a restart
reads the disk, as LeanP1's `Restart` assumes.
