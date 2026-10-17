# ForgeSync: an inductive proof with TLAPS — the plan (2026-10-17)

**Status: DRAFT, not started.** Nothing below has been run. It was
written against `formal/ForgeSync.tla` at `d6aa9f10` (md5 `3f296d75`,
1,872 lines). That is the module the 2026-10-05 code worlds checked: 41
variables, 34 per-syncer actions plus 3 per-push actions and the client's
hangup, and 50 constants, most of them rule and mutation switches. It follows the LeanP1 plan
(`docs/plans/lean-tlaps-inductive-proof-plan.md`), whose proof is DONE:
19,235 obligations. The method, the controls and the traps it records all
carry over. Where this plan and that one disagree on a tlapm fact, the
Lean record is right, because it was measured.

The subject is the protocol between the git server and S3:
hook → batch → pack upload → ONE snapshot CAS → update-ref → ack, along
with the lease and its fencing, the claim's rotation and restore, the
fold, the sweep, the kept-set reclaim and (M5) ref rewinds.

## 0. What a proof buys, and what it does not

**Buys.** Every ForgeSync world TLC has checked is small:

- two syncers, two pushes, one fold;
- `MaxCrashes = 1`, `MaxClaims = 3`, `MaxRenews = 3`, `Misses = 2`;
- at most one rewind and one re-push.

`ForgeSyncCode` is 30,277,694 distinct states. The rewind worlds are
1,535,961,438 each and take hours, so they are not in CI. An inductive
invariant proved in TLAPS holds for every `Syncers`, `Pushes` and
`FoldIds`, with the budgets irrelevant: any number of crashes, claims,
renewals, folds, rewinds and re-pushes. It turns three findings that
currently hold "at these bounds" into findings with no bound, or refutes
them:

1. **The rewind question** (`formal/results/2026-10-05-forgesync-rewind/SUMMARY.txt`).
   The restore fix (`ce2a1af6`, `RestoreDropsNamedRetention`) and its
   batch-side twin, which the code lacks (`NamingUnretains`; `batch.rs`
   `needed_packs` pass 3 leaves a re-named pack on `retained`), change NO
   reachable state at one rewind and one re-push. Both stay "defence in
   depth" because nobody knows whether two rewinds or three pushes reach
   the loss. M5 settles it either way:
   - The proof closes under the code's rules without either one. Then
     the batch twin is never needed.
   - The proof needs one of them, at a named obligation. That obligation
     names the state, and TLC at the bound it implies gives the trace that
     justifies mirroring `ce2a1af6` in `batch.rs`.
2. **The kept-set reclaim.** With disjoint packs it never commits. It
   acts only under `PacksOverlap` (155,567,536 distinct). A proof covers
   every overlap pattern, not just "every pack after the first carries
   the first push's object".
3. **Fencing at more than two incarnations.** A straggler never lands
   after its successor restored (`Inv_NoStragglerLandAfterRestore`). TLC
   has checked that only for two syncers and three claims. The two
   counterexamples in the module header were both about the ORDER of
   claim, rotation and restore. A third incarnation is exactly where such
   an ordering could fail and two cannot show it.

**Does not buy.**

- **It is a claim about the MODEL.** Unlike Lean, forge has no trace
  check tying `ForgeSync.tla` to the running syncer. The bridge is the
  header's list of facts "each from the implementation", the runbw/runbx
  drills and the unit tests. A trace check is a separate item (§7). It is
  arguably worth more than M3–M5 and should not be skipped because a
  proof exists.
- **The two axioms stay axioms.** `PollsNoFasterThanHeartbeat` (a
  challenger's poll of a live holder follows that holder's heartbeat) and
  `GraceOutlivesUpload` (`orphan_grace_secs` outlives the longest upload)
  are timing facts TLA+ cannot discharge. They appear in the theorem's
  hypothesis by name, and the proof shows which claims rest on them (§5).
