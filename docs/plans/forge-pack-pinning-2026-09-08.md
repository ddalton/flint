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

## Where the dead objects come from

`28abff1b` holds 263 objects, 260 of which are in the consolidated pack.
The other three are:

    55f954e0…  commit   unreachable
    f9c8c878…  tree     unreachable
    01bb2964…  blob     unreachable

One commit+tree+blob triple, unreachable from the tip — the shape of a
single client commit touching one file. F14's arm A produced 28
refusals and arm E 15.

**A push forge refuses still leaves its objects in the repository, and
forge never had a say in it.** `receive-pack` migrates the push's
quarantine into the object store as soon as the *pre-receive* hook
passes, which is before the *proc-receive* hook that relays the decision
to the syncer. With `receive.unpackLimit = 1` every push is a pack, so
by the time forge says `ng` the pack is already on disk. (A conflicted
`refs/for` merge leaves objects too — `merge-tree --write-tree` writes
the conflicted tree and its blobs — but those stay LOOSE and are never
named.)

## Why they cannot be dropped — the knot, in four steps

1. **A batch names the DIRECTORY.** `batch.rs` step 5 is
   `next.packs = local_packs`. So the refused push's pack is named.
   This is not an oversight: naming the directory is exactly what makes
   the *queued* push safe, because git migrates its pack before the
   hook that queues it runs. Unnaming it is runcd (2026-09-07).

2. **A tier fold propagates it, and must.** `pack-objects --stdin-packs`
   is a pure roll-up and holds every object its inputs hold, dead ones
   included. This is what turns a ~600-byte residue into a 22,832-byte
   pin: the triple gets carried into a big pack.

3. **A base rebuild drops it, and must.** `pack-objects --all` is the
   only place unreachable objects are collected — that is what makes it
   a collection.

4. **The supersede check is strict object coverage** (`fold.rs`: `safe =
   false` on the first object of an input the roll-up does not hold), so
   every input holding a dead object stays named. Forever.

The knot is that each step is individually right: collection requires
dropping, dropping requires unnaming the inputs, unnaming requires
coverage, and coverage forbids dropping.

**Correction to this document's first draft.** It said the redundancy
survives because the repository is below `fold_min_bytes`. That is the
wrong reason. `fold::plan` sets `forced = n >= cap || cap_tripped`, and a
forced plan skips the byte floor — which is why seq 68 ran at all on an
80 KB repository. The floor delays the ladder; the pack-COUNT cap
overrides it. The redundancy is permanent because coverage pins it, at
**any** repository size, and a large repository pays it too — it is
simply a smaller fraction there.

## Direction 1 — supersede on reachable coverage: REFUTED

Drop a pack when the new pack holds everything REACHABLE that it holds.

**TLC finds the loss.** `formal/ForgeSyncFoldReachableCoverage.cfg`,
mutation constant `FoldReachableCoverage`, violates
`Inv_LandedPackComplete` in 35 s. The counterexample is runcd with the
arrow reversed:

| state | what happens |
|---|---|
| 10 | push `p2`'s pack lands on disk (`IdxLand`) — quarantine migrated, hook not yet served |
| 14 | `p1`'s batch CASes; `snap.packs = {p1, p2}`, `history = {p1}` — **`p2`'s pack is named, `p2` has not landed** |
| 17 | a base rebuild is planned; `--all` reaches only `p1`, so `holds[f1] = {p1}` |
| 21 | reachable-coverage: `holds[p2] ∩ history = {}` ⊆ `holds[f1]`, so **`p2`'s pack is unnamed**; `snap.packs = {f1}` |
| 27 | `p2` lands. `history = {p1, p2}`, and `p2`'s objects are in nothing the snapshot names |

The crux is that state 21 and the runcl measurement are **the same
observation**: a named pack whose extra objects no ref can reach. One is
a refusal's residue and one is a push about to land, and on a serving
syncer nothing distinguishes them. This is now the THIRD weakening of
the coverage rule TLC has refuted — after "unname every input" (runcd
itself) and "unname an input whose uncovered objects have landed".

The age-qualified variant the first draft floated ("older than the undo
window") is a timer, and a timer is not a proof of ordering. It would
need the same counterexample re-run with the clock in the model.

## Direction 2 — do not pack what was refused: ALREADY THE CASE

The first draft said the objects enter the pack "because the pack is
built before the batch judges", and that fixing it would mean packing
later, against the ordering `8381b557` depends on. **Both halves are
wrong, and the code says so.**

`pack_new_objects` is called *after* the judging loop, from
`merge_tips` — a list only `Judged::Accepted` appends to, and which an
atomic push's rollback truncates along with `accepted`. The server-built
pack is *already* the accepted set. Nothing in this direction touches
the publish-before-ref-transaction ordering, and no model run was
needed to say so.

The dead objects are not in that pack. They are in the client's own,
migrated by git before forge was consulted (above). Forge cannot decline
to *pack* them; it could only decline to *name* them, and that is
step 5's directory naming — which is direction 1's problem again, from
the other end, and refuted with it.

## Direction 3 — leave it

Still available, and now better priced: it is not self-correcting above
the fold floor, because the pin is coverage and not size. The cost is
three tiny objects' worth of pack per refusal, permanently, on a
workload whose refusals are correct and routine.

## Direction 4 — reclaim under quiescence (the one that survives)

Cut the knot where the ambiguity does not exist. The hazard is entirely
"a pack on disk whose ref has not moved YET". There is a window in which
no such pack can exist: `server.rs` publishes `Phase::Importing`,
restores, and only then publishes `Phase::Serving` — and until it does,
the hook answers every command `ng … the repository server is not
accepting writes`. No ref can move in that window, so an object
unreachable there is unreachable, full stop.

A reclaiming base rebuild taken there — `pack-objects --all`, name only
the result, let the ledger sweep take the rest — collects the dead
objects with no weakening of the coverage rule, because it is not a
fold and supersedes nothing.

What it owes before any code:

- **Its own model run.** The claim "no ref moves between `Importing` and
  `Serving`" is an ordering claim about the shipped phase machine, and
  the whole point of this document is that such claims get refuted. It
  needs an action in `ForgeSync.tla` and `Inv_LandedPackComplete` green.
- **A cost gate.** It rewrites the repository at every start. It should
  run only when the redundancy is worth it, measured from the indices —
  the same read `fold.rs` already does for coverage.
- **The restore-window push.** A push can still *migrate a pack* while
  the syncer is importing, even though it is refused. Dropping that pack
  is safe (nothing names it, the client retries and re-sends), but the
  test has to state it rather than assume it.
- **Not on the wake path.** A wake from idle-to-zero is when a storm
  arrives; a whole-repository repack there is the wrong trade.

## How to reproduce

`forge/e2e/f14-make-corrupt.sh` classifies every snapshot-named pack by
dropping it and running the syncer's own `fsck --connectivity-only`. On
runcl it reported two of four as "redundant (folded elsewhere)" — that
line is this finding.
