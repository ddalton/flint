# LeanP1: an inductive proof with TLAPS — the plan (2026-10-05)

`lean/SAFETY.md` §5 item 8: "an inductive invariant (TLAPS) for S2 and S5
— weeks; the only route to a claim that does not say 'in this world'".
This is that plan, written against `lean/formal/LeanP1.tla` as it is at
`99beb8d0` (md5 `94da7541`, the module the 2026-10-04 gate checked: 31
variables, 29 actions, a 27-field writer record, 14 claims).

Nothing below is done. No TLAPS is installed on the Mac (arm64; `tlapm`
absent, no OCaml, no Apalache) and the Linux box is powered off.

## 0. What a proof buys, and what it does not

**Buys.** Every world in `WORLDS-LeanP1.tsv` is two syncers, one or two
paths, two or three barriers, four generations. An inductive invariant
proved in TLAPS holds for every `Paths`, every `Writers`, every `MaxMint`,
every `Scopes`, every `Readers`, and with the budgets (`MaxUI`,
`MaxBarriers`, `MaxRestarts`, …) irrelevant: safety never depends on them.
That retires §4 item 1 ("nothing here rules out a failure that needs a
fourth writer or a third path") for the invariants it covers, and item 13
(a three-syncer world) with it.

**Does not buy.**
- It is still a claim about the MODEL. The bridge to the code stays the
  trace check (41/41 on `LeanP1.tla`) and the unit battery.
- It covers the SHIPPED constants only (every rule constant `TRUE`,
  `RenameAtomic`, `RetireAge`). A mutation world is not a theorem; it is
  the control that the theorem's hypothesis is load-bearing (§5).
- No liveness. `Prop_UISaveCompletes` stays with TLC at a bound
  (`LiveHoldsSmall`). TLAPS can prove liveness; it is not worth it here.
- `Inv_NoRegress` and `Inv_AckedNamed` reason about derivation history
  (`Derives`, `took`, `regressed`), which is the hard tail (M5). The plan
  front-loads what S2 and S5 actually need.

## 1. The target, in order

| # | claim | kind | promise | why this order |
|---|---|---|---|---|
| 1 | `Inv_OneHolder` | state | S5 | one conjunct, every action; the warm-up that proves the toolchain |
| 2 | `Prop_DeleteSettles`, `Prop_NarrowNeverDeletes` | action | S3, S4 | action properties need no new strengthening beyond a line each |
| 3 | `Inv_CitationsLive` | state | S2 | the "freshness" family of conjuncts (§3); the sweep, the reap, the verify |
| 4 | `Inv_OneName` | state | S4 | same family; one more conjunct about `snap` |
| 5 | `Inv_ShortcutSound`, `Inv_ReaderSound` | state | S3 | the claim that failed the first gate; a compact strengthening (§3, I8) |
| 6 | `Inv_ReaderFetches` | state | S2 (M1) | the retire age; needs "an uncited handle is never cited again" |
| 7 | `Prop_AgentWorkKept`, `Prop_ScopeRespected` | action | L-130/L-131 | cheap once TypeOK exists; no history |
| 8 | `Inv_AckedNamed`, `Prop_NoSilentRevert`, `Inv_NoRegress` | state/action | S1, S3 | need `Derives` restated (§2); the long tail |

Items 1–6 are the S2/S5 rows item 8 of the open list asks for, plus the
two claims the code's cheap path rests on. Items 7–8 are follow-ons.

## 2. What stands in the way, in the module as written

| obstacle | where | what to do | cost |
|---|---|---|---|
| `RECURSIVE Derives(_, _)` and `RECURSIVE Supersedes(_, _, _)` | `LeanP1.tla:190`, `:845` | TLAPS does not reason about recursively defined operators (it never unfolds them; older releases refused the module outright). Items 1–7 never mention them, so the proof module can leave them opaque — IF `tlapm` parses the module at all (M0 decides). For item 8: a ghost `anc[h]` (the set a handle derives from, written by the three minting steps `GPut`, `Edit`, `Upload`'s copy) and `Derives(k, h) == h = k \/ h \in anc[k] \/ Content(k) = Content(h)` — non-recursive, cheap for TLC | M5 |
| `CHOOSE q \in Paths : …` | `GRenameFinish`, `:258` | reachable only under `RenameAtomic = FALSE`. Conjunct `mv = Nil` makes the action's guard false; the proof never evaluates the CHOOSE | one line |
| no `ASSUME Nil \notin Handles`, no `ASSUME "none" \notin Writers` | the ASSUME block | TLC's model values make both true for free; TLAPS needs them stated (otherwise `doc[p] # Nil` says nothing about `doc[p]` being a pair, and `holder = s` can be `"none"`). Also `ASSUME Writers \cap {"off","on"} = {}` is NOT needed (strings are only compared to strings) | two lines in `LeanP1.tla`; the gate journal's fingerprint moves, the state space does not — confirm by `Holds1p3b` = 461,094,969 distinct on tlc-rs |
| `Next` conjoins `TookUpdate /\ RetUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>` to every ordinary step | `:759` | every per-action lemma takes the conjunction as its hypothesis; `RetUpdate` is where `retiring` grows, which item 6 needs. No change | — |
| the 27-field `Writer` record, 38 `EXCEPT`s, nested `![s].local[p]` | throughout | TLAPS's SMT backend handles records and `EXCEPT`, but an obligation that mentions the whole record is slow or times out. The proof decomposes per action and per conjunct, with `USE DEF` kept narrow. This is where the days go, not the mathematics | TypeOK ≈ a week |
| `Opt(Handles)` mixes a model value with pairs | `doc`, `tomb`, `base`, `gw`, `local`, … | fine for TLAPS (untyped set theory). It is the reason NOT to route through Apalache: Apalache's type system has no `Nil ∪ (Paths × Gens)` without a variant encoding of the whole module | — |
| `took`, `regressed`, `rdoc`/`rlag`, `upped`, `copies`, `orig` | ghosts and aux | all are STATE a step writes, so a predicate over them is a legitimate invariant. `upped` is the one the freshness family turns on | — |
| 29 actions × 31 variables | | the proof is `IndInv /\ [Next]_vars => IndInv'` as 29 lemmas plus Init. Nothing can shorten that count; it can only be made mechanical (M0's TypeOK skeleton is the template every later milestone copies) | — |

**Where the proof lives.** `lean/formal/proof/LeanP1Proof.tla`, `EXTENDS
LeanP1, TLAPS, FiniteSetTheorems, FunctionTheorems`, with the shipped
constants fixed by one named assumption:

```
ASSUME Shipped ==
  /\ CommitSurfacesForeign /\ CommitVerifiesUploads /\ SweepUnderLease
  /\ CollectorSparesCited /\ CommitRecordsDeleteOverride /\ DeleteWinsPreserved
  /\ ContentConverges /\ RecheckSkipped /\ CommitAdvanceGuarded /\ RetireAge
  /\ GatewayIgnoresLease /\ GatewayJudgesRead /\ GatewaySweepGrace /\ RenameAtomic
  /\ ConsumeHonorsScope /\ RescopeUnciteFirst /\ RescopeKeepsDirty
  /\ WidenKeepsLocal /\ UnlinkChecksBytes /\ ConsumeKeepsLeft /\ SyncKeepsLeft
  /\ ReaderRechecksOwed
```

`LeanP1.tla` itself gains only the two ASSUMEs (and, at M5, the ghost).
A separate module keeps the gate's module fingerprint stable between
milestones and keeps tlc-rs out of it (tlc-rs skips THEOREM/proof
syntax, but there is no reason to make it parse `TLAPS.tla`).

## 3. The method, and the strengthening already in view

The method is the one every inductive proof uses, with the house rule
applied: **a conjunct goes to TLC before it goes to TLAPS.** A candidate
conjunct is added as an `INVARIANT` to `LeanP1Holds1p3b` (one path, three
barriers, a restart, a sync) and `LeanP1AllHolds` (scope, rescope, reader,
failed fetch) on tlc-rs; one that is not even an invariant dies there in
minutes instead of in an obligation that will not close. Only a candidate
that holds in both worlds is worth a lemma. (TLC cannot check
INDUCTIVENESS at this size — `Init == IndInv` over `[Handles -> Opt(Handles)]`
is 9^8 states for `base` alone — so the inductiveness check IS the TLAPS
obligation. That is fine: a failed obligation names the action and the
conjunct.)

Candidate conjuncts, read off the actions (each to be TLC-checked first;
`On(s)` and `W == w[s]` as in the module):

```
I0  mv = Nil                                              \* RenameAtomic
I1  holder # "none" => w[holder].pc \in {"claimed","cased"} /\ On(holder)
    (with Inv_OneHolder itself)
I2  FRESHNESS
    live \subseteq upped /\ upped \subseteq minted
    retiring \cup aged \subseteq upped
    nextGen <= MaxMint + 1
    \A h \in minted : Gen(h) < nextGen \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies)
    every handle in doc, tomb, gw, rdoc, conflicts, acked, udel and in
      w[s].local / baseline / snap / inst / sHeld is in minted
I3  A SAVE IN FLIGHT
    gw[p] # Nil => gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p] \notin retiring \cup aged
                   /\ gw[p][1] = p /\ no tree holds gw[p] (local or baseline)
I4  AN UPLOAD BEFORE ITS CAS
    W.pc \in {"scanned","claimed"} /\ p \in W.upDone =>
      W.snap[p][1] = p /\ W.snap[p] \in upped /\ ~Cited(W.snap[p])
      /\ W.snap[p] \notin retiring \cup aged
      /\ no OTHER tree holds W.snap[p]
I5  VERIFIED
    W.pc = "claimed" /\ W.verified =>
      \A p \in (W.uploads \cap W.upDone) \ W.gone : W.snap[p] \in live
I6  CASED
    W.pc = "cased" => \A p \in W.deletes : W.inst[p] = Nil        \* DeleteSettles
I7  RESCOPE
    W.pc \in {"consumed","scanned","claimed","cased"} => W.sStage = "none"
    p \in W.unlinked /\ W.sStage = "none" => W.local[p] = W.baseline[p]
I8  THE CHEAP PATH'S RECORD (ShortcutSound)
    W.derived = seq /\ W.sStage = "none" =>
      \A p \in Paths : Held(s, p) /\ doc[p] # W.baseline[p] => p \in W.skipped
I9  A READER'S MEMO
    s \in Readers => W.memo <= seq /\ W.rnow <= seq
                     /\ (W.pc = "idle" /\ W.derived # 0 => W.memo <= W.derived)
I10 W.derived <= seq /\ W.synced <= seq
I11 THE RETIRE AGE (ReaderFetches)
    (retiring \cup aged) \cap {doc[p] : p \in Paths} = {}
    ~rlag => \A p : rdoc[p] # Nil => rdoc[p] \in live /\ (Cited(rdoc[p]) \/ rdoc[p] \in retiring)
```

Why each is there, in one line each:
- I8 is the whole of `Inv_ShortcutSound`: `CheapPath` says `derived = seq`
  and every skipped path dirty; I8 says every owed-looking path is
  skipped; so an owed path is dirty, and `Owed` says it is clean. The
  conjunct is read straight off `Consume`'s `skipped` and `Finish`'s
  `adv` guard, and it is the first thing to TLC-check: if it fails, the
  model's own explanation of the scan trigger is not the one in the
  comment.
- I9 turns `Inv_ReaderSound` into I8: `memo = seq` with `derived # 0`
  forces `derived = seq` because `seq` only grows.
- I3/I4 are why `Install` and `GCas` cite only a handle nothing else
  cites (`Inv_OneName`) and that is live (`Inv_CitationsLive`), and why
  `retiring` never meets a citation (the first line of I11): a step cites
  a fresh handle or moves a cited one, never re-cites an uncited one.
  That lemma — "once uncited, never cited again" — is the one the retire
  age rests on, and it falls out of I2–I4 as a state predicate.
- I5 is R4a's postcondition and the reason a sweep between `Verify` and
  `Install` cannot dangle a citation: only the holder sweeps or reaps,
  the holder's sweep spares its own `upDone`, and its reap needs `aged`,
  which I4 rules out.
- I7's second line is `Prop_NarrowNeverDeletes`: a path the rescope
  unlinked is clean, a clean path is never in `Scan`'s `absent`, so never
  in `deletes`. It may need weakening (a widen whose fetch fails leaves
  the path in `unlinked` and in scope; a later consume takes it clean —
  still clean). TLC will say.

Expect the list to change. The conjuncts that survive TLC are the proof's
table of contents; the ones TLC kills are findings about the model's
comments, and go in `FINDINGS.md` like any other.

**TLC-checked 2026-10-05** (`lean/formal/results/2026-10-05-leanp1-ind/`,
module `MCLeanP1Ind.tla` over `LeanP1.tla` unchanged): all twelve hold in
`Holds1p3b` (461,094,969 distinct, exactly the gate's count), `ScopeHolds`,
`ReaderHolds` and `FetchHolds`, with the positive control (I4 without its
`pc` guard) violated at depth 8. None had to be weakened. That makes them
invariants in those worlds, not yet inductive: the inductiveness obligation
is M0–M4's.

## 4. Milestones

Each milestone ends with a run of `tlapm` whose output is committed under
`lean/formal/results/<date>-tlaps/` (the obligations, the backends that
closed each, the wall-clock), the way every gate run is.

**M0 — toolchain, parse, TypeOK.** Install TLAPS on the Linux box
(x86_64; the Mac is arm64 and whether the current TLAPS release runs
there is a thing to try, not to plan on). Confirm `tlapm` parses
`LeanP1.tla` with its two `RECURSIVE`s present; if it refuses, the proof
module is built on a copy without them and the copy is tied to the
original by an exact distinct count on `Holds1p3b` (461,094,969). Add the
two ASSUMEs. Prove `Spec => []TypeOK`. Acceptance: every obligation
closed by a backend, none `OMITTED`. Effort: ~1 week, most of it the
`Consume`, `Install`, `RescopeSecond` and `Finish` obligations over the
record.

*M0 status, 2026-10-05* (`lean/formal/results/2026-10-05-tlaps-m0/`):
TLAPS installed on the box — tlapm 1.6.0-pre (build bfa9468, asset
2026-10-01) at `/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm`, with Zenon, Z3,
LS4 and Isabelle2025 bundled; the smoke proof closes 6/6. **tlapm refuses
`LeanP1.tla` as written**: `Failure("Expr.Anon: Recursive")` in module
elaboration, before any theorem, and a two-module control (the same
theorem with and without an unused `RECURSIVE` operator) pins it on the
keyword's presence alone. So the fallback is the path: the proof module
is built on a copy of `LeanP1.tla` whose `Derives` and `Supersedes` are
restated over the ghost `anc` that §2 planned for M5, and the copy is
tied to the original by the exact counts.

*The copy, 2026-10-06* (`lean/formal/LeanP1Anc.tla`, generated by
`results/2026-10-05-tlaps-m0-tie/make-anc.py`; the tie in that
directory's `RESULTS.txt`): tlapm accepts it, and under the gate's
`Holds1p3b` cfg byte for byte it gives 461,094,969 distinct,
1,922,766,889 generated, depth 46, every claim holding — exactly the
gate's numbers — and the three small worlds match too, with a TLC-only
module checking the restated `Derives`/`Supersedes` against the recursive
originals state by state. TypeOK is M0's remaining step.

*TypeOK, 2026-10-06* (`lean/formal/LeanP1Proof.tla`; the record in
`lean/formal/results/2026-10-06-tlaps-typeok/`): three things about
tlapm had to be found and worked around before any obligation closed,
each pinned by a one-screen control module in `micro/`:

1. **tlapm uses no unnamed ASSUME.** The proof module restates
   `LeanP1Anc.tla`'s six with names and adds the two the cfg gives TLC
   for free (`MaxCopies \in Nat`, `IsFiniteSet(Paths)`).
2. **The default backend ladder is Zenon then Isabelle; Z3 is never
   tried unless asked** (`--method smt,zenon`). The bundled Isabelle was
   also broken: its TLA+ heap records its parent heap at the GitHub
   build path; three symlinks on the box fix it, and it proves nothing
   Z3 cannot here.
3. **tlapm prepares every obligation against the whole module context,
   and `LeanP1.tla`'s shapes made that cost 9 s and 2.2 GB per
   obligation** -- `TRUE OBVIOUS` included -- so the first run of the
   module was OOM-killed at 31 GB with no verdict. Bisected to two
   things (`probes/`): hidden LET chains are expanded by substitution
   (`Install`, `RescopeSecond`), and every record EXCEPT in a hidden
   definition hands Z3 an axiom set, so under the actions' context any
   record goal timed out. Generator v3 therefore writes every writer
   step's new tree as an explicit 27-field record operator `<Step>W(..)`
   and `w' = [w EXCEPT ![s] = <Step>W(..)]`, with the LET names lifted
   to operators (`LeanP1Anc.tla` is now 213 lines further from the
   original, still generated and still tied by the exact counts:
   `RESULTS.txt`). Under it a record obligation closes in 7-12 s.
   Two rules for every later milestone: never write a tree update as an
   operator over `[w[s] EXCEPT ..]` (Z3 fails outright), and Z3 cannot
   take a record EXCEPT membership past about seven clauses.

The proof's shape: `IndTypeOK` = TypeOK + the three untyped upload
ghosts + `mv = Nil` (I0) + `nextGen > Seed` + `Minted` (every handle the
document, a save in flight or a tree names is minted; an upload's
snapshot is minted). One lemma per step, all the same shape; a step
cites only the facts it needs, since an obligation carrying the whole
invariant expanded puts Z3 past its limit on goals it closes in seconds
otherwise. Also found: `TypeOK` as shipped omits `upped`, `copies` and
`orig`; the proof's invariant types them (whether to fold that into
`LeanP1.tla` is flint-46's call).

*M0 result, 2026-10-06* (`results/2026-10-06-tlaps-typeok/RESULTS.txt`;
the record run `out/run13-record.out`): all 1,917 obligations of
`LeanP1Proof.tla` closed, none failed, none `OMITTED` -- 1,377 trivial,
540 by Z3 (the one temporal step by LS4), the slowest 6.3 s of a 60 s
budget, 445 s in all on the box from an erased fingerprint cache. The
proof text is generated (`gen-proof.py` there, byte for byte) and
reruns with `run-record.sh`. The v3 copy's tie is exact on all four
worlds (`Holds1p3b` 461,094,969 distinct, 1,922,766,889 generated,
depth 46, every claim holding). **M0 is met.**

**M1 — `Inv_OneHolder`, `Prop_DeleteSettles`, `Prop_NarrowNeverDeletes`.**
I0, I1, I6, I7. Acceptance: three theorems closed; the control of §5 run
for `Inv_OneHolder` (drop `Claim`'s `holder = "none"` guard in a scratch
copy: the `Claim` obligation must fail). Effort: 2 days.

*M1 result, 2026-10-07* (`lean/formal/results/2026-10-07-tlaps-m1/`; the
record run `out/run17-record.out`): the three theorems are closed in
`LeanP1Proof.tla` -- `Spec => []Inv_OneHolder`, `Spec =>
Prop_DeleteSettles`, `Spec => Prop_NarrowNeverDeletes` -- through
`IndM1`, which is `IndTypeOK` with I1, I6, I7 and six more facts read off
the actions while writing the step lemmas: the scan's two sets are
disjoint and the verify's withheld set is a subset of the uploads; from the
scan to the finish no published or deleted path is one a rescope unlinked;
a writer that is off is pristine; and three about a rescope in flight --
an unlinked path in `saved`/`mid` is clean or uncited by the first half
(dropped, not kept, baseline gone, bytes recorded in `sHeld`), between the
halves every dropped-and-not-kept path is uncited, and a reader's pull
never spans the halves. All six went to TLC first (house rule): they hold
in the four gate-derived worlds with the gate's exact counts, and the
control for the rescope conjunct was VACUOUS there -- its second case
needs three rescopes, a removal and a save -- so a three-rescope world was
added, where the control fires at depth 14 and the conjuncts hold. The
record: 4,304 obligations (M0's 1,917 plus 2,387), all closed, none
omitted, the slowest 6.4 s of 60, 906 s in all from an erased cache. The
section-5 controls each fail exactly the named obligation: `Claim` without
its lease guard fails `Inv_OneHolder'` at `Claim`; `Shipped` without
`DeleteWinsPreserved` fails the delete step of `Install`'s `Cased'`;
without `RescopeUnciteFirst` the two halves' tree-shape reads fail. Both
rules' TLC worlds fire on the same two claims in the gate. Effort: one
day, not two: the per-conjunct generic lemmas (one hypothesis on the new
tree each) made every step lemma a list of field reads.

**M2 — `Inv_CitationsLive`, `Inv_OneName`.** I2–I5. Acceptance: both
closed; controls: `CommitVerifiesUploads` and `SweepUnderLease` and
`GatewaySweepGrace` each removed from `Shipped` in turn, each with a named
obligation that then fails (`Install`, `Sweep`, `Sweep`), matching the
three mutation worlds `VerifyOff`, `SweepFree`, `SweepNoGrace` that fire
in TLC. Effort: 1–1.5 weeks.

*M2 result, 2026-10-07* (`lean/formal/results/2026-10-07-tlaps-m2/`; the
record run `out/run23-record.out`): both theorems are closed in
`LeanP1Proof.tla` -- `Spec => []Inv_CitationsLive`, `Spec =>
[]Inv_OneName` -- through `IndM2`, which is `IndM1` with eight conjuncts:
freshness (I2: `live` within `upped` within `minted`, the retiring and
aged sets within `upped`, every minted generation below `nextGen` or a
copy's), every snapshot minted, a save in flight (I3: live, uncited, not
retiring, named by its path, in no tree), a tree entry never PUT is
private to its minter at its path, an upload's snapshot before its PUT is
such an entry, an upload PUT but not installed (I4: uncited, not retiring,
not in flight, private), a verified upload not withheld is live (I5), and
a scanned writer is unverified. The first draft of the privacy conjunct
("every dirty tree entry is its writer's own uncited handle") was refuted
on paper before TLC: after an `Install` the uploaded entries are cited and
still dirty until `Finish`, and a `Restart` keeps them; stating privacy
over `upped` is what holds. All eight went to TLC first: exact counts in
the four gate-derived worlds and in M1's three-rescope world, two
controls firing (the citation clause admitted past the CAS, depth 8;
privacy stated over tree entries, depth 2); the big world's row is in
`SUMMARY.txt` there. The record: 8,659 obligations (M0's 1,917, M1's
2,387, M2's 4,355), all closed, none omitted, the slowest 6.2 s of 60,
1,829 s from an erased cache. The section-5 controls are four, not three,
and each fails exactly the predicted obligations: `SweepUnderLease` and
`GatewaySweepGrace` each fail `Sweep`'s events step (the sweep spares
the holder's PUTs; a save in flight stays live); `CommitVerifiesUploads`
fails `Verify`'s `Verified'` -- the plan above named `Install`, but the
rule is used where `verified` is set, `Install` only cites; and
`RetireAge`, listed under M4, is load-bearing here too (it is what keeps
`Collect` from shrinking `live` and gives every step its retiring-set
events), failing 26 obligations. The three named rules' TLC worlds fire on
`Inv_CitationsLive` in the gate. Effort: one day, not 1–1.5 weeks; the
cost was in finding the invariant, and tlapm's two traps of the day
(priming an application whose argument is primed crashes it; a generated
`\A p` captured a step's own `p`) are in `NOTES.txt`.

**M3 — `Inv_ShortcutSound`, `Inv_ReaderSound`.** I8–I10. Acceptance: both
closed; controls `RecheckSkipped`, `CommitAdvanceGuarded`, `ConsumeKeepsLeft`,
`SyncKeepsLeft`, `ReaderRechecksOwed` (five mutation worlds fire in TLC;
five named obligations must fail). Effort: 3–5 days. This is the milestone
with the best ratio: the claim the code's one-GET fast path rests on,
proved for any number of writers.

*M3 result, 2026-10-07* (`lean/formal/results/2026-10-07-tlaps-m3/`; the
record run `out/runrecord.out`): both theorems are closed in
`LeanP1Proof.tla` -- `Spec => []Inv_ShortcutSound`, `Spec =>
[]Inv_ReaderSound` -- through `IndM3`, which is `IndM2` with six
conjuncts: the plan's I8 (while `derived = seq` and no rescope is in
flight, every held path where the document and the baseline differ is
skipped), I9 and I10 for every tree, `seq >= 1` (so a record of 0 never
matches), and three facts a writer carries from its CAS to its finish,
which the plan did not have: the install's own paths hold the snapshot;
with `adv`, every other held path where the INSTALLED document differs
from the baseline is skipped (this is where `CommitAdvanceGuarded` is
used); and a record still current after the CAS means the CAS moved
nothing. Both claims are lemmas off the invariant, not conjuncts. All six
went to TLC first: exact counts in the four gate-derived worlds and the
three-rescope world, two controls firing (the `adv` guard dropped, depth 8;
the rescope guard dropped from I8, depth 4). The record: 10,850
obligations (M0-M2's 8,659 and M3's 2,191), all closed, none omitted, the
slowest 6.6 s of 60, 2,502 s from an erased cache. The section-5 controls
are six, not five: each of the plan's five fails exactly at the step that
uses it -- `RecheckSkipped` and `ReaderRechecksOwed` at the two claim
lemmas, `CommitAdvanceGuarded` at `Install`'s `adv` fact,
`ConsumeKeepsLeft` at `Consume`'s record, `SyncKeepsLeft` at `Sync`'s and
`RPullSync`'s -- and `ConsumeHonorsScope`, which the claims also need (an
owed path is held), fails both claim lemmas; its gate world fires only on
scope respect, so a TLC run of its constants against the claim was added
(the shortcut claim fails at depth 2). Effort: one day.

**M4 — `Inv_ReaderFetches`.** I11 and the never-re-cited lemma. Control:
`RetireAge` (world `ReaderLoses`). Effort: 2–3 days.

*M4 result, 2026-10-08* (`lean/formal/results/2026-10-07-tlaps-m4/`; the
record run `out/runrecord.out`): `Spec => []Inv_ReaderFetches` is closed in
`LeanP1Proof.tla` through `IndM4`, which is `IndM3` with two conjuncts: the
never-re-cited lemma as a state predicate (a cited handle is neither
retiring nor aged) and the plan's I11 (while the reader is not lagging,
every handle it loaded is live, not aged, and cited or retiring). Because
every gateway and writer step logs exactly what it stops citing, one lemma
covers them all from two facts per step -- what the document may newly
cite, and what `live` may lose; only four steps move the document and two
shrink `live`. Both conjuncts went to TLC first: exact counts in the
gate-derived worlds and in LiveHoldsSmall, the one small world where the
retire age elapses (10,676,334 states); both controls (the retiring case
dropped, depth 3; the lag guard dropped, depth 4) and the rule world fire.
The record: 11,746 obligations (M0-M3's 10,850 and M4's 896), all closed,
none omitted, the slowest 6.2 s of 60, 2,679 s from an erased cache. The
`RetireAge` control fails 53 obligations, exactly the predicted set: M2's
26 (the rule is one conjunct of `Shipped`, which M2 also cites) and M4's
27 -- every frame step's retire log, the sweep's and the collector's
`live` facts. Effort: under a day.

**M5 — `Inv_AckedNamed`, `Prop_NoSilentRevert`, `Inv_NoRegress`.** The
ghost `anc` goes into `LeanP1.tla`; `Derives` and `Supersedes` are
restated over it; TLC ties the edit to the original by (a) the exact
distinct count on `Holds1p3b` and `AllHolds`, (b) an `INSTANCE` refinement
of the original module by the ghosted one (tlc-rs checks refinement
properties; `LeanRefine` is the precedent), (c) a one-run invariant
`Derives(k,h) <=> DerivesGhost(k,h)` over all minted pairs. The
strengthening for `Inv_AckedNamed` is the real work: the `Sweep` obligation
demands that an acked handle which is live, uncited, unpreserved and not
retiring is already named by a non-live disjunct (a tombstone or citation
derived from it, `took`, `udel`, a later ack), and that is a new conjunct
per disjunct. Effort: 2–3 weeks, uncertain; may end at "`Inv_AckedNamed`
on one path" with the multi-path case left to TLC. Decide whether to start
it after M4 reports.

*M5 started, 2026-10-08* (`lean/formal/results/2026-10-07-tlaps-m5/`,
NOTES.txt). Part A is proved: `Spec => []IndM5` with `IndM5 = IndM4 /\
Hist`. `Hist` says `base`, `anc` and `orig` are written only at the handle
a step mints, so `Derives` and `Content` between minted handles never move
across a step (`DerivesKeep`). Every M5 claim compares `Derives` before
and after a step, so the rest of M5 builds on this. 1,347 obligations,
all closed, M5's section only, on M4's fingerprints; the record run and a
control are still to do. NO M5 CLAIM IS PROVED, and two cannot be as stated.
Every gate world allows at most one gateway removal (`MaxRemovals <= 1`);
with three -- a rename, a rename back, a delete -- TLC finds, with one
writer at depth 13: `Inv_NoRegress` fails (the writer re-creates a path
over a renamed fork, the gateway deletes it and renames the fork back, and
the consume steps the tree back with no conflict naming its baseline), and
`Inv_AckedNamed` fails once the retire age lets the reaper run (the
rename's ack at the second path is named only at that path, and the record
that names the version -- the acknowledged delete -- is at the first).
Neither is silent: a record names the version each time; the claims
account per path more narrowly than the protocol's records. Restating
them is `LeanP1.tla`'s, flint-46's call. `Prop_NoSilentRevert` holds there
without the retire age (7,469,230 states) and with it (78,039,582).

Total for M0–M4: 4–5 weeks of one person's time, box time negligible. M5
on top: 2–3 more, with the least certainty.

## 5. Controls: what makes a proof evidence

The house rules apply to a proof as they do to a run:

1. **A theorem whose hypothesis is not load-bearing proves nothing.** For
   each rule constant a theorem's proof uses, the same proof with that
   constant dropped from `Shipped` must FAIL at a named obligation, and the
   TLC mutation world for that rule must FIRE (it does: 19/19 on
   2026-10-04). Both halves are recorded with the milestone. A proof that
   still closes without the rule means the rule is not what holds the
   claim up — which is itself a finding about `PROTOCOL.md`'s "holds up"
   column.
2. **The module proved is the module checked.** `LeanP1Proof.tla` EXTENDS
   `LeanP1.tla`; the results record carries the md5 of both. A copy (M0's
   fallback, M5's ghost) is tied to the original by an exact distinct
   count, never by reading.
3. **No `OMITTED`, no `BY ... PROOF OMITTED` in a milestone's acceptance.**
   An omitted step is a conjecture, and the record says which theorems
   still rest on one, if any.
4. **A conjunct TLC refutes is written down**, with the trace, before it
   is weakened. The weakening is a claim about the model that the first
   draft got wrong, and SAFETY.md's history says the first draft was
   sometimes right.

## 6. Housekeeping

- `lean/formal` is flint-46's area; the two ASSUMEs and the M5 ghost are
  edits to `LeanP1.tla` and want their say first. Nothing else touches
  the gate's inputs. NEVER edit `LeanP1.tla` while a run is on.
- `tlapm` writes a fingerprint cache (`.tlacache/`): gitignore it; the
  results directory carries the output, not the cache.
- Proof checking is minutes to hours, not CI material. A
  `lean/formal/proof/check-proofs.sh` runs it on the box on demand, like
  `check.sh`, and asserts the obligation count the way the gate asserts
  its run count.
- When M3 closes, SAFETY.md §3.2 gains a row per proved claim ("holds for
  every `Paths`, `Writers`, `MaxMint` under the shipped constants; proof
  `results/<date>-tlaps/`"), §4 item 1 is reworded to name what is still
  bounded, and §5 item 8 is split into the done part and the M5 part.

## 7. Risks

| risk | likelihood | what it costs | mitigation |
|---|---|---|---|
| `tlapm` refuses the module on `RECURSIVE` | medium | a proof-copy of the module, tied by distinct count | M0's first hour decides it |
| the record's obligations time out in Zenon/SMT | high for `Consume`, `Install`, `RescopeSecond` | days of splitting steps | per-field lemmas; `USE DEF` narrow; Isabelle as the slow backend of last resort |
| a conjunct in §3 is not an invariant | certain for one or two of them | a finding, then a weaker conjunct | TLC first, always |
| `Inv_AckedNamed` needs a strengthening nobody can state over state | medium | M5 ends at one path | `took` and `udel` exist precisely because the state had to carry the memory; the model has already paid most of this price |
| TLAPS on the Mac (arm64) | unknown | none: the box is the plan | try it once; do not depend on it |
| the box is down and the Mac is the only machine | now | M0 waits | M0's TLC pre-checks of §3's conjuncts need only tlc-rs on the Mac and can start today |
