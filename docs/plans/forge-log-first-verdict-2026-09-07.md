<!-- Produced by a 14-agent adversarial review, 2026-09-07, run against
docs/plans/forge-log-first-multiwriter.md. Six load-bearing claims were put to
independent skeptics told to refute rather than confirm; four came back FATAL.
Four of its findings were spot-checked by hand against the source before this
was accepted (sweep.rs's upload abort, P2's per-pusher branch, log::put's
Unconditional write, server.rs's prewarm guard) and all four held. -->

# forge log-first + multi-writer: the verdict

*Read against `docs/plans/forge-log-first-multiwriter.md` (267 lines), the syncer's 13,516 lines, and this project's own drill logs. Constraint held throughout: ONE bucket, ONE region, and that bucket is the truth.*

---

## 1. WHICH CLAIMS DID NOT SURVIVE

### FATAL — removing the lease breaks large pushes, deterministically, and the note never mentions the module

§4 says "the lease stops being a correctness mechanism… That is a large simplification." It is not a simplification; it is the removal of the only thing that makes `sweep.rs` sound.

`sweep::sweep` (`forge/syncer/src/sweep.rs:62-69`) begins by calling `abort_orphaned_uploads`, which lists every pending multipart upload under `<prefix>/git/` and aborts **all** of them (`sweep.rs:50-59`). Its justification is a statement about a single process — *"Called after the claim and between batches — the two moments this process can have nothing of its own in flight, which is why there is no grace"* (`sweep.rs:36-42`) — and its only guard is this process's own state, `if sc.fold.is_some()` (`sweep.rs:47`, `sweep.rs:64`).

**Breaking sequence.** Writers A and B, no lease. A accepts a 1 GiB push; `packio::upload_file` takes the compose path because 1 GiB > `WHOLE_PUT_MAX` (`packio.rs:23`, `packio.rs:118`), so an MPU is open under `git/objects/pack/`. B finishes an unrelated batch and runs its routine sweep. B aborts A's upload. A's `complete_multipart_upload` returns `NoSuchUpload`; A's push is refused.

This is not a race window. It is every sweep against every concurrent upload over 64 MiB. §6 rates `sweep.rs` "medium," for reference-set reasons only.

### FATAL — the rebase reverts the winner, in the case §4 calls easy

§4: losers "read the winner's entry, re-validate their own ref preconditions against it, and retry at `N+2`." Two shipped code paths make that corrupting.

`LogEntry::apply` assigns wholesale (`log.rs:107-109`):

```rust
snap.bundles = self.bundles.clone();
snap.exported_commit = self.exported_commit.clone();
```

Correct today, because the entry's author *was* the truth. Rebased, an entry built at seq N and applied at N+2 silently reverts whatever the winner published at N+1. **Disjoint refs do not save you** — this reverts on every rebase regardless of which refs moved.

Worse on the pack list: `batch.rs:273` sets `next.packs = local_packs` — the whole list from this writer's own directory. That is authoritative only because one writer's disk *is* the truth. Rebased, it resurrects packs another writer's fold removed and **drops packs the winner added and this writer never fetched**, leaving a head whose refs point into objects no named pack holds. That is `Inv_LandedPackComplete`, violated by the rebase itself.

### FATAL — the loser cannot rebase; today's code refuses, and the fix costs a pack fetch inside the push

`batch.rs:127-138` builds the effective ref map by requiring local ref == bucket ref; any ref that differs is refused at `batch.rs:384-391` with *"differs between this server and the bucket; the server will reconcile on restart — retry then."* Under multi-writer, `local != head` **is the steady state**, so the refusal is the steady state.

Push past it and the fast-forward test `is_ancestor` (`batch.rs:403`) shells out to `merge-base --is-ancestor` (`gitcmd.rs:284-291`); without the winner's pack, git exits 128 and the client gets `ng` with git's raw stderr.

§4's "it is the same non-fast-forward check the door and `pre-receive` already enforce" is exactly wrong about cost. **That check needs objects, not an entry.** P7 measures a 1 GiB pack at 17 s. So:

> P(lose | retry) > P(lose | first attempt), monotonically.

That is the textbook condition under which optimistic concurrency control collapses — goodput falls as offered load rises. To be precise and not manufacture doubt: this is **not** livelock, because someone wins every round. It is biased starvation, and the loser is always the writer with the most catch-up debt. P10 already shows what that looks like on the wire (`runce-fold-fix-2026-09-07.log:45`, "a push during the outage TIMED OUT — not acknowledged").

### REFUTED — the scalability argument, by forge's own capacity model

`docs/plans/flint-forge-design.md:258-260`: *"Capacity is batches per second — one two-round-trip chain per batch — and pushes per batch grow with load, so the syncer gets faster per push as the fleet gets busier."*

The amortizer is batching, and batching is **strictly per-process**: `server.rs:509-556` drains one syncer's mpsc channel, `batch_window_ms: 0` (`lib.rs:282`). Measured: 15.5 acked pushes/s (`runce-fold-fix-2026-09-07.log:54`) at ~23-30 pushes per CAS, i.e. forge puts roughly **0.5-0.7 conditional writes/s** on the hot key while acknowledging 15.5 pushes/s.

Split the same load across N writers and each queue is 1/N as deep, so each batch is 1/N as large, so the commit rate is N× higher. At N=32 the offered rate on a key whose width is fixed at one rises from ~0.7/s toward 15.5/s. **Multi-writer trades away the super-linear property to buy contention on a serialization point.**

And the ceiling is N-independent: entry N+2 needs entry N+1's *content*, and a 412 tells you the key exists, not what is in it. PUT + GET ≈ 40-80 ms in-region ⇒ ~12-25 commits/s per repository, forever, for any N. **One writer with batching already does 15.5/s.** There is no headroom to buy.

### REFUTED — "the append path is ~50 lines" (§9)

The append is 50 lines. Everything that must be true around it is not:

- `log::put` uses `PutCondition::Unconditional` (`log.rs:163-179`) on the stated grounds that *"the seq is written once by construction, because the CAS that produced it made the next one."* True only because the CAS fenced everyone first. Log-first, it must become `IfNoneMatchAny` — which then collides with `snapshot::rotate_for_takeover`'s own entry at seq N+1 (`snapshot.rs:196-204`).
- `log::emit` is best-effort and prints on failure (`log.rs:203-207`, *"Refusing the push because the hint did not land would trade a cheap wake for an acknowledged push, which is the wrong way round"*), fired **after** the CAS and **after** the ref transaction inside a `tokio::join!` with the derived files (`batch.rs:328-343`). The moment the entry carries the only copy of the delta, that comment inverts and every push in the batch must get `ng`.
- `log::prune` deletes the oldest entries by count (`log.rs:294-317`, `log_max_entries: 512`), driven from `sweep.rs:181`. `IfNoneMatchAny` guarantees "created once, ever" — but the design *requires* deleting these keys, and a deleted key is absent again. A writer stalled past 512 commits (33 s at 15.5/s) will `PUT log/K+1.json If-None-Match:*` and **succeed**, committing in the past. §3 rule 3 orders checkpointing before pruning, which protects a *reader's* start point, not a *writer's* target. Closing it requires reading the mutable checkpoint on the commit path — a second round trip and a second hot object, which retires §4's "the append race is solved and its solution is already in the tree."
- S3 answers contended conditional writes with **409 ConditionalRequestConflict**, which is indeterminate about whether the PUT landed. `batch.rs:296-305` matches only `PreconditionFailed`; everything else falls to `Err(e) => return Err(e)`, and `run_batch`'s contract (`batch.rs:104-106`) is `ng` to every push in the batch — up to 30 clients rejected per 409. The in-tree fake never produces it (`memory.rs:481-494`), and `probe.rs:139+` has no concurrency arm. **The contended path is untestable with what is in the tree.**
- Retry ambiguity: a landed PUT whose response is lost gets SDK-retried into a 412 (global 10 s read timeout, `s3.rs:64-70`). The writer reads `log/N+1` and finds an entry whose only identity fields are `writer` (= `holder_id`, which `lease.rs:88-91` deliberately persists across restarts), `epoch` (deleted with the lease), and `unix` (whole seconds). "Did I win?" becomes a content guess. Note the mirror the note misses: **today this is safe *because* of the lease** — a 412 is a fence, and `fold.rs:834-841`'s lost-response recovery keys on `fresh.snap.epoch == epoch`. Removing the lease removes the discriminator the existing recovery uses. Repairable with one idempotency field; must be stated.

### REFUTED — §2's headline: "It is also why forge loses the tiny-push storm"

This is the load-bearing motivation for the whole document, and the harness refutes it.

P2's pushers each push to **one branch each**: `HEAD:refs/heads/agent/p2-$run-$tag-$i` at `run-compare.sh:216`, `P2_N=32` at `run-compare.sh:48`. P2's repository therefore holds ~32 refs and its snapshot is ~2-3 KB. Against `runce-fold-fix-2026-09-07.log:55` (1.136 GB over 931 acked pushes = 1.22 MB/push), the pointer rewrite is **under 0.3% of the bytes**.

The 127x is pack/fold economics, and the log names the mechanism itself at `runce-fold-fix-2026-09-07.log:70-73`: *"an input the roll-up does not fully hold stays named, so a later fold takes it as an input again and re-uploads its contents… A same-cluster A/B of the two fold rules is the experiment that would settle it, and it has not been run."*

**Log-first does not move the 127x by one percent.**

### REFUTED — "walgit is log-first" (§2, §4, §10)

walgit at `e5295e6` is **snapshot-CAS-first, exactly like forge**. `wal.proto:10` — *"manifest.pb: tiny, CAS-rewritten: the linearization point."* Creating `log/<N+1>.pb` is step 4 of 6, a **slot claim**; the commit is step 5, `PutMode::Update(known_version)` on `manifest.pb` (`publish.rs:671-680`). No lease is taken on the write path; the manifest CAS is the primitive.

What walgit actually did is move the **ref map out of the CAS'd object**: the manifest is `head_seq`, a checkpoint ref, ≤256 `LogSegmentRef` and ≤16 `PackRef` — ~2-12 KB, **independent of ref count**. Refs live in `checkpoints/<seq>/refs.pb` plus each entry's `RefTransaction`.

**That is the entire X19 win, and it does not require inverting authority.** The note's §2→§3 argument — that the byte win needs log-first authority — does not hold. This is the single most actionable finding in the whole review.

### What I could not break

- `IfNoneMatchAny` exists, works, and is exercised (`crates/flint-store/src/lib.rs:357-380`). §1's four facts are all correct.
- The log tail is already **complete**: every one of the four `snapshot::cas` sites already emits an entry — `batch.rs:296/333`, `fold.rs:824/844`, `restore.rs:360/369`, `snapshot.rs:196/203`. Stage 1 is genuinely contained because of this.
- **X19 is real.** `refscale-2026-09-06.log:6-18`, timer arm, 8,000 refs: `forge_ms` 1464, `plain_ms` 1079, `snapshot_B` 545,613. forge's own share is 385 ms, and the snapshot rewrite is part of it. `packrefs-ab-2026-09-07.log:59` confirms both arms of the pack-refs A/B were byte-identical — nothing shipped this session touched it.
- Read-after-write under one writer is free and airtight: `batch.rs:309-317` applies the whole ref transaction before any report leaves `run_batch`.
- §10's refusal to take walgit as a dependency is right.
- The note's own bottom line — *"do the first, treat the second as a separate decision"* — is right. It is §4's "multi-writer then follows" that is wrong.

---

## 2. WHAT PRODUCTION GIT SYSTEMS ACTUALLY DO

I looked for a production git server that lets N writers concurrently commit to one repository without a per-repository serializer. **There isn't one.** Every system converges on one of two shapes, and neither is a global append-log-as-commit.

**Shape 1 — one primary per repository, elected, with replicas for durability and reads.**

- **GitLab / Gitaly + Praefect.** One primary per repository; Praefect routes every write to it, secondaries replicate asynchronously and serve reads. Reference transactions add quorum *voting* on a ref update, but the vote is still initiated by a single primary. GitLab evaluated writer-anywhere and did not build it.
- **GitHub / Spokes (DGit).** Three replicas per repository, writes three-phase-committed to a quorum. This is replication for durability and read capacity — ref updates to the same repository still serialize through a coordination point. Nobody gets throughput out of it.
- **Bitbucket Data Center.** All nodes write the same repository directory on shared NFS; git's own `refs/<name>.lock` (`O_EXCL`) and `packed-refs.lock` do the serializing. Multi-writer via a POSIX lock — i.e. a lease, on a filesystem.
- **Gerrit multi-site.** The closest thing to the note's proposal in production, and instructive: it needed a **global refdb** (ZooKeeper) doing CAS on `(ref, sha)` for fencing, plus sticky routing, plus explicit eventual-consistency semantics for reads. It took years, it is still classified as hard, and its unit of CAS is **one ref**, not one global log position.

**Shape 2 — per-ref compare-and-swap, packs immutable in a blob store.**

- **JGit DFS / `DfsRefDatabase`** (the Google-internal git-on-Borg backend): refs in a distributed store with `compareAndPut(oldRef, newRef)` per ref; packs immutable and content-named in the blob store. This is *architecturally the closest system to forge*, and its concurrency primitive is per-ref CAS, not a chain.
- **git's own `reftable`** (now in git, default in JGit/Gerrit): an ordered stack of immutable tables plus a mutable `tables.list` that is atomically replaced. **The linearization point is the mutable list file, not the creation of a table.** Same answer as walgit's manifest, arrived at independently by the git project itself.

**On the note's own citations.** Delta Lake genuinely is create-if-absent-as-commit at `_delta_log/<N>.json` — and on S3 it required an *external* DynamoDB coordinator for years precisely because that primitive was missing, and Delta's own guidance is that concurrent-write conflicts are expensive and should be avoided by partitioning. Iceberg is **not** what the note implies: its production catalogs (Hive/Glue/JDBC/REST) commit by CAS'ing a **pointer** to metadata; only the `hadoop` catalog uses filename-based create-if-absent, and Iceberg documents that as unsafe for concurrent writers. Both citations, read properly, point at *shape 2 with a mutable pointer* — which is what forge already has.

**The synthesis, and it is unanimous.** Every production system that serves git at scale (a) puts the ref map somewhere other than a whole-rewritten pointer, and (b) serializes writes to one repository through a primary or a per-ref CAS. **The note's §5 option (a) — "sticky routing per repository at the door" — is the industry answer, and forge already implements it. It is called `lease.rs`.**

---

## 3. THE RECOMMENDATION

### Multi-writer: **REFUSE.** Not "defer" — refuse, and say why in the design note.

Every one of §5's three options loses:

- **(a) Sticky routing per repository** *is one writer per repository*. It is what forge does today, except the writer is elected by the door instead of by a fenced CAS lease — which is strictly worse, because the door is not a fencing authority and cannot produce `ForgeError::Fenced`. §5 concedes it "concedes most of the multi-writer benefit for writes"; the accurate statement is that it concedes **all** of it and spends the fencing story to do so.
- **(b) Read barrier** is the worst of the three: it inherits every fatal item above *and* adds read-side hazards. The barrier's target is not discoverable in O(1) — there is no head pointer, so a follower learns the head by probing `seq+1` to a 404 (`log::read`, `log.rs:242-266`, driven at `follow.rs:230-241`) or by a LIST (`log.rs:272-280`), per read. The fix is a mutable CAS'd head object: the pointer you just deleted, hot again. And `follow::warm` unlinks packs the target does not name with **no grace** (`follow.rs:319-336`) while `git http-backend` children stream from the same directory (`bin/flint_forge_gitcgi.rs:163-230`); `Syncer::retained` / `fold_retain_secs: 900` (`lib.rs:289`, `fold.rs:887-914`) covers only packs *this* process superseded. Falling off the log is a read **outage**: `log_max_entries: 512` and `MAX_CHASE: 512` (`follow.rs:202`) at 15.5 commits/s is **≈33 seconds** of retained history.
- **(c) Bounded staleness** is unacceptable for a git server, and §5 already says so correctly.

There is no fourth option that keeps the total order and scales, because every mechanism that would make the chain scale (cross-writer batching) *is a leader*, and forge already has a cheaper leader than a distributed one.

The one shape that genuinely does scale for forge's measured workload — 32 pushers to **distinct** branches — is a **partition of the ref namespace** with one writer per shard. That is sharded single-writer, not multi-writer. If pushes-per-repository ever becomes the binding constraint, that is the design to write, and it is a different document.

### Stage 1: **worth it — but build Design B, not the note's Stage 1, and sequence it second.**

Do **not** invert authority. Do what walgit and reftable both did: **split the pointer from the ref map, keep the CAS as the linearization point.**

```
snapshot.json  = POINTER, CAS'd: version, seq, epoch, head_seq, checkpoint_seq,
                 packs[], bundles[], exported_commit, writer, unix     (~2-4 KB)
checkpoints/<seq>/refs.json = the full ref map, every K entries
log/<seq>.json = the ref delta, written BEFORE the CAS, IfNoneMatchAny
```

Per batch: PUT entry at N+1 → CAS the pointer to `head_seq = N+1`. Refs at any seq = checkpoint + replay, which `follow.rs` already does (`follow.rs:226-262`) and `LogEntry::apply` already implements (`log.rs:93-114`).

What Design B preserves that the note's Stage 1 destroys:

| | note's Stage 1 (invert) | Design B (shrink) |
|---|---|---|
| `batch.rs:298-303` fence, "412 = a second server" | gone, needs a new story | **unchanged** |
| `snapshot::rotate_for_takeover` + its two TLC counterexamples (`snapshot.rs:164-213`) | no CAS left to rotate | **unchanged** |
| `fold.rs:836-841` lost-response recovery on `epoch` | discriminator gone | **unchanged** |
| `sweep.rs` reference predicate (§9's "riskiest part") | rewritten | **unchanged** — packs stay in the pointer |
| `ForgeSync.tla` actions | rewritten | +1 action, +1 invariant |
| rollback | hard | trivial — dual-write the ref map for one release |

`snapshot.rs:5-8` justifies refs-in-the-snapshot as *"what makes a batch that moves thirty refs one CAS and makes a concurrent reader's view whole."* walgit refutes that rationale directly: atomicity comes from `head_seq` gating **which entries count**, not from the refs being physically inside the CAS'd bytes. A reader that fixes `head_seq` sees a whole batch or none of it.

There are only **10 non-test `snap.refs` sites** — `batch.rs:129/132/209/268/270`, `restore.rs:216/223/269`, `follow.rs:154/340-366`, `status.rs:131`. The ref map is the small-blast-radius half. The pack list is the wide one (`fold.rs:308/335/705/837/934`, `restore.rs:140/173/339`, `sweep.rs:76-93`, `batch.rs:222/591`) — **leave it in the pointer and none of that moves.**

### And price Stage 1 honestly

The win is **≤385 ms of a 1,464 ms push at 8,000 refs** — call it 4-10%, and **0% below ~2,000 refs**. Plus follower catch-up bandwidth, which is real. On S3 in-region, PUT *requests* cost and ingress bytes do not, and the request count is unchanged (today's batch already does CAS **plus** a log PUT). Resident storage is unchanged. **The win is latency and follower bandwidth, not dollars.**

It does not touch P2's 127x or P9's 1.75x. Both are fold amplification.

### Therefore, the sequencing

1. **Fast failover** — mostly done, cheapest remaining win, ~20 lines (below).
2. **The fold A/B drill** — your own log says it is the unrun experiment that settles the byte gaps (`runce-fold-fix-2026-09-07.log:70-73`). Costs a cluster, not a format version.
3. **Design B** — ~480 lines, one format bump, real but modest payoff.
4. **Multi-writer** — refused; record the refusal and the reasoning in §5 so it is not re-litigated.

---

## 4. WHAT TO BUILD FIRST

### Step 0 — the failover work, ~20 lines, no format change, do it this week

Warm followers already ship on (`server.rs:125-136`, `prewarm: true`) and `c7-prewarm-2026-09-06.log` measured them: claim→serving **222 ms vs 861 ms**, restore **232 ms vs 845 ms**, **0 files vs 430 MiB**, proof `Nothing` vs `Full`. Graceful release exists (`lease.rs:333-341`, `server.rs:256`).

What is left is two things, and neither is log-first:

- The outage is dominated by `QUIET_POLLS × heartbeat_secs` = **6 × 10 = 60 s** (`lease.rs:33`, `lib.rs:281`). That is a knob traded against false takeover, not a build.
- **`server.rs:125` stops warming the moment `quiet_polls != 0`** — a warm standby deliberately goes cold for the last 60 s before it takes over. The rationale at `server.rs:116-124` is about a *cold* follower not pulling 40 GiB during the race; it does not apply to a follower one entry behind. Let a warm follower keep chasing the **log tail only** (never the snapshot, never a large pack) during the quiet window.

**This is the best availability work in the entire document and it depends on nothing else.**

### Step 1 — ordering and durability, under the existing format

Testable end-to-end against the in-tree fake store. Do it while the snapshot still carries the refs and a mistake is recoverable.

1. `log::put` → `PutCondition::IfNoneMatchAny` (`log.rs:163-179`); delete the "Unconditional and idempotent" rationale.
2. Move the entry PUT out of the step-7 `tokio::join!` (`batch.rs:328-343`) to **before** `snapshot::cas` (`batch.rs:296`), and make its failure return `Err` so every push gets `ng` per `batch.rs:104-106`. Same at `fold.rs:824/844` and `restore.rs:360/369`. **Measure what it costs the push** — `c7-prewarm-2026-09-06.log:43` has one arm each (5033 vs 5419 ms, wall clock only); make it a real interleaved A/B.
3. Add one idempotency field (`attempt_id`) to `LogEntry` (`log.rs:66-88`) so "did my write land?" is a lookup, not a guess.
4. **Write the takeover orphan rule: burn the seq, never delete.** A straggler holding the cell at N can write entry N+1 and then block; `snapshot::rotate_for_takeover` (`snapshot.rs:188-213`) then collides with it under `IfNoneMatchAny`. Reusing the orphan replays refs whose pushes were told `ng`. Deleting it is unsound on a stale read (successor deletes N+1, its own rotation CAS 412s into `Fenced`, `head_seq` is N+1 with no entry). walgit's answer is the only sound one: **burn the seq and move to N+2, never delete a key you did not write** (`publish.rs:230-310`). That makes the log sparse, so `follow.rs`'s contiguity rule (`log.rs:22-27`, `follow.rs:230-241`) must tolerate a gap it currently treats as a stop. Add both to `formal/ForgeSync.tla` as an action, with the straggler sequence as a falsifier.

### Step 2 — the split (Design B), ~480 lines product / ~350 tests

| module | LOC | change | touched |
|---|---:|---|---:|
| `snapshot.rs` | 213 | pointer/refs split, `checkpoint_key`, checkpoint writer | ~120 |
| `log.rs` | 317 | burn-on-412; **prune floor at `checkpoint_seq`** (`log.rs:294-317`) | ~90 |
| `batch.rs` | 627 | step 5 = entry-then-CAS; `emit` fatal | ~50 |
| `restore.rs` | 389 | pointer → checkpoint → replay tail into the existing `update-ref --stdin` (`restore.rs:229`) | ~60 |
| `follow.rs` | 378 | reuse restore's replay; tolerate a burned seq | ~35 |
| `fold.rs` | 1207 | same ordering flip at `:824/844`; **packs stay in the pointer** so `:308/335/705/934` untouched | ~30 |
| `sweep.rs` | 196 | sweep stale checkpoints; **pack predicate unchanged** | ~25 |
| `undo.rs` | 180 | an undo point carries its own ref map (destructive pushes only; O(refs) is fine there) | ~30 |
| `status.rs` / `prune.rs` / `lib.rs` | 184/90/638 | `log head`, `applied seq`, `checkpoint_seq`, one knob | ~40 |
| `tests.rs` | 4442 | replay, prune floor, **crash between entry and CAS**, **straggler orphan at takeover** | ~350 |

Also steal, unconditionally and regardless of what you decide about log-first: **walgit's ambiguity discipline.** A 412 deletes only the version *you* wrote, never what is at the key now (`publish.rs:306-310`); a non-412 error is **ambiguous** — re-read and check whether your write landed before telling the client anything (`publish.rs:313-324`); when you cannot prove it did not land, **leave the object** and let a later writer sweep it. This is the same class as the runcd defect: a model that *assumed* rather than *checked*.

Do **not** steal walgit's un-jittered CAS retry loop (`publish.rs:846-854`), which violates its own `ROUNDTRIPS.md:84`/`:97`.

Do steal `ROUNDTRIPS.md` as an artefact: a per-operation table of sequential **depth** and total request **count**, pinned by a test (`sim.rs:1658-1700` asserts push=5, warm refs=1, cold refs=2). Given this project's documented history of measurements that measured nothing, a checked-in budget with an assertion behind it is a higher-grade instrument than a drill log.

---

## 5. WHAT TO MEASURE, PRE-REGISTERED

Baselines, all from this repository: P2 15.5 pushes/s and 1.22 MB/push (`runce-fold-fix-2026-09-07.log:54-55`); P9 705.5 MB for 402.7 MB = 1.75x (`:52`); 8,000 refs → `forge_ms` 1464 / `plain_ms` 1079 / `snapshot_B` 545,613 (`refscale-2026-09-06.log:16`); P5 cold start 13 s to first correct `ls-remote`, 29 s to full clone; warm claim→serving 222 ms (`c7-prewarm-2026-09-06.log:29`).

**M1 — the fold A/B. Run this before writing any log-first code.** Two arms, **same cluster, same repository state, interleaved**, differing only in the fold input rule. Legs: P9 and P2.
- *Predict:* the strict rule's P9 amplification is ≥1.5x and the permissive rule's is ≤1.0x, i.e. the 0.94→1.75 cross-run gap reproduces **in-run** within ±0.25x.
- *Falsifier:* the two arms are within 0.15x of each other on P9 ⇒ the cross-run difference was repository order, the mechanism named at `:70-73` is wrong, and the byte problem is somewhere else entirely — stop and re-diagnose before touching `fold.rs`.
- *Vacuity guard:* the arms must differ **only** in the fold rule, and P9 must run at the same ordinal position in both.

**M2 — the ref-scale win, controlled.** Extend `refscale` to a third arm: Design B pointer, checkpoint every 512.
- *Predict:* at 8,000 refs, `snapshot_B` drops 545,613 → **<8,000** (>65x), and `forge_ms` drops 1464 → **1150-1300** (a 165-315 ms cut of forge's 385 ms share). At 0 refs, `forge_ms` moves by <20 ms in either direction.
- *Falsifier:* `forge_ms` at 8,000 refs improves by **<100 ms**. Then X19 is not where forge's remaining share lives — it is the `for-each-ref` — and Design B's latency case collapses to follower bandwidth alone, which is not worth 480 lines on its own.
- *Falsifier 2:* the extra checkpoint PUT costs the **push** more than 30 ms at the p50. Then the checkpoint must move off the push path onto the derived tick, which is a design change, not a tuning knob.

**M3 — the entry-before-CAS cost.** Interleaved A/B (baseline/after per rep, per the perf-rig rule), P1 and P2.
- *Predict:* moving the log PUT ahead of the CAS and making it fatal costs the push **<40 ms at p50** (it is a parallel-issue PUT today, becoming a serial one) and P2 rate falls from 15.5/s by **<1.0/s**.
- *Falsifier:* P2 rate falls **>2 pushes/s**, or p99 push latency rises >200 ms. The entry then belongs in the same request as the CAS (one object), and the checkpoint/log split has to be redesigned around that.

**M4 — the burn-and-orphan drill, on a real cluster.** Deliberately park a writer between its entry PUT and its CAS (the gate, not a timer — a timer is not a proof of ordering), roll the pod, let the successor rotate, unpark.
- *Predict:* the successor **burns** seq N+1, commits at N+2, and no ref the straggler's batch carried appears in the head. The straggler's pushes were told `ng` and stay `ng`. A follower crosses the gap without falling back to the snapshot.
- *Falsifier:* any ref from the parked batch lands, or the follower takes a full restore. Then contiguity was load-bearing in a way the design did not model.
- *Control arm, mandatory:* the identical drill with a **decoy** — a second parked writer whose entry must *not* be burned — and a pass **past** the event, because three green shapes of the X15 drill measured nothing without exactly those two things.

**M5 — the failover knob, before and after the `quiet_polls == 0` change.** Kill the holder with sysrq power-off (a graceful shutdown drains and measures the wrong thing).
- *Predict:* time from node loss to first correct `ls-remote` on the successor is dominated by 60 s of quiet polls and is **55-75 s** today; letting a warm follower chase the log tail through the quiet window moves the *post-claim* term from ~222 ms to **<150 ms** and leaves the 60 s untouched.
- *Falsifier:* the post-claim term does not improve, because the follower was already caught up — in which case the whole failover story is already finished and the only remaining lever is `heartbeat_secs`, and you should say so and stop.

**One thing not to measure:** anything about multi-writer throughput. The ceiling is arithmetic — two in-region round trips per commit, ~12-25 commits/s, independent of N — and one writer with batching already sustains 15.5 pushes/s. A drill would only confirm what the capacity model at `flint-forge-design.md:258-260` already says.

---

## THE ONE-LINE VERDICT

**Multi-writer: no.** It removes the lease that `sweep.rs` depends on, its rebase reverts the winner in the case the note calls easy, its retry window is larger than its first-attempt window, and its ceiling is below what one batching writer already delivers — on a single-region bucket, with no production git system anywhere doing otherwise.

**Log-first: half-yes.** Take the byte win, which is real and measured; do **not** take the authority inversion, which buys only the multi-writer story you just refused. Split the pointer from the ref map the way walgit's manifest and git's own `reftable` both did, keep the CAS as the linearization point, and keep `lease.rs` as a correctness mechanism.

**And do the fold A/B first.** The design note's headline motivation — the 127x — is fold amplification, not the snapshot, and the experiment that settles it is already specified in your own log and still unrun.