- **S3 and git are abstracted.** The proof assumes:
  - S3 is strongly consistent, `If-Match` and `If-None-Match` are atomic,
    and an incomplete multipart upload is invisible;
  - git is linear, one ref, push id = commit id = pack id, with a pack
    invisible until its index lands.

  None of these gets stronger by being proved.
- **Only the code's combination (§2).** A mutation world is not a theorem.
  It is the control showing that a rule in the hypothesis is load-bearing.
- **No liveness.** `DeadHolderWatchResolves` and `SentPushResolves` stay
  with TLC (`ForgeSyncCodeLive`, 1,500,985 distinct, which finishes).
  Lean's L1 showed TLAPS can do liveness; here there is no world TLC
  fails to finish, so there is no reason to.

## 1. The target, in order

| # | claim | kind | promise | why this order |
|---|---|---|---|---|
| 0 | `TypeOK` | state | — | the skeleton every later step copies |
| 1 | `Inv_NoSkipOverMovement`, `Inv_NoRenewOverWedge`, `Inv_ProofIsOfTheBucket`, `Inv_ProofNeverRestsOnRetention` | witness | sensor honesty; F2 | each flag is set by one or two actions on a local condition: the warm-up |
| 2 | `Inv_NamedIsUploaded`, `Inv_LandedPackComplete`, `Inv_NoUnrestorable` | state + witness | the bucket is always restorable | the storage family: upload before name, index gate, sweep under grace, fold coverage |
| 3 | `Inv_AckedIsDurable` | state | **told ok ⇒ in S3** | the headline. On top of 2 it needs ack-after-CAS-and-refs and "a CAS never unnames a held push" |
| 4 | `Inv_NoStragglerLandAfterRestore` | witness | fencing | tokens, epochs, rotation before restore; the strengthening is the least obvious (§3) |
| 5 | 2–4 with `MaxRewinds`, `MaxResends` unbounded | state | the rewind question (§0 item 1) | the payoff; needs `anc`/`par`/`rewound`, already in the module as state |

**Out:** `Inv_NamedIsLanded`. It is FALSE under `PacksOverlap` and under
rewinds, and `gen-forgesync-code.sh` already drops it from those worlds
as "not a durability claim". Proving it in the disjoint, rewind-free
world would prove an idealisation. Record it as out, with that reason.

## 2. What stands in the way, in the module as written

| obstacle | where | what to do | cost |
|---|---|---|---|
| 50 constants, most switching rules and mutations | `:172-230` | fix the code's combination with one named assumption (below). Every mutation branch's guard becomes FALSE and drops out of the proof. This is the biggest simplification on offer | one ASSUME |
| witness flags (`stragglerLand`, `unrestorable`, `skipOverMovement`, `renewOverWedge`, `provedOffBucket`, `provedOverRetained`) | `:599, :683, :689, :992, :1406, :1499, :1555, :1564, :1663` | `~flag` is inductive only if the condition that sets it is false whenever its action fires. The real claim is a STATE invariant implying that condition false. For example, for `stragglerLand`: any syncer whose CAS could land holds `belief[s].etag = snap.etag`, and that is impossible once a successor restored. Finding each one is the work of M1, M2 and M4 | the milestones |
| one token counter `nextTok` for tokens and etags | throughout | freshness conjunct: every token or etag in the state is `< nextTok`. That is standard, and it is what makes "a rotated etag is new" provable | one conjunct |
| no `ASSUME` at all | the module | TLC's model values give these for free; TLAPS needs them named (the Lean M0 finding: tlapm uses no unnamed ASSUME): `NoSyncer \notin Syncers`, `0 \notin Pushes \cup FoldIds`, `Pushes \cap FoldIds = {}`, `Misses \in Nat`, the budgets `\in Nat`, `IsFiniteSet` where `Cardinality` is used (`:1096`, `:1456`) | a few lines |
| `FirstPush == CHOOSE p \in Pushes : TRUE` | `:297` | used only under `PacksOverlap`. Fine for TLAPS; needs `Pushes # {}` | one line |
| `View`, `Rank`, `Sym`, `SymOverlap` | `:1733-1777` | TLC-only; the proof never mentions them | — |
| no `RECURSIVE` | — | unlike LeanP1, tlapm should parse the module as written. M0's first hour confirms it | — |
| records with nested `EXCEPT` (`batch[s]`, `fold[s]` with 6 fields, `belief[s]`, `cell`, `snap`) | throughout | as in Lean: per-action lemmas, narrow `USE DEF`, per-field facts. This is where the days go | TypeOK ≈ days |

