# Stamp proof, invariant layer: brief for the parallel agents (2026-10-31)

Goal: a TLAPS proof of `Safety == Spec => []Goal` for FlintCatchupS **rev 16** under `ASSUME Full`, by the inductive
invariant `IndInv`. Your job: for the Next disjuncts assigned to you, prove
`LEMMA I_<Name> == ASSUME IndInv, <the disjunct, copied verbatim from Next, existentials as NEW r \in R etc.> PROVE IndInv'`.
Another session (main) owns everything not assigned to you and assembles I_Next / Safety.

## Files (read-only unless named yours)
- Model: `formal/sproof/model/FlintCatchupS.tla` (rev 16, md5 d9062ab6). `Next` near line 1350. rev 16 vs rev 12:
  FailBegin has no "another healthy member" guard (LastFails TRUE: every member may begin to fail); MarkStale marks
  every in_sync leg the live raid lacks (WholeMark) and only while a live member stays in_sync (MonitorGuard); a new
  Drain(r) that never steps (OperatorMark FALSE). Everything else is rev 12.
- Candidate conjuncts: `formal/FlintCatchupSInd.tla` (md5 932bb626). Do not edit.
- Generator: `formal/gen-FlintCatchupSProof.py`. Do not edit. It emits a head (ASSUME Const, ASSUME Full = every knob,
  TypeOKT, TyStampS), a prelude, a type layer copied from the earlier completed proof `formal/FlintCatchupTProof.tla`
  (T_* lemmas over the shared variables + the chain lemmas AncBound .. CopyRead), then the part files
  `formal/sproof/*.tla-part` in name order:
  - `10-ty-base`, `20-ty-zombie`, `30-ty-stamp`: `S_Ty*` lemmas, one per Next disjunct: `ASSUME TypeOK, <disjunct> PROVE TypeOK'`
    (some carry an extra hypothesis that IndInv provides: J_GenBound / J_ZGen; S_TyCutSmall needs `cutK # 0 => cutK = nextSeq`).
  - `40-ind-base`: `IndInv == TypeOK /\ IndJ /\ IndT /\ IndS`, `IndSplit`, `IndSplitP`, a frame `F_<conjunct>` for every
    conjunct (`X /\ UNCHANGED <<what X reads>> => X'`), `F_IndJ` (all of IndJ at once), `SessionOther(G)`, `RevOther`,
    `I_Init`, `I_Goal`. Read it first.
  - YOURS: `formal/sproof/<NN>-<tag>.tla-part` (named in your assignment). Plain TLA+ text, no MODULE header/footer.
  - `formal/sproof/ovr/<Lemma>.tla`: replaces a copied lemma's block. Only if your assignment says so.
- Reference proof: `formal/FlintCatchupTProof.tla` (FlintCatchupT, 7673 obligations, all proved). Its `I_<Action>` lemmas
  (lines ~1385-4300) prove the same actions for T's invariant. T's IndInv differs: S DROPS I_Hold, I_Legal, I_Fresh,
  I_AckedEver, I_WHi, I_HeadHi, I_HeadLo, I_Ranges (it had zx = 0), I_Members (it had Healthy # {}); S ADDS the stamp
  layer J_* (17 + J_GenBound, J_ZGen), I_RangesS, I_MembersS, I_HoldAtt, I_NoBadLoss, I_AckedEverS, I_LegalS, I_WHiS,
  I_AckMono, I_HeadLoS, I_FreshS, Inv_AckedHeld. Port T's step structure where the action and the conjunct are unchanged.
  T's copy lemmas (CopyFacts, CopySnap, CopyHeadOnly, AdmHold, CoverSetFacts, TgtFacts, PendOne ...) are NOT in S: port
  what you need into your part under your prefix.
- Under Full: Crash = TRUE and ZombieMoves = TRUE (both faults on), Capture = FALSE, Policy = "trust", Stamp = TRUE,
  MarkGate = TRUE (ZombieMark never steps), DeferExpires = FALSE (StageRisk never steps), StageOracle = FALSE.

## Generate and check
- `SPROOF_PARTS=10-ty,20-ty,30-ty,40-ind,<NN>-<tag> python3 ~/github/flint-rebuild-proof/formal/gen-FlintCatchupSProof.py <your scratch dir>/sp.tla`
  (scratch dir: `/private/tmp/claude-503/-Users-ddalton-github-flint/a7784df4-9203-4039-a972-c39f69476396/scratchpad/<your tag>/`).
- `cd ~/github/flint-rebuild-proof/formal/results/2026-10-31-c33-ec2 && PROOF=<that file> CAP=1500 ./sjob3.sh <prefix><N> 4 <bl> <el>`
  Job names: your prefix + 1, 2, 3 ... (never reused). **4 threads**: about ten agents share one 64-core machine. Check
  your part's line range, or one lemma's range while iterating. It prints `RESULT ...` and `FAILED <lines>`; the full
  tlapm output (failed obligations carry their text, `@!!obl:`) is in `jobs/<name>/tlapm.out`. Typical turnaround:
  1-3 min plus tlapm time.
- Split proofs finely: one obligation per conjunct, `<2>` steps per case, `DEF` only what the step needs; z3 times out
  on a goal over a whole group. A timeout can mean a FALSE goal: check the step against the definitions first.

## When the invariant is not enough
- A conjunct not preserved: find the concrete situation (a state satisfying IndInv where your step breaks it). If it
  is real, SendMessage to `main` AT ONCE (do not wait for your report) with the state and the argument; then carry on,
  stating the gap as an extra ASSUME hypothesis on your lemma marked `\* NEEDS-INV: <fact>`.
- A fact that holds in reachable states but is not in IndInv: propose the smallest strengthening the same way
  (NEEDS-INV + a message to main). main model-checks it and adds it to IndInv for everyone.

## Never
No git, no commits, no push. No ssh to the box or any other machine; no AWS command except through sjob3.sh. Never
upload a STOP file. Do not edit the generator, the model, FlintCatchupSInd.tla, or another agent's part.

## Report (when every lemma of yours checks, or after ~3 h)
At most 40 lines: each lemma and its status, the final job name + its RESULT line, every NEEDS-INV with why, surprises.
