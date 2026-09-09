# Dead objects pin live packs — measured on runcl, 2026-09-08

> **STATUS 2026-09-09 — BOTH DIRECTIONS ARE BUILT AND DRILLED.**
> Direction 5 (the reducer) and direction 4 (the collector) are in
> production code behind two flags, both OFF, opted into per repository
> with `spec.packs.{nameAcceptedSet,reclaimAtRest}`. Commits
> `2c5c7c1d` (syncer), `baf11c7b` (operator + CRD schema guard),
> `9d6890a8` (the drill).
>
> **On the wire**, two repositories on one cluster differing only in
> that block (`forge/e2e/residue/`, artefacts
> `residue-drill-20260909-default.log` and `-compact1.log`):
>
> | | control | treated |
> |---|---|---|
> | the pack a REFUSED push leaves | snapshot **NAMES** it | **not named** |
> | after a restart of both | 8 packs / 76,972 B, unchanged | 4 packs / **38,532 B** |
> | clone + `fsck --strict` | clean | clean |
>
> **The rig condition selects which claim is testable.** At the shipped
> compaction thresholds (`COMPACT=0`, the default) both legs work. At
> `COMPACT=1` a base rebuild runs every cycle and `--all` drops dead
> objects, so the residue is collected in BOTH arms and direction 5 has
> no pinning left to observe — that leg is SKIPPED there, with its own
> exit code, because a leg that could not run is not a leg that passed.
>
> **The first drill run reported GREEN while both rules did nothing**
> (`residue-drill-20260909-vacuous-first-run.log`, kept deliberately).
> Four defects, all in the checks rather than the rules: the recorder
> was not running at all (`pre_receive` has three exits and one was
> instrumented); a three-byte difference was credited as a reduction;
> aggregate named bytes was the wrong oracle entirely; and the leg that
> replaced it measured the wrong pack, with the control passing by luck
> because the directory rule names everything. See `9d6890a8`.


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

## MEASURED ON REAL CONTENT — 81%, and the number got worse

The runcl figure below (58%) came from one ~80 KB repository whose
pushes do not deltify, and it existed only as prose. It is now measured
through forge's own chain — a real `git push`, a real `receive-pack`,
both hooks, the serving loop —
(`forge/syncer/tests/push_chain.rs::measure_what_a_refused_push_leaves_in_the_snapshot`,
artefact `forge/e2e/results/residue-measure-20260908.log`):

| | |
|---|---|
| corpus | one 4,000-line file, one line edited per push (pushes DELTIFY) |
| refusals | 3 non-fast-forward + 3 `refs/for` conflicts (syncer) + 3 policy (pre-receive) |
| named | 107,232 B in 6 packs |
| **redundant** | **87,139 B — 81%**, n=3, 81/81/81 (82 on a fourth, differently-timed run) |

Five of six named packs are redundant; the only load-bearing one is the
base rebuild's own output. The syncer says so in its log:
`the fold keeps 5 input pack(s) named: they hold objects the roll-up
does not`.

**Every redundant pack holds REACHABLE objects — 3, 3, 3, 12, 15 — and
not one holds zero.** That is `index-pack --fix-thin` completing a thin
push with delta bases the server already has. So a rule keyed on "no
object in this pack is reachable" fires on NONE of them, which is why
the fold-input filter in
`forge-pack-residue-plan-2026-09-08.md` was rejected. The right
predicate is *contributes no reachable object that no other named pack
holds* — but see that document: anything reachability-keyed in front of
the coverage rule disarms the rule's own tests.

**The control holds.** The three policy-refused pushes go through
`pre-receive`, and `count-objects -v` is byte-identical across them
(`count: 6, in-pack: 180, packs: 19` before and after). Refusing before
git migrates the quarantine really does leave nothing — measured through
forge's own hook, not from the git manual.

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

## Direction 5 — name only the accepted set: SAFE, and it removes 4/5 of it

Record each push's pack at `pre-receive`, where the quarantine pack is
still identifiable as *this push's* pack, and have step 5 name only the
packs of pushes something was accepted from — instead of naming the
whole directory.

**The model says it is safe.** `formal/ForgeSyncNameAcceptedSet.cfg`
HOLDS: 25,203,710 distinct states, depth 58, 22min 12s, all nine
invariants including `Inv_LandedPackComplete` — the one that refuted
direction 1 in 35 s. The control has teeth: `ForgeSyncForgetPushPack`,
the same rule with the push→pack mapping lost, violates it in 1 s. That
control is in the gate; the 22-minute strict run is not.

