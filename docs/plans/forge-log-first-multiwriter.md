# forge, log-first: making history the truth and the pointer derived

*An investigation, 2026-09-07. Question asked: keep the operator, door, CRD
and status surface; change forge's own storage discipline to be log-first;
how hard is multi-writer then? No external pre-1.0 dependency, and local-clone
reads are not given up.*

---

## 0. The short answer

The primitive this needs already exists in the codebase, and so does the log.
What is missing is **authority**: forge's log is deliberately built to have
none. Inverting that is a contained change to four modules. Multi-writer then
follows from a property of object stores forge already relies on, and the real
cost is not the append race at all — it is a **user-visible consistency
semantic** that today is free and afterwards must be chosen and paid for.

My estimate: **log-first single-writer is a medium change with a strong
independent payoff. Multi-writer on top of it is a large one**, and the hard
part is read-after-write across writers, not concurrency control.

I would do the first and treat the second as a separate decision, because the
first is worth shipping even if the second is never built.

---

## 1. Where forge already stands

Four facts, each verified in the code rather than assumed:

**The log exists.** `log.rs` (317 lines) already writes one immutable object
per commit at `git/log/<seq>.json`, naming the refs that moved, the packs that
appeared and the packs that left. `follow.rs` (378 lines) already applies those
entries to a repository.

**But it is deliberately powerless.** Its own header states the three rules
that keep it a hint: the entry is written *after* its CAS, a follower advances
only along a contiguous chain, and *nothing in the log is a reference* — log
entries do not keep packs alive. The sweep's reference predicate is "named by
the snapshot whose etag this sweep read" (`sweep.rs`). The log is a delta
beside the truth, and the truth is the snapshot.

**The conditional primitive is already there.** `flint-store` has
`PutCondition::IfNoneMatchAny` — "first generation of a new key: fail if ANY
object exists (closes the create race with outside writers)". That is precisely
the compare-and-swap a log-first design serializes on, already implemented and
already exercised.

**Packs are immutable and content-named.** Uploading them needs no
coordination now and would need none with ten writers. This is the property
that makes the whole idea viable.

So forge is not far from this shape. It arrived beside it from the other
direction.

---

## 2. Why this is worth doing even without multi-writer

This is not a speculative benefit. It is the defect this session measured
twice.

**The snapshot is O(refs) and is rewritten whole by every batch.** From the
ref-scale rig, at 8,000 refs: `snapshot_B` = 545,613 bytes and `inforefs_B` =
511,008 bytes, *rewritten by every push*, whatever that push touched. A push
that moves one ref pays for all 8,000. That is X19, and it is why a push's
"fixed" cost is not fixed.

**It is also why forge loses the tiny-push storm.** On runcd, P2 (32 pushers,
60 s, one-commit pushes) cost forge **1.44 GB** against walgit's **11 MB** —
127x. walgit is log-first: a small push appends a small entry. forge rewrites
the whole ref map and re-uploads pack sets. runce improved this to 1.22 MB per
push but did not change its shape.

Log-first makes a push cost **O(refs touched)** instead of O(refs held). That
is the single largest structural inefficiency the drills have found, and it is
not fixable inside the current discipline — packing refs (shipped this session,
≈1.0 s a push at 8,000 refs) attacks git's ref walk, not the snapshot rewrite,
and the snapshot bytes were identical in both arms of that A/B.

---

## 3. What "log-first" means concretely here

Today:

    snapshot.json   = TRUTH  (full ref map + full pack list, CAS on etag)
    log/<seq>.json  = hint   (delta, written after, no authority)