**The code's combination**, from `formal/ForgeSyncCode.cfg`. It is the
shipped baseline plus `NameNeeded`, `ReclaimKeptSet` and
`RestoreDropsNamedRetention`:

```
ASSUME CodeRules ==
  /\ IdxGate /\ TickOnHash /\ RotateOnTakeover /\ AckAfterCas
  /\ PacksBeforeCas /\ SweepAtClaim /\ GraceOutlivesUpload
  /\ PollsNoFasterThanHeartbeat
  /\ ReclaimAtRestore /\ ReclaimBySet /\ RestoreLosesQueue /\ ReclaimUnlinks
  /\ NameAcceptedSet /\ NameNeeded /\ ReclaimKeptSet /\ RestoreDropsNamedRetention
  /\ ~NamingUnretains                         \* the code: batch.rs pass 3
  /\ ~FoldCasBeforeUpload /\ ~FoldCasFromDisk /\ ~FoldInputsAfterStart
  /\ ~FoldCommitMidBatch /\ ~SweepDuringFold /\ ~FoldTicksBatchSensor
  /\ ~FoldNoRenew /\ ~FoldNoCoverageCheck /\ ~FoldSupersedesArrivals
  /\ ~FoldReachableCoverage /\ ~ReclaimWhileServing /\ ~ForgetPushPack
  /\ ~ProveFromDisk /\ ~ListingKeepsRetained /\ ~NeededNamesQueued
  /\ ~NeededTrustsDisk /\ ~NeededSkipsRetained
  /\ ~KeptSetAnyDrop /\ ~KeptSetVsOriginal /\ ~KeptSetWhileServing
  /\ ~KeptSetNoRenew
```

`PacksOverlap` is NOT fixed. The proof is over both values, which covers
the disjoint and the overlap worlds at once. The milestone that first
uses it says so. Each conjunct above is cross-checked against the cfg by
a script, not by reading.

**Where the proof lives.** Beside the model in `formal/`, where Lean
keeps `lean/formal/LeanP1Proof.tla`: one module per milestone
(`ForgeSyncProofM0.tla`, `…M1.tla`, …), each EXTENDING the last, with
`EXTENDS ForgeSync, TLAPS, FiniteSetTheorems, FunctionTheorems` at the
root. Splitting from the start applies the Lean
lesson: the single-file record run reached 28.9 GiB on the 30 GiB box,
and split it peaked at 9.6 GB. `ForgeSync.tla` itself gains only the
named ASSUMEs, which must leave `ForgeSyncCode` at exactly 30,277,694
distinct.

## 3. The method, and the strengthening in view

**House rule: a conjunct goes to TLC before it goes to TLAPS.** A
candidate conjunct must hold, as an invariant, in:

- `ForgeSyncCode`;
- `ForgeSyncCodeOverlap`;
- for M5, `ForgeSyncCodeRewind`.

That means tla-mc by default, with TLC as the reference. It must also
FAIL in the mutation world of the rule it encodes. Only then is it worth
an obligation.

Candidates already visible from the module's comments. These are
hypotheses, not findings:

- **S1 (upload before name).** A pack in `snap.packs` or in any
  `belief[s].packs` whose CAS might land is in `packObj ∩ idxObj`. The
  fold side: `fold[s].stage ∈ {"uploaded","renewed","keptRenewed"} ⇒
  fold[s].id ∈ packObj ∩ idxObj`.
