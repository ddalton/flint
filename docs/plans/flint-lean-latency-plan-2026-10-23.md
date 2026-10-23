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
| B | an adaptive news poll: read the pointer at 1 s while the agent is active, back to the floor when idle; the request cell rides the same read | visibility from up to 60 s to about 1 s | none: more consumes, each the same step | not started. Cost: up to 1 GET/s per active workspace |
| D | the baseline in memory, loaded once per process, persisted incrementally | at 1M, the baseline file is 38% of a 10 s publish: two parses (about 27%, removed by loading it once) and one full rewrite (about 9%, mostly removed by persisting incrementally); and most of the idle tick | none | assigned to flint-c2 (2026-10-23), off the A+C commit; not started |
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
3. **D: assigned to flint-c2** (user, 2026-10-23), in its own worktree off
   the A+C commit. Still open: B (1 s visibility for an active agent, up to
   1 GET/s each), G (the document under the lease, 38% of a 1M publish;
   D and G together are 76%), the post-checkout re-hash (§8, item 5), and
   E, the hardest and the last.
4. **The models: no change, no re-proof** (asked by the user, 2026-10-23).
   A and C stay inside behaviours LeanP1 and LeanChunkGC already have
   (§5, §6); the code-trace check at the shipped cadence is the evidence
   for C (§6).