**Real git says the name is stable, for a structural reason.**
`index-pack --fix-thin` completes BEFORE `pre-receive` runs, so the hook
already sees the final post-fix pack name — 9/9 across fat, thin,
50-ref/40 MB, mixed and relative-invocation shapes, and replicated in
forge's own chain (16 of 19 recorded names survive migration; the 3 that
do not are the policy arm's, which git discards). Also measured: one
pack maximum per push; a ref-only push produces no pack at all;
`GIT_QUARANTINE_PATH` is always absolute.

**But it is a reduction, not an elimination — and the ceiling is
measured.** `receive.procReceiveRefs = refs/` lets the syncer answer per
ref, but git built ONE pack for the whole push. A push with one ref
accepted and one refused produces a single pack holding both verdicts'
objects (measured: 6 objects, 3 reachable), and direction 5 names it
because something in it was accepted. Arm M of
`measure_what_a_refused_push_leaves_in_the_snapshot`
(artefact `forge/e2e/results/residue-direction5-20260908.log`, n=7):

| | |
|---|---|
| named | 147,562–166,076 B |
| redundant | 92–93% of it |
| **direction 5 removes** | **78–81% of the residue** |
| direction 5 keeps | 18–21%, all of it in mixed-push packs |
| direction 5 keeps (not from a push) | 0% |

**And what it keeps GROWS.** Eight identical rounds (one accepted push,
one mixed push, one wholly-refused push each), n=3, artefact
`forge/e2e/results/residue-growth-20260908.log`: the bytes direction 5
keeps rise **+9,651 B per mixed push, with a dead-constant slope** —
9,767 B after round 0 and 77,445 B after round 7, +2 pinned packs per
round. That is expected and it is the point: direction 5 changes which
packs step 5 NAMES, not the coverage rule that pins them, and the only
collector is a base rebuild's `--all`, whose output by construction
cannot cover what it dropped. **Direction 5 lowers the slope; it does
not make it zero. Direction 4 is the only thing that collects.**

The *percentage* converges rather than growing, because live content
grows too — to ~53% here, tending to 50.4%, which is only this rig's 1:1
mix of mixed to wholly-refused pushes. The 78–81% above came from a 1:3
mix. **Neither ratio is a prediction for a real repository**: the rate of
mixed pushes on one is unmeasured, and it is the single input that
decides how much direction 5 is worth.

**Two constraints it inherits.** The pack→push map is MANY-TO-ONE — two
pushes of identical content produce the same checksum and so the same
name — so a pack is droppable only if EVERY producer was refused; record
`pack → {pushes}`, never `push → pack` as ownership. And `gc.auto` /
`receive.autogc` would invalidate every recorded name (measured: with
autogc on, all three recorded names vanished between pushes). `gitcmd.rs`
already pins both off, which turns that config from tidiness into a
correctness dependency deserving a start-up assertion.

## Direction 3 — leave it

Still available, and now better priced: it is not self-correcting above
the fold floor, because the pin is coverage and not size. The cost is
three tiny objects' worth of pack per refusal, permanently, on a
workload whose refusals are correct and routine.

## Direction 4 — SAFETY IS UNRESOLVED (2026-09-08, late)

**The green this section rested on was VACUOUS, and the correction is
not yet finished.** Read this before the rest of the section, which was
written when `ForgeSyncReclaimAtRestore` was believed to hold.

`FoldPlan` was extended to let a fold be planned in the reclaim window;
`FoldInit` and `FoldComplete` were NOT — both required
`st \in {"serving", "pushing"}`. So a fold planned while reclaiming could
never reach `"uploaded"`, therefore never `"renewed"`, therefore never
COMMIT; and `ReclaimDone` requires `fold[s].stage = "none"`, so the
syncer could not carry one out of the window either. **The `atRest` arm
of `FoldCommit` was unreachable.** The 47,449,859-state green proved
nothing, and neither did the `ReclaimBySet` run built on top of it.

It surfaced only because `ForgeSyncReclaimBySet` returned EXACTLY
`ForgeSyncReclaimAtRestore`'s 215,837,588 generated / 47,449,859
distinct. A mutation that fires cannot leave the generated count
untouched. **Comparing counts against a neighbouring run is now the
acceptance test for anything in this family.**