- **S2 (index gate).** `localPacks[s] ∩ migrating[s] = {}`. A listed
  pack has its index.
- **S3 (sweep spares the in-flight).** Under `GraceOutlivesUpload`, no
  `SweepDelete` takes a pack another incarnation uploaded and has not
  yet named. The module encodes the grace as a guard, so this is a
  conjunct about which uploads are "inside the grace".
- **S4 (coverage).** Every push in `snap.history` is in `holds[q]` for
  some named `q`. A fold's commit and the kept-set drop each preserve
  that because they remove only what the kept set covers (the
  `KeptSetVsOriginal` mutation shows "what stays kept" matters, not the
  original set).
- **A1 (ack).** `pushState[p] = "acked"` implies that a CAS naming `p`'s
  holder landed and refs moved. Together with S4 and S1 that is
  `Durable(p)`.
- **F1 (fencing).** After `SuccessorRestored`, the etag every older
  incarnation believes is not `snap.etag`. Rotation mints a fresh etag
  before any restore (`RotateOnTakeover`), and freshness makes it unequal
  to every belief. The hard part: an incarnation that died mid-rotation
  (the second counterexample in the header) must still be covered. Expect
  F1 to need a conjunct relating `cell.ep`, `lease[s].ep` and the stage
  of every claim in flight.

## 4. Milestones

Each milestone ends with a tlapm run on the Linux box. Its output is
committed under `formal/results/<date>-forgesync-tlaps-m<N>/` with:

- the md5 of `ForgeSync.tla` and of every proof module;
- the obligation count, which backends closed them, and the wall clock.

The Lean milestones M0–M4 took three calendar days (10-05 to 10-07); the
history tail (M5) took a week more. Forge's module is about the same size
(41 vs 31 variables, 38 vs 29 actions), so M0–M4 should take about a
week and M5 is open-ended.

**M0: parse, ASSUMEs, TypeOK.**
- tlapm 1.6.0-pre is already installed on the box (`/mnt/nvme/tlaps/`),
  along with the three Isabelle heap symlinks Lean needed.
- Confirm tlapm parses `ForgeSync.tla`.
- Add the named ASSUMEs, then re-run `ForgeSyncCode` and require exactly
  30,277,694 distinct.
- Prove `Spec => []TypeOK` under `CodeRules`.
- Acceptance: no failed or `OMITTED` obligation, and no
  `@!!status:omitted`. Run with `--method smt,zenon` (Z3 is never tried
  unless asked).

