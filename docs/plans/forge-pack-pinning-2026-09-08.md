# Dead objects pin live packs — measured on runcl, 2026-09-08

**Not a correctness defect. No data is lost and nothing is served
wrong.** This is byte amplification with an exact mechanism, found while
building the positive control for F14's restorability oracle.

## The measurement

F14's repository on runcl, after the full drill and its P8 restore:

| pack | objects | bytes | needed for reachability? |
|---|---|---|---|
| `754746bb` | 370 | 32,912 | **yes** — the consolidated pack |
| `6a824d32` | 5 | 583 | **yes** — holds the tip |
| `28abff1b` | 263 | 22,832 | no |
| `9c638c70` | 263 | 22,832 | no |

Keeping only the two load-bearing packs, `git fsck --connectivity-only`
passes clean. So **45,664 of 79,159 snapshot-named bytes — 58% — are
redundant**, and every restore downloads them.

## Why they cannot be dropped

`28abff1b` holds 263 objects, 260 of which are in the consolidated pack.
The other three are:

    55f954e0…  commit   unreachable
    f9c8c878…  tree     unreachable
    01bb2964…  blob     unreachable

One commit+tree+blob triple, unreachable from the tip. That is the shape
a REFUSED push or a losing `refs/for` merge leaves behind — F14's arm E
produced 15 refusals and arm A 28. The objects still rode into a pack
that was uploaded.

The supersede rule is **object coverage, not reachability**: a pack is
dropped when the new pack holds everything it holds. Three dead objects
are enough to pin 22,832 live bytes forever.

## Why nothing else reclaims them

- **No fold.** `foldsCommitted: 0`, `baseRebuilds: 0`. The ladder starts
  at `fold_min_bytes` (256 MiB) by design (X18 rule 4) and this whole
  repository is ~80 KB of named packs, so compaction never runs.
- **The consolidation did run and did work**: batch log seq 68 is
  `+1 pack, -66 removed`. It reclaimed 66 packs and stopped exactly at
  the two that dead objects pin.

So on any repository below the fold floor the redundancy is permanent.
Above the floor a fold would eventually absorb it — which is why this
has not shown up in the byte campaigns, all of which ran large.

## Why it is worth fixing rather than noting

The workload that creates it is the workload forge is built for.
Concurrent `refs/for` proposals arrive in one batch (four at once is
ordinary — that is what `8381b557` coalesces), and every refusal is a
candidate dead triple. F14's arm E refuses 15 of 20 BY DESIGN: the
refusals are content conflicts, deterministic and correct. Correct
refusals should not cost permanent bytes.

## Directions, not a decision

1. **Supersede on reachable coverage.** Drop a pack when the new pack
   holds everything REACHABLE that it holds. Needs care: "reachable"
   must be computed against the snapshot's refs, and an object
   unreachable now can be made reachable later by a push that names it —
   which is exactly what `8381b557` was about. Safer variant: only for
   objects also absent from every other named pack and older than the
   undo window (X15 keeps 7 days).
2. **Do not pack what was refused.** The objects enter the pack because
   the pack is built before the batch judges. Building the pack from the
   ACCEPTED set only would stop it at the source, at the cost of packing
   later in the batch — which the ordering after `8381b557` may not
   allow, since the publish must precede the ref transaction.
3. **Leave it.** Above the fold floor it self-corrects. Small
   repositories pay a bounded, small absolute cost.

Direction 2 is the one that removes the cause rather than the symptom,
and it is also the one that touches the ordering the F14 fix depends on,
so it wants a model run before code — `ForgeMergeChain.tla` already has
server-built commits and the pack a batch writes.

## How to reproduce

`forge/e2e/f14-make-corrupt.sh` classifies every snapshot-named pack by
dropping it and running the syncer's own `fsck --connectivity-only`. On
runcl it reported two of four as "redundant (folded elsewhere)" — that
line is this finding.