After:

    log/<seq>.json  = TRUTH  (append-only; the entry's CREATION is the commit)
    snapshot.json   = CHECKPOINT (derived; a reader's shortcut past the log's tail)

Three rules invert with it, and each is load-bearing today, so each has to be
re-established rather than deleted:

1. **The entry is written before/as the commit, not after.** Today's ordering
   exists so a fenced writer cannot leave an entry for a batch that never
   landed. Log-first, the entry *is* the landing, so that failure mode
   disappears — but its mirror appears: an entry that lands whose pusher then
   dies is committed and must be honoured by everyone. That is correct
   behaviour, and it is a change in what "acknowledged" means internally.
2. **Log entries become references.** The sweep's reference set can no longer
   be "the packs the snapshot names"; it becomes "the packs reachable from the
   checkpoint plus every entry after it, plus undo points". `sweep.rs` rule 1
   ("list candidates first, then read the reference set, and abort if it
   moved") survives unchanged in spirit; the set it reads gets bigger.
3. **The log can no longer be pruned freely.** `FLINT_FORGE_LOG_MAX_ENTRIES`
   currently prunes a hint. Log-first, pruning past the checkpoint is
   destroying history — so checkpointing must lead pruning, exactly as
   compaction leads log truncation in any write-ahead design.

---

## 4. Why multi-writer follows

With the log authoritative, the commit is: **create `log/<N+1>.json` with
`IfNoneMatchAny`.** Exactly one writer wins that race; the losers get
`PreconditionFailed`, read the winner's entry, re-validate their own ref
preconditions against it, and retry at `N+2`.

This is the standard log-first concurrency argument (Iceberg, Delta, and
walgit's own `wal`), and forge can make it without a new dependency because
`IfNoneMatchAny` is already there.

Git's push semantics fit it unusually well: every ref update already carries an
`old_oid`, so "re-validate against the winner's entry" is not a new concept
that has to be invented for the storage layer — it is the same non-fast-forward
check the door and `pre-receive` already enforce. A loser's retry is a genuine
rebase of intent, and where it cannot rebase, the correct answer is the one git
already gives the client: rejected, non-fast-forward.

**The lease stops being a correctness mechanism.** `lease.rs` (425 lines) exists
to guarantee one writer. Under log-first it becomes at most an *optimisation*
(reduce contention by preferring one writer per repository) or it goes away.
That is a large simplification, and it removes the fencing class of bug the
formal model spends most of its mutations on.

---

## 5. The crux, and it is not concurrency control

**Read-after-write across writers.** Today a client that pushes and then fetches
talks to the one server that has the repository on local disk, so it sees its
own write. That is free, and it is the property "local-clone reads" buys.

With N writers each serving from their own local clone, a push accepted by A is
not on B's disk until B applies the entry *and fetches the packs it names*.
A client that pushes to A and fetches from B can see a stale ref. Git clients do
this constantly (CI that pushes then clones; `push && fetch` in scripts).

There are three honest options and they should be chosen deliberately:

- **(a) Sticky routing per repository at the door.** Cheapest, preserves the
  semantic exactly — and concedes most of the multi-writer benefit for writes,
  though it still buys concurrent *reads* from warm replicas and instant
  failover. Probably the right first destination.
- **(b) A read barrier.** A reader is served only once its syncer has applied
  the log head as of the request. Correct, and costs a HEAD on `log/<seq+1>`
  per read plus a wait — bounded, but it puts the object store in the read path
  forge worked hard to keep it out of.
- **(c) Bounded staleness, documented.** Cheapest to run, and a real change to
  what forge promises. I would not choose this for a git server; people's
  tooling assumes read-your-writes.

**This is the decision the investigation turns on, not the append race.** The
append race is solved and its solution is already in the tree.

A second, smaller consistency wrinkle: catching up is not just applying refs, it
is *fetching the packs an entry names*. A writer that commits a 1 GiB pack makes
every peer's catch-up cost that GiB before it can serve the new tip. Option (b)
therefore has a latency tail governed by pack size, not by entry size.

---

## 6. What changes, module by module

| module | LOC | change | why |
|---|---:|---|---|
| `log.rs` | 317 | **large** | entry becomes the commit; `IfNoneMatchAny` append; version bump |
| `snapshot.rs` | 213 | **large** | becomes a derived checkpoint; no longer the CAS target |
| `batch.rs` | 627 | **large** | step 5 stops being "one CAS on the snapshot" and becomes "append, or rebase and retry" |
| `fold.rs` | 1207 | **medium** | commit becomes an append; the strict supersede rule shipped this session already makes fold commits validate rather than assume |
| `sweep.rs` | 196 | **medium** | reference set = checkpoint + log tail + undo points |
| `restore.rs` | 389 | **medium** | restore = checkpoint, then replay the tail |
| `follow.rs` | 378 | **medium→small** | already does replay; becomes a steady-state path rather than a warm-up |
| `lease.rs` | 425 | **shrinks or goes** | no longer a correctness mechanism |
| `server.rs`, door, operator, CRD, `status.rs` | — | **unchanged** | as required |

The user's constraint — keep the operator, door, CRD and status surface — holds
up. Nothing above reaches them. `status.rs` gains fields (log head, applied
seq, lag) but keeps its shape.

---

## 7. What has to be re-proved

`formal/ForgeSync.tla` is built around one cell, one holder and a lease, and
most of its 17 configurations are mutations of that fencing. Log-first
invalidates a good part of that structure — not because the invariants change
(`Inv_AckedIsDurable`, `Inv_LandedPackComplete` are exactly as relevant) but
because the actions do.

That is not a reason against the design; it is a cost line. And this session
supplies two cautions worth carrying into it:

- The model's `FoldPlan` once *assumed* what it should have checked, and that
  is why runcd's defect got past a checker carrying the very invariant it
  violated. A log-first spec must not assume that replaying a log reproduces
  the writer's state — that is the same class of axiom.
- A whole branch of `FoldRenew` was dead for as long as it existed and TLC
  warned about it on every run. A rewritten spec needs the UNCHANGED-conflict
  warnings read, not scrolled past.

---

## 8. Staged plan

**Stage 1 — checkpoint/log split, still single writer.** Make the log
authoritative and the snapshot a checkpoint; keep the lease. No consistency
change, because there is still one writer. Independently valuable: it is what
makes a push O(refs touched), which is the X19 cost measured above. Reversible
in the sense that the checkpoint alone still restores.

**Stage 2 — concurrent appenders, sticky reads.** Allow N writers to append,
route reads stickily per repository at the door (option (a)). Buys failover
without a restore, and concurrent read capacity.

**Stage 3 — non-sticky reads.** Only if wanted, and only with option (b)'s read
barrier and its latency measured on a real cluster.

Stage 1 is worth doing on its own merits. Stages 2 and 3 should each be a
separate decision with its own drill.

---

## 9. Difficulty

- **Stage 1: medium.** Contained to four modules, one on-disk format version
  bump, and a compatibility path (a reader meeting an unknown version must
  fail closed, which the codebase already does consistently). The riskiest part
  is the sweep's reference set, because getting it wrong deletes live packs —
  and that is the one place where this session's evidence says forge's
  invariants are already sharp.
- **Stage 2: large**, but mostly in testing and modelling rather than in the
  append path, which is ~50 lines. Budget most of it for the formal model and a
  concurrency drill.
- **Stage 3: large and semantic**, and I would not start it without a measured
  answer to what the read barrier costs.

## 10. What I would not do

Adopt walgit itself, or its format, as a dependency. The shape is worth
learning from — this session measured where it wins (tiny-push storms, by 127x)
and where it does not (P9 bytes, P1 latency at 1 GiB, and it has no equivalent
of the local-clone read path). Taking the shape without the dependency is the
right instinct and it is what this note describes.

---

*Empirical basis in this repository: `forge/e2e/results/packrefs-ab-2026-09-07.log`
(snapshot bytes identical across arms — packing refs does not touch X19),
`forge/e2e/results/runce-fold-fix-2026-09-07.log` and the runcd log (P2 bytes,
forge vs walgit). Code read for this note: `snapshot.rs`, `log.rs`, `follow.rs`,
`sweep.rs`, `lease.rs`, `batch.rs`, `fold.rs`, `crates/flint-store/src/lib.rs`.*