**M1: the witnesses (warm-up).**
- The goals: `Inv_NoSkipOverMovement` and `Inv_NoRenewOverWedge` (the
  renewer's sensor), plus `Inv_ProofIsOfTheBucket` and
  `Inv_ProofNeverRestsOnRetention` (`Checkpoint`).
- Each flag has one or two writers, so each claim is a one-conjunct
  strengthening about `sensorMoved`/`realMoved` or `ProofScope`.
- Controls: `TickOnHash`, `FoldTicksBatchSensor`, `ProveFromDisk`.

**M2: the storage family.**
- The goals: `Inv_NamedIsUploaded`, `Inv_LandedPackComplete` and
  `Inv_NoUnrestorable`, through conjuncts S1–S4.
- `PacksOverlap` enters here, with the kept-set drop.
- Controls: `PacksBeforeCas`, `IdxGate`, `GraceOutlivesUpload`, and the
  `ReclaimKeptSet` coverage test.

**M3: `Inv_AckedIsDurable`.**
- A1 on top of M2.
- Controls: `AckAfterCas` (EarlyAck) and `FoldInputsAfterStart`.

**M4: fencing (`Inv_NoStragglerLandAfterRestore`).**
- F1. This is the likeliest milestone to need several TLC rounds before
  the conjunct is right.
- Controls: `RotateOnTakeover` (NoRotate) and `KeptSetNoRenew`.

**M5: rewinds.**
- Re-prove M2–M4 with `MaxRewinds`, `MaxResends \in Nat` unconstrained.
  `Inv_AckedIsDurable` already excuses `rewound` pushes.
- **Then the question run:** the same proof with
  `RestoreDropsNamedRetention` removed from `CodeRules`.
  - If it still closes, the restore fix is not load-bearing at ANY bound,
    and the batch twin is unneeded.
  - If it fails, record the obligation, then build the state it names
    and run TLC at the bound it implies, expecting a trace.
- Either outcome goes into `forge/` docs and settles the `batch.rs`
  pass-3 question that has been "not changed, unreachable at these
  bounds" since 10-05.

## 5. Controls: what makes a proof evidence

These are the Lean rules, unchanged:

1. **The hypothesis must be load-bearing.**
   - For each rule a theorem's proof uses, drop it from `CodeRules`; the
     proof must FAIL at a named obligation.
   - The TLC mutation world for that rule must FIRE. Each one already
     exists (`formal/ForgeSync*.cfg`; 22 fire in the 09-28 gate).
   - A rule whose removal changes nothing is a finding about the
     protocol: the code carries a rule nothing needs.
2. **The module proved is the module checked.** Proof modules EXTEND
   `ForgeSync.tla`, and every results record carries the md5 of every
   input, the model included. If a copy is ever needed, it is tied to the
   original by an exact distinct count, never by reading.
3. **No `OMITTED` in an acceptance.** An omitted step is a conjecture.
4. **A conjunct TLC refutes is written down**, with its trace, before it
   is weakened.

## 6. Housekeeping

- **The box is SHUT DOWN** (since 2026-10-16 23:35Z, user pause). M0 needs
  it powered on, so ask first.
  - Until then, the TLC pre-checks of §3's candidates can run on the Mac
    with tla-mc on `ForgeSyncCode` (30M states: 17 s on the 192-core
    EC2 run, so minutes on the Mac).
  - The overlap and rewind worlds are box-only.
- Run tlapm under `systemd-run --user -p MemoryMax=...`. tlapm runs z3
  under `sh`, so check the grandparent before killing a "stray" z3.
- `.tlacache/` is gitignored; the results carry the output.
- Commit by pathspec. Results logs need `git add -f`.
- When M3 closes, the forge design docs gain a row per proved claim:
  "holds for every `Syncers`, `Pushes`, `FoldIds` and budget under the
  code's rules; proof `formal/results/<date>-forgesync-tlaps-m3/`".

## 7. Risks, and the alternative worth weighing

| risk | likelihood | what it costs | mitigation |
|---|---|---|---|
| F1 (fencing) has no compact state invariant | medium | M4 stalls | the claim stage is already state (`st`, `lease`, `cell.ep`); add a ghost "restored epoch" if needed, and tie it by exact count |
| fold and kept-set obligations time out | high | days of step splitting | per-field lemmas, narrow `USE DEF`, split modules from M0 |
| a §3 conjunct is not an invariant | certain for some | a finding, then a weaker conjunct | TLC first, always |
| `PacksOverlap` left free doubles the case analysis | medium | slower M2 | if it bites, prove the two values as two theorems |
| the proof is sound but the model drifted from the code | **the real risk** | a proof of the wrong protocol | see below |

**The alternative:** a trace check of the syncer against `ForgeSync.tla`,
as Lean has (41/41 on `LeanP1.tla`). Instrument
`forge/syncer/src/{batch,lease,restore,sweep}.rs` to log the model's
variables at each step, then replay the e2e drills' logs as TLC
behaviours. That catches model–code drift, which no proof can see. The
model has already had to be re-fitted to the syncer twice (`381bcc90` on
10-02, `ce2a1af6` on 10-05), and only reading caught either change.
Recommended order:
- M0–M3 first (one week; the headline claim at every bound);
- then the trace check;
- then M4–M5, if the trace check shows the model is faithful.
