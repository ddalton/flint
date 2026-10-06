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

**M1 — `Inv_OneHolder`, `Prop_DeleteSettles`, `Prop_NarrowNeverDeletes`.**
I0, I1, I6, I7. Acceptance: three theorems closed; the control of §5 run
for `Inv_OneHolder` (drop `Claim`'s `holder = "none"` guard in a scratch
copy: the `Claim` obligation must fail). Effort: 2 days.

**M2 — `Inv_CitationsLive`, `Inv_OneName`.** I2–I5. Acceptance: both
closed; controls: `CommitVerifiesUploads` and `SweepUnderLease` and
`GatewaySweepGrace` each removed from `Shipped` in turn, each with a named
obligation that then fails (`Install`, `Sweep`, `Sweep`), matching the
three mutation worlds `VerifyOff`, `SweepFree`, `SweepNoGrace` that fire
in TLC. Effort: 1–1.5 weeks.

**M3 — `Inv_ShortcutSound`, `Inv_ReaderSound`.** I8–I10. Acceptance: both
closed; controls `RecheckSkipped`, `CommitAdvanceGuarded`, `ConsumeKeepsLeft`,
`SyncKeepsLeft`, `ReaderRechecksOwed` (five mutation worlds fire in TLC;
five named obligations must fail). Effort: 3–5 days. This is the milestone
with the best ratio: the claim the code's one-GET fast path rests on,
proved for any number of writers.

**M4 — `Inv_ReaderFetches`.** I11 and the never-re-cited lemma. Control:
`RetireAge` (world `ReaderLoses`). Effort: 2–3 days.

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
