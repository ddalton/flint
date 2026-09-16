# Finishing the `churn/p47.txt` replay — 2026-09-15

`churn/p47.txt` was the one W4 phase-2 replay still rejected, recorded as
open in `lean/SAFETY.md`. The question it was supposed to answer: is the
MODEL's consume wrong, or the CODE's?

**Neither, twice.** Two separate gaps, both in what the model could
express or what the code bothered to say. A third divergence is now
visible behind them and is NOT closed.

Leg: `storm/round3/R3-S2-churn-ui`, 6 writers on 3 nodes, real S3,
projected onto one path (`drill2tla.py`). The recorded blocker was model
step 245.

## 1. The model had no window where the code has one — FIXED

Step 245 is a consume by B that adopts `<<churn/p47.txt, 3>>`. The model
refused it. Instrumenting the replay printed why:

    inbox = {}            fq[B] = {<<churn/p47.txt, 0>>}   (a tombstone)
    objects = (p47 :> 3)  manifest = (p47 :> 3)

The model's candidate set is `inbox \cup QueuedUpserts(B)`, which is
empty, so there was nothing to adopt. But `barrier.rs` step 1 says it
outright — **"the cell, read ONCE"**: the barrier loads the inbox at its
first step and the consume integrates THAT snapshot. Between the two,
another writer's window clear can empty the shared inbox, and this leg
does exactly that (`lean-088172f` clears at line 394; `lean-f05010e`
started its barrier at 390 and adopts at 398).

The model read `inbox` at the instant of the consume, so it could not
take a step the code takes. Fixed by modelling the read as its own step:
constant `InboxSnapshot`, `sc.inboxSeen`/`inboxLoaded`, action
`LoadInbox`, and a `load` step emitted at each `barrier_start`.

**The replay advanced 245 -> 321.** And the question the fix raises is now
asked rather than assumed:

| run | verdict |
|---|---|
| `LeanBarrierLeaseInboxSnapshot` (strict, every invariant) | **holds** |
| `LeanProbeStaleInboxAdopt` (must fail) | violated — a consume really does adopt an entry the cell no longer holds |

So the code's read-once is safe, exhaustively, in the breadth world.
Phase 1 stayed 15/15 with the snapshot on.

## 2. The GC's outranked branch said nothing — FIXED (code)

Step 321 is D's `finish`, and the model owes a GC step for every path in
its delete set. D's barrier in the raw trace (`traces/n1w1.jsonl`):

    consume action=dirty-preserved  path=churn/p47.txt
    scan deletes=1 uploads=1
    merge adds_nothing=true deletes=1 foreign=6 gone=2
    window_clear
    barrier_end consumed=13 deleted=0 outranked=1 parked=1 no_change=true

`deleted=0`, `outranked=1`: the delete was resolved foreign-wins. The GC
loop DOES process the path — and takes the one branch of that loop that
emitted no trace event:

    if installed.entries.contains_key(path) {
        report.outranked.push(path.clone());
        continue;                     // <- nothing traced
    }

Every other outcome (absent, deleted, replaced-absent, skip, and now
leaked) is traced. A replay therefore cannot tell "this barrier resolved
the delete as outranked" from "this barrier never looked at the path".
Not a correctness defect — outranked is right — but it makes that branch
unverifiable. One line added in `barrier.rs`; the model needed NOTHING,
because `GCDelete`'s skip arm already covers `manifest[p] # 0` and
`TraceLean` already maps any non-`deleted` result to "objects unchanged".

Traces already collected were written by a binary without the event, so
THIS replay cannot use it; the next drill's can.

## 3. What is still open

Injecting the event the fixed binary would emit does NOT get past 321:
TLC then stalls on the injected step itself, because

    model:  manifest(churn/p47.txt) = 0      (not cited)
    code:   installed.entries.contains_key(p47) = true  (cited)

The model's `GCDelete` would therefore COLLECT the object where the code
outranked it. So the two had already diverged about what D's installed
manifest cites, somewhere before 321 — the model's manifest still cited
p47 at step 245 and does not at 321.

That is the next thing to bisect: the earliest step where the model's
manifest for this path disagrees with the leg's. The trace carries `cas`
events with seq numbers and `observed` events with etags, so the
comparison is mechanical.

`R2-S2-churn-ui` rejects at a different place again (step 575, an
`abandon`), so there are at least two independent replays still open.