With the two guards fixed, `ForgeSyncReclaimAtRestore` **VIOLATES**
`Inv_LandedPackComplete` in 2min 33s (31-state trace), and still
violates with `MaxCrashes = 0` in 1min 08s — so it does not depend on
the crash path.

**DIRECTION 4 IS ALIVE — IF IT UNLINKS INSTEAD OF RETAINING.**

The refutation below is real but NARROWER than it looks. The loss does
not come from UNNAMING the pack; it comes from unnaming it into
`retained`, because `Listing(s) == localPacks[s] \ retained[s]` excludes
a retained pack permanently, so a retry reusing that pack NAME cannot be
named. **Retention exists for readers mid-clone, and in the reclaim
window there are none** — the syncer is not serving. So the reclaim may
UNLINK what it drops.

`ForgeSyncReclaimUnlinks` **HOLDS**: 409,336,983 generated, 86,039,237
distinct, depth 62, 1h 33min. Flipping the one constant flips the
verdict — `ReclaimUnlinks = FALSE` violates in 4 min at 13,226,538
distinct — so this is not a vacuous green.

**And the window is load-bearing**, which is the control that matters:
`ForgeSyncReclaimUnlinksServing`, differing in EXACTLY ONE constant
(`ReclaimWhileServing`), violates `Inv_AckedIsDurable` in 1min 12s.
Outside the window, unlinking destroys a pack an ACKED push needed. So
the green is "safe *there*", not "unlinking is safe".

**What this gives forge: a COLLECTOR.** Combined with the measurement —
the greedy set collects 100% of the residue at every base cadence
building nothing — the reclaim costs a reachability read and a snapshot
CAS. No `pack-objects`, no upload, nothing on the wake path. That is
what turns the growth from unbounded into bounded by the restart
interval.

**Direction 5 also survives this hazard.** `ForgeSyncNameAcceptedSetRetry`
HOLDS at 26,867,483 distinct (vs 25,203,710 without the retry path, so
the added behaviours are real). `AcceptedListing == (belief.packs \
retained) \cup {batch.push}` names the current push's pack EXPLICITLY,
which is exactly the escape `Listing` lacks.

**So the finding now has both halves: a REDUCER (direction 5, −78-81%)
and a COLLECTOR (direction 4 with unlink).** Neither is built.

### The refutation this replaced, kept because it is why unlink is needed

**THE RETAIN FORM IS DEAD.**

The first counterexample ran `CleanRelease -> ClaimReleased -> Restore`
with a push still queued, which the code cannot do: `restore::restore`
is called ONCE (`server.rs:238`) before the first `Phase::Serving`, and a
lease loss returns `Fenced` and exits, so a real restore always begins a
FRESH process with an empty request queue. `Queued(s)` in the model was
derived from the pack being on DISK, which is more permissive.

So the model was corrected — constant `RestoreLosesQueue`, TRUE only in
the two reclaim cfgs, leaving the other 21 runs untouched. A restore now
resets every pending push destined for that syncer to **"new"**: the
request is gone, and the client is FREE TO RETRY. Resetting to a dead
state instead would have assumed away the hazard while removing the
other one.

**It still violates** — 4min 11s, 51,070,395 generated / 13,226,538
distinct, counts distinct from both the vacuous baseline and the earlier
run, so the change took. The set form `ForgeSyncReclaimBySet` violates
too (4min 01s, 45,151,741 / 11,868,582).

**The mechanism, and it is grounded in a measured property of real git:**

| state | |
|---|---|
| 19 | `Restore` — the syncer enters the window; p2's pending request is lost |
| 20-24 | the reclaim commits: p2's pack is unreachable, so it goes to `retained` |
| 25 | `ReclaimDone` — the window closes, the syncer serves |
| **26** | **the client RETRIES p2** |
| 27-32 | the batch lands p2: `history = {p1,p2}` but `snap.packs = {p1,f1}` |

`Listing(s) == localPacks[s] \ retained[s]` excludes the pack, so the
landed push's objects are in nothing the snapshot names.

**The window stops pushes ARRIVING. It does not stop the client
RETRYING afterwards.** And the retry reuses the pack NAME, because pack
names are many-to-one — identical content, identical checksum, measured
2026-09-08. That is the same property recorded above as a constraint on
direction 5; here it is what kills direction 4.

**Why the STRICT rule is safe and every relaxed one is not.** Under
strict coverage an input is superseded only when the roll-up HOLDS
everything it holds, so a later landing is covered by the roll-up. The
relaxed rule drops objects that are NOT in the roll-up — and those are
exactly the objects a retry makes reachable. Quiescence does not change
that, because the retry happens after the window.

**So all four relaxed-coverage variants are now refuted:** direction 1
while serving (`FoldReachableCoverage`), `ReclaimWhileServing`,
`ReclaimAtRestore`, and `ReclaimBySet`. **The residue has no safe
collector under the current listing rule**, and direction 5 — name the
packs of accepted pushes — is the only surviving fix. Note it is
probably immune to this specific hazard by construction:
`AcceptedListing == (belief.packs \ retained) \cup {batch.push}` names
the current push's pack EXPLICITLY rather than reading the directory
minus retained. That is worth checking rather than assuming.

## Direction 4 — reclaim under quiescence (REFUTED; the original argument, kept for the record)

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

### It probably does not need the repack at all

**The model plans a FRESH pack** — `FoldPlan` requires `holds[f] = {}`
(`ForgeSync.tla:925`) — so what TLC proved safe always pays
`pack-objects --all --indexed-objects --write-bitmap-index` over the
whole repository plus the upload of its output. **But the rule it proves
never asks the coverer to be new:**

    D == {q \in S : (holds[q] \cap fold[s].at) \subseteq holds[f]}

If a pack the snapshot ALREADY names covers the reachable set, every
other named pack can be unnamed for the cost of a snapshot CAS.
Measured, n=3 × 8 rounds
(`forge/e2e/results/direction4-check-20260908.log`): **24 of 24 rounds a
named pack already covered every reachable object**, making 92% of named
bytes unnamable with no `pack-objects` run and nothing uploaded. The
coverer is the base rebuild's own output, and it grows only with live
content.

That is the shape of the whole finding: **forge is already paying for
that pack during normal serving. What the coverage rule withholds is not
the pack — it is the PERMISSION TO UNNAME, and the quiescent window is
exactly what grants it.**

**The single-pack form does not survive a real cadence — the SET does.**
That 24-of-24 ran at `base_rebuild_min_secs = 0`; shipped is 3600
(`lib.rs:288`). Re-measured with that as the only dimension
(`forge/e2e/results/direction4-cadence-20260908.log`, n=3 × 3 cadences ×
6 rounds):

| `base_rebuild_min_secs` | single coverer | greedy SET collects |
|---|---|---|
| 0 | 6/6 rounds | 105,656 of 105,656 B |
| 6 | **0/6** | 86,668 of 86,674 B |
| **3600 (shipped)** | **0/6** | 48,017 of 48,017 B |

The greedy — keep a set holding every reachable object, drop a pack when
every reachable object it holds is in another kept pack — collects
**100% of the redundant bytes at every cadence, in every rep, building
nothing**. It equals the drop-and-fsck oracle exactly. So the reclaim
never runs `pack-objects` and never uploads: it reads reachability once
in the window and writes a shorter pack list.

A second reading: at the shipped cadence the residue is itself smaller
(48,017 B vs 105,656 B), because residue *requires* a base rebuild to
create it — `--all` is the only thing that drops dead objects and so the
only thing that leaves an uncoverable input.

Neither form is what TLC checked: `FoldPlan` always builds a fresh
coverer. The set rule owes a model run.

### Checked in the shipped code

- **The window is real.** The UDS listener is bound only after
  `publish(Phase::Serving)` (`server.rs:267`); when `ask` fails the hook
  writes `ng … the repository server is not accepting writes` for every
  command (`hook.rs:216-224`).
- **Undo does not conflict.** `undo::referenced` unions the undo points'
  pack stems into the sweep's `named` set (`fold.rs:965-968`),
  independent of the current snapshot, so unnaming cannot delete one.
- **The reclaim is already optional and time-boxable inside the proof.**
  `FoldAbandon` exists and `ReclaimDone` is enabled as soon as no fold
  is in flight, so a syncer may simply leave.
- **It is on the WAKE path, not just cold start.** `restore()` runs
  before every `Phase::Serving`, and a slept repo wakes on a plain read.
  That contradicts the "not on the wake path" bullet below — and the
  zero-work form is what dissolves the contradiction.

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
