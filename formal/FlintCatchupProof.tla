------------------------- MODULE FlintCatchupProof --------------------------
(***************************************************************************)
(* TLAPS proof that FlintCatchup.tla's safety theorems hold for EVERY      *)
(* bound -- any blocks, writes, epochs, failures and retention depth:     *)
(*   THEOREM Safety == Spec => []Inv                                       *)
(* (Inv_AdmitFaithful, Inv_MemberAgrees, Inv_SourceHoldsAcked, TypeOK)     *)
(* under the shipped catch-up (Shipped: Backoff, Revert, BaseInclusive,   *)
(* ReapNewer, FinalDelta, FullBuildReaps, FinalCutStrict all TRUE), with   *)
(* PinRetention left free: the pin buys progress, not safety.             *)
(*                                                                         *)
(* The inductive invariant is the model's IndInv (FlintCatchupProbe*.cfg   *)
(* and FlintCatchupNoPinRetention.cfg check it at both bounds):            *)
(*   S's chain is LINEAR (each parent the newest present snapshot below)  *)
(*   and READS THROUGH (a blob holds, on a block no newer blob allocated, *)
(*   what the older one holds); one write in flight per block, S always   *)
(*   its target, A1 (cutAt is this name or the last), and a write S took  *)
(*   sits in S's head at or after its cut; while D took every write since *)
(*   it was last whole (riskRec = NoRisk) the heads agree off writes in   *)
(*   flight; Agree(e) -- D's epoch e is S's except where S wrote at or    *)
(*   after e or a write is in flight to S -- for every common epoch at or *)
(*   below the risk bound, and for a cut D took clean; the session: a     *)
(*   selected base and every common epoch below it agree, a reverted head *)
(*   IS its base with nothing newer left on D (ReapNewer), a full build's *)
(*   head is empty with nothing left at a recorded name (FullBuildReaps), *)
(*   and a standby is S at its mark except where S wrote after the mark.  *)
(* The two copy facts carry §5: CopyTracks (a base-inclusive copy of S's  *)
(* lineage onto a head that is S's base, off S's later writes, lands D on *)
(* S's target epoch, off S's later writes) and AdmExact (the final delta  *)
(* from the mark, with S's head as the final epoch, leaves D's head EQUAL *)
(* to S's -- the strict final cut is what makes S's head that epoch).     *)
(*                                                                         *)
(* tlapm cannot read a module that defines a RECURSIVE operator (Lean M0, *)
(* 2026-10-05), so FlintCatchup.tla states S's linear chain without one.  *)
(* Controls: each Shipped guard set FALSE in turn must fail the steps that *)
(* use it (formal/results/2026-10-28-flintcatchup-tlaps/RESULTS.txt).     *)
(***************************************************************************)
EXTENDS FlintCatchup, TLAPS, NaturalsInduction, FiniteSetTheorems

ASSUME Const ==
  /\ MaxVer \in Nat /\ MaxEpoch \in Nat /\ MaxFails \in Nat
  /\ Retain \in Nat /\ Retain >= 1

ASSUME Shipped ==
  /\ Backoff = TRUE /\ Revert = TRUE /\ BaseInclusive = TRUE
  /\ ReapNewer = TRUE /\ FinalDelta = TRUE /\ FullBuildReaps = TRUE
  /\ FinalCutStrict = TRUE
  /\ PinRetention \in BOOLEAN

-----------------------------------------------------------------------------
(* Min and Max.                                                            *)

LEMMA MinProp ==
  ASSUME NEW T \in SUBSET Nat, T # {}
  PROVE  Min(T) \in T /\ \A u \in T : Min(T) <= u
<1>1. PICK t \in T : TRUE OBVIOUS
<1>2. \E m \in Nat : m \in T /\ \A k \in 0 .. m-1 : k \notin T
  <2>. DEFINE P(n) == n \in T
  <2>. HIDE DEF P
  <2>0. t \in Nat /\ P(t) BY <1>1 DEF P
  <2>1. \E m \in Nat : P(m) /\ \A k \in 0 .. m-1 : ~P(k)
    BY <2>0, SmallestNatural
  <2>. QED BY <2>1 DEF P
<1>3. PICK m \in Nat : m \in T /\ \A k \in 0 .. m-1 : k \notin T BY <1>2
<1>4. \A u \in T : m <= u
  <2>. TAKE u \in T
  <2>1. u \in Nat OBVIOUS
  <2>2. ~(u < m) BY <1>3, <2>1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>3, <1>4 DEF Min

LEMMA MaxProp ==
  ASSUME NEW N \in Nat, NEW T \in SUBSET (0..N), T # {}
  PROVE  Max(T) \in T /\ \A u \in T : u <= Max(T)
<1>1. \E m \in T : \A u \in T : u <= m
  <2>. DEFINE U == {N - t : t \in T}
  <2>1. U \in SUBSET Nat /\ U # {} OBVIOUS
  <2>2. Min(U) \in U /\ \A u \in U : Min(U) <= u BY <2>1, MinProp
  <2>3. PICK t0 \in T : Min(U) = N - t0 BY <2>2
  <2>. QED BY <2>2, <2>3
<1>. QED BY <1>1 DEF Max

LEMMA Min2 ==
  ASSUME NEW a \in Nat, NEW b \in Nat
  PROVE  Min({a, b}) \in {a, b} /\ Min({a, b}) <= a /\ Min({a, b}) <= b
BY MinProp

-----------------------------------------------------------------------------
(* Small facts.                                                            *)

LEMMA RDistinct == "S" # "D" /\ R = {"S", "D"}
BY DEF R

LEMMA NoRiskNat == NoRisk \in Nat /\ NoRisk = MaxEpoch + 1
BY Const DEF NoRisk

-----------------------------------------------------------------------------
(* TypeOK, step by step.  One obligation holding all of TypeOK expanded   *)
(* times Z3 out (as in the Lean proof); five groups each close at once.  *)

TyScalar ==
  /\ ver \in Vers /\ nextSeq \in 1..(MaxEpoch + 1) /\ cutK \in 0..MaxEpoch
  /\ attached \in BOOLEAN
  /\ dPath \in {"ok", "failing", "out", "back"}
  /\ dState \in {"insync", "stale", "standby"}
  /\ mark \in 0..MaxEpoch /\ reverted \in -1..MaxEpoch /\ sb \in -1..MaxEpoch
  /\ pin \in 0..MaxEpoch /\ fails \in 0..MaxFails /\ riskRec \in 0..NoRisk
  /\ badAdmit \in BOOLEAN
TySets ==
  /\ recorded \in SUBSET Epochs /\ cutTgt \in SUBSET R /\ cutDone \in SUBSET R
  /\ members \in SUBSET R
TyHead ==
  /\ head \in [R -> [Blocks -> Vers]] /\ hAl \in [R -> SUBSET Blocks]
  /\ hPar \in [R -> 0..MaxEpoch]
TySnap ==
  /\ sPres \in [R -> SUBSET Epochs]
  /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]]
  /\ sAl \in [R -> [Epochs -> SUBSET Blocks]]
  /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]]
TyPend == pend \subseteq Write /\ acked \in [Blocks -> Vers]

LEMMA TypeOKSplit == TypeOK <=> TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend
BY DEF TypeOK, TyScalar, TySets, TyHead, TySnap, TyPend
LEMMA TypeOKSplitP == TypeOK' <=> TyScalar' /\ TySets' /\ TyHead' /\ TySnap' /\ TyPend'
BY DEF TypeOK, TyScalar, TySets, TyHead, TySnap, TyPend

LEMMA T_Init == Init => TypeOK
<1>. SUFFICES ASSUME Init PROVE TypeOK OBVIOUS
<1>1. TyScalar BY Const DEF Init, TyScalar, NoRisk, Vers
<1>2. TySets BY DEF Init, TySets, R
<1>3. TyHead BY Const DEF Init, TyHead, Zero, Vers
<1>4. TySnap BY Const DEF Init, TySnap, Zero, Vers, Epochs
<1>5. TyPend BY Const DEF Init, TyPend, Zero, Vers
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplit

\* The usual shape: the groups an action leaves alone carry over.
LEMMA T_Submit ==
  ASSUME TypeOK, NEW x \in Blocks, Submit(x) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. Latest \in 0..MaxEpoch BY <1>0, Const DEF TyScalar, Latest
<1>b. riskRec' \in 0..NoRisk BY <1>0, <1>a, Min2, NoRiskNat DEF Submit, TyScalar
<1>1. TyScalar' BY <1>0, <1>b, Const DEF Submit, TyScalar, Vers
<1>2. TySets' BY <1>0 DEF Submit, TySets
<1>3. TyHead' BY <1>0 DEF Submit, TyHead
<1>4. TySnap' BY <1>0 DEF Submit, TySnap
<1>5. TyPend' BY <1>0, <1>a, Const DEF Submit, TyPend, TyScalar, TySets, Write, Vers
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

LEMMA T_Land ==
  ASSUME TypeOK, NEW w \in pend, NEW r \in R, Land(w, r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. w \in Write BY <1>0 DEF TyPend
<1>1. TyScalar' BY <1>0 DEF Land, TyScalar
<1>2. TySets' BY <1>0 DEF Land, TySets
<1>3. TyHead' BY <1>0, <1>a DEF Land, TyHead, Write, Vers
<1>4. TySnap' BY <1>0 DEF Land, TySnap
<1>5. TyPend' BY <1>0, <1>a DEF Land, TyPend, Write
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

LEMMA T_Drop ==
  ASSUME TypeOK, NEW w \in pend, Drop(w) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. w \in Write BY <1>0 DEF TyPend
<1>b. riskRec' \in 0..NoRisk
  <2>1. riskRec \in Nat /\ w.recAt \in Nat /\ w.recAt <= MaxEpoch
    BY <1>0, <1>a, Const DEF TyScalar, Write
  <2>2. Min({riskRec, w.recAt}) \in {riskRec, w.recAt} BY <2>1, Min2
  <2>3. riskRec' = Min({riskRec, w.recAt}) BY DEF Drop
  <2>. QED BY <1>0, <2>1, <2>2, <2>3, NoRiskNat, Const DEF TyScalar
<1>1. TyScalar' BY <1>0, <1>b DEF Drop, TyScalar
<1>2. TySets' BY <1>0 DEF Drop, TySets
<1>3. TyHead' BY <1>0 DEF Drop, TyHead
<1>4. TySnap' BY <1>0 DEF Drop, TySnap
<1>5. TyPend' BY <1>0, <1>a DEF Drop, TyPend, Write
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

LEMMA T_Complete ==
  ASSUME TypeOK, NEW w \in pend, Complete(w) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. w \in Write BY <1>0 DEF TyPend
<1>1. TyScalar' BY <1>0 DEF Complete, TyScalar
<1>2. TySets' BY <1>0 DEF Complete, TySets
<1>3. TyHead' BY <1>0 DEF Complete, TyHead
<1>4. TySnap' BY <1>0 DEF Complete, TySnap
<1>5. TyPend' BY <1>0, <1>a DEF Complete, TyPend, Write, Vers
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

LEMMA T_Zombie ==
  ASSUME TypeOK, NEW x \in Blocks, Zombie(x) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. Latest \in 0..MaxEpoch BY <1>0, Const DEF TyScalar, Latest
<1>b. riskRec' \in 0..NoRisk BY <1>0, <1>a, Min2, NoRiskNat DEF Zombie, TyScalar
<1>1. TyScalar' BY <1>0, <1>b, Const DEF Zombie, TyScalar, Vers
<1>2. TySets' BY <1>0 DEF Zombie, TySets
<1>3. TyHead' BY <1>0, Const DEF Zombie, TyHead, TyScalar, Vers
<1>4. TySnap' BY <1>0 DEF Zombie, TySnap
<1>5. TyPend' BY <1>0 DEF Zombie, TyPend
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

\* An action that touches only scalars and the four small sets.
LEMMA T_Small ==
  ASSUME TypeOK, TyScalar', TySets',
         UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>>
  PROVE  TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>3. TyHead' BY <1>0 DEF TyHead
<1>4. TySnap' BY <1>0 DEF TySnap
<1>5. TyPend' BY <1>0 DEF TyPend
<1>. QED BY <1>3, <1>4, <1>5, TypeOKSplitP

LEMMA T_Path ==
  ASSUME TypeOK, FailBegin \/ FaultOut \/ MarkStale \/ Return PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>1. TyScalar' BY <1>0, Const DEF FailBegin, FaultOut, MarkStale, Return, TyScalar
<1>2. TySets' BY <1>0 DEF FailBegin, FaultOut, MarkStale, Return, TySets
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>>
  BY DEF FailBegin, FaultOut, MarkStale, Return
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_CutBegin == ASSUME TypeOK, CutBegin PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>1. TyScalar' BY <1>0 DEF CutBegin, TyScalar
<1>2. TySets' BY <1>0 DEF CutBegin, TySets, InSyncR, R
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>> BY DEF CutBegin
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_CutRecord ==
  ASSUME TypeOK, cutK # 0 => cutK = nextSeq, CutRecord PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>1. TyScalar' BY <1>0, Const DEF CutRecord, TyScalar
<1>2. TySets' BY <1>0 DEF CutRecord, TySets, TyScalar, Epochs
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>> BY DEF CutRecord
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_CutAbort == ASSUME TypeOK, CutAbort PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>1. TyScalar' BY <1>0, Const DEF CutAbort, TyScalar
<1>2. TySets' BY <1>0 DEF CutAbort, TySets
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>> BY DEF CutAbort
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_Retire == ASSUME TypeOK, Retire PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>1. TyScalar' BY <1>0 DEF Retire, TyScalar
<1>2. TySets' BY <1>0 DEF Retire, TySets
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>> BY DEF Retire
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_SelectBase == ASSUME TypeOK, SelectBase PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>a. recorded # {} => Min(recorded) \in recorded
  BY <1>0, MinProp, Const DEF TySets, Epochs
<1>1. TyScalar' BY <1>0, <1>a DEF SelectBase, TyScalar, TySets, BaseCandidates, Epochs
<1>2. TySets' BY <1>0 DEF SelectBase, TySets
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>> BY DEF SelectBase
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_SessionFail == ASSUME TypeOK, SessionFail PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>1. TyScalar' BY <1>0, Const DEF SessionFail, TyScalar
<1>2. TySets' BY <1>0 DEF SessionFail, TySets
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>> BY DEF SessionFail
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_Stage ==
  ASSUME TypeOK, Unstage \/ Stage \/ StageDefer PROVE TypeOK'
<1>0. TyScalar /\ TySets BY TypeOKSplit
<1>1. TyScalar' BY <1>0 DEF Unstage, Stage, StageDefer, TyScalar
<1>2. TySets' BY <1>0 DEF Unstage, Stage, StageDefer, TySets, R
<1>3. UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked>>
  BY DEF Unstage, Stage, StageDefer
<1>. QED BY <1>1, <1>2, <1>3, T_Small

LEMMA T_CutOne ==
  ASSUME TypeOK, NEW r \in R, CutOne(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. cutK \in Epochs BY <1>0 DEF CutOne, TyScalar, Epochs
<1>1. TyScalar' BY <1>0 DEF CutOne, TyScalar
<1>2. TySets' BY <1>0 DEF CutOne, TySets
<1>5. TyPend' BY <1>0 DEF CutOne, TyPend
<1>6. CASE cutK \in sPres[r]
  <2>3. TyHead' BY <1>0, <1>6 DEF CutOne, TyHead
  <2>4. TySnap' BY <1>0, <1>6 DEF CutOne, TySnap
  <2>. QED BY <1>1, <1>2, <2>3, <2>4, <1>5, TypeOKSplitP
<1>7. CASE cutK \notin sPres[r]
  <2>0. SnapHead(r, cutK) BY <1>7 DEF CutOne
  <2>3. TyHead' BY <1>0, <1>a, <2>0 DEF CutOne, SnapHead, TyHead, Epochs
  <2>4. TySnap' BY <1>0, <1>a, <2>0 DEF SnapHead, TySnap, TyHead, Epochs
  <2>. QED BY <1>1, <1>2, <2>3, <2>4, <1>5, TypeOKSplitP
<1>. QED BY <1>6, <1>7

LEMMA T_GC ==
  ASSUME TypeOK, NEW r \in R, NEW e \in Epochs, GC(r, e) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>1. TyScalar' BY <1>0 DEF GC, TyScalar
<1>2. TySets' BY <1>0 DEF GC, TySets
<1>3. TyHead' BY <1>0 DEF GC, TyHead, TySnap, Epochs
<1>4. TySnap' BY <1>0 DEF GC, TySnap, Epochs
<1>5. TyPend' BY <1>0 DEF GC, TyPend
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

LEMMA T_RevertD == ASSUME TypeOK, RevertD PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>1. TyScalar' BY <1>0 DEF RevertD, TyScalar
<1>2. TySets' BY <1>0 DEF RevertD, TySets
<1>3. TyHead'
  <2>1. sb = 0 => (Zero \in [Blocks -> Vers]) BY Const DEF Zero, Vers
  <2>2. sb # 0 /\ sb \in sPres["D"] => sCon["D"][sb] \in [Blocks -> Vers] /\ sb \in 0..MaxEpoch
    BY <1>0 DEF TySnap, R, Epochs
  <2>. QED BY <1>0, <2>1, <2>2 DEF RevertD, TyHead, TyScalar, R
<1>4. TySnap' BY <1>0 DEF RevertD, TySnap
<1>5. TyPend' BY <1>0 DEF RevertD, TyPend
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

\* A copy session's result is a head.
LEMMA AfterCopyType ==
  ASSUME TySnap, NEW start \in [Blocks -> Vers], NEW lo \in Int, NEW target \in Int
  PROVE  AfterCopy(start, lo, target) \in [Blocks -> Vers]
<1>. SUFFICES ASSUME NEW x \in Blocks,
                    {e \in CopyElems(lo, target) : x \in sAl["S"][e]} # {}
             PROVE  sCon["S"][Max({e \in CopyElems(lo, target) : x \in sAl["S"][e]})][x] \in Vers
  BY DEF AfterCopy
<1>. DEFINE H == {e \in CopyElems(lo, target) : x \in sAl["S"][e]}
<1>1. H \in SUBSET (0..MaxEpoch) BY Const DEF TySnap, CopyElems, SLine, Epochs, R
<1>2. Max(H) \in H BY <1>1, Const, MaxProp
<1>3. Max(H) \in Epochs BY <1>2 DEF TySnap, CopyElems, SLine, R
<1>. QED BY <1>3 DEF TySnap, R

LEMMA T_CopyAlign ==
  ASSUME TyHead, TySnap, NEW from \in 0..MaxEpoch, NEW target \in Epochs,
         CopyAlign(from, target)
  PROVE  TyHead' /\ TySnap'
<1>. DEFINE lo == IF BaseInclusive \/ from = 0 THEN from ELSE from + 1
<1>0. head["D"] \in [Blocks -> Vers] /\ lo \in Int /\ target \in Int BY DEF TyHead, R, Epochs
<1>1. AfterCopy(head["D"], lo, target) \in [Blocks -> Vers]
  BY <1>0, AfterCopyType
<1>2. hAl["D"] \cup Copied(lo, target) \subseteq Blocks BY DEF Copied, TyHead, R
<1>3. TyHead' BY <1>1, <1>2 DEF CopyAlign, TyHead, Epochs, R
<1>4. TySnap'
  <2>1. CASE from = target \/ target \in sPres["D"]
    BY <2>1 DEF CopyAlign, TySnap
  <2>2. CASE from # target /\ target \notin sPres["D"]
    <3>a. sPres' = [sPres EXCEPT !["D"] = @ \cup {target}] BY <2>2 DEF CopyAlign
    <3>b. sCon' = [sCon EXCEPT !["D"][target] = AfterCopy(head["D"], lo, target)] BY <2>2 DEF CopyAlign
    <3>c. sAl' = [sAl EXCEPT !["D"][target] = hAl["D"] \cup Copied(lo, target)] BY <2>2 DEF CopyAlign
    <3>d. sPar' = [sPar EXCEPT !["D"][target] = hPar["D"]] BY <2>2 DEF CopyAlign
    <3>e. hPar["D"] \in 0..MaxEpoch BY DEF TyHead, R
    <3>. QED BY <3>a, <3>b, <3>c, <3>d, <3>e, <1>1, <1>2 DEF TySnap, Epochs, R
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>3, <1>4

LEMMA T_BulkCopy == ASSUME TypeOK, BulkCopy PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. recorded # {} => Max(recorded) \in recorded
  BY <1>0, MaxProp, Const DEF TySets, Epochs
<1>b. Max(recorded) \in Epochs BY <1>0, <1>a DEF BulkCopy, TySets
<1>c. sb \in 0..MaxEpoch BY <1>0 DEF BulkCopy, TyScalar
<1>d. CopyAlign(sb, Max(recorded)) BY DEF BulkCopy
<1>1. TyScalar' BY <1>0, <1>b, Const, Shipped DEF BulkCopy, TyScalar, Epochs
<1>2. TySets' BY <1>0 DEF BulkCopy, TySets
<1>3. TyHead' /\ TySnap' BY <1>0, <1>b, <1>c, <1>d, T_CopyAlign
<1>5. TyPend' BY <1>0 DEF BulkCopy, TyPend
<1>. QED BY <1>1, <1>2, <1>3, <1>5, TypeOKSplitP

LEMMA T_Chase == ASSUME TypeOK, Chase PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>a. recorded # {} => Max(recorded) \in recorded
  BY <1>0, MaxProp, Const DEF TySets, Epochs
<1>b. Max(recorded) \in Epochs BY <1>0, <1>a DEF Chase, TySets
<1>d. CopyAlign(mark, Max(recorded)) BY DEF Chase
<1>1. TyScalar' BY <1>0, <1>b, Const, Shipped DEF Chase, TyScalar, Epochs
<1>2. TySets' BY <1>0 DEF Chase, TySets
<1>3. TyHead' /\ TySnap' BY <1>0, <1>b, <1>d, T_CopyAlign DEF TyScalar
<1>5. TyPend' BY <1>0 DEF Chase, TyPend
<1>. QED BY <1>1, <1>2, <1>3, <1>5, TypeOKSplitP

-----------------------------------------------------------------------------
(* StageAdmit's LET, named (the shipped branch: FinalDelta).              *)

AdmK == nextSeq
AdmElems == {e \in SLine \cup {AdmK} : mark <= e /\ e <= AdmK}
AdmAl(e) == IF e = AdmK /\ ~(AdmK \in sPres["S"]) THEN hAl["S"] ELSE sAl["S"][e]
AdmCon(e) == IF e = AdmK /\ ~(AdmK \in sPres["S"]) THEN head["S"] ELSE sCon["S"][e]
AdmHits(x) == {e \in AdmElems : x \in AdmAl(e)}
AdmHead == [x \in Blocks |-> IF AdmHits(x) = {} THEN head["D"][x]
                             ELSE AdmCon(Max(AdmHits(x)))[x]]
AdmAlD == hAl["D"] \cup {x \in Blocks : \E e \in AdmElems : x \in AdmAl(e)}

\* What the shipped StageAdmit does, in those names (the strict final cut
\* refused a leftover, so S's head IS snapshotted under AdmK).
LEMMA StageAdmitFacts ==
  ASSUME StageAdmit
  PROVE  /\ ~attached /\ cutK = 0 /\ dState = "standby" /\ dPath = "back"
         /\ mark \in SLine /\ nextSeq <= MaxEpoch
         /\ AdmK \notin sPres["S"]
         /\ sPres' = [sPres EXCEPT !["S"] = @ \cup {AdmK}, !["D"] = @ \cup {AdmK}]
         /\ sCon' = [sCon EXCEPT !["S"][AdmK] = head["S"],
                                 !["D"][AdmK] = IF AdmK \in sPres["D"] THEN @ ELSE AdmHead]
         /\ sAl' = [sAl EXCEPT !["S"][AdmK] = hAl["S"],
                               !["D"][AdmK] = IF AdmK \in sPres["D"] THEN @ ELSE AdmAlD]
         /\ sPar' = [sPar EXCEPT !["S"][AdmK] = hPar["S"],
                                 !["D"][AdmK] = IF AdmK \in sPres["D"] THEN @ ELSE hPar["D"]]
         /\ hPar' = [hPar EXCEPT !["S"] = AdmK,
                                 !["D"] = IF AdmK \in sPres["D"] THEN @ ELSE AdmK]
         /\ hAl' = [hAl EXCEPT !["S"] = {},
                               !["D"] = IF AdmK \in sPres["D"] THEN AdmAlD ELSE {}]
         /\ head' = [head EXCEPT !["D"] = AdmHead]
         /\ recorded' = recorded \cup {AdmK}
         /\ nextSeq' = nextSeq + 1
         /\ mark' = AdmK
         /\ badAdmit' = (badAdmit \/ AdmHead # head["S"])
         /\ dState' = "insync" /\ reverted' = -1 /\ dPath' = "ok" /\ pin' = 0
         /\ attached' = TRUE /\ members' = R /\ riskRec' = NoRisk
         /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, sb, pend, acked, fails>>
BY Shipped DEF StageAdmit, AdmK, AdmElems, AdmAl, AdmCon, AdmHits, AdmHead, AdmAlD, SLine

LEMMA AdmHeadType ==
  ASSUME TyScalar, TyHead, TySnap, nextSeq <= MaxEpoch
  PROVE  AdmHead \in [Blocks -> Vers] /\ AdmAlD \subseteq Blocks
<1>1. AdmK \in Epochs BY DEF TyScalar, AdmK, Epochs
<1>2. \A x \in Blocks : AdmHits(x) \in SUBSET (0..MaxEpoch)
  BY <1>1 DEF AdmHits, AdmElems, SLine, TySnap, Epochs, R
<1>3. \A x \in Blocks : AdmHits(x) # {} => Max(AdmHits(x)) \in AdmHits(x)
  BY <1>2, MaxProp, Const
<1>4. \A x \in Blocks : AdmHits(x) # {} => AdmCon(Max(AdmHits(x)))[x] \in Vers
  <2>. SUFFICES ASSUME NEW x \in Blocks, AdmHits(x) # {}
               PROVE  AdmCon(Max(AdmHits(x)))[x] \in Vers
    OBVIOUS
  <2>1. Max(AdmHits(x)) \in AdmElems BY <1>3 DEF AdmHits
  <2>2. Max(AdmHits(x)) \in Epochs BY <1>1, <2>1 DEF AdmElems, SLine, TySnap, R
  <2>3. \A e \in Epochs : AdmCon(e) \in [Blocks -> Vers] BY DEF AdmCon, TySnap, TyHead, R
  <2>. QED BY <2>2, <2>3
<1>5. AdmHead \in [Blocks -> Vers] BY <1>4 DEF AdmHead, TyHead, R
<1>. QED BY <1>5 DEF AdmAlD, TyHead, R

LEMMA T_StageAdmit == ASSUME TypeOK, StageAdmit PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyHead /\ TySnap /\ TyPend BY TypeOKSplit
<1>g. /\ ~attached /\ cutK = 0 /\ nextSeq <= MaxEpoch BY StageAdmitFacts
<1>h. AdmHead \in [Blocks -> Vers] /\ AdmAlD \subseteq Blocks
  BY <1>0, <1>g, AdmHeadType
<1>k. AdmK \in Epochs BY <1>0, <1>g DEF TyScalar, AdmK, Epochs
<1>1. TyScalar' BY <1>0, <1>g, <1>k, StageAdmitFacts, NoRiskNat, Const DEF TyScalar, AdmK, Epochs
<1>2. TySets' BY <1>0, <1>k, StageAdmitFacts DEF TySets, R
<1>3. TyHead' BY <1>0, <1>h, <1>k, StageAdmitFacts DEF TyHead, Epochs, R
<1>4. TySnap'
  <2>a. sPres' \in [R -> SUBSET Epochs] BY <1>0, <1>k, StageAdmitFacts DEF TySnap, R
  <2>b. sCon' \in [R -> [Epochs -> [Blocks -> Vers]]]
    <3>1. head["S"] \in [Blocks -> Vers] BY <1>0 DEF TyHead, R
    <3>. QED BY <1>0, <1>h, <1>k, <3>1, StageAdmitFacts DEF TySnap, R
  <2>c. sAl' \in [R -> [Epochs -> SUBSET Blocks]]
    <3>1. hAl["S"] \subseteq Blocks BY <1>0 DEF TyHead, R
    <3>. QED BY <1>0, <1>h, <1>k, <3>1, StageAdmitFacts DEF TySnap, R
  <2>d. sPar' \in [R -> [Epochs -> 0..MaxEpoch]]
    <3>1. hPar["S"] \in 0..MaxEpoch /\ hPar["D"] \in 0..MaxEpoch BY <1>0 DEF TyHead, R
    <3>. QED BY <1>0, <1>k, <3>1, StageAdmitFacts DEF TySnap, R
  <2>. QED BY <2>a, <2>b, <2>c, <2>d DEF TySnap
<1>5. TyPend' BY <1>0, StageAdmitFacts DEF TyPend
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, TypeOKSplitP

LEMMA T_Next == ASSUME TypeOK, cutK # 0 => cutK = nextSeq, Next PROVE TypeOK'
<1>. USE DEF Next
<1>1. CASE \E x \in Blocks : Submit(x) \/ Zombie(x) BY <1>1, T_Submit, T_Zombie
<1>2. CASE \E w \in pend : Drop(w) \/ Complete(w) \/ \E r \in R : Land(w, r)
  BY <1>2, T_Drop, T_Complete, T_Land
<1>3. CASE FailBegin \/ FaultOut \/ MarkStale \/ Return BY <1>3, T_Path
<1>4. CASE CutBegin \/ CutRecord \/ CutAbort \/ Retire
  BY <1>4, T_CutBegin, T_CutRecord, T_CutAbort, T_Retire
<1>5. CASE \E r \in R : CutOne(r) \/ \E e \in Epochs : GC(r, e) BY <1>5, T_CutOne, T_GC
<1>6. CASE SelectBase \/ RevertD \/ BulkCopy \/ SessionFail \/ Chase
  BY <1>6, T_SelectBase, T_RevertD, T_BulkCopy, T_SessionFail, T_Chase
<1>7. CASE Unstage \/ Stage \/ StageAdmit \/ StageDefer BY <1>7, T_Stage, T_StageAdmit
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7

-----------------------------------------------------------------------------
(* The inductive invariant: Init, and the facts every step reads.          *)

LEMMA I_Init == Init => IndInv
<1>. SUFFICES ASSUME Init PROVE IndInv OBVIOUS
<1>1. TypeOK BY T_Init
<1>2. ~badAdmit /\ Inv_SourceHoldsAcked BY Const DEF Init, Inv_SourceHoldsAcked, Zero, R
<1>3. Inv_SLinear BY DEF Init, Inv_SLinear, R
<1>4. Inv_SReads BY DEF Init, Inv_SReads, SC, Zero, R
<1>5. Inv_Pend BY Const DEF Init, Inv_Pend, PendUniq, PendW, PendGlobal, Zero, R
<1>6. Inv_Phase BY DEF Init, Inv_Phase, R
<1>7. Inv_Epochs BY DEF Init, Inv_Epochs, R
<1>8. Inv_Heads BY DEF Init, Inv_Heads, HeadsW, Pending, R
<1>9. Inv_Agree BY DEF Init, Inv_Agree, AgreeLow, AgreeCut, Common
<1>10. Inv_Session BY DEF Init, Inv_Session, SessSel, SessBase, SessRev, SessFull, SessStandby
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

\* S's chain and head untouched by a step.
SFix ==
  /\ sPres'["S"] = sPres["S"] /\ sCon'["S"] = sCon["S"] /\ sAl'["S"] = sAl["S"]
  /\ sPar'["S"] = sPar["S"] /\ hPar'["S"] = hPar["S"] /\ hAl'["S"] = hAl["S"]
  /\ head'["S"] = head["S"]

\* A block stops being in flight to S only by landing there.
IFMono == \A x \in Blocks : InFlightS(x) => (InFlightS(x)' \/ x \in hAl'["S"])

LEMMA SFixAlloc ==
  ASSUME SFix PROVE \A e : AllocFrom(e)' = AllocFrom(e) /\ AllocAfter(e)' = AllocAfter(e)
BY DEF SFix, AllocFrom, AllocAfter

LEMMA SFixReads == ASSUME SFix, Inv_SReads PROVE Inv_SReads'
BY DEF SFix, Inv_SReads, SC

LEMMA SFixLinear ==
  ASSUME SFix, Inv_SLinear, recorded' \subseteq recorded PROVE Inv_SLinear'
BY DEF SFix, Inv_SLinear

LEMMA KeepAgree ==
  ASSUME NEW e, Agree(e), IFMono,
         sCon'["D"][e] = sCon["D"][e], sCon'["S"][e] = sCon["S"][e],
         AllocFrom(e) \subseteq AllocFrom(e)'
  PROVE  Agree(e)'
<1>1. hAl'["S"] \subseteq AllocFrom(e)' BY DEF AllocFrom
<1>. QED BY <1>1 DEF Agree, IFMono

LEMMA KeepStandby ==
  ASSUME NEW m, StandbyAt(m), IFMono,
         head'["D"] = head["D"], sCon'["S"][m] = sCon["S"][m],
         AllocAfter(m) \subseteq AllocAfter(m)'
  PROVE  StandbyAt(m)'
<1>1. hAl'["S"] \subseteq AllocAfter(m)' BY DEF AllocAfter
<1>. QED BY <1>1 DEF StandbyAt, IFMono

\* A step that leaves every snapshot, the record and the session alone,
\* grows S's allocation, changes S's head only where it allocates, and only
\* lowers the risk bound, keeps every agreement fact.
LEMMA KeepAgreeSessionGen ==
  ASSUME Inv_Agree, Inv_Session, IFMono,
         sCon' = sCon, sPres'["D"] = sPres["D"], sPres'["S"] \subseteq sPres["S"],
         cutK \in sPres["S"] => cutK \in sPres'["S"],
         \A e \in Int : AllocFrom(e) \subseteq AllocFrom(e)' /\ AllocAfter(e) \subseteq AllocAfter(e)',
         hAl["S"] \subseteq hAl'["S"],
         \A y \in Blocks : y \notin hAl'["S"] => head'["S"][y] = head["S"][y],
         recorded' = recorded, nextSeq' = nextSeq, cutK' = cutK, cutDone' = cutDone,
         dState' = dState, sb' = sb, reverted' = reverted, mark' = mark,
         riskRec' = NoRisk => riskRec = NoRisk,
         \A e \in Nat : e <= riskRec' => e <= riskRec,
         dState \in {"stale", "standby"} => head'["D"] = head["D"],
         TypeOK
  PROVE  Inv_Agree' /\ Inv_Session'
<1>0. /\ Common \subseteq Nat /\ sb \in Int /\ mark \in Int /\ cutK \in Int
  BY DEF TypeOK, Common, Epochs
<1>1. \A e \in Int : Agree(e) => Agree(e)' BY KeepAgree
<1>2. Common' \subseteq Common BY DEF Common
<1>3. AgreeLow' BY <1>0, <1>1, <1>2 DEF Inv_Agree, AgreeLow
<1>4. AgreeCut'
  <2>1. CutVsHead => CutVsHead' BY DEF CutVsHead, IFMono
  <2>. QED BY <1>0, <1>1, <2>1 DEF Inv_Agree, AgreeCut
<1>5. SessSel' /\ SessBase' /\ SessFull' BY <1>0, <1>1, <1>2 DEF Inv_Session, SessSel, SessBase, SessFull
<1>6. SessRev' BY <1>0, <1>1, <1>2 DEF Inv_Session, SessRev
<1>7. SessStandby'
  <2>1. dState = "standby" => StandbyAt(mark)'
    BY <1>0, KeepStandby DEF Inv_Session, SessStandby
  <2>. QED BY <1>0, <1>1, <1>2, <2>1 DEF Inv_Session, SessStandby
<1>. QED BY <1>3, <1>4, <1>5, <1>6, <1>7 DEF Inv_Agree, Inv_Session

\* The same, for a step that leaves S's chain and head alone.
LEMMA KeepAgreeSession ==
  ASSUME Inv_Agree, Inv_Session, IFMono, SFix,
         sCon' = sCon, sPres' = sPres,
         recorded' = recorded, nextSeq' = nextSeq, cutK' = cutK, cutDone' = cutDone,
         dState' = dState, sb' = sb, reverted' = reverted, mark' = mark,
         riskRec' = NoRisk => riskRec = NoRisk,
         \A e \in Nat : e <= riskRec' => e <= riskRec,
         dState \in {"stale", "standby"} => head'["D"] = head["D"],
         TypeOK
  PROVE  Inv_Agree' /\ Inv_Session'
<1>0. \A e : AllocFrom(e)' = AllocFrom(e) /\ AllocAfter(e)' = AllocAfter(e) BY SFixAlloc
<1>. QED BY <1>0, KeepAgreeSessionGen DEF SFix

LEMMA CommonNat == ASSUME TypeOK PROVE Common \subseteq Nat
BY DEF TypeOK, Common, Epochs

\* Field updates of a write in flight.
LEMMA LandedRec ==
  ASSUME NEW w \in Write, NEW X
  PROVE  LET w1 == [w EXCEPT !.landed = X]
         IN  /\ w1.b = w.b /\ w1.v = w.v /\ w1.to = w.to /\ w1.landed = X
             /\ w1.dropped = w.dropped /\ w1.recAt = w.recAt /\ w1.cutAt = w.cutAt
BY DEF Write
LEMMA DroppedRec ==
  ASSUME NEW w \in Write, NEW X
  PROVE  LET w1 == [w EXCEPT !.dropped = X]
         IN  /\ w1.b = w.b /\ w1.v = w.v /\ w1.to = w.to /\ w1.landed = w.landed
             /\ w1.dropped = X /\ w1.recAt = w.recAt /\ w1.cutAt = w.cutAt
BY DEF Write

-----------------------------------------------------------------------------
(* The data plane.                                                          *)

LEMMA I_Submit ==
  ASSUME IndInv, NEW x \in Blocks, Submit(x) PROVE IndInv'
<1>. DEFINE w0 == [b |-> x, v |-> ver + 1, to |-> members, landed |-> {},
                   dropped |-> {}, recAt |-> Latest, cutAt |-> nextSeq]
<1>0. /\ pend' = pend \cup {w0} /\ ver' = ver + 1
      /\ attached /\ ~Pending(x)
      /\ riskRec' = IF "D" \in members THEN riskRec ELSE Min({riskRec, Latest})
      /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
                     cutK, cutTgt, cutDone, members, attached, dPath, dState, mark,
                     reverted, sb, pin, acked, fails, badAdmit>>
  BY DEF Submit
<1>t. TypeOK BY DEF IndInv
<1>n. /\ ver \in Nat /\ nextSeq \in 1..(MaxEpoch + 1) /\ riskRec \in 0..NoRisk
      /\ acked \in [Blocks -> Nat] /\ pend \subseteq Write
  BY <1>t, Const DEF TypeOK, Vers
<1>m. Min({riskRec, Latest}) \in {riskRec, Latest}
      /\ Min({riskRec, Latest}) <= riskRec /\ Min({riskRec, Latest}) <= Latest
  BY <1>n, Min2, NoRiskNat DEF Latest
<1>r. /\ riskRec' \in Nat /\ riskRec' <= riskRec
      /\ (riskRec' = NoRisk => riskRec = NoRisk)
      /\ ("D" \notin members => riskRec' < nextSeq)
      /\ ("D" \in members => riskRec' = riskRec)
  BY <1>0, <1>m, <1>n, NoRiskNat, Const DEF Latest
<1>a. SFix BY <1>0 DEF SFix
<1>i. IFMono BY <1>0 DEF IFMono, InFlightS
<1>1. TypeOK' BY <1>t, T_Submit
<1>2. ~badAdmit' BY <1>0 DEF IndInv
<1>3. Inv_SourceHoldsAcked' BY <1>0 DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>a, <1>0, SFixLinear DEF IndInv
<1>5. Inv_SReads' BY <1>a, SFixReads DEF IndInv
<1>6. Inv_Pend'
  <2>0. Inv_Pend BY DEF IndInv
  <2>1. PendUniq' BY <1>0, <2>0 DEF Inv_Pend, PendUniq, Pending
  <2>2. \A w \in {w0} : PendW(w)'
    <3>1. "S" \in members BY <1>0, <2>0 DEF Inv_Pend, PendGlobal
    <3>2. acked[x] <= ver BY <2>0 DEF Inv_Pend, PendGlobal
    <3>. QED BY <1>0, <1>n, <3>1, <3>2 DEF PendW, Latest
  <2>3. \A w \in pend : PendW(w)'
    <3>0. \A e : AllocFrom(e)' = AllocFrom(e) BY <1>a, SFixAlloc
    <3>. QED BY <1>0, <1>n, <2>0, <3>0 DEF Inv_Pend, PendW, Write
  <2>4. PendGlobal' BY <1>0, <1>n, <2>0 DEF Inv_Pend, PendGlobal
  <2>. QED BY <1>0, <2>1, <2>2, <2>3, <2>4 DEF Inv_Pend
<1>7. Inv_Phase'
  <2>0. Inv_Phase /\ Inv_Pend BY DEF IndInv
  <2>1. dPath = "ok" => "D" \in members BY <1>0, <2>0 DEF Inv_Phase
  <2>. QED BY <1>0, <1>r, <2>0, <2>1 DEF Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Inv_Epochs
<1>9. Inv_Heads'
  <2>. SUFFICES ASSUME HeadsTracked'
               PROVE  /\ \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
                      /\ \A w \in pend' : HeadsW(w)'
    BY DEF Inv_Heads
  <2>1. "D" \in members /\ HeadsTracked BY <1>0, <1>r, <1>n, NoRiskNat, Const DEF HeadsTracked
  <2>2. Inv_Heads BY DEF IndInv
  <2>3. \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
    BY <1>0, <2>1, <2>2 DEF Inv_Heads, Pending
  <2>4. \A w \in {w0} : HeadsW(w)' BY <1>0, <2>1, <2>2 DEF Inv_Heads, HeadsW
  <2>5. \A w \in pend : HeadsW(w)' BY <1>0, <2>1, <2>2 DEF Inv_Heads, HeadsW
  <2>. QED BY <1>0, <2>3, <2>4, <2>5
<1>10. Inv_Agree' /\ Inv_Session'
  <2>1. Common \subseteq Nat BY <1>t DEF TypeOK, Common, Epochs
  <2>2. \A e \in Nat : e <= riskRec' => e <= riskRec BY <1>r, <1>n
  <2>. QED BY <1>0, <1>a, <1>i, <1>r, <2>1, <2>2, KeepAgreeSession DEF IndInv
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_LandS ==
  ASSUME IndInv, NEW w \in pend, Land(w, "S") PROVE IndInv'
<1>. DEFINE w1 == [w EXCEPT !.landed = @ \cup {"S"}]
           x == w.b
<1>t. TypeOK BY DEF IndInv
<1>w. w \in Write BY <1>t DEF TypeOK
<1>f. /\ w1.b = x /\ w1.v = w.v /\ w1.to = w.to /\ w1.landed = w.landed \cup {"S"}
      /\ w1.dropped = w.dropped /\ w1.recAt = w.recAt /\ w1.cutAt = w.cutAt
  BY <1>w, LandedRec
<1>0. /\ "S" \in w.to \ (w.landed \cup w.dropped)
      /\ head' = [head EXCEPT !["S"][x] = w.v]
      /\ hAl' = [hAl EXCEPT !["S"] = @ \cup {x}]
      /\ pend' = (pend \ {w}) \cup {w1}
      /\ UNCHANGED <<ver, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq, cutK,
                     cutTgt, cutDone, members, attached, dPath, dState, mark,
                     reverted, sb, pin, acked, fails, riskRec, badAdmit>>
  BY DEF Land
<1>x. x \in Blocks BY <1>w DEF Write
<1>hd. /\ head["S"] \in [Blocks -> Vers] /\ head["D"] \in [Blocks -> Vers]
       /\ acked \in [Blocks -> Nat] /\ \A w2 \in pend : w2.b \in Blocks
  BY <1>t, Const DEF TypeOK, R, Vers, Write
<1>h. /\ head'["S"] = [head["S"] EXCEPT ![x] = w.v] /\ head'["D"] = head["D"]
      /\ hAl'["S"] = hAl["S"] \cup {x} /\ hAl'["D"] = hAl["D"]
  BY <1>0, <1>t, RDistinct DEF TypeOK
<1>u. Inv_Pend BY DEF IndInv
<1>uq. \A w2 \in pend : w2.b = x => w2 = w BY <1>u DEF Inv_Pend, PendUniq
<1>pw. PendW(w) BY <1>u DEF Inv_Pend
<1>a. \A e : AllocFrom(e) \subseteq AllocFrom(e)' /\ AllocAfter(e) \subseteq AllocAfter(e)'
  BY <1>0, <1>h DEF AllocFrom, AllocAfter
<1>a2. \A e : x \in AllocFrom(e)' BY <1>h DEF AllocFrom
<1>i. IFMono
  <2>. SUFFICES ASSUME NEW y \in Blocks, InFlightS(y), y # x PROVE InFlightS(y)'
    BY <1>h DEF IFMono
  <2>1. PICK w2 \in pend : w2.b = y /\ "S" \notin w2.landed BY DEF InFlightS
  <2>2. w2 # w BY <2>1
  <2>. QED BY <1>0, <2>1, <2>2 DEF InFlightS
<1>p. \A y \in Blocks : Pending(y)' = Pending(y) BY <1>0, <1>f DEF Pending
<1>1. TypeOK' BY <1>t, T_Land, RDistinct DEF R
<1>2. ~badAdmit' BY <1>0 DEF IndInv
<1>3. Inv_SourceHoldsAcked'
  <2>1. acked[x] < w.v BY <1>pw DEF PendW
  <2>2. w.v \in Nat /\ acked[x] \in Nat BY <1>w, <1>x, <1>t, Const DEF Write, TypeOK, Vers
  <2>. QED BY <1>0, <1>h, <1>hd, <1>x, <2>1, <2>2 DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>0 DEF IndInv, Inv_SLinear
<1>5. Inv_SReads' BY <1>0, <1>h, <1>hd, <1>x DEF IndInv, Inv_SReads, SC
<1>6. Inv_Pend'
  <2>1. PendUniq'
    BY <1>0, <1>f, <1>uq, <1>u DEF Inv_Pend, PendUniq
  <2>2. \A v1 \in {w1} : PendW(v1)'
    BY <1>0, <1>f, <1>h, <1>hd, <1>x, <1>a2, <1>pw DEF PendW
  <2>3. \A w2 \in pend \ {w} : PendW(w2)'
    <3>. SUFFICES ASSUME NEW w2 \in pend \ {w} PROVE PendW(w2)' OBVIOUS
    <3>1. w2.b # x BY <1>uq
    <3>2. PendW(w2) BY <1>u DEF Inv_Pend
    <3>3. head'["S"][w2.b] = head["S"][w2.b] BY <3>1, <1>h, <1>hd
    <3>. QED BY <1>0, <1>a, <3>2, <3>3 DEF PendW
  <2>4. PendGlobal' BY <1>0, <1>u DEF Inv_Pend, PendGlobal
  <2>. QED BY <1>0, <2>1, <2>2, <2>3, <2>4 DEF Inv_Pend
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Inv_Epochs
<1>9. Inv_Heads'
  <2>. SUFFICES ASSUME HeadsTracked'
               PROVE  /\ \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
                      /\ \A v1 \in pend' : HeadsW(v1)'
    BY DEF Inv_Heads
  <2>0. Inv_Heads /\ HeadsTracked BY <1>0 DEF IndInv, HeadsTracked
  <2>1. \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
    <3>. SUFFICES ASSUME NEW y \in Blocks, ~Pending(y)' PROVE head'["D"][y] = head'["S"][y]
      OBVIOUS
    <3>1. y # x /\ ~Pending(y) BY <1>p, <1>0 DEF Pending
    <3>. QED BY <2>0, <3>1, <1>h, <1>hd DEF Inv_Heads
  <2>2. \A v1 \in {w1} : HeadsW(v1)'
    <3>1. HeadsW(w) BY <2>0 DEF Inv_Heads
    <3>2. head'["S"][x] = w.v BY <1>h, <1>x, <1>hd
    <3>. QED BY <3>1, <3>2, <1>f, <1>h, RDistinct DEF HeadsW
  <2>3. \A w2 \in pend \ {w} : HeadsW(w2)'
    <3>. SUFFICES ASSUME NEW w2 \in pend \ {w} PROVE HeadsW(w2)' OBVIOUS
    <3>1. w2.b # x BY <1>uq
    <3>2. head'["S"][w2.b] = head["S"][w2.b] BY <3>1, <1>h, <1>hd
    <3>. QED BY <2>0, <3>2, <1>h DEF Inv_Heads, HeadsW
  <2>. QED BY <1>0, <2>1, <2>2, <2>3
<1>10. Inv_Agree' /\ Inv_Session'
  <2>1. \A y \in Blocks : y \notin hAl'["S"] => head'["S"][y] = head["S"][y]
    BY <1>h, <1>hd
  <2>2. \A e \in Nat : e <= riskRec' => e <= riskRec BY <1>0
  <2>. QED BY <1>0, <1>a, <1>i, <1>h, <2>1, <2>2, <1>t, CommonNat, KeepAgreeSessionGen DEF IndInv
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_LandD ==
  ASSUME IndInv, NEW w \in pend, Land(w, "D") PROVE IndInv'
<1>. DEFINE w1 == [w EXCEPT !.landed = @ \cup {"D"}]
           x == w.b
<1>t. TypeOK BY DEF IndInv
<1>w. w \in Write BY <1>t DEF TypeOK
<1>f. /\ w1.b = x /\ w1.v = w.v /\ w1.to = w.to /\ w1.landed = w.landed \cup {"D"}
      /\ w1.dropped = w.dropped /\ w1.recAt = w.recAt /\ w1.cutAt = w.cutAt
  BY <1>w, LandedRec
<1>0. /\ "D" \in w.to \ (w.landed \cup w.dropped)
      /\ dPath \in {"ok", "failing"}
      /\ head' = [head EXCEPT !["D"][x] = w.v]
      /\ hAl' = [hAl EXCEPT !["D"] = @ \cup {x}]
      /\ pend' = (pend \ {w}) \cup {w1}
      /\ UNCHANGED <<ver, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq, cutK,
                     cutTgt, cutDone, members, attached, dPath, dState, mark,
                     reverted, sb, pin, acked, fails, riskRec, badAdmit>>
  BY RDistinct DEF Land
<1>x. x \in Blocks BY <1>w DEF Write
<1>hd. /\ head["S"] \in [Blocks -> Vers] /\ head["D"] \in [Blocks -> Vers]
       /\ acked \in [Blocks -> Nat] /\ \A w2 \in pend : w2.b \in Blocks
  BY <1>t, Const DEF TypeOK, R, Vers, Write
<1>h. /\ head'["D"] = [head["D"] EXCEPT ![x] = w.v] /\ head'["S"] = head["S"]
      /\ hAl'["S"] = hAl["S"]
  BY <1>0, <1>t, RDistinct DEF TypeOK
<1>ins. dState = "insync" BY <1>0 DEF IndInv, Inv_Phase
<1>u. Inv_Pend BY DEF IndInv
<1>uq. \A w2 \in pend : w2.b = x => w2 = w BY <1>u DEF Inv_Pend, PendUniq
<1>pw. PendW(w) BY <1>u DEF Inv_Pend
<1>s. SFix BY <1>0, <1>h DEF SFix
<1>i. IFMono
  <2>. SUFFICES ASSUME NEW y \in Blocks, InFlightS(y) PROVE InFlightS(y)'
    BY DEF IFMono
  <2>1. PICK w2 \in pend : w2.b = y /\ "S" \notin w2.landed BY DEF InFlightS
  <2>2. CASE w2 = w BY <2>1, <2>2, <1>0, <1>f, RDistinct DEF InFlightS
  <2>3. CASE w2 # w BY <2>1, <2>3, <1>0 DEF InFlightS
  <2>. QED BY <2>2, <2>3
<1>p. \A y \in Blocks : Pending(y)' = Pending(y) BY <1>0, <1>f DEF Pending
<1>1. TypeOK' BY <1>t, T_Land, RDistinct DEF R
<1>2. ~badAdmit' BY <1>0 DEF IndInv
<1>3. Inv_SourceHoldsAcked' BY <1>0, <1>h DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>0 DEF IndInv, Inv_SLinear
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend'
  <2>0. \A e : AllocFrom(e)' = AllocFrom(e) BY <1>s, SFixAlloc
  <2>1. PendUniq'
    BY <1>0, <1>f, <1>uq, <1>u DEF Inv_Pend, PendUniq
  <2>2. \A v1 \in {w1} : PendW(v1)'
    BY <1>0, <1>f, <1>h, <2>0, <1>pw, RDistinct DEF PendW
  <2>3. \A w2 \in pend \ {w} : PendW(w2)'
    BY <1>0, <1>h, <2>0, <1>u DEF Inv_Pend, PendW
  <2>4. PendGlobal' BY <1>0, <1>u DEF Inv_Pend, PendGlobal
  <2>. QED BY <1>0, <2>1, <2>2, <2>3, <2>4 DEF Inv_Pend
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Inv_Epochs
<1>9. Inv_Heads'
  <2>. SUFFICES ASSUME HeadsTracked'
               PROVE  /\ \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
                      /\ \A v1 \in pend' : HeadsW(v1)'
    BY DEF Inv_Heads
  <2>0. Inv_Heads /\ HeadsTracked BY <1>0 DEF IndInv, HeadsTracked
  <2>1. \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
    <3>. SUFFICES ASSUME NEW y \in Blocks, ~Pending(y)' PROVE head'["D"][y] = head'["S"][y]
      OBVIOUS
    <3>1. y # x /\ ~Pending(y) BY <1>p, <1>0 DEF Pending
    <3>. QED BY <2>0, <3>1, <1>h, <1>hd DEF Inv_Heads
  <2>2. \A v1 \in {w1} : HeadsW(v1)'
    <3>1. HeadsW(w) BY <2>0 DEF Inv_Heads
    <3>2. head'["D"][x] = w.v BY <1>h, <1>x, <1>hd
    <3>3. "S" \in w.landed => head["S"][x] = w.v BY <1>pw DEF PendW
    <3>. QED BY <3>1, <3>2, <3>3, <1>f, <1>h, <1>0, RDistinct DEF HeadsW
  <2>3. \A w2 \in pend \ {w} : HeadsW(w2)'
    <3>. SUFFICES ASSUME NEW w2 \in pend \ {w} PROVE HeadsW(w2)' OBVIOUS
    <3>1. w2.b # x BY <1>uq
    <3>2. head'["D"][w2.b] = head["D"][w2.b] BY <3>1, <1>h, <1>hd
    <3>. QED BY <2>0, <3>2, <1>h DEF Inv_Heads, HeadsW
  <2>. QED BY <1>0, <2>1, <2>2, <2>3
<1>10. Inv_Agree' /\ Inv_Session'
  BY <1>0, <1>s, <1>i, <1>ins, <1>t, CommonNat, KeepAgreeSession DEF IndInv
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_Drop ==
  ASSUME IndInv, NEW w \in pend, Drop(w) PROVE IndInv'
<1>. DEFINE w1 == [w EXCEPT !.dropped = @ \cup {"D"}]
<1>t. TypeOK BY DEF IndInv
<1>w. w \in Write BY <1>t DEF TypeOK
<1>f. /\ w1.b = w.b /\ w1.v = w.v /\ w1.to = w.to /\ w1.landed = w.landed
      /\ w1.dropped = w.dropped \cup {"D"} /\ w1.recAt = w.recAt /\ w1.cutAt = w.cutAt
  BY <1>w, DroppedRec
<1>0. /\ "D" \in w.to \ (w.landed \cup w.dropped)
      /\ dPath \in {"failing", "out", "back"}
      /\ pend' = (pend \ {w}) \cup {w1}
      /\ riskRec' = Min({riskRec, w.recAt})
      /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                     nextSeq, cutK, cutTgt, cutDone, members, attached, dPath,
                     dState, mark, reverted, sb, pin, acked, fails, badAdmit>>
  BY DEF Drop
<1>u. Inv_Pend BY DEF IndInv
<1>uq. \A w2 \in pend : w2.b = w.b => w2 = w BY <1>u DEF Inv_Pend, PendUniq
<1>pw. PendW(w) BY <1>u DEF Inv_Pend
<1>n. /\ riskRec \in Nat /\ w.recAt \in Nat /\ w.recAt <= MaxEpoch
      /\ nextSeq \in Nat /\ w.recAt < nextSeq
  BY <1>t, <1>w, <1>pw, Const DEF TypeOK, Write, PendW
<1>r. /\ riskRec' \in Nat /\ riskRec' <= riskRec /\ riskRec' < nextSeq
      /\ riskRec' # NoRisk
  <2>1. Min({riskRec, w.recAt}) \in {riskRec, w.recAt}
        /\ Min({riskRec, w.recAt}) <= riskRec /\ Min({riskRec, w.recAt}) <= w.recAt
    BY <1>n, Min2
  <2>. QED BY <1>0, <1>n, <2>1, NoRiskNat, Const
<1>s. SFix BY <1>0 DEF SFix
<1>i. IFMono
  <2>. SUFFICES ASSUME NEW y \in Blocks, InFlightS(y) PROVE InFlightS(y)'
    BY DEF IFMono
  <2>1. PICK w2 \in pend : w2.b = y /\ "S" \notin w2.landed BY DEF InFlightS
  <2>2. CASE w2 = w BY <2>1, <2>2, <1>0, <1>f DEF InFlightS
  <2>3. CASE w2 # w BY <2>1, <2>3, <1>0 DEF InFlightS
  <2>. QED BY <2>2, <2>3
<1>1. TypeOK' BY <1>t, T_Drop
<1>2. ~badAdmit' BY <1>0 DEF IndInv
<1>3. Inv_SourceHoldsAcked' BY <1>0 DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>0 DEF IndInv, Inv_SLinear
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend'
  <2>0. \A e : AllocFrom(e)' = AllocFrom(e) BY <1>s, SFixAlloc
  <2>1. PendUniq' BY <1>0, <1>f, <1>uq, <1>u DEF Inv_Pend, PendUniq
  <2>2. \A v1 \in {w1} : PendW(v1)' BY <1>0, <1>f, <2>0, <1>pw, RDistinct DEF PendW
  <2>3. \A w2 \in pend \ {w} : PendW(w2)' BY <1>0, <2>0, <1>u DEF Inv_Pend, PendW
  <2>4. PendGlobal' BY <1>0, <1>u DEF Inv_Pend, PendGlobal
  <2>. QED BY <1>0, <2>1, <2>2, <2>3, <2>4 DEF Inv_Pend
<1>7. Inv_Phase' BY <1>0, <1>r DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Inv_Epochs
<1>9. Inv_Heads' BY <1>r DEF Inv_Heads, HeadsTracked
<1>10. Inv_Agree' /\ Inv_Session'
  <2>1. \A e \in Nat : e <= riskRec' => e <= riskRec BY <1>r, <1>n
  <2>. QED BY <1>0, <1>s, <1>i, <1>r, <2>1, <1>t, CommonNat, KeepAgreeSession DEF IndInv
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_Complete ==
  ASSUME IndInv, NEW w \in pend, Complete(w) PROVE IndInv'
<1>. DEFINE x == w.b
<1>t. TypeOK BY DEF IndInv
<1>w. w \in Write BY <1>t DEF TypeOK
<1>0. /\ w.landed \cup w.dropped = w.to
      /\ pend' = pend \ {w}
      /\ acked' = IF w.landed # {} THEN [acked EXCEPT ![w.b] = w.v] ELSE acked
      /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                     nextSeq, cutK, cutTgt, cutDone, members, attached, dPath,
                     dState, mark, reverted, sb, pin, fails, riskRec, badAdmit>>
  BY DEF Complete
<1>u. Inv_Pend BY DEF IndInv
<1>uq. \A w2 \in pend : w2.b = x => w2 = w BY <1>u DEF Inv_Pend, PendUniq
<1>pw. PendW(w) BY <1>u DEF Inv_Pend
<1>sl. "S" \in w.landed BY <1>0, <1>pw DEF PendW
<1>hd. /\ head["S"] \in [Blocks -> Vers] /\ acked \in [Blocks -> Nat] /\ x \in Blocks
       /\ \A w2 \in pend : w2.b \in Blocks /\ w.v \in Nat /\ ver \in Nat
  BY <1>t, <1>w, Const DEF TypeOK, R, Vers, Write
<1>ak. acked' = [acked EXCEPT ![x] = w.v] BY <1>0, <1>sl
<1>s. SFix BY <1>0 DEF SFix
<1>i. IFMono
  <2>. SUFFICES ASSUME NEW y \in Blocks, InFlightS(y) PROVE InFlightS(y)'
    BY DEF IFMono
  <2>1. PICK w2 \in pend : w2.b = y /\ "S" \notin w2.landed BY DEF InFlightS
  <2>2. w2 # w BY <2>1, <1>sl
  <2>. QED BY <2>1, <2>2, <1>0 DEF InFlightS
<1>1. TypeOK' BY <1>t, T_Complete
<1>2. ~badAdmit' BY <1>0 DEF IndInv
<1>3. Inv_SourceHoldsAcked'
  <2>1. head["S"][x] = w.v BY <1>pw, <1>sl DEF PendW
  <2>. QED BY <1>0, <1>ak, <1>hd, <2>1 DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>0 DEF IndInv, Inv_SLinear
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend'
  <2>1. PendUniq' BY <1>0, <1>u DEF Inv_Pend, PendUniq
  <2>2. \A w2 \in pend \ {w} : PendW(w2)'
    <3>. SUFFICES ASSUME NEW w2 \in pend \ {w} PROVE PendW(w2)' OBVIOUS
    <3>1. w2.b # x BY <1>uq
    <3>0. w2.b \in Blocks BY <1>hd
    <3>2. acked'[w2.b] = acked[w2.b] BY <3>0, <3>1, <1>ak, <1>hd
    <3>. QED BY <1>0, <3>2, <1>u DEF Inv_Pend, PendW, AllocFrom
  <2>3. PendGlobal'
    <3>a. w.v <= ver BY <1>pw DEF PendW
    <3>b. attached BY <1>u DEF Inv_Pend, PendGlobal
    <3>. QED BY <1>0, <1>ak, <1>hd, <1>u, <3>a, <3>b DEF Inv_Pend, PendGlobal
  <2>. QED BY <1>0, <2>1, <2>2, <2>3 DEF Inv_Pend
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Inv_Epochs
<1>9. Inv_Heads'
  <2>. SUFFICES ASSUME HeadsTracked'
               PROVE  /\ \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
                      /\ \A v1 \in pend' : HeadsW(v1)'
    BY DEF Inv_Heads
  <2>0. Inv_Heads /\ HeadsTracked BY <1>0 DEF IndInv, HeadsTracked
  <2>1. head["D"][x] = head["S"][x]
    <3>1. HeadsW(w) BY <2>0 DEF Inv_Heads
    <3>2. "D" \in w.landed BY <3>1, <1>0 DEF HeadsW
    <3>. QED BY <3>1, <3>2, <1>sl DEF HeadsW
  <2>2. \A y \in Blocks : ~Pending(y)' => head'["D"][y] = head'["S"][y]
    <3>. SUFFICES ASSUME NEW y \in Blocks, ~Pending(y)' PROVE head'["D"][y] = head'["S"][y]
      OBVIOUS
    <3>1. CASE y = x BY <3>1, <2>1, <1>0
    <3>2. CASE y # x
      <4>1. ~Pending(y) BY <3>2, <1>0 DEF Pending
      <4>. QED BY <4>1, <2>0, <1>0 DEF Inv_Heads
    <3>. QED BY <3>1, <3>2
  <2>3. \A v1 \in pend' : HeadsW(v1)' BY <2>0, <1>0 DEF Inv_Heads, HeadsW
  <2>. QED BY <2>2, <2>3
<1>10. Inv_Agree' /\ Inv_Session'
  BY <1>0, <1>s, <1>i, <1>t, CommonNat, KeepAgreeSession DEF IndInv
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_Zombie ==
  ASSUME IndInv, NEW x \in Blocks, Zombie(x) PROVE IndInv'
<1>t. TypeOK BY DEF IndInv
<1>0. /\ dPath = "failing" /\ ver' = ver + 1
      /\ head' = [head EXCEPT !["D"][x] = ver + 1]
      /\ hAl' = [hAl EXCEPT !["D"] = @ \cup {x}]
      /\ riskRec' = Min({riskRec, Latest})
      /\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt,
                     cutDone, members, attached, dPath, dState, mark, reverted, sb,
                     pin, pend, acked, fails, badAdmit>>
  BY DEF Zombie
<1>n. /\ ver \in Nat /\ nextSeq \in 1..(MaxEpoch + 1) /\ riskRec \in 0..NoRisk
      /\ acked \in [Blocks -> Nat] /\ pend \subseteq Write
  BY <1>t, Const DEF TypeOK, Vers
<1>r. /\ riskRec' \in Nat /\ riskRec' <= riskRec /\ riskRec' < nextSeq /\ riskRec' # NoRisk
  <2>1. Min({riskRec, Latest}) \in {riskRec, Latest}
        /\ Min({riskRec, Latest}) <= riskRec /\ Min({riskRec, Latest}) <= Latest
    BY <1>n, Min2, NoRiskNat DEF Latest
  <2>. QED BY <1>0, <1>n, <2>1, NoRiskNat, Const DEF Latest
<1>h. head'["S"] = head["S"] /\ hAl'["S"] = hAl["S"]
  BY <1>0, <1>t, RDistinct DEF TypeOK
<1>ins. dState = "insync" BY <1>0 DEF IndInv, Inv_Phase
<1>s. SFix BY <1>0, <1>h DEF SFix
<1>i. IFMono BY <1>0 DEF IFMono, InFlightS
<1>1. TypeOK' BY <1>t, T_Zombie
<1>2. ~badAdmit' BY <1>0 DEF IndInv
<1>3. Inv_SourceHoldsAcked' BY <1>0, <1>h DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>0 DEF IndInv, Inv_SLinear
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend'
  <2>0. \A e : AllocFrom(e)' = AllocFrom(e) BY <1>s, SFixAlloc
  <2>1. Inv_Pend BY DEF IndInv
  <2>2. \A w \in pend : w.v \in Nat BY <1>n, Const DEF Write
  <2>. QED BY <1>0, <1>n, <1>h, <2>0, <2>1, <2>2 DEF Inv_Pend, PendUniq, PendW, PendGlobal
<1>7. Inv_Phase' BY <1>0, <1>r DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Inv_Epochs
<1>9. Inv_Heads' BY <1>r DEF Inv_Heads, HeadsTracked
<1>10. Inv_Agree' /\ Inv_Session'
  <2>1. \A e \in Nat : e <= riskRec' => e <= riskRec BY <1>r, <1>n
  <2>. QED BY <1>0, <1>s, <1>i, <1>r, <1>ins, <2>1, <1>t, CommonNat, KeepAgreeSession DEF IndInv
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

-----------------------------------------------------------------------------
(* D's path, the consumer, and the session bookkeeping.                    *)

\* A step that touches none of the contents, the chain, the writes or the
\* agreement inputs: what it changes is in its own lemma's hypotheses.
Quiet ==
  UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, pend, acked, badAdmit>>

LEMMA QuietKeep ==
  ASSUME IndInv, Quiet, recorded' = recorded
  PROVE  /\ ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads'
         /\ SFix /\ IFMono
BY DEF IndInv, Quiet, Inv_SourceHoldsAcked, Inv_SLinear, Inv_SReads, SC, SFix,
       IFMono, InFlightS

LEMMA I_FailBegin == ASSUME IndInv, FailBegin PROVE IndInv'
<1>0. /\ dPath = "ok" /\ "D" \in members /\ dState = "insync" /\ dPath' = "failing"
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, members,
                              attached, dState, mark, reverted, sb, riskRec>>
  BY DEF FailBegin, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>1. TypeOK' BY <1>t, T_Path DEF FailBegin
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' /\ Inv_Session' BY <1>0, <1>q, <1>t, CommonNat, KeepAgreeSession DEF IndInv, Quiet
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_FaultOut == ASSUME IndInv, FaultOut PROVE IndInv'
<1>0. /\ dPath = "failing" /\ dPath' = "out" /\ members' = members \ {"D"}
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone,
                              attached, dState, mark, reverted, sb, riskRec>>
  BY DEF FaultOut, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>ins. dState = "insync" BY <1>0 DEF IndInv, Inv_Phase
<1>1. TypeOK' BY <1>t, T_Path DEF FaultOut
<1>6. Inv_Pend' BY <1>0, RDistinct DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0, <1>ins DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' /\ Inv_Session' BY <1>0, <1>q, <1>t, CommonNat, KeepAgreeSession DEF IndInv, Quiet
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_Return == ASSUME IndInv, Return PROVE IndInv'
<1>0. /\ dPath = "out" /\ dPath' = "back"
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, members,
                              attached, dState, mark, reverted, sb, riskRec>>
  BY DEF Return, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>1. TypeOK' BY <1>t, T_Path DEF Return
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' /\ Inv_Session'
  <2>1. dState \in {"stale", "standby"} => head'["D"] = head["D"] BY <1>0 DEF Quiet
  <2>. QED BY <1>0, <1>q, <1>t, <2>1, CommonNat, KeepAgreeSession DEF IndInv, Quiet
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_MarkStale == ASSUME IndInv, MarkStale PROVE IndInv'
<1>0. /\ "D" \notin members /\ dState = "insync" /\ dPath \in {"out", "back"}
      /\ dState' = "stale"
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, members,
                              attached, dPath, mark, reverted, sb, riskRec>>
  BY DEF MarkStale, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>ph. sb = -1 /\ reverted = -1 BY <1>0 DEF IndInv, Inv_Phase
<1>1. TypeOK' BY <1>t, T_Path DEF MarkStale
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0, <1>ph DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' BY <1>0 DEF IndInv, Quiet, Inv_Agree, AgreeLow, AgreeCut, Agree,
                             Common, CutVsHead, AllocFrom, InFlightS
<1>11. Inv_Session' BY <1>0, <1>ph DEF Inv_Session, SessSel, SessBase, SessRev, SessFull, SessStandby
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_CutBegin == ASSUME IndInv, CutBegin PROVE IndInv'
<1>0. /\ attached /\ cutK = 0 /\ nextSeq <= MaxEpoch
      /\ \A w \in pend : w.cutAt >= nextSeq
      /\ cutK' = nextSeq /\ cutTgt' = InSyncR /\ cutDone' = {}
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, members, attached, dPath, dState,
                              mark, reverted, sb, riskRec>>
  BY DEF CutBegin, Quiet
<1>t. TypeOK BY DEF IndInv
<1>n. nextSeq \in Nat /\ nextSeq # 0 /\ pend \subseteq Write BY <1>t DEF TypeOK
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>e. Inv_Epochs BY DEF IndInv
<1>1. TypeOK' BY <1>t, T_CutBegin
<1>6. Inv_Pend'
  <2>1. Inv_Pend BY DEF IndInv
  <2>2. \A w \in pend : w.cutAt \in Int BY <1>n, Const DEF Write
  <2>3. \A w \in pend : PendW(w)'
    <3>. SUFFICES ASSUME NEW w \in pend PROVE PendW(w)' OBVIOUS
    <3>1. PendW(w) BY <2>1 DEF Inv_Pend
    <3>2. w.cutAt = nextSeq BY <3>1, <1>0, <1>n, <2>2 DEF PendW
    <3>. QED BY <3>1, <3>2, <1>0 DEF PendW, Quiet, AllocFrom
  <2>4. PendUniq' /\ PendGlobal' BY <1>0, <2>1 DEF Inv_Pend, PendUniq, PendGlobal, Quiet
  <2>. QED BY <2>3, <2>4, <1>0 DEF Inv_Pend, Quiet
<1>7. Inv_Phase' BY <1>0, <1>n DEF IndInv, Inv_Phase, InSyncR
<1>8. Inv_Epochs' BY <1>0, <1>e, <1>n DEF Quiet, Inv_Epochs, InSyncR
<1>9. Inv_Heads'
  <2>1. HeadsTracked' = HeadsTracked BY <1>0, <1>n, RDistinct DEF HeadsTracked, InSyncR
  <2>. QED BY <1>0, <2>1 DEF IndInv, Quiet, Inv_Heads, HeadsW, Pending
<1>10. Inv_Agree' BY <1>0 DEF IndInv, Quiet, Inv_Agree, AgreeLow, AgreeCut, Agree,
                             Common, AllocFrom, InFlightS
<1>11. Inv_Session' BY <1>0 DEF IndInv, Quiet, Inv_Session, SessSel, SessBase, SessRev,
                               SessFull, SessStandby, StandbyAt, Agree, Common,
                               AllocFrom, AllocAfter, InFlightS
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_CutAbort == ASSUME IndInv, CutAbort PROVE IndInv'
<1>0. /\ cutK # 0 /\ \E r \in cutTgt \ cutDone : ~Reachable(r)
      /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {}
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, members, attached, dPath, dState,
                              mark, reverted, sb, riskRec>>
  BY DEF CutAbort, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>d. "D" \notin cutDone /\ "D" \in cutTgt
  <2>1. PICK r \in cutTgt \ cutDone : ~Reachable(r) BY <1>0
  <2>2. r = "D" BY <2>1, <1>t, RDistinct DEF Reachable, TypeOK
  <2>. QED BY <2>1, <2>2
<1>1. TypeOK' BY <1>t, T_CutAbort
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0, <1>d DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' BY <1>0 DEF IndInv, Quiet, Inv_Agree, AgreeLow, AgreeCut, Agree,
                             Common, AllocFrom, InFlightS
<1>11. Inv_Session' BY <1>0 DEF IndInv, Quiet, Inv_Session, SessSel, SessBase, SessRev,
                               SessFull, SessStandby, StandbyAt, Agree, Common,
                               AllocFrom, AllocAfter, InFlightS
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_Retire == ASSUME IndInv, Retire PROVE IndInv'
<1>. DEFINE m == Min(recorded)
<1>0. /\ Cardinality(recorded) > Retain
      /\ recorded' = recorded \ {m}
      /\ Quiet /\ UNCHANGED <<nextSeq, cutK, cutTgt, cutDone, members, attached,
                              dPath, dState, mark, reverted, sb, riskRec>>
  BY DEF Retire, Quiet
<1>t. TypeOK BY DEF IndInv
<1>e. Inv_Epochs BY DEF IndInv
<1>n. recorded \subseteq 1..MaxEpoch /\ nextSeq \in Nat BY <1>t DEF TypeOK, Epochs
<1>fin. IsFiniteSet(recorded) BY <1>n, Const, FS_Interval, FS_Subset
<1>ne. recorded # {} BY <1>0, <1>fin, FS_EmptySet, Const
<1>mm. m \in recorded /\ \A u \in recorded : m <= u BY <1>ne, <1>n, MinProp
<1>top. nextSeq > 1 /\ nextSeq - 1 \in recorded BY <1>ne, <1>e, <1>n DEF Inv_Epochs
<1>neq. m # nextSeq - 1
  <2>. SUFFICES ASSUME m = nextSeq - 1 PROVE FALSE OBVIOUS
  <2>1. recorded = {nextSeq - 1} BY <1>mm, <1>e, <1>top, <1>n DEF Inv_Epochs
  <2>2. Cardinality(recorded) = 1 BY <2>1, FS_Singleton
  <2>. QED BY <1>0, <2>2, Const
<1>sub. recorded' \subseteq recorded /\ nextSeq - 1 \in recorded' BY <1>0, <1>top, <1>neq
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  <2>1. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SReads' /\ SFix /\ IFMono
    BY <1>0 DEF IndInv, Quiet, Inv_SourceHoldsAcked, Inv_SReads, SC, SFix, IFMono, InFlightS
  <2>2. Inv_SLinear' BY <2>1, <1>sub, SFixLinear DEF IndInv
  <2>. QED BY <2>1, <2>2
<1>1. TypeOK' BY <1>t, T_Retire
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0, <1>e, <1>sub DEF Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>cm. Common' \subseteq Common BY <1>0, <1>sub DEF Common, Quiet
<1>ag. \A e : Agree(e)' = Agree(e) BY <1>0 DEF Quiet, Agree, AllocFrom, InFlightS
<1>10. Inv_Agree' BY <1>0, <1>cm, <1>ag DEF IndInv, Quiet, Inv_Agree, AgreeLow, AgreeCut,
                                          CutVsHead, InFlightS
<1>11. Inv_Session'
  <2>0. Inv_Session BY DEF IndInv
  <2>1. SessSel' BY <1>0, <1>sub DEF SessSel
  <2>2. SessBase' BY <1>0, <1>cm, <1>ag, <2>0 DEF Inv_Session, SessBase
  <2>3. SessRev' BY <1>0, <1>cm, <1>ag, <2>0 DEF Inv_Session, SessRev, Quiet
  <2>4. SessFull' BY <1>0, <1>sub, <2>0 DEF Inv_Session, SessFull, Quiet
  <2>5. SessStandby'
    <3>1. StandbyAt(mark)' = StandbyAt(mark) BY <1>0 DEF Quiet, StandbyAt, AllocAfter, InFlightS
    <3>. QED BY <1>0, <1>cm, <1>ag, <2>0, <3>1 DEF Inv_Session, SessStandby, Quiet
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF Inv_Session
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_SelectBase == ASSUME IndInv, SelectBase PROVE IndInv'
<1>0. /\ dState = "stale" /\ dPath = "back" /\ sb = -1 /\ recorded # {} /\ cutK = 0
      /\ sb' = IF reverted # -1 THEN reverted ELSE sb'
      /\ reverted = -1 => sb' \in BaseCandidates \cup {0}
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, members,
                              attached, dPath, dState, mark, reverted, riskRec>>
  BY DEF SelectBase, Quiet
<1>t. TypeOK BY DEF IndInv
<1>rv. reverted = -1 BY <1>0, <1>t DEF IndInv, Inv_Phase, TypeOK
<1>sb. sb' \in BaseCandidates \cup {0} BY <1>0, <1>rv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>1. TypeOK' BY <1>t, T_SelectBase
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0, <1>rv DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>ag. \A e : Agree(e)' = Agree(e) BY <1>0 DEF Quiet, Agree, AllocFrom, InFlightS
<1>cm. Common' = Common BY <1>0 DEF Common, Quiet
<1>10. Inv_Agree' BY <1>0, <1>ag, <1>cm DEF IndInv, Quiet, Inv_Agree, AgreeLow, AgreeCut,
                                          CutVsHead, InFlightS
<1>11. Inv_Session'
  <2>0. Inv_Agree /\ Inv_Epochs BY DEF IndInv
  <2>1. sb' > 0 => /\ sb' \in Common /\ sb' <= riskRec /\ sb' < nextSeq
    BY <1>sb, <2>0, Shipped DEF BaseCandidates, Common, SLine, Inv_Epochs
  <2>2. riskRec \in Int /\ Common \subseteq Int BY <1>t DEF TypeOK, Common, Epochs
  <2>3. SessBase'
    BY <1>0, <1>ag, <1>cm, <2>0, <2>1, <2>2 DEF SessBase, Inv_Agree, AgreeLow
  <2>4. SessSel' /\ SessRev' /\ SessFull' /\ SessStandby'
    BY <1>0, <1>rv DEF SessSel, SessRev, SessFull, SessStandby
  <2>. QED BY <2>3, <2>4 DEF Inv_Session
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_SessionFail == ASSUME IndInv, SessionFail PROVE IndInv'
<1>0. /\ dState = "stale" /\ sb # -1 /\ sb' = -1 /\ reverted' = -1
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, members,
                              attached, dPath, dState, mark, riskRec>>
  BY DEF SessionFail, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>1. TypeOK' BY <1>t, T_SessionFail
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' BY <1>0 DEF IndInv, Quiet, Inv_Agree, AgreeLow, AgreeCut, Agree,
                             Common, CutVsHead, AllocFrom, InFlightS
<1>11. Inv_Session' BY <1>0 DEF Inv_Session, SessSel, SessBase, SessRev, SessFull, SessStandby
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_Unstage == ASSUME IndInv, Unstage PROVE IndInv'
<1>0. /\ attached /\ pend = {} /\ cutK = 0 /\ attached' = FALSE /\ members' = {}
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, dPath, dState,
                              mark, reverted, sb, riskRec>>
  BY DEF Unstage, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>1. TypeOK' BY <1>t, T_Stage DEF Unstage
<1>6. Inv_Pend' BY <1>0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal
<1>7. Inv_Phase' BY <1>0 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' /\ Inv_Session' BY <1>0, <1>q, <1>t, CommonNat, KeepAgreeSession DEF IndInv, Quiet
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_Stage == ASSUME IndInv, Stage PROVE IndInv'
<1>0. /\ ~attached /\ attached' = TRUE
      /\ members' = {"S"} \cup (IF dState = "insync" /\ dPath = "ok" THEN {"D"} ELSE {})
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, dPath, dState,
                              mark, reverted, sb, riskRec>>
  BY DEF Stage, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>p0. pend = {} BY <1>0 DEF IndInv, Inv_Pend, PendGlobal
<1>1. TypeOK' BY <1>t, T_Stage DEF Stage
<1>6. Inv_Pend' BY <1>0, <1>p0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal
<1>7. Inv_Phase' BY <1>0, RDistinct DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' /\ Inv_Session' BY <1>0, <1>q, <1>t, CommonNat, KeepAgreeSession DEF IndInv, Quiet
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

LEMMA I_StageDefer == ASSUME IndInv, StageDefer PROVE IndInv'
<1>0. /\ ~attached /\ dState = "standby" /\ dPath = "back"
      /\ attached' = TRUE /\ members' = {"S"}
      /\ Quiet /\ UNCHANGED <<recorded, nextSeq, cutK, cutTgt, cutDone, dPath, dState,
                              mark, reverted, sb, riskRec>>
  BY DEF StageDefer, Quiet
<1>t. TypeOK BY DEF IndInv
<1>q. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads' /\ SFix /\ IFMono
  BY <1>0, QuietKeep
<1>p0. pend = {} BY <1>0 DEF IndInv, Inv_Pend, PendGlobal
<1>1. TypeOK' BY <1>t, T_Stage DEF StageDefer
<1>6. Inv_Pend' BY <1>0, <1>p0 DEF IndInv, Quiet, Inv_Pend, PendUniq, PendW, PendGlobal
<1>7. Inv_Phase' BY <1>0, RDistinct DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>0 DEF IndInv, Quiet, Inv_Epochs
<1>9. Inv_Heads' BY <1>0 DEF IndInv, Quiet, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' /\ Inv_Session'
  <2>1. dState \in {"stale", "standby"} => head'["D"] = head["D"] BY <1>0 DEF Quiet
  <2>. QED BY <1>0, <1>q, <1>t, <2>1, CommonNat, KeepAgreeSession DEF IndInv, Quiet
<1>. QED BY <1>1, <1>q, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

-----------------------------------------------------------------------------
(* Cuts.                                                                    *)

LEMMA AllocFromMem ==
  \A e, x : x \in AllocFrom(e) <=> (x \in hAl["S"] \/ \E f \in sPres["S"] : f >= e /\ x \in sAl["S"][f])
BY DEF AllocFrom
LEMMA AllocFromMemP ==
  \A e, x : x \in AllocFrom(e)' <=> (x \in hAl'["S"] \/ \E f \in sPres'["S"] : f >= e /\ x \in sAl'["S"][f])
BY DEF AllocFrom
LEMMA AllocAfterMem ==
  \A e, x : x \in AllocAfter(e) <=> (x \in hAl["S"] \/ \E f \in sPres["S"] : f > e /\ x \in sAl["S"][f])
BY DEF AllocAfter
LEMMA AllocAfterMemP ==
  \A e, x : x \in AllocAfter(e)' <=> (x \in hAl'["S"] \/ \E f \in sPres'["S"] : f > e /\ x \in sAl'["S"][f])
BY DEF AllocAfter

LEMMA AgreeSame ==
  ASSUME NEW e, sCon'["D"][e] = sCon["D"][e], sCon'["S"][e] = sCon["S"][e],
         AllocFrom(e)' = AllocFrom(e), \A x \in Blocks : InFlightS(x)' = InFlightS(x)
  PROVE  Agree(e)' = Agree(e)
BY DEF Agree

LEMMA StandbySame ==
  ASSUME NEW m, head'["D"] = head["D"], sCon'["S"][m] = sCon["S"][m],
         AllocAfter(m)' = AllocAfter(m), \A x \in Blocks : InFlightS(x)' = InFlightS(x)
  PROVE  StandbyAt(m)' = StandbyAt(m)
BY DEF StandbyAt

\* S snapshots its head under a fresh name above all of its others.
LEMMA SnapS ==
  ASSUME NEW k \in Epochs, Inv_SLinear, Inv_SReads, TySnap, TyHead,
         k \notin sPres["S"], \A f \in sPres["S"] : f < k,
         sPres'["S"] = sPres["S"] \cup {k},
         sCon'["S"] = [sCon["S"] EXCEPT ![k] = head["S"]],
         sAl'["S"] = [sAl["S"] EXCEPT ![k] = hAl["S"]],
         sPar'["S"] = [sPar["S"] EXCEPT ![k] = hPar["S"]],
         hPar'["S"] = k, hAl'["S"] = {}, head'["S"] = head["S"],
         recorded' \subseteq sPres'["S"]
  PROVE  /\ Inv_SLinear' /\ Inv_SReads'
         /\ \A e \in Int : e <= k => AllocFrom(e)' = AllocFrom(e)
         /\ \A e \in Int : e < k => AllocAfter(e)' = AllocAfter(e)
         /\ \A f \in sPres["S"] : sCon'["S"][f] = sCon["S"][f]
         /\ \A f \in Epochs : f # k => sCon'["S"][f] = sCon["S"][f]
<1>0. /\ sCon["S"] \in [Epochs -> [Blocks -> Vers]] /\ sAl["S"] \in [Epochs -> SUBSET Blocks]
      /\ sPar["S"] \in [Epochs -> 0..MaxEpoch] /\ sPres["S"] \subseteq Epochs
      /\ head["S"] \in [Blocks -> Vers] /\ hAl["S"] \subseteq Blocks
      /\ hPar["S"] \in 0..MaxEpoch
  BY DEF TySnap, TyHead, R
<1>k. k \in Nat /\ k >= 1 BY DEF Epochs
<1>c. /\ sCon'["S"][k] = head["S"] /\ sAl'["S"][k] = hAl["S"] /\ sPar'["S"][k] = hPar["S"]
      /\ \A f \in Epochs : f # k => /\ sCon'["S"][f] = sCon["S"][f] /\ sAl'["S"][f] = sAl["S"][f]
                                    /\ sPar'["S"][f] = sPar["S"][f]
  BY <1>0
<1>sp. \A f \in sPres["S"] : f \in Epochs /\ f \in Int /\ f # k BY <1>0 DEF Epochs
<1>1. Inv_SLinear'
  <2>0. Inv_SLinear BY DEF Inv_SLinear
  <2>h. hPar["S"] \in sPres["S"] \cup {0} /\ \A f \in sPres["S"] : f <= hPar["S"]
    BY DEF Inv_SLinear
  <2>1. \A e \in sPres'["S"] :
          /\ sPar'["S"][e] \in sPres'["S"] \cup {0} /\ sPar'["S"][e] < e
          /\ \A f \in sPres'["S"] : f < e => f <= sPar'["S"][e]
    <3>. SUFFICES ASSUME NEW e \in sPres'["S"]
                 PROVE  /\ sPar'["S"][e] \in sPres'["S"] \cup {0} /\ sPar'["S"][e] < e
                        /\ \A f \in sPres'["S"] : f < e => f <= sPar'["S"][e]
      OBVIOUS
    <3>1. CASE e = k
      <4>1. hPar["S"] < k BY <2>h, <1>k, <1>sp, <1>0
      <4>. QED BY <3>1, <1>c, <2>h, <4>1, <1>k, <1>0, <1>sp
    <3>2. CASE e # k
      <4>1. e \in sPres["S"] BY <3>2
      <4>2. /\ sPar["S"][e] \in sPres["S"] \cup {0} /\ sPar["S"][e] < e
            /\ \A f \in sPres["S"] : f < e => f <= sPar["S"][e]
        BY <4>1 DEF Inv_SLinear
      <4>3. e < k /\ e \in Epochs BY <4>1, <1>sp
      <4>4. sPar'["S"][e] = sPar["S"][e] BY <4>3, <1>c
      <4>5. \A f \in sPres'["S"] : f < e => f \in sPres["S"] BY <4>3, <1>k, <1>sp
      <4>. QED BY <4>2, <4>4, <4>5
    <3>. QED BY <3>1, <3>2
  <2>2. hPar'["S"] \in sPres'["S"] \cup {0} /\ \A f \in sPres'["S"] : f <= hPar'["S"]
    BY <1>sp, <1>k
  <2>. QED BY <2>1, <2>2 DEF Inv_SLinear
<1>2. Inv_SReads'
  <2>0. /\ \A t \in sPres["S"] : \A a \in sPres["S"] \cup {0} : \A x \in Blocks :
             (a < t /\ \A f \in sPres["S"] : (a < f /\ f <= t) => x \notin sAl["S"][f])
               => sCon["S"][t][x] = SC(a)[x]
        /\ \A a \in sPres["S"] \cup {0} : \A x \in Blocks :
             (x \notin hAl["S"] /\ \A f \in sPres["S"] : a < f => x \notin sAl["S"][f])
               => head["S"][x] = SC(a)[x]
    BY DEF Inv_SReads
  <2>sc. \A a \in sPres["S"] \cup {0} : SC(a)' = SC(a) BY <1>c, <1>sp DEF SC
  <2>1. \A t \in sPres'["S"] : \A a \in sPres'["S"] \cup {0} : \A x \in Blocks :
          (a < t /\ \A f \in sPres'["S"] : (a < f /\ f <= t) => x \notin sAl'["S"][f])
            => sCon'["S"][t][x] = SC(a)'[x]
    <3>. SUFFICES ASSUME NEW t \in sPres'["S"], NEW a \in sPres'["S"] \cup {0}, NEW x \in Blocks,
                        a < t, \A f \in sPres'["S"] : (a < f /\ f <= t) => x \notin sAl'["S"][f]
                 PROVE  sCon'["S"][t][x] = SC(a)'[x]
      OBVIOUS
    <3>a. a \in sPres["S"] \cup {0} /\ a # k
      <4>1. a # k BY <1>sp, <1>k
      <4>. QED BY <4>1
    <3>1. CASE t = k
      <4>1. x \notin hAl["S"] BY <3>1, <1>c, <1>k
      <4>2. \A f \in sPres["S"] : a < f => x \notin sAl["S"][f] BY <3>1, <1>c, <1>sp
      <4>3. head["S"][x] = SC(a)[x] BY <2>0, <3>a, <4>1, <4>2
      <4>. QED BY <3>1, <1>c, <4>3, <2>sc, <3>a
    <3>2. CASE t # k
      <4>0. t \in sPres["S"] BY <3>2
      <4>1. \A f \in sPres["S"] : (a < f /\ f <= t) => x \notin sAl["S"][f] BY <1>c, <1>sp
      <4>2. sCon["S"][t][x] = SC(a)[x] BY <2>0, <3>a, <4>0, <4>1
      <4>. QED BY <4>0, <4>2, <1>c, <1>sp, <2>sc, <3>a
    <3>. QED BY <3>1, <3>2
  <2>2. \A a \in sPres'["S"] \cup {0} : \A x \in Blocks :
          (x \notin hAl'["S"] /\ \A f \in sPres'["S"] : a < f => x \notin sAl'["S"][f])
            => head'["S"][x] = SC(a)'[x]
    <3>. SUFFICES ASSUME NEW a \in sPres'["S"] \cup {0}, NEW x \in Blocks,
                        \A f \in sPres'["S"] : a < f => x \notin sAl'["S"][f]
                 PROVE  head'["S"][x] = SC(a)'[x]
      OBVIOUS
    <3>1. CASE a = k BY <3>1, <1>c, <1>k DEF SC
    <3>2. CASE a # k
      <4>0. a \in sPres["S"] \cup {0} BY <3>2
      <4>1. a < k BY <4>0, <1>k
      <4>2. x \notin hAl["S"] BY <4>1, <1>c
      <4>3. \A f \in sPres["S"] : a < f => x \notin sAl["S"][f] BY <1>c, <1>sp
      <4>. QED BY <2>0, <4>0, <4>2, <4>3, <2>sc
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2 DEF Inv_SReads
<1>3. \A e \in Int : e <= k => AllocFrom(e)' = AllocFrom(e)
  <2>. SUFFICES ASSUME NEW e \in Int, e <= k PROVE AllocFrom(e)' = AllocFrom(e) OBVIOUS
  <2>1. \A x : x \in AllocFrom(e)' <=> x \in AllocFrom(e)
    BY AllocFromMem, AllocFromMemP, <1>c, <1>sp
  <2>. QED BY <2>1
<1>4. \A e \in Int : e < k => AllocAfter(e)' = AllocAfter(e)
  <2>. SUFFICES ASSUME NEW e \in Int, e < k PROVE AllocAfter(e)' = AllocAfter(e) OBVIOUS
  <2>1. \A x : x \in AllocAfter(e)' <=> x \in AllocAfter(e)
    BY AllocAfterMem, AllocAfterMemP, <1>c, <1>sp
  <2>. QED BY <2>1
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>c, <1>sp

\* D's head against S's at the next name, while D took every write: the
\* cut D is about to take agrees with S's (or with S's head, before S cut).
LEMMA HeadsAtCut ==
  ASSUME Inv_Heads, HeadsTracked, Inv_Pend, Inv_SReads, Inv_Epochs, TypeOK, cutK # 0
  PROVE  \A x \in Blocks : x \notin AllocFrom(nextSeq) =>
           /\ nextSeq \in sPres["S"] => (head["D"][x] = sCon["S"][nextSeq][x] \/ InFlightS(x))
           /\ nextSeq \notin sPres["S"] => (head["D"][x] = head["S"][x] \/ InFlightS(x))
<1>. SUFFICES ASSUME NEW x \in Blocks, x \notin AllocFrom(nextSeq), ~InFlightS(x)
             PROVE  /\ nextSeq \in sPres["S"] => head["D"][x] = sCon["S"][nextSeq][x]
                    /\ nextSeq \notin sPres["S"] => head["D"][x] = head["S"][x]
  OBVIOUS
<1>1. ~Pending(x)
  <2>. SUFFICES ASSUME NEW w \in pend, w.b = x PROVE FALSE BY DEF Pending
  <2>1. "S" \in w.landed BY ~InFlightS(x) DEF InFlightS
  <2>2. PendW(w) BY DEF Inv_Pend
  <2>3. w.cutAt = nextSeq BY <2>2 DEF PendW
  <2>. QED BY <2>1, <2>2, <2>3 DEF PendW
<1>2. head["D"][x] = head["S"][x] BY <1>1 DEF Inv_Heads
<1>3. x \notin hAl["S"] BY AllocFromMem
<1>4. CASE nextSeq \in sPres["S"]
  <2>1. \A f \in sPres["S"] : ~(nextSeq < f) BY DEF Inv_Epochs, TypeOK, Epochs, R
  <2>2. head["S"][x] = SC(nextSeq)[x] BY <1>4, <1>3, <2>1 DEF Inv_SReads
  <2>3. nextSeq # 0 BY DEF TypeOK
  <2>. QED BY <1>2, <1>4, <2>2, <2>3 DEF SC
<1>5. CASE nextSeq \notin sPres["S"] BY <1>2, <1>5
<1>. QED BY <1>4, <1>5

LEMMA I_CutOneD == ASSUME IndInv, CutOne("D") PROVE IndInv'
<1>. DEFINE k == cutK
<1>t. TypeOK BY DEF IndInv
<1>e. Inv_Epochs BY DEF IndInv
<1>g. /\ cutK # 0 /\ "D" \in cutTgt \ cutDone /\ cutDone' = cutDone \cup {"D"}
      /\ UNCHANGED <<ver, head, recorded, nextSeq, cutK, cutTgt, members, attached,
                     dPath, dState, mark, reverted, sb, pin, pend, acked, fails,
                     riskRec, badAdmit>>
  BY DEF CutOne
<1>kD. k \notin sPres["D"] /\ k = nextSeq BY <1>g, <1>e DEF Inv_Epochs
<1>0. SnapHead("D", k) BY <1>g, <1>kD DEF CutOne
<1>ty. /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]] /\ sPres \in [R -> SUBSET Epochs]
       /\ sAl \in [R -> [Epochs -> SUBSET Blocks]] /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]]
       /\ hAl \in [R -> SUBSET Blocks] /\ hPar \in [R -> 0..MaxEpoch]
       /\ head \in [R -> [Blocks -> Vers]] /\ k \in Epochs
  BY <1>t, <1>g DEF TypeOK, Epochs
<1>sn. /\ sPres'["D"] = sPres["D"] \cup {k} /\ sCon'["D"][k] = head["D"]
       /\ \A f \in Epochs : f # k => sCon'["D"][f] = sCon["D"][f]
  BY <1>0, <1>ty DEF SnapHead, R
<1>s. SFix BY <1>0, <1>g, <1>ty, RDistinct DEF SnapHead, SFix
<1>al. \A e : AllocFrom(e)' = AllocFrom(e) /\ AllocAfter(e)' = AllocAfter(e) BY <1>s, SFixAlloc
<1>if. \A x \in Blocks : InFlightS(x)' = InFlightS(x) BY <1>g DEF InFlightS
<1>sc. sCon'["S"] = sCon["S"] BY <1>s DEF SFix
<1>1. TypeOK' BY <1>t, T_CutOne, RDistinct DEF R
<1>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>g, <1>s DEF IndInv, Inv_SourceHoldsAcked, SFix
<1>4. Inv_SLinear' BY <1>s, <1>g, SFixLinear DEF IndInv
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend' BY <1>g, <1>s, <1>al DEF IndInv, Inv_Pend, PendUniq, PendW, PendGlobal, SFix
<1>7. Inv_Phase' BY <1>g DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>g, <1>e, <1>s, <1>sn, <1>kD DEF Inv_Epochs, SFix
<1>9. Inv_Heads' BY <1>g DEF IndInv, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>cm. Common' = Common
  <2>1. k \notin recorded BY <1>kD, <1>e DEF Inv_Epochs
  <2>. QED BY <2>1, <1>sn, <1>s, <1>g DEF Common, SFix
<1>agl. \A e \in Common : Agree(e)' = Agree(e)
  <2>. SUFFICES ASSUME NEW e \in Common PROVE Agree(e)' = Agree(e) OBVIOUS
  <2>1. e \in Epochs /\ e # k BY <1>kD, <1>e, <1>ty DEF Common, Inv_Epochs, Epochs, R
  <2>. QED BY <2>1, <1>sn, <1>sc, <1>al, <1>if, AgreeSame
<1>10. Inv_Agree'
  <2>1. AgreeLow' BY <1>cm, <1>agl, <1>g DEF IndInv, Inv_Agree, AgreeLow
  <2>2. AgreeCut'
    <3>. SUFFICES ASSUME riskRec = NoRisk
                 PROVE  IF k \in sPres["S"] THEN Agree(k)' ELSE CutVsHead'
      BY <1>g, <1>s DEF AgreeCut, SFix
    <3>0. HeadsTracked BY <1>g DEF HeadsTracked
    <3>1. \A x \in Blocks : x \notin AllocFrom(nextSeq) =>
            /\ nextSeq \in sPres["S"] => (head["D"][x] = sCon["S"][nextSeq][x] \/ InFlightS(x))
            /\ nextSeq \notin sPres["S"] => (head["D"][x] = head["S"][x] \/ InFlightS(x))
      BY <3>0, <1>g, HeadsAtCut DEF IndInv
    <3>2. CASE k \in sPres["S"]
      <4>. SUFFICES ASSUME NEW x \in Blocks, x \notin AllocFrom(k)'
                   PROVE  sCon'["D"][k][x] = sCon'["S"][k][x] \/ InFlightS(x)'
        BY <1>g, <3>2 DEF Agree
      <4>1. x \notin AllocFrom(nextSeq) BY <1>g, <1>al, <1>kD
      <4>2. head["D"][x] = sCon["S"][nextSeq][x] \/ InFlightS(x) BY <3>1, <3>2, <4>1, <1>kD
      <4>. QED BY <4>2, <1>kD, <1>sn, <1>sc, <1>if
    <3>3. CASE k \notin sPres["S"]
      <4>1. AllocFrom(nextSeq) = hAl["S"]
        <5>1. \A f \in sPres["S"] : ~(f >= nextSeq) BY <3>3, <1>kD, <1>e, <1>ty DEF Inv_Epochs, Epochs, R
        <5>. QED BY <5>1, AllocFromMem
      <4>. SUFFICES ASSUME NEW x \in Blocks, x \notin hAl'["S"]
                   PROVE  sCon'["D"][k][x] = head'["S"][x] \/ InFlightS(x)'
        BY <1>g, <3>3 DEF CutVsHead
      <4>2. x \notin AllocFrom(nextSeq) BY <4>1, <1>s DEF SFix
      <4>3. head["D"][x] = head["S"][x] \/ InFlightS(x) BY <3>1, <3>3, <4>2, <1>kD
      <4>. QED BY <4>3, <1>kD, <1>sn, <1>s, <1>if DEF SFix
    <3>. QED BY <3>2, <3>3
  <2>. QED BY <2>1, <2>2 DEF Inv_Agree
<1>11. Inv_Session'
  <2>1. dState = "insync" \/ (dState = "stale" /\ sb = -1 /\ reverted = -1)
    BY <1>g DEF IndInv, Inv_Phase
  <2>. QED BY <2>1, <1>g DEF Inv_Session, SessSel, SessBase, SessRev, SessFull, SessStandby
<1>. QED BY <1>1, <1>2, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_CutOneS == ASSUME IndInv, CutOne("S") PROVE IndInv'
<1>. DEFINE k == cutK
<1>t. TypeOK BY DEF IndInv
<1>e. Inv_Epochs BY DEF IndInv
<1>g. /\ cutK # 0 /\ "S" \in cutTgt \ cutDone /\ cutDone' = cutDone \cup {"S"}
      /\ UNCHANGED <<ver, head, recorded, nextSeq, cutK, cutTgt, members, attached,
                     dPath, dState, mark, reverted, sb, pin, pend, acked, fails,
                     riskRec, badAdmit>>
  BY DEF CutOne
<1>k. k = nextSeq /\ k \in Epochs BY <1>g, <1>e, <1>t DEF Inv_Epochs, TypeOK, Epochs
<1>if. \A x \in Blocks : InFlightS(x)' = InFlightS(x) BY <1>g DEF InFlightS
<1>pw. \A w \in pend : w.cutAt \in Int BY <1>t DEF TypeOK, Write
<1>ns. nextSeq \in 1..(MaxEpoch + 1) BY <1>t DEF TypeOK
<1>1. TypeOK' BY <1>t, T_CutOne, RDistinct DEF R
<1>A. CASE k \in sPres["S"]
  <2>0. UNCHANGED <<sPres, sCon, sAl, sPar, hPar, hAl>> BY <1>A DEF CutOne
  <2>2. ~badAdmit' /\ Inv_SourceHoldsAcked' /\ Inv_SLinear' /\ Inv_SReads'
    BY <1>g, <2>0 DEF IndInv, Inv_SourceHoldsAcked, Inv_SLinear, Inv_SReads, SC
  <2>6. Inv_Pend' BY <1>g, <2>0 DEF IndInv, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
  <2>7. Inv_Phase' BY <1>g DEF IndInv, Inv_Phase
  <2>8. Inv_Epochs' BY <1>g, <1>e, <1>A, <2>0 DEF Inv_Epochs
  <2>9. Inv_Heads' BY <1>g DEF IndInv, Inv_Heads, HeadsTracked, HeadsW, Pending
  <2>10. Inv_Agree' BY <1>g, <2>0, RDistinct DEF IndInv, Inv_Agree, AgreeLow, AgreeCut, Agree,
                                     Common, CutVsHead, AllocFrom, InFlightS
  <2>11. Inv_Session' BY <1>g, <2>0 DEF IndInv, Inv_Session, SessSel, SessBase, SessRev,
                                       SessFull, SessStandby, StandbyAt, Agree, Common,
                                       AllocFrom, AllocAfter, InFlightS
  <2>. QED BY <1>1, <2>2, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11 DEF IndInv
<1>B. CASE k \notin sPres["S"]
  <2>0. SnapHead("S", k) BY <1>B, <1>g DEF CutOne
  <2>ty. TySnap /\ TyHead BY <1>t, TypeOKSplit
  <2>tf. /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]] /\ sPres \in [R -> SUBSET Epochs]
         /\ sAl \in [R -> [Epochs -> SUBSET Blocks]] /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]]
         /\ hAl \in [R -> SUBSET Blocks] /\ hPar \in [R -> 0..MaxEpoch]
    BY <1>t DEF TypeOK
  <2>below. \A f \in sPres["S"] : f < k BY <1>B, <1>k, <1>e, <2>tf DEF Inv_Epochs, Epochs
  <2>u. /\ sPres'["S"] = sPres["S"] \cup {k}
        /\ sCon'["S"] = [sCon["S"] EXCEPT ![k] = head["S"]]
        /\ sAl'["S"] = [sAl["S"] EXCEPT ![k] = hAl["S"]]
        /\ sPar'["S"] = [sPar["S"] EXCEPT ![k] = hPar["S"]]
        /\ hPar'["S"] = k /\ hAl'["S"] = {} /\ head'["S"] = head["S"]
        /\ sPres'["D"] = sPres["D"] /\ sCon'["D"] = sCon["D"] /\ head'["D"] = head["D"]
    BY <2>0, <2>tf, <1>g, RDistinct DEF SnapHead
  <2>rec. recorded' \subseteq sPres'["S"] BY <2>u, <1>g DEF IndInv, Inv_SLinear
  <2>sn. /\ Inv_SLinear' /\ Inv_SReads'
         /\ \A e \in Int : e <= k => AllocFrom(e)' = AllocFrom(e)
         /\ \A e \in Int : e < k => AllocAfter(e)' = AllocAfter(e)
         /\ \A f \in Epochs : f # k => sCon'["S"][f] = sCon["S"][f]
    BY <1>k, <2>ty, <1>B, <2>below, <2>u, <2>rec, SnapS DEF IndInv
  <2>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>g, <2>u DEF IndInv, Inv_SourceHoldsAcked
  <2>6. Inv_Pend'
    <3>0. Inv_Pend BY DEF IndInv
    <3>1. \A w \in pend : PendW(w)'
      <4>. SUFFICES ASSUME NEW w \in pend PROVE PendW(w)' OBVIOUS
      <4>1. PendW(w) BY <3>0 DEF Inv_Pend
      <4>2. w.cutAt = k /\ w.cutAt \in Int BY <4>1, <1>g, <1>k, <1>pw DEF PendW
      <4>3. AllocFrom(w.cutAt)' = AllocFrom(w.cutAt) BY <4>2, <2>sn, <1>k DEF Epochs
      <4>. QED BY <4>1, <4>3, <1>g, <2>u DEF PendW
    <3>. QED BY <3>0, <3>1, <1>g DEF Inv_Pend, PendUniq, PendGlobal
  <2>7. Inv_Phase' BY <1>g DEF IndInv, Inv_Phase
  <2>8. Inv_Epochs' BY <1>g, <1>e, <1>k, <2>u, <2>tf DEF Inv_Epochs, Epochs
  <2>9. Inv_Heads' BY <1>g, <2>u DEF IndInv, Inv_Heads, HeadsTracked, HeadsW, Pending
  <2>cm. Common' = Common /\ \A e \in Common : e \in Epochs /\ e < k
    <3>1. k \notin recorded BY <1>k, <1>e DEF Inv_Epochs
    <3>2. recorded \subseteq Epochs BY <1>t DEF TypeOK
    <3>. QED BY <3>1, <3>2, <2>u, <1>g, <1>k, <1>e DEF Common, Inv_Epochs, Epochs
  <2>ag. \A e \in Epochs : e < k => Agree(e)' = Agree(e)
    <3>. SUFFICES ASSUME NEW e \in Epochs, e < k PROVE Agree(e)' = Agree(e) OBVIOUS
    <3>1. e \in Int /\ e # k BY DEF Epochs
    <3>2. sCon'["S"][e] = sCon["S"][e] BY <3>1, <2>sn
    <3>3. AllocFrom(e)' = AllocFrom(e) BY <3>1, <2>sn
    <3>4. sCon'["D"][e] = sCon["D"][e] BY <2>u
    <3>. QED BY <3>2, <3>3, <3>4, <1>if, AgreeSame
  <2>10. Inv_Agree'
    <3>1. AgreeLow' BY <2>cm, <2>ag, <1>g DEF IndInv, Inv_Agree, AgreeLow
    <3>2. AgreeCut'
      <4>. SUFFICES ASSUME "D" \in cutDone, riskRec = NoRisk PROVE Agree(k)'
        BY <1>g, <2>u, RDistinct DEF AgreeCut
      <4>1. CutVsHead BY <1>B, <1>g DEF IndInv, Inv_Agree, AgreeCut
      <4>2. AllocFrom(k)' = hAl["S"]
        <5>1. \A x : x \in AllocFrom(k)' <=> x \in hAl["S"]
          <6>0. sAl'["S"][k] = hAl["S"] BY <2>u, <2>tf, <1>k DEF R
          <6>1. \A f \in sPres["S"] : ~(f >= k) BY <2>below, <2>tf, <1>k DEF Epochs, R
          <6>2. \A f \in sPres'["S"] : f >= k => f = k BY <6>1, <2>u
          <6>3. \A x : x \in AllocFrom(k)' <=>
                  (x \in hAl'["S"] \/ \E f \in sPres'["S"] : f >= k /\ x \in sAl'["S"][f])
            BY <1>g DEF AllocFrom
          <6>. QED BY <6>0, <6>2, <6>3, <2>u, <1>k DEF Epochs
        <5>. QED BY <5>1
      <4>3. sCon'["S"][k] = head["S"] BY <2>u, <2>tf, <1>k DEF Epochs, R
      <4>. SUFFICES ASSUME NEW x \in Blocks, x \notin AllocFrom(k)'
                   PROVE  sCon'["D"][k][x] = sCon'["S"][k][x] \/ InFlightS(x)'
        BY <1>g DEF Agree
      <4>4. x \notin hAl["S"] BY <4>2
      <4>. QED BY <4>1, <4>3, <4>4, <2>u, <1>if DEF CutVsHead
    <3>. QED BY <3>1, <3>2 DEF Inv_Agree
  <2>11. Inv_Session'
    <3>0. Inv_Session BY DEF IndInv
    <3>1. SessSel' /\ SessFull' BY <3>0, <1>g, <2>u DEF Inv_Session, SessSel, SessFull
    <3>2. SessBase'
      <4>1. (dState = "stale" /\ sb > 0) => sb \in Epochs /\ sb < k
        BY <3>0, <1>k, <1>t, <1>ns DEF Inv_Session, SessBase, TypeOK, Epochs
      <4>. SUFFICES ASSUME dState = "stale", sb > 0
                   PROVE  /\ sb < nextSeq /\ Agree(sb)' /\ \A e \in Common' : e <= sb => Agree(e)'
        BY <1>g DEF SessBase
      <4>2. Agree(sb) /\ \A e \in Common : e <= sb => Agree(e) BY <3>0 DEF Inv_Session, SessBase
      <4>3. Agree(sb)' BY <1>g, <4>1, <4>2, <2>ag
      <4>4. \A e \in Common' : e <= sb => Agree(e)' BY <4>2, <2>ag, <2>cm
      <4>. QED BY <4>1, <4>3, <4>4, <1>k
    <3>3. SessRev' BY <3>0, <2>ag, <2>cm, <1>g, <2>u DEF Inv_Session, SessRev
    <3>4. SessStandby'
      <4>. SUFFICES ASSUME dState = "standby"
                   PROVE  /\ mark \in 1..(nextSeq - 1) /\ StandbyAt(mark)'
                          /\ \A e \in sPres["D"] : e <= mark
                          /\ \A e \in Common' : Agree(e)'
        BY <1>g, <2>u DEF SessStandby
      <4>1. mark \in 1..(nextSeq - 1) BY <3>0 DEF Inv_Session, SessStandby
      <4>2. mark \in Epochs /\ mark \in Int /\ mark < k /\ mark # k BY <4>1, <1>k, <1>ns, Const DEF Epochs
      <4>3. StandbyAt(mark)' = StandbyAt(mark)
        BY <1>g, <4>2, <2>sn, <2>u, <1>if, StandbySame
      <4>. QED BY <3>0, <4>1, <4>3, <2>ag, <2>cm DEF Inv_Session, SessStandby
    <3>. QED BY <3>1, <3>2, <3>3, <3>4 DEF Inv_Session
  <2>. QED BY <1>1, <2>2, <2>sn, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11 DEF IndInv
<1>. QED BY <1>A, <1>B

LEMMA I_CutRecord == ASSUME IndInv, CutRecord PROVE IndInv'
<1>. DEFINE k == cutK
<1>t. TypeOK BY DEF IndInv
<1>e. Inv_Epochs BY DEF IndInv
<1>g. /\ cutK # 0 /\ cutDone = cutTgt
      /\ recorded' = recorded \cup {k} /\ nextSeq' = nextSeq + 1
      /\ mark' = IF "D" \in cutTgt THEN k ELSE mark
      /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {}
      /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, members,
                     attached, dPath, dState, reverted, sb, pin, pend, acked, fails,
                     riskRec, badAdmit>>
  BY DEF CutRecord
<1>k. /\ k = nextSeq /\ k \in Nat /\ k >= 1 /\ k \in sPres["S"]
  BY <1>g, <1>e, <1>t DEF Inv_Epochs, TypeOK
<1>s. SFix BY <1>g DEF SFix
<1>if. \A x \in Blocks : InFlightS(x)' = InFlightS(x) BY <1>g DEF InFlightS
<1>ag. \A e : Agree(e)' = Agree(e) BY <1>g DEF Agree, AllocFrom, InFlightS
<1>1. TypeOK' BY <1>t, <1>e, T_CutRecord DEF Inv_Epochs
<1>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>g DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>g, <1>k DEF IndInv, Inv_SLinear
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend'
  <2>0. Inv_Pend BY DEF IndInv
  <2>1. \A w \in pend : PendW(w)'
    <3>. SUFFICES ASSUME NEW w \in pend PROVE PendW(w)' OBVIOUS
    <3>1. PendW(w) BY <2>0 DEF Inv_Pend
    <3>2. w.cutAt = nextSeq BY <3>1, <1>g DEF PendW
    <3>3. nextSeq + 1 \notin sPres["S"] BY <1>e, <1>k DEF Inv_Epochs
    <3>. QED BY <3>1, <3>2, <3>3, <1>g, <1>k DEF PendW, AllocFrom
  <2>. QED BY <2>0, <2>1, <1>g DEF Inv_Pend, PendUniq, PendGlobal
<1>7. Inv_Phase'
  <2>1. riskRec \in Int BY <1>t DEF TypeOK
  <2>. QED BY <1>g, <1>k, <2>1 DEF IndInv, Inv_Phase
<1>8. Inv_Epochs'
  <2>1. recorded \subseteq 1..(nextSeq - 1) BY <1>e DEF Inv_Epochs
  <2>2. \A e \in sPres["D"] : e < nextSeq \/ e = cutK BY <1>e DEF Inv_Epochs
  <2>3. \A e \in sPres["S"] : e <= nextSeq BY <1>e DEF Inv_Epochs
  <2>4. sPres["S"] \subseteq Int /\ sPres["D"] \subseteq Int BY <1>t DEF TypeOK, Epochs, R
  <2>. QED BY <1>g, <1>k, <2>1, <2>2, <2>3, <2>4 DEF Inv_Epochs
<1>9. Inv_Heads' BY <1>g DEF IndInv, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree'
  <2>1. \A e \in Common' : e <= riskRec => Agree(e)
    <3>. SUFFICES ASSUME NEW e \in Common', e <= riskRec PROVE Agree(e) OBVIOUS
    <3>1. CASE e \in Common BY <3>1 DEF IndInv, Inv_Agree, AgreeLow
    <3>2. CASE e \notin Common
      <4>1. e = k /\ k \in sPres["D"] BY <3>2, <1>g DEF Common
      <4>2. "D" \in cutDone BY <4>1, <1>e, <1>k DEF Inv_Epochs
      <4>3. riskRec = NoRisk
        <5>1. riskRec \in Int BY <1>t DEF TypeOK
        <5>. QED BY <4>1, <5>1, <1>k DEF IndInv, Inv_Phase
      <4>. QED BY <4>1, <4>2, <4>3, <1>g, <1>k DEF IndInv, Inv_Agree, AgreeCut
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <1>ag, <1>g DEF Inv_Agree, AgreeLow, AgreeCut
<1>11. Inv_Session'
  <2>0. Inv_Session /\ Inv_Phase BY DEF IndInv
  <2>p. (dState # "insync") /\ (sb # -1 \/ reverted # -1 \/ dState = "standby") => "D" \notin cutTgt
    BY <2>0, <1>g DEF Inv_Phase
  <2>c. ("D" \notin cutTgt) => Common' = Common
    <3>1. "D" \notin cutTgt => k \notin sPres["D"] BY <1>e, <1>g, <1>k DEF Inv_Epochs
    <3>. QED BY <3>1, <1>g DEF Common
  <2>1. SessSel' BY <2>0, <1>g DEF Inv_Session, SessSel
  <2>2. SessBase'
    <3>. SUFFICES ASSUME dState = "stale", sb > 0
                 PROVE  /\ sb < nextSeq + 1 /\ Agree(sb)' /\ \A e \in Common' : e <= sb => Agree(e)'
      BY <1>g DEF SessBase
    <3>1. sb \in Int /\ nextSeq \in Int BY <1>t DEF TypeOK
    <3>2. "D" \notin cutTgt BY <2>p, <3>1
    <3>3. Common' = Common BY <3>2, <2>c
    <3>4. sb < nextSeq /\ Agree(sb) /\ \A e \in Common : e <= sb => Agree(e)
      BY <2>0 DEF Inv_Session, SessBase
    <3>5. Agree(sb)' /\ \A e \in Common' : e <= sb => Agree(e)' BY <1>g, <3>3, <3>4, <1>ag
    <3>. QED BY <3>1, <3>4, <3>5
  <2>3. SessRev'
    <3>1. reverted \in Int BY <1>t DEF TypeOK
    <3>2. (dState = "stale" /\ reverted > 0) => sb = reverted BY <2>0, <3>1 DEF Inv_Phase
    <3>. QED BY <2>0, <2>p, <2>c, <1>ag, <1>g, <3>1, <3>2 DEF Inv_Session, SessRev
  <2>4. SessFull'
    <3>. SUFFICES ASSUME dState = "stale", reverted = 0
                 PROVE  /\ head["D"] = Zero /\ recorded' # {}
                        /\ \A e \in sPres["D"] : \A f \in recorded' : e < f
      BY <1>g DEF SessFull
    <3>1. "D" \notin cutTgt BY <2>p
    <3>2. \A e \in sPres["D"] : e < nextSeq BY <3>1, <1>e, <1>g DEF Inv_Epochs
    <3>. QED BY <2>0, <3>2, <1>g, <1>k DEF Inv_Session, SessFull
  <2>5. SessStandby'
    <3>. SUFFICES ASSUME dState = "standby"
                 PROVE  /\ mark' \in 1..(nextSeq' - 1) /\ StandbyAt(mark)'
                        /\ \A e \in sPres["D"] : e <= mark'
                        /\ \A e \in Common' : Agree(e)'
      BY <1>g DEF SessStandby
    <3>1. "D" \notin cutTgt /\ mark' = mark BY <2>p, <1>g
    <3>2. StandbyAt(mark)' = StandbyAt(mark) BY <1>g, <3>1 DEF StandbyAt, AllocAfter, InFlightS
    <3>. QED BY <2>0, <3>1, <3>2, <2>c, <1>ag, <1>g, <1>k DEF Inv_Session, SessStandby
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF Inv_Session
<1>. QED BY <1>1, <1>2, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_GCD ==
  ASSUME IndInv, NEW e0 \in Epochs, GC("D", e0) PROVE IndInv'
<1>t. TypeOK BY DEF IndInv
<1>e. Inv_Epochs BY DEF IndInv
<1>g. /\ recorded # {} /\ e0 \in sPres["D"] /\ e0 < Min(recorded)
      /\ sPres' = [sPres EXCEPT !["D"] = @ \ {e0}]
      /\ UNCHANGED <<ver, head, sCon, recorded, nextSeq, cutK, cutTgt, cutDone,
                     members, attached, dPath, dState, mark, reverted, sb, pin, pend,
                     acked, fails, riskRec, badAdmit>>
  BY DEF GC
<1>ty. /\ sPres \in [R -> SUBSET Epochs] /\ sAl \in [R -> [Epochs -> SUBSET Blocks]]
       /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]] /\ hAl \in [R -> SUBSET Blocks]
       /\ hPar \in [R -> 0..MaxEpoch] /\ recorded \subseteq 1..MaxEpoch
  BY <1>t DEF TypeOK, Epochs
<1>s. SFix
  <2>1. sAl'["S"] = sAl["S"] /\ sPar'["S"] = sPar["S"] /\ hAl'["S"] = hAl["S"]
        /\ hPar'["S"] = hPar["S"]
    BY <1>ty, RDistinct DEF GC
  <2>. QED BY <2>1, <1>g, <1>ty, RDistinct DEF SFix
<1>sp. sPres'["D"] = sPres["D"] \ {e0} BY <1>g, <1>ty DEF R
<1>n. nextSeq \in Nat /\ cutK \in Nat BY <1>t DEF TypeOK
<1>min. Min(recorded) \in recorded BY <1>g, <1>ty, MinProp, Const
<1>lt. e0 < nextSeq /\ e0 \notin recorded
  <2>1. \A f \in recorded : Min(recorded) <= f BY <1>g, <1>ty, MinProp, Const
  <2>2. Min(recorded) <= nextSeq - 1 BY <1>min, <1>e DEF Inv_Epochs
  <2>. QED BY <1>min, <2>1, <2>2, <1>g, <1>ty, <1>n DEF Epochs
<1>al. \A e : AllocFrom(e)' = AllocFrom(e) /\ AllocAfter(e)' = AllocAfter(e) BY <1>s, SFixAlloc
<1>ag. \A e : Agree(e)' = Agree(e) BY <1>g, <1>al DEF Agree, InFlightS
<1>1. TypeOK' BY <1>t, T_GC, RDistinct DEF R
<1>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>g DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>s, <1>g, SFixLinear DEF IndInv
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend' BY <1>g, <1>s, <1>al DEF IndInv, Inv_Pend, PendUniq, PendW, PendGlobal, SFix
<1>7. Inv_Phase' BY <1>g DEF IndInv, Inv_Phase
<1>8. Inv_Epochs'
  <2>1. cutK # 0 => e0 # cutK BY <1>lt, <1>e, <1>g, <1>n DEF Inv_Epochs
  <2>. QED BY <1>e, <1>g, <1>s, <1>sp, <2>1 DEF Inv_Epochs, SFix
<1>9. Inv_Heads' BY <1>g DEF IndInv, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>cm. Common' \subseteq Common BY <1>sp, <1>s, <1>g DEF Common, SFix
<1>10. Inv_Agree' BY <1>g, <1>cm, <1>ag, <1>s DEF IndInv, Inv_Agree, AgreeLow, AgreeCut,
                                                CutVsHead, InFlightS, SFix
<1>11. Inv_Session'
  <2>0. Inv_Session BY DEF IndInv
  <2>1. StandbyAt(mark)' = StandbyAt(mark) BY <1>g, <1>al DEF StandbyAt, InFlightS
  <2>. QED BY <2>0, <2>1, <1>g, <1>cm, <1>ag, <1>sp DEF Inv_Session, SessSel, SessBase,
                                                        SessRev, SessFull, SessStandby
<1>. QED BY <1>1, <1>2, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

-----------------------------------------------------------------------------
(* S's garbage collection: the chain stays linear, reads do not move.      *)

LEMMA I_GCS ==
  ASSUME IndInv, NEW e0 \in Epochs, GC("S", e0) PROVE IndInv'
<1>. DEFINE Sp == sPres["S"]
           P(e) == sPar["S"][e]
           kids == Children("S", e0)
           U == {f \in Sp : f > e0}
           SFacts == /\ Inv_SLinear' /\ Inv_SReads'
                     /\ \A e \in Int : AllocFrom(e) \subseteq AllocFrom(e)' /\ AllocAfter(e) \subseteq AllocAfter(e)'
                     /\ hAl["S"] \subseteq hAl'["S"]
<1>t. TypeOK BY DEF IndInv
<1>ty. /\ sPres \in [R -> SUBSET Epochs] /\ sAl \in [R -> [Epochs -> SUBSET Blocks]]
       /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]] /\ hAl \in [R -> SUBSET Blocks]
       /\ hPar \in [R -> 0..MaxEpoch] /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]]
       /\ recorded \subseteq Epochs /\ nextSeq \in Nat /\ cutK \in Nat
  BY <1>t DEF TypeOK
<1>tS. /\ Sp \subseteq Epochs /\ sAl["S"] \in [Epochs -> SUBSET Blocks]
       /\ sPar["S"] \in [Epochs -> 0..MaxEpoch] /\ hAl["S"] \subseteq Blocks
       /\ hPar["S"] \in 0..MaxEpoch
  BY <1>ty DEF R
<1>g. /\ recorded # {} /\ e0 \in Sp /\ e0 < Min(recorded)
      /\ sPres' = [sPres EXCEPT !["S"] = @ \ {e0}]
      /\ UNCHANGED <<ver, head, sCon, recorded, nextSeq, cutK, cutTgt, cutDone,
                     members, attached, dPath, dState, mark, reverted, sb, pin, pend,
                     acked, fails, riskRec, badAdmit>>
  BY DEF GC
<1>sp. sPres'["S"] = Sp \ {e0} /\ sPres'["D"] = sPres["D"] BY <1>g, <1>ty, RDistinct DEF R
<1>lin. /\ \A e \in Sp : P(e) \in Sp \cup {0} /\ P(e) < e /\ \A f \in Sp : f < e => f <= P(e)
        /\ hPar["S"] \in Sp \cup {0} /\ \A f \in Sp : f <= hPar["S"]
        /\ recorded \subseteq Sp
  BY DEF IndInv, Inv_SLinear
<1>e. Inv_Epochs BY DEF IndInv
<1>min. Min(recorded) \in recorded /\ \A f \in recorded : Min(recorded) <= f
  BY <1>g, <1>ty, MinProp DEF Epochs
<1>e0r. e0 \notin recorded /\ e0 \in Nat /\ e0 >= 1
  BY <1>min, <1>g, <1>ty DEF Epochs
<1>e0c. cutK # 0 => cutK # e0
  <2>1. Min(recorded) <= nextSeq - 1 BY <1>min, <1>e DEF Inv_Epochs
  <2>. QED BY <2>1, <1>g, <1>e, <1>ty, <1>min DEF Inv_Epochs, Epochs
<1>if. \A x \in Blocks : InFlightS(x)' = InFlightS(x) BY <1>g DEF InFlightS
\* Which blob absorbs e0's clusters.
<1>S. SFacts
  <2>H. CASE hPar["S"] = e0
    <3>k0. kids = {}
      <4>. SUFFICES ASSUME NEW c \in Sp, P(c) = e0 PROVE FALSE BY DEF Children
      <4>1. c > e0 BY <1>lin
      <4>2. c <= e0 BY <1>lin, <2>H
      <4>. QED BY <4>1, <4>2, <1>tS DEF Epochs
    <3>u. /\ hAl'["S"] = hAl["S"] \cup sAl["S"][e0] /\ hPar'["S"] = P(e0)
          /\ sAl' = sAl /\ sPar' = sPar
      <4>1. GC("S", e0) BY DEF GC
      <4>. QED BY <4>1, <2>H, <1>ty, <3>k0, RDistinct DEF GC, R, Children
    <3>top. \A f \in Sp : f # e0 => f < e0 BY <2>H, <1>lin, <1>tS DEF Epochs
    <3>L. Inv_SLinear'
      <4>1. \A e \in Sp \ {e0} : P(e) # e0 BY <3>k0 DEF Children
      <4>2. P(e0) \in Sp \cup {0} /\ P(e0) # e0 /\ \A f \in Sp : f < e0 => f <= P(e0)
        BY <1>lin, <1>g
      <4>. QED BY <4>1, <4>2, <3>u, <1>sp, <1>lin, <3>top, <1>e0r, <1>g DEF Inv_SLinear
    <3>R. Inv_SReads'
      <4>0. Inv_SReads BY DEF IndInv
      <4>1. \A t \in Sp \ {e0} : \A a \in (Sp \ {e0}) \cup {0} : \A x \in Blocks :
              (a < t /\ \A f \in Sp \ {e0} : (a < f /\ f <= t) => x \notin sAl["S"][f])
                => sCon["S"][t][x] = SC(a)[x]
        <5>. SUFFICES ASSUME NEW t \in Sp \ {e0}, NEW a \in (Sp \ {e0}) \cup {0}, NEW x \in Blocks,
                            a < t, \A f \in Sp \ {e0} : (a < f /\ f <= t) => x \notin sAl["S"][f]
                     PROVE  sCon["S"][t][x] = SC(a)[x]
          OBVIOUS
        <5>1. t < e0 BY <3>top
        <5>2. \A f \in Sp : (a < f /\ f <= t) => x \notin sAl["S"][f] BY <5>1, <1>tS DEF Epochs
        <5>. QED BY <4>0, <5>2 DEF Inv_SReads
      <4>2. \A a \in (Sp \ {e0}) \cup {0} : \A x \in Blocks :
              (x \notin hAl'["S"] /\ \A f \in Sp \ {e0} : a < f => x \notin sAl["S"][f])
                => head["S"][x] = SC(a)[x]
        BY <4>0, <3>u, <1>tS DEF Inv_SReads
      <4>. QED BY <4>1, <4>2, <3>u, <1>sp, <1>g DEF Inv_SReads, SC
    <3>A. \A e \in Int : AllocFrom(e) \subseteq AllocFrom(e)' /\ AllocAfter(e) \subseteq AllocAfter(e)'
      BY <3>u, <1>sp, AllocFromMem, AllocFromMemP, AllocAfterMem, AllocAfterMemP
    <3>hd. hAl["S"] \subseteq hAl'["S"] BY <3>u
    <3>. QED BY <3>L, <3>R, <3>A, <3>hd DEF SFacts
  <2>K. CASE hPar["S"] # e0
    <3>ne. U # {} /\ U \subseteq Nat
      <4>1. hPar["S"] \in U BY <2>K, <1>lin, <1>g, <1>tS DEF Epochs
      <4>. QED BY <4>1, <1>tS DEF Epochs
    <3>. DEFINE cs == Min(U)
    <3>cs. cs \in U /\ \A u \in U : cs <= u BY <3>ne, MinProp
    <3>pc. P(cs) = e0
      <4>1. cs \in Sp /\ cs > e0 BY <3>cs
      <4>2. P(cs) \in Sp \cup {0} /\ P(cs) < cs /\ e0 <= P(cs) BY <4>1, <1>lin, <1>g
      <4>3. P(cs) \in Sp BY <4>2, <1>e0r, <1>tS DEF Epochs
      <4>4. ~(P(cs) > e0) BY <4>2, <4>3, <3>cs, <1>tS DEF Epochs
      <4>. QED BY <4>2, <4>4, <1>tS, <1>e0r DEF Epochs
    <3>k1. kids = {cs}
      <4>1. cs \in kids BY <3>cs, <3>pc DEF Children
      <4>2. \A c \in kids : c = cs
        <5>. SUFFICES ASSUME NEW c \in Sp, P(c) = e0 PROVE c = cs BY DEF Children
        <5>1. c > e0 BY <1>lin
        <5>2. cs <= c BY <5>1, <3>cs
        <5>3. ~(cs < c)
          <6>. SUFFICES ASSUME cs < c PROVE FALSE OBVIOUS
          <6>1. cs <= P(c) BY <3>cs, <1>lin
          <6>. QED BY <6>1, <3>cs, <1>tS DEF Epochs
        <5>. QED BY <5>2, <5>3, <3>cs, <1>tS DEF Epochs
      <4>. QED BY <4>1, <4>2
    <3>u. /\ hAl'["S"] = hAl["S"] /\ hPar'["S"] = hPar["S"]
          /\ \A c \in Epochs : sAl'["S"][c] = IF c = cs THEN sAl["S"][c] \cup sAl["S"][e0] ELSE sAl["S"][c]
          /\ \A c \in Epochs : sPar'["S"][c] = IF c = cs THEN P(e0) ELSE P(c)
          /\ sAl'["D"] = sAl["D"] /\ sPar'["D"] = sPar["D"]
          /\ hAl'["D"] = hAl["D"] /\ hPar'["D"] = hPar["D"]
      <4>1. GC("S", e0) BY DEF GC
      <4>. QED BY <4>1, <2>K, <1>ty, <3>k1, RDistinct DEF GC, R
    <3>csE. cs \in Epochs /\ cs \in Sp /\ cs > e0 /\ cs # e0 BY <3>cs, <1>tS
    <3>between. \A f \in Sp : ~(e0 < f /\ f < cs) BY <3>cs, <1>tS DEF Epochs
    <3>L. Inv_SLinear'
      <4>1. \A e \in Sp \ {e0} : e # cs => P(e) # e0 BY <3>k1 DEF Children
      <4>2. P(e0) \in Sp \cup {0} /\ P(e0) < e0 /\ \A f \in Sp : f < e0 => f <= P(e0)
        BY <1>lin, <1>g
      <4>3. \A f \in Sp \ {e0} : f < cs => f < e0 BY <3>between, <1>tS, <3>csE DEF Epochs
      <4>4. hPar["S"] # e0 /\ hPar["S"] \in Sp BY <2>K, <1>lin, <1>tS, <3>ne DEF Epochs
      <4>. QED BY <4>1, <4>2, <4>3, <4>4, <3>u, <3>csE, <1>sp, <1>lin, <1>tS, <1>e0r, <1>g
           DEF Inv_SLinear, Epochs
    <3>R. Inv_SReads'
      <4>0. Inv_SReads BY DEF IndInv
      <4>grow. \A f \in Sp \ {e0} : sAl["S"][f] \subseteq sAl'["S"][f] /\ sAl'["S"][f] \subseteq Blocks
        BY <3>u, <1>tS
      <4>cs. sAl["S"][e0] \subseteq sAl'["S"][cs] BY <3>u, <3>csE
      <4>1. \A t \in Sp \ {e0} : \A a \in (Sp \ {e0}) \cup {0} : \A x \in Blocks :
              (a < t /\ \A f \in Sp \ {e0} : (a < f /\ f <= t) => x \notin sAl'["S"][f])
                => sCon["S"][t][x] = SC(a)[x]
        <5>. SUFFICES ASSUME NEW t \in Sp \ {e0}, NEW a \in (Sp \ {e0}) \cup {0}, NEW x \in Blocks,
                            a < t, \A f \in Sp \ {e0} : (a < f /\ f <= t) => x \notin sAl'["S"][f]
                     PROVE  sCon["S"][t][x] = SC(a)[x]
          OBVIOUS
        <5>1. \A f \in Sp : (a < f /\ f <= t) => x \notin sAl["S"][f]
          <6>. SUFFICES ASSUME NEW f \in Sp, a < f, f <= t PROVE x \notin sAl["S"][f] OBVIOUS
          <6>1. CASE f # e0 BY <6>1, <4>grow
          <6>2. CASE f = e0
            <7>1. e0 < t BY <6>2, <1>tS DEF Epochs
            <7>2. cs <= t BY <7>1, <3>cs
            <7>3. a < cs BY <6>2, <3>csE, <1>tS DEF Epochs
            <7>. QED BY <6>2, <7>2, <7>3, <4>cs, <3>csE
          <6>. QED BY <6>1, <6>2
        <5>. QED BY <4>0, <5>1 DEF Inv_SReads
      <4>2. \A a \in (Sp \ {e0}) \cup {0} : \A x \in Blocks :
              (x \notin hAl["S"] /\ \A f \in Sp \ {e0} : a < f => x \notin sAl'["S"][f])
                => head["S"][x] = SC(a)[x]
        <5>. SUFFICES ASSUME NEW a \in (Sp \ {e0}) \cup {0}, NEW x \in Blocks, x \notin hAl["S"],
                            \A f \in Sp \ {e0} : a < f => x \notin sAl'["S"][f]
                     PROVE  head["S"][x] = SC(a)[x]
          OBVIOUS
        <5>1. \A f \in Sp : a < f => x \notin sAl["S"][f]
          <6>. SUFFICES ASSUME NEW f \in Sp, a < f PROVE x \notin sAl["S"][f] OBVIOUS
          <6>1. CASE f # e0 BY <6>1, <4>grow
          <6>2. CASE f = e0
            <7>1. a < cs BY <6>2, <3>csE, <1>tS DEF Epochs
            <7>. QED BY <6>2, <7>1, <4>cs, <3>csE
          <6>. QED BY <6>1, <6>2
        <5>. QED BY <4>0, <5>1 DEF Inv_SReads
      <4>. QED BY <4>1, <4>2, <3>u, <1>sp, <1>g DEF Inv_SReads, SC
    <3>A. \A e \in Int : AllocFrom(e) \subseteq AllocFrom(e)' /\ AllocAfter(e) \subseteq AllocAfter(e)'
      <4>1. \A f \in Sp \ {e0} : sAl["S"][f] \subseteq sAl'["S"][f] BY <3>u, <1>tS
      <4>2. sAl["S"][e0] \subseteq sAl'["S"][cs] BY <3>u, <3>csE
      <4>. QED BY <4>1, <4>2, <3>u, <1>sp, <3>csE, <1>tS, AllocFromMem, AllocFromMemP,
                  AllocAfterMem, AllocAfterMemP DEF Epochs
    <3>hd. hAl["S"] \subseteq hAl'["S"] BY <3>u
    <3>. QED BY <3>L, <3>R, <3>A, <3>hd
  <2>. QED BY <2>H, <2>K
<1>S2. /\ Inv_SLinear' /\ Inv_SReads'
       /\ \A e \in Int : AllocFrom(e) \subseteq AllocFrom(e)' /\ AllocAfter(e) \subseteq AllocAfter(e)'
       /\ hAl["S"] \subseteq hAl'["S"]
  BY <1>S DEF SFacts
<1>D. sAl'["D"] = sAl["D"] /\ sPar'["D"] = sPar["D"] /\ hAl'["D"] = hAl["D"] /\ hPar'["D"] = hPar["D"]
  BY <1>ty, RDistinct DEF GC
<1>i. IFMono BY <1>if DEF IFMono
<1>1. TypeOK' BY <1>t, T_GC, RDistinct DEF R
<1>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>g DEF IndInv, Inv_SourceHoldsAcked
<1>6. Inv_Pend'
  <2>0. Inv_Pend BY DEF IndInv
  <2>1. \A w \in pend : PendW(w)'
    <3>. SUFFICES ASSUME NEW w \in pend PROVE PendW(w)' OBVIOUS
    <3>1. PendW(w) BY <2>0 DEF Inv_Pend
    <3>2. nextSeq \in sPres'["S"] => nextSeq \in sPres["S"] BY <1>sp
    <3>3. w.cutAt \in Int BY <1>t DEF TypeOK, Write
    <3>. QED BY <3>1, <3>2, <3>3, <1>S2, <1>g DEF PendW
  <2>. QED BY <2>0, <2>1, <1>g DEF Inv_Pend, PendUniq, PendGlobal
<1>7. Inv_Phase' BY <1>g DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>e, <1>g, <1>sp, <1>e0c DEF Inv_Epochs
<1>9. Inv_Heads' BY <1>g DEF IndInv, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' /\ Inv_Session'
  <2>1. cutK \in sPres["S"] => cutK \in sPres'["S"] BY <1>sp, <1>e0c, <1>e0r
  <2>2. \A y \in Blocks : y \notin hAl'["S"] => head'["S"][y] = head["S"][y] BY <1>g
  <2>. QED BY <1>g, <1>sp, <1>S2, <1>i, <2>1, <2>2, <1>t, CommonNat, KeepAgreeSessionGen DEF IndInv
<1>. QED BY <1>1, <1>2, <1>S2, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndInv

-----------------------------------------------------------------------------
(* The catch-up: revert, copy, chase.                                       *)

LEMMA MaxRec ==
  ASSUME TypeOK, Inv_Epochs, recorded # {}
  PROVE  Max(recorded) = nextSeq - 1 /\ nextSeq - 1 \in recorded /\ nextSeq > 1
<1>1. recorded \subseteq 0..MaxEpoch /\ nextSeq \in Nat BY Const DEF TypeOK, Epochs
<1>2. recorded \subseteq 1..(nextSeq - 1) BY DEF Inv_Epochs
<1>3. nextSeq > 1 BY <1>1, <1>2
<1>4. nextSeq - 1 \in recorded BY <1>3 DEF Inv_Epochs
<1>5. Max(recorded) \in recorded /\ \A u \in recorded : u <= Max(recorded)
  BY <1>1, MaxProp, Const
<1>. QED BY <1>2, <1>3, <1>4, <1>5, <1>1

\* §5 at one copy: a base-inclusive copy of S's lineage from lo to t, onto a
\* head that is S's epoch lo wherever S has not written since, is S's epoch
\* t wherever S has not written since t.
LEMMA CopyTracks ==
  ASSUME Inv_SReads, TySnap, NEW lo \in Nat, NEW t \in sPres["S"], lo <= t,
         lo = 0 \/ lo \in sPres["S"], NEW H,
         \A x \in Blocks : x \notin AllocFrom(lo) => (H[x] = SC(lo)[x] \/ InFlightS(x))
  PROVE  \A x \in Blocks : x \notin AllocAfter(t) =>
           (AfterCopy(H, lo, t)[x] = sCon["S"][t][x] \/ InFlightS(x))
<1>. SUFFICES ASSUME NEW x \in Blocks, x \notin AllocAfter(t)
             PROVE  AfterCopy(H, lo, t)[x] = sCon["S"][t][x] \/ InFlightS(x)
  OBVIOUS
<1>. DEFINE hits == {e \in CopyElems(lo, t) : x \in sAl["S"][e]}
<1>ty. /\ sPres["S"] \subseteq Epochs /\ t \in Epochs /\ t # 0 /\ t \in Nat
  BY DEF TySnap, R, Epochs
<1>0. AfterCopy(H, lo, t)[x] = IF hits = {} THEN H[x] ELSE sCon["S"][Max(hits)][x]
  BY DEF AfterCopy
<1>na. x \notin hAl["S"] /\ \A f \in sPres["S"] : f > t => x \notin sAl["S"][f]
  BY AllocAfterMem
<1>rd. \A tt \in sPres["S"] : \A a \in sPres["S"] \cup {0} :
         (a < tt /\ \A f \in sPres["S"] : (a < f /\ f <= tt) => x \notin sAl["S"][f])
           => sCon["S"][tt][x] = SC(a)[x]
  BY DEF Inv_SReads
<1>1. CASE hits # {}
  <2>1. hits \subseteq 0..MaxEpoch BY <1>ty, Const DEF CopyElems, SLine, Epochs
  <2>2. Max(hits) \in hits /\ \A u \in hits : u <= Max(hits) BY <1>1, <2>1, MaxProp, Const
  <2>. DEFINE m == Max(hits)
  <2>3. m \in sPres["S"] /\ lo <= m /\ m <= t /\ m \in Nat /\ m # 0
    BY <2>2, <1>ty DEF CopyElems, SLine, Epochs
  <2>4. \A f \in sPres["S"] : (m < f /\ f <= t) => x \notin sAl["S"][f]
    BY <2>2, <2>3, <1>ty DEF CopyElems, SLine, Epochs
  <2>5. sCon["S"][t][x] = sCon["S"][m][x]
    <3>1. CASE m = t BY <3>1
    <3>2. CASE m # t
      <4>1. m < t BY <3>2, <2>3, <1>ty
      <4>2. sCon["S"][t][x] = SC(m)[x] BY <1>rd, <4>1, <2>3, <2>4
      <4>. QED BY <4>2, <2>3 DEF SC
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <1>0, <1>1, <2>5
<1>2. CASE hits = {}
  <2>1. \A f \in sPres["S"] : (lo <= f /\ f <= t) => x \notin sAl["S"][f]
    BY <1>2 DEF CopyElems, SLine
  <2>2. x \notin AllocFrom(lo)
    <3>1. \A f \in sPres["S"] : f >= lo => x \notin sAl["S"][f] BY <2>1, <1>na, <1>ty DEF Epochs
    <3>. QED BY <3>1, <1>na, AllocFromMem
  <2>3. H[x] = SC(lo)[x] \/ InFlightS(x) BY <2>2
  <2>4. sCon["S"][t][x] = SC(lo)[x]
    <3>1. CASE lo = t BY <3>1, <1>ty DEF SC
    <3>2. CASE lo # t
      <4>1. lo < t BY <3>2, <1>ty
      <4>2. \A f \in sPres["S"] : (lo < f /\ f <= t) => x \notin sAl["S"][f] BY <2>1, <1>ty DEF Epochs
      <4>. QED BY <1>rd, <4>1, <4>2
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <1>0, <1>2, <2>3, <2>4
<1>. QED BY <1>1, <1>2

\* What CopyAlign does, in the two shapes the shipped catch-up reaches.
LEMMA CopyAlignSame ==
  ASSUME TypeOK, NEW from, NEW target, CopyAlign(from, target), from = target
  PROVE  /\ head' = [head EXCEPT !["D"] = AfterCopy(head["D"], from, target)]
         /\ UNCHANGED <<sPres, sCon, sAl, sPar, hPar>>
         /\ hAl'["S"] = hAl["S"]
<1>1. (IF BaseInclusive \/ from = 0 THEN from ELSE from + 1) = from BY Shipped
<1>. QED BY <1>1 DEF CopyAlign, TypeOK, R

LEMMA CopyAlignFresh ==
  ASSUME TypeOK, NEW from, NEW target \in Epochs, CopyAlign(from, target),
         from # target, target \notin sPres["D"]
  PROVE  /\ head' = [head EXCEPT !["D"] = AfterCopy(head["D"], from, target)]
         /\ sPres' = [sPres EXCEPT !["D"] = @ \cup {target}]
         /\ sCon' = [sCon EXCEPT !["D"][target] = AfterCopy(head["D"], from, target)]
         /\ sAl'["S"] = sAl["S"] /\ sPar'["S"] = sPar["S"] /\ hPar'["S"] = hPar["S"]
         /\ hAl'["S"] = hAl["S"]
<1>1. (IF BaseInclusive \/ from = 0 THEN from ELSE from + 1) = from BY Shipped
<1>2. /\ head' = [head EXCEPT !["D"] = AfterCopy(head["D"], from, target)]
      /\ sPres' = [sPres EXCEPT !["D"] = @ \cup {target}]
      /\ sCon' = [sCon EXCEPT !["D"][target] = AfterCopy(head["D"], from, target)]
      /\ sAl' = [sAl EXCEPT !["D"][target] = hAl["D"] \cup Copied(from, target)]
      /\ sPar' = [sPar EXCEPT !["D"][target] = hPar["D"]]
      /\ hPar' = [hPar EXCEPT !["D"] = target]
      /\ hAl' = [hAl EXCEPT !["D"] = {}]
  BY <1>1 DEF CopyAlign
<1>. QED BY <1>2, RDistinct DEF TypeOK, R

\* The facts every catch-up step's S side needs: S is untouched.
LEMMA DOnly ==
  ASSUME TypeOK, sPres'["S"] = sPres["S"], sCon'["S"] = sCon["S"], sAl'["S"] = sAl["S"],
         sPar'["S"] = sPar["S"], hPar'["S"] = hPar["S"], hAl'["S"] = hAl["S"],
         head'["S"] = head["S"]
  PROVE  SFix
BY DEF SFix

LEMMA I_RevertD == ASSUME IndInv, RevertD PROVE IndInv'
<1>t. TypeOK BY DEF IndInv
<1>g. /\ dState = "stale" /\ dPath = "back" /\ sb # -1 /\ reverted = -1 /\ reverted' = sb
      /\ UNCHANGED <<ver, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt,
                     cutDone, members, attached, dPath, dState, mark, sb, pin,
                     pend, acked, fails, riskRec, badAdmit>>
  BY DEF RevertD
<1>ty. /\ head \in [R -> [Blocks -> Vers]] /\ hAl \in [R -> SUBSET Blocks]
       /\ hPar \in [R -> 0..MaxEpoch] /\ sPres \in [R -> SUBSET Epochs]
       /\ sb \in -1..MaxEpoch /\ recorded \subseteq Epochs
  BY <1>t DEF TypeOK
<1>ph. ~(cutK # 0 /\ "D" \in cutTgt) /\ "D" \notin cutDone
  BY <1>g, <1>ty DEF IndInv, Inv_Phase, Inv_Epochs
<1>s. SFix
  <2>1. head'["S"] = head["S"] /\ hAl'["S"] = hAl["S"] /\ hPar'["S"] = hPar["S"]
        /\ sPres'["S"] = sPres["S"]
    BY <1>ty, RDistinct DEF RevertD
  <2>. QED BY <2>1, <1>g, <1>t, DOnly
<1>al. \A e : AllocFrom(e)' = AllocFrom(e) /\ AllocAfter(e)' = AllocAfter(e) BY <1>s, SFixAlloc
<1>if. \A x \in Blocks : InFlightS(x)' = InFlightS(x) BY <1>g DEF InFlightS
<1>ag. \A e : Agree(e)' = Agree(e) BY <1>g, <1>al, <1>if DEF Agree
<1>dsub. sPres'["D"] \subseteq sPres["D"] BY <1>ty, Shipped DEF RevertD
<1>cm. Common' \subseteq Common BY <1>dsub, <1>s, <1>g DEF Common, SFix
<1>1. TypeOK' BY <1>t, T_RevertD
<1>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>g, <1>s DEF IndInv, Inv_SourceHoldsAcked, SFix
<1>4. Inv_SLinear' BY <1>s, <1>g, SFixLinear DEF IndInv
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend' BY <1>g, <1>s, <1>al DEF IndInv, Inv_Pend, PendUniq, PendW, PendGlobal, SFix
<1>7. Inv_Phase' BY <1>g, <1>ty DEF IndInv, Inv_Phase
<1>8. Inv_Epochs' BY <1>g, <1>s, <1>dsub, <1>ph DEF IndInv, Inv_Epochs, SFix
<1>9. Inv_Heads' BY <1>g, <1>ph DEF Inv_Heads, HeadsTracked
<1>10. Inv_Agree' BY <1>g, <1>ph, <1>cm, <1>ag DEF IndInv, Inv_Agree, AgreeLow, AgreeCut
<1>11. Inv_Session'
  <2>0. Inv_Session BY DEF IndInv
  <2>1. SessSel' /\ SessStandby' BY <2>0, <1>g DEF Inv_Session, SessSel, SessStandby
  <2>2. CASE sb = 0
    <3>1. /\ head'["D"] = Zero
          /\ sPres'["D"] = {e \in sPres["D"] : e < Min(recorded)}
      BY <2>2, <1>ty, Shipped DEF RevertD, R
    <3>2. recorded # {} BY <2>0, <1>g DEF Inv_Session, SessSel
    <3>3. Min(recorded) \in recorded /\ \A f \in recorded : Min(recorded) <= f
      BY <3>2, <1>ty, MinProp DEF Epochs
    <3>4. SessFull'
      <4>1. \A e \in sPres'["D"] : \A f \in recorded : e < f
        BY <3>1, <3>3, <1>ty DEF Epochs, R
      <4>. QED BY <3>1, <3>2, <4>1, <1>g, <2>2 DEF SessFull
    <3>5. SessBase' /\ SessRev' BY <2>2, <1>g DEF SessBase, SessRev
    <3>. QED BY <2>1, <3>4, <3>5 DEF Inv_Session
  <2>3. CASE sb # 0
    <3>0. sb > 0 /\ sb \in sPres["D"] BY <2>3, <1>g, <1>ty DEF RevertD
    <3>1. /\ head'["D"] = sCon["D"][sb]
          /\ sPres'["D"] = {e \in sPres["D"] : e <= sb}
      BY <3>0, <1>ty, Shipped DEF RevertD, R
    <3>2. Agree(sb) /\ \A e \in Common : e <= sb => Agree(e)
      BY <2>0, <3>0, <1>g DEF Inv_Session, SessBase
    <3>3. SessBase' BY <2>0, <3>0, <1>g, <1>ag, <1>cm DEF Inv_Session, SessBase
    <3>4. SessRev'
      <4>1. \A e \in Common' : e <= sb BY <3>1, <1>s DEF Common, SFix
      <4>. QED BY <3>1, <3>2, <4>1, <1>cm, <1>ag, <1>g, <3>0 DEF SessRev
    <3>5. SessFull' BY <3>0, <1>g DEF SessFull
    <3>. QED BY <2>1, <3>3, <3>4, <3>5 DEF Inv_Session
  <2>. QED BY <2>2, <2>3
<1>. QED BY <1>1, <1>2, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

\* The newest recorded epoch: every copy session's target; and its base
\* (a standby chases from its mark, a stale leg copies from its base).
Tgt == Max(recorded)
CopyBase == IF dState = "standby" THEN mark ELSE sb

\* The shared tail of BulkCopy and Chase: D is now a standby at `target`
\* whose head tracks S's epoch there; its snapshots are at or below it.
LEMMA StandbyAfterCopy ==
  ASSUME IndInv, CopyBase \in Nat, Tgt \in Epochs,
         CopyBase <= Tgt, Tgt = nextSeq - 1, Tgt \in recorded,
         CopyBase = 0 \/ CopyBase \in sPres["S"],
         \A x \in Blocks : x \notin AllocFrom(CopyBase) => (head["D"][x] = SC(CopyBase)[x] \/ InFlightS(x)),
         \A e \in sPres["D"] : e <= CopyBase \/ e < Tgt,
         \A e \in Common : Agree(e),
         Tgt \in sPres["D"] => CopyBase = Tgt,
         CopyAlign(CopyBase, Tgt),
         dState' = "standby", mark' = Tgt, sb' = -1, dPath = "back", cutK = 0,
         TypeOK',
         UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members, attached,
                     dPath, pend, acked, fails, riskRec, badAdmit>>
  PROVE  IndInv'
<1>tg. PICK target \in Epochs : target = Tgt OBVIOUS
<1>tb. PICK base \in Nat : base = CopyBase OBVIOUS
<1>. USE <1>tg, <1>tb
<1>t. TypeOK BY DEF IndInv
<1>ty. /\ head \in [R -> [Blocks -> Vers]] /\ sPres \in [R -> SUBSET Epochs]
       /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]] /\ nextSeq \in Nat
  BY <1>t DEF TypeOK
<1>tS. TySnap BY <1>t, TypeOKSplit
<1>rS. recorded \subseteq sPres["S"] BY DEF IndInv, Inv_SLinear
<1>NH. DEFINE NH == AfterCopy(head["D"], base, target)
<1>tr. \A x \in Blocks : x \notin AllocAfter(target) =>
         (NH[x] = sCon["S"][target][x] \/ InFlightS(x))
  BY <1>tS, <1>rS, CopyTracks DEF IndInv
<1>sh. /\ head'["D"] = NH /\ head'["S"] = head["S"]
       /\ sPres'["S"] = sPres["S"] /\ sCon'["S"] = sCon["S"] /\ sAl'["S"] = sAl["S"]
       /\ sPar'["S"] = sPar["S"] /\ hPar'["S"] = hPar["S"] /\ hAl'["S"] = hAl["S"]
       /\ (base = target => sPres'["D"] = sPres["D"] /\ sCon'["D"] = sCon["D"])
       /\ (base # target => /\ sPres'["D"] = sPres["D"] \cup {target}
                            /\ sCon'["D"][target] = NH
                            /\ \A f \in Epochs : f # target => sCon'["D"][f] = sCon["D"][f])
  <2>1. CASE base = target
    BY <2>1, <1>t, <1>ty, CopyAlignSame, RDistinct
  <2>2. CASE base # target
    <3>1. target \notin sPres["D"] BY <2>2
    <3>. QED BY <2>2, <3>1, <1>t, <1>ty, CopyAlignFresh, RDistinct
  <2>. QED BY <2>1, <2>2
<1>s. SFix BY <1>sh, <1>t, DOnly
<1>al. \A e : AllocFrom(e)' = AllocFrom(e) /\ AllocAfter(e)' = AllocAfter(e) BY <1>s, SFixAlloc
<1>if. \A x \in Blocks : InFlightS(x)' = InFlightS(x) BY DEF InFlightS
<1>dn. \A e \in sPres'["D"] : e <= target BY <1>sh, <1>ty DEF Epochs, R
<1>agO. \A e \in Common : e # target => Agree(e)'
  <2>. SUFFICES ASSUME NEW e \in Common, e # target PROVE Agree(e)' OBVIOUS
  <2>1. e \in Epochs BY <1>ty DEF Common, R
  <2>2. sCon'["D"][e] = sCon["D"][e] BY <2>1, <1>sh
  <2>. QED BY <2>2, <1>sh, <1>al, <1>if, AgreeSame
<1>agT. target \in Common' => Agree(target)'
  <2>0. SUFFICES ASSUME target \in Common' PROVE Agree(target)' OBVIOUS
  <2>1. CASE base = target
    <3>0a. recorded' = recorded OBVIOUS
    <3>0b. target \in Common' BY <2>0
    <3>0. recorded' = recorded /\ target \in Common' BY <3>0a, <3>0b
    <3>1. target \in Common BY <3>0, <2>1, <1>sh DEF Common
    <3>2. sCon'["D"][target] = sCon["D"][target] /\ sCon'["S"][target] = sCon["S"][target]
      BY <2>1, <1>sh
    <3>3. AllocFrom(target)' = AllocFrom(target) BY <1>al
    <3>4. Agree(target)' = Agree(target) BY <3>2, <3>3, <1>if, AgreeSame
    <3>. QED BY <3>1, <3>4
  <2>2. CASE base # target
    <3>. SUFFICES ASSUME NEW x \in Blocks, x \notin AllocFrom(target)'
                 PROVE  sCon'["D"][target][x] = sCon'["S"][target][x] \/ InFlightS(x)'
      BY DEF Agree
    <3>1. x \notin AllocAfter(target) BY <1>al, AllocFromMem, AllocAfterMem, <1>ty DEF Epochs
    <3>. QED BY <3>1, <1>tr, <2>2, <1>sh, <1>if
  <2>. QED BY <2>1, <2>2
<1>agC. \A e \in Common' : Agree(e)'
  <2>1. Common' \subseteq Common \cup {target} BY <1>sh, <1>s DEF Common, SFix
  <2>. QED BY <2>1, <1>agO, <1>agT
<1>1. TypeOK' OBVIOUS
<1>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>sh DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' BY <1>s, SFixLinear DEF IndInv
<1>5. Inv_SReads' BY <1>s, SFixReads DEF IndInv
<1>6. Inv_Pend' BY <1>s, <1>al DEF IndInv, Inv_Pend, PendUniq, PendW, PendGlobal, SFix
<1>7. Inv_Phase' BY DEF IndInv, Inv_Phase
<1>8. Inv_Epochs'
  <2>0. sPres'["D"] \subseteq Epochs BY <1>sh, <1>ty DEF R
  <2>1. \A e \in sPres'["D"] : e < nextSeq BY <2>0, <1>dn, <1>ty DEF Epochs
  <2>2. cutDone = {} BY DEF IndInv, Inv_Epochs
  <2>. QED BY <2>1, <2>2, <1>s DEF IndInv, Inv_Epochs, SFix
<1>9. Inv_Heads' BY DEF Inv_Heads, HeadsTracked
<1>10. Inv_Agree' BY <1>agC DEF Inv_Agree, AgreeLow, AgreeCut
<1>11. Inv_Session'
  <2>1. StandbyAt(target)' BY <1>tr, <1>sh, <1>al, <1>if DEF StandbyAt
  <2>2. target \in 1..(nextSeq - 1) BY DEF Epochs
  <2>. QED BY <2>1, <2>2, <1>dn, <1>agC DEF Inv_Session, SessSel, SessBase, SessRev,
                                          SessFull, SessStandby
<1>. QED BY <1>1, <1>2, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_BulkCopy == ASSUME IndInv, BulkCopy PROVE IndInv'
<1>t. TypeOK BY DEF IndInv
<1>. DEFINE target == Tgt
<1>g. /\ dState = "stale" /\ dPath = "back" /\ reverted # -1 /\ sb = reverted
      /\ recorded # {} /\ cutK = 0
      /\ target \in SLine /\ (sb = 0 \/ sb \in SLine)
      /\ CopyAlign(sb, target) /\ mark' = target
      /\ dState' = "standby" /\ sb' = -1
      /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members,
                     attached, dPath, reverted, pend, acked, fails, riskRec, badAdmit>>
  BY DEF BulkCopy, Tgt
<1>m. target = nextSeq - 1 /\ target \in recorded /\ nextSeq > 1
  BY <1>t, <1>g, MaxRec DEF IndInv, Tgt
<1>ty. /\ target \in Epochs /\ sb \in Nat /\ nextSeq \in Nat /\ recorded \subseteq Epochs
       /\ sPres \in [R -> SUBSET Epochs]
  BY <1>m, <1>g, <1>t DEF TypeOK
<1>ss. Inv_Session BY DEF IndInv
<1>T. TypeOK' BY <1>t, T_BulkCopy
<1>cb. CopyBase = sb BY <1>g DEF CopyBase
<1>7. UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members, attached,
                  dPath, pend, acked, fails, riskRec, badAdmit>>
  BY <1>g
<1>1. CASE sb = 0
  <2>1. head["D"] = Zero /\ \A e \in sPres["D"] : \A f \in recorded : e < f
    BY <1>1, <1>g, <1>ss DEF Inv_Session, SessFull
  <2>2. \A x \in Blocks : x \notin AllocFrom(0) => (head["D"][x] = SC(0)[x] \/ InFlightS(x))
    BY <2>1 DEF SC
  <2>3. \A e \in sPres["D"] : e <= 0 \/ e < target BY <2>1, <1>m
  <2>4. \A e \in Common : Agree(e)
    <3>. SUFFICES ASSUME NEW e \in Common PROVE FALSE OBVIOUS
    <3>1. e \in recorded /\ e \in sPres["D"] BY DEF Common
    <3>2. e < e BY <3>1, <2>1
    <3>. QED BY <3>2, <3>1, <1>ty DEF Epochs
  <2>5. target \in sPres["D"] => 0 = target BY <2>1, <1>m, <1>ty DEF Epochs
  <2>. QED BY <1>1, <1>cb, <1>7, <1>g, <1>m, <1>ty, <1>T, <2>2, <2>3, <2>4, <2>5, StandbyAfterCopy
<1>2. CASE sb # 0
  <2>0. sb > 0 /\ sb \in sPres["S"] BY <1>2, <1>g, <1>ty DEF SLine
  <2>1. /\ head["D"] = sCon["D"][sb] /\ \A e \in sPres["D"] : e <= sb
        /\ \A e \in Common : Agree(e)
    BY <2>0, <1>g, <1>ss DEF Inv_Session, SessRev
  <2>2. sb < nextSeq /\ Agree(sb) BY <2>0, <1>g, <1>ss DEF Inv_Session, SessBase
  <2>3. \A x \in Blocks : x \notin AllocFrom(sb) => (head["D"][x] = SC(sb)[x] \/ InFlightS(x))
    BY <2>0, <2>1, <2>2 DEF Agree, SC
  <2>4. sb <= target BY <2>2, <1>m, <1>ty
  <2>5. target \in sPres["D"] => sb = target BY <2>1, <2>4, <1>ty DEF Epochs
  <2>. QED BY <1>cb, <1>7, <1>g, <1>m, <1>ty, <1>T, <2>0, <2>1, <2>3, <2>4, <2>5, StandbyAfterCopy
<1>. QED BY <1>1, <1>2

LEMMA I_Chase == ASSUME IndInv, Chase PROVE IndInv'
<1>t. TypeOK BY DEF IndInv
<1>. DEFINE target == Tgt
<1>g. /\ dState = "standby" /\ dPath = "back" /\ cutK = 0
      /\ recorded # {} /\ target > mark /\ mark \in SLine
      /\ CopyAlign(mark, target) /\ mark' = target
      /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members,
                     attached, dPath, dState, reverted, sb, pend, acked, fails,
                     riskRec, badAdmit>>
  BY DEF Chase, Tgt
<1>m. target = nextSeq - 1 /\ target \in recorded /\ nextSeq > 1
  BY <1>t, <1>g, MaxRec DEF IndInv, Tgt
<1>ty. /\ target \in Epochs /\ nextSeq \in Nat /\ recorded \subseteq Epochs
  BY <1>m, <1>g, <1>t DEF TypeOK
<1>ss. Inv_Session BY DEF IndInv
<1>sb. sb = -1 BY <1>g DEF IndInv, Inv_Phase
<1>T. TypeOK' BY <1>t, T_Chase
<1>1. /\ mark \in 1..(nextSeq - 1) /\ StandbyAt(mark)
      /\ \A e \in sPres["D"] : e <= mark
      /\ \A e \in Common : Agree(e)
  BY <1>g, <1>ss DEF Inv_Session, SessStandby
<1>2. \A x \in Blocks : x \notin AllocFrom(mark) => (head["D"][x] = SC(mark)[x] \/ InFlightS(x))
  <2>. SUFFICES ASSUME NEW x \in Blocks, x \notin AllocFrom(mark)
               PROVE  head["D"][x] = SC(mark)[x] \/ InFlightS(x)
    OBVIOUS
  <2>1. x \notin AllocAfter(mark) BY <1>1, AllocFromMem, AllocAfterMem
  <2>. QED BY <2>1, <1>1 DEF StandbyAt, SC
<1>3. mark \in Nat /\ mark <= target /\ mark \in sPres["S"] BY <1>1, <1>g, <1>m, <1>ty DEF SLine
<1>4. target \in sPres["D"] => mark = target BY <1>1, <1>g, <1>ty DEF Epochs
<1>5. \A e \in sPres["D"] : e <= mark \/ e < target BY <1>1
<1>6. dState' = "standby" /\ sb' = -1 BY <1>g, <1>sb
<1>7. UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members, attached,
                  dPath, pend, acked, fails, riskRec, badAdmit>>
  BY <1>g
<1>8. /\ dPath = "back" /\ cutK = 0 /\ CopyAlign(mark, Tgt) /\ mark' = Tgt
      /\ Tgt \in Epochs /\ Tgt = nextSeq - 1 /\ Tgt \in recorded /\ mark <= Tgt
  BY <1>g, <1>m, <1>ty, <1>3
<1>cb. CopyBase = mark BY <1>g DEF CopyBase
<1>h1. CopyBase \in Nat /\ Tgt \in Epochs BY <1>cb, <1>3, <1>8
<1>h2. CopyBase <= Tgt /\ Tgt = nextSeq - 1 /\ Tgt \in recorded BY <1>cb, <1>8
<1>h3. CopyBase = 0 \/ CopyBase \in sPres["S"] BY <1>cb, <1>3
<1>h4. \A x \in Blocks : x \notin AllocFrom(CopyBase) => (head["D"][x] = SC(CopyBase)[x] \/ InFlightS(x))
  BY <1>cb, <1>2
<1>h5. \A e \in sPres["D"] : e <= CopyBase \/ e < Tgt BY <1>cb, <1>5
<1>h6. \A e \in Common : Agree(e) BY <1>1
<1>h7. Tgt \in sPres["D"] => CopyBase = Tgt BY <1>cb, <1>4
<1>h8. CopyAlign(CopyBase, Tgt) BY <1>cb, <1>8
<1>h9. dState' = "standby" /\ mark' = Tgt /\ sb' = -1 /\ dPath = "back" /\ cutK = 0
  BY <1>6, <1>8
<1>. QED BY <1>h1, <1>h2, <1>h3, <1>h4, <1>h5, <1>h6, <1>h7, <1>h8, <1>h9, <1>7, <1>T, StandbyAfterCopy

-----------------------------------------------------------------------------
(* Admission.                                                               *)

\* The final delta lands D on S's head exactly: S's head blob is the final
\* epoch, every block S wrote after the mark comes from S's newest blob
\* holding it, and every other block is the mark's on both sides.
LEMMA AdmExact ==
  ASSUME TySnap, TyHead, Inv_SReads, NEW k \in Nat, k = nextSeq,
         k \notin sPres["S"], \A f \in sPres["S"] : f <= k,
         mark \in sPres["S"], mark \in 1..(k - 1),
         StandbyAt(mark), pend = {}, k <= MaxEpoch
  PROVE  \A x \in Blocks : AdmHead[x] = head["S"][x]
<1>. SUFFICES ASSUME NEW x \in Blocks PROVE AdmHead[x] = head["S"][x] OBVIOUS
<1>ty. /\ sPres["S"] \subseteq Epochs /\ k \in Epochs /\ mark \in Epochs
  BY DEF TySnap, R, Epochs
<1>below. \A f \in sPres["S"] : f < k BY <1>ty DEF Epochs
<1>al. /\ AdmAl(k) = hAl["S"] /\ AdmCon(k) = head["S"]
       /\ \A e \in sPres["S"] : AdmAl(e) = sAl["S"][e] /\ AdmCon(e) = sCon["S"][e]
  BY <1>below, <1>ty DEF AdmAl, AdmCon, AdmK, Epochs
<1>el. AdmElems = {e \in sPres["S"] \cup {k} : mark <= e /\ e <= k} BY DEF AdmElems, AdmK, SLine
<1>0. AdmHead[x] = IF AdmHits(x) = {} THEN head["D"][x] ELSE AdmCon(Max(AdmHits(x)))[x]
  BY DEF AdmHead
<1>hs. AdmHits(x) \subseteq 0..MaxEpoch BY <1>el, <1>ty, Const DEF AdmHits, Epochs
<1>nf. ~InFlightS(x) BY DEF InFlightS
<1>rd. /\ \A a \in sPres["S"] \cup {0} :
            (x \notin hAl["S"] /\ \A f \in sPres["S"] : a < f => x \notin sAl["S"][f])
              => head["S"][x] = SC(a)[x]
  BY DEF Inv_SReads
<1>1. CASE x \in hAl["S"]
  <2>1. k \in AdmHits(x) BY <1>1, <1>al, <1>el, <1>ty DEF AdmHits, Epochs
  <2>2. Max(AdmHits(x)) \in AdmHits(x) /\ \A u \in AdmHits(x) : u <= Max(AdmHits(x))
    BY <2>1, <1>hs, MaxProp, Const
  <2>3. \A u \in AdmHits(x) : u <= k BY <1>el, <1>ty DEF AdmHits, Epochs
  <2>4. Max(AdmHits(x)) = k BY <2>1, <2>2, <2>3, <1>hs, <1>ty DEF Epochs
  <2>. QED BY <1>0, <2>1, <2>4, <1>al
<1>2. CASE x \notin hAl["S"] /\ AdmHits(x) # {}
  <2>. DEFINE m == Max(AdmHits(x))
  <2>1. m \in AdmHits(x) /\ \A u \in AdmHits(x) : u <= m BY <1>2, <1>hs, MaxProp, Const
  <2>2. m # k BY <2>1, <1>2, <1>al DEF AdmHits
  <2>3. m \in sPres["S"] /\ mark <= m /\ x \in sAl["S"][m] /\ m \in Nat /\ m # 0
    BY <2>1, <2>2, <1>el, <1>al, <1>ty DEF AdmHits, Epochs
  <2>4. \A f \in sPres["S"] : m < f => x \notin sAl["S"][f]
    <3>. SUFFICES ASSUME NEW f \in sPres["S"], m < f, x \in sAl["S"][f] PROVE FALSE OBVIOUS
    <3>1. f \in AdmHits(x) BY <2>3, <1>below, <1>el, <1>al, <1>ty DEF AdmHits, Epochs
    <3>2. f <= m BY <3>1, <2>1
    <3>3. f \in Int BY <1>ty DEF Epochs
    <3>. QED BY <3>2, <3>3, <2>3
  <2>5. head["S"][x] = SC(m)[x] BY <1>rd, <1>2, <2>3, <2>4
  <2>. QED BY <1>0, <1>2, <2>5, <2>3, <1>al DEF SC
<1>3. CASE x \notin hAl["S"] /\ AdmHits(x) = {}
  <2>1. \A f \in sPres["S"] : mark <= f => x \notin sAl["S"][f]
    BY <1>3, <1>below, <1>el, <1>al, <1>ty DEF AdmHits, Epochs
  <2>2. x \notin AllocAfter(mark) BY <1>3, <2>1, <1>ty, AllocAfterMem DEF Epochs
  <2>3. head["D"][x] = sCon["S"][mark][x] BY <2>2, <1>nf DEF StandbyAt
  <2>4. \A f \in sPres["S"] : mark < f => x \notin sAl["S"][f] BY <2>1, <1>ty DEF Epochs
  <2>5. head["S"][x] = SC(mark)[x] BY <1>rd, <1>3, <2>4
  <2>. QED BY <1>0, <1>3, <2>3, <2>5, <1>ty DEF SC, Epochs
<1>. QED BY <1>1, <1>2, <1>3

LEMMA I_StageAdmit == ASSUME IndInv, StageAdmit PROVE IndInv'
<1>t. TypeOK BY DEF IndInv
<1>F. /\ ~attached /\ cutK = 0 /\ dState = "standby" /\ dPath = "back"
      /\ mark \in SLine /\ nextSeq <= MaxEpoch
      /\ AdmK \notin sPres["S"]
      /\ sPres' = [sPres EXCEPT !["S"] = @ \cup {AdmK}, !["D"] = @ \cup {AdmK}]
      /\ sCon' = [sCon EXCEPT !["S"][AdmK] = head["S"],
                              !["D"][AdmK] = IF AdmK \in sPres["D"] THEN @ ELSE AdmHead]
      /\ sAl' = [sAl EXCEPT !["S"][AdmK] = hAl["S"],
                            !["D"][AdmK] = IF AdmK \in sPres["D"] THEN @ ELSE AdmAlD]
      /\ sPar' = [sPar EXCEPT !["S"][AdmK] = hPar["S"],
                              !["D"][AdmK] = IF AdmK \in sPres["D"] THEN @ ELSE hPar["D"]]
      /\ hPar' = [hPar EXCEPT !["S"] = AdmK,
                              !["D"] = IF AdmK \in sPres["D"] THEN @ ELSE AdmK]
      /\ hAl' = [hAl EXCEPT !["S"] = {},
                            !["D"] = IF AdmK \in sPres["D"] THEN AdmAlD ELSE {}]
      /\ head' = [head EXCEPT !["D"] = AdmHead]
      /\ recorded' = recorded \cup {AdmK}
      /\ nextSeq' = nextSeq + 1
      /\ mark' = AdmK
      /\ badAdmit' = (badAdmit \/ AdmHead # head["S"])
      /\ dState' = "insync" /\ reverted' = -1 /\ dPath' = "ok" /\ pin' = 0
      /\ attached' = TRUE /\ members' = R /\ riskRec' = NoRisk
      /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, sb, pend, acked, fails>>
  BY StageAdmitFacts
<1>. DEFINE k == AdmK
<1>ty. /\ TySnap /\ TyHead /\ TyScalar
       /\ sPres \in [R -> SUBSET Epochs] /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]]
       /\ sAl \in [R -> [Epochs -> SUBSET Blocks]] /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]]
       /\ hAl \in [R -> SUBSET Blocks] /\ hPar \in [R -> 0..MaxEpoch]
       /\ head \in [R -> [Blocks -> Vers]] /\ recorded \subseteq Epochs
  BY <1>t, TypeOKSplit DEF TypeOK
<1>k. k = nextSeq /\ k \in Epochs /\ k \in Nat /\ k >= 1 BY <1>F, <1>ty DEF AdmK, TyScalar, Epochs
<1>e. Inv_Epochs BY DEF IndInv
<1>kD. k \notin sPres["D"] /\ \A e \in sPres["D"] : e < k BY <1>e, <1>F, <1>k DEF Inv_Epochs
<1>Sle. \A f \in sPres["S"] : f <= k BY <1>e, <1>k DEF Inv_Epochs
<1>p0. pend = {} BY <1>F DEF IndInv, Inv_Pend, PendGlobal
<1>sb. sb = -1 BY <1>F DEF IndInv, Inv_Phase
<1>ss. /\ mark \in 1..(nextSeq - 1) /\ StandbyAt(mark) /\ \A e \in Common : Agree(e)
  BY <1>F DEF IndInv, Inv_Session, SessStandby
<1>ah. AdmHead \in [Blocks -> Vers] BY <1>F, <1>ty, AdmHeadType
<1>ex. AdmHead = head["S"]
  <2>1. \A x \in Blocks : AdmHead[x] = head["S"][x]
    BY <1>F, <1>ty, <1>k, <1>Sle, <1>ss, <1>p0, AdmExact DEF IndInv, SLine
  <2>2. head["S"] \in [Blocks -> Vers] BY <1>ty DEF R
  <2>. QED BY <2>1, <2>2, <1>ah
<1>u. /\ sPres'["S"] = sPres["S"] \cup {k}
      /\ sCon'["S"] = [sCon["S"] EXCEPT ![k] = head["S"]]
      /\ sAl'["S"] = [sAl["S"] EXCEPT ![k] = hAl["S"]]
      /\ sPar'["S"] = [sPar["S"] EXCEPT ![k] = hPar["S"]]
      /\ hPar'["S"] = k /\ hAl'["S"] = {} /\ head'["S"] = head["S"]
      /\ sPres'["D"] = sPres["D"] \cup {k}
      /\ sCon'["D"] = [sCon["D"] EXCEPT ![k] = AdmHead]
      /\ head'["D"] = AdmHead
  BY <1>F, <1>ty, <1>kD, RDistinct DEF R
<1>rec. recorded' \subseteq sPres'["S"] BY <1>u, <1>F DEF IndInv, Inv_SLinear
<1>sn. /\ Inv_SLinear' /\ Inv_SReads'
       /\ \A e \in Int : e <= k => AllocFrom(e)' = AllocFrom(e)
       /\ \A f \in Epochs : f # k => sCon'["S"][f] = sCon["S"][f]
  BY <1>k, <1>ty, <1>F, <1>Sle, <1>u, <1>rec, SnapS DEF IndInv, Epochs
<1>if. \A x \in Blocks : InFlightS(x)' = InFlightS(x) BY <1>F DEF InFlightS
<1>1. TypeOK' BY <1>t, T_StageAdmit
<1>2. ~badAdmit' BY <1>F, <1>ex DEF IndInv
<1>3. Inv_SourceHoldsAcked' BY <1>F, <1>u DEF IndInv, Inv_SourceHoldsAcked
<1>6. Inv_Pend' BY <1>F, <1>p0 DEF IndInv, Inv_Pend, PendUniq, PendGlobal, R
<1>7. Inv_Phase' BY <1>F, <1>sb, RDistinct DEF Inv_Phase, R
<1>8. Inv_Epochs'
  <2>1. recorded \subseteq 1..(nextSeq - 1) /\ \A e \in sPres["S"] : e <= nextSeq
    BY <1>e DEF Inv_Epochs
  <2>2. cutTgt = {} /\ cutDone = {} BY <1>e, <1>F DEF Inv_Epochs
  <2>3. cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} BY <2>2, <1>F
  <2>4. recorded' \subseteq 1..(nextSeq' - 1) BY <2>1, <1>F, <1>k
  <2>5. nextSeq' - 1 \in recorded' BY <1>F, <1>k
  <2>6. \A e \in sPres'["S"] : e <= nextSeq'
    <3>1. sPres["S"] \subseteq Int BY <1>ty DEF R, Epochs
    <3>. QED BY <3>1, <2>1, <1>u, <1>F, <1>k
  <2>7. \A e \in sPres'["D"] : e < nextSeq'
    <3>1. sPres["D"] \subseteq Int BY <1>ty DEF R, Epochs
    <3>. QED BY <3>1, <1>kD, <1>u, <1>F, <1>k
  <2>. QED BY <2>3, <2>4, <2>5, <2>6, <2>7 DEF Inv_Epochs
<1>9. Inv_Heads'
  <2>1. \A y \in Blocks : head'["D"][y] = head'["S"][y] BY <1>u, <1>ex
  <2>. QED BY <2>1, <1>F, <1>p0 DEF Inv_Heads, Pending
<1>10. Inv_Agree'
  <2>1. \A e \in Common' : Agree(e)'
    <3>. SUFFICES ASSUME NEW e \in Common' PROVE Agree(e)' OBVIOUS
    <3>1. CASE e = k
      <4>1. sCon'["D"][k] = head["S"] /\ sCon'["S"][k] = head["S"]
        BY <1>u, <1>ex, <1>k, <1>ty DEF R
      <4>. QED BY <3>1, <4>1 DEF Agree
    <3>2. CASE e # k
      <4>1. e \in Common BY <3>2, <1>F, <1>u DEF Common
      <4>2. e \in Epochs /\ e \in Int /\ e < k BY <4>1, <1>ty, <1>kD DEF Common, Epochs
      <4>3. sCon'["D"][e] = sCon["D"][e] BY <4>2, <3>2, <1>u, <1>ty DEF R
      <4>4. Agree(e)' = Agree(e) BY <4>2, <4>3, <3>2, <1>sn, <1>if, AgreeSame
      <4>. QED BY <4>1, <4>4, <1>ss
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <1>F DEF Inv_Agree, AgreeLow, AgreeCut
<1>11. Inv_Session' BY <1>F DEF Inv_Session, SessSel, SessBase, SessRev, SessFull, SessStandby
<1>. QED BY <1>1, <1>2, <1>3, <1>sn, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndInv

-----------------------------------------------------------------------------
(* The theorem.                                                             *)

LEMMA I_Stutter == ASSUME IndInv, UNCHANGED vars PROVE IndInv'
<1>u. /\ ver' = ver /\ head' = head /\ hAl' = hAl /\ hPar' = hPar /\ sPres' = sPres
      /\ sCon' = sCon /\ sAl' = sAl /\ sPar' = sPar /\ recorded' = recorded
      /\ nextSeq' = nextSeq /\ cutK' = cutK /\ cutTgt' = cutTgt /\ cutDone' = cutDone
      /\ members' = members /\ attached' = attached /\ dPath' = dPath /\ dState' = dState
      /\ mark' = mark /\ reverted' = reverted /\ sb' = sb /\ pin' = pin /\ pend' = pend
      /\ acked' = acked /\ fails' = fails /\ riskRec' = riskRec /\ badAdmit' = badAdmit
  BY DEF vars
<1>1. TypeOK' BY <1>u, TypeOKSplit, TypeOKSplitP DEF IndInv, TyScalar, TySets, TyHead, TySnap, TyPend
<1>2. ~badAdmit' /\ Inv_SourceHoldsAcked' BY <1>u DEF IndInv, Inv_SourceHoldsAcked
<1>4. Inv_SLinear' /\ Inv_SReads' BY <1>u DEF IndInv, Inv_SLinear, Inv_SReads, SC
<1>6. Inv_Pend' BY <1>u DEF IndInv, Inv_Pend, PendUniq, PendW, PendGlobal, AllocFrom
<1>7. Inv_Phase' /\ Inv_Epochs' BY <1>u DEF IndInv, Inv_Phase, Inv_Epochs
<1>9. Inv_Heads' BY <1>u DEF IndInv, Inv_Heads, HeadsTracked, HeadsW, Pending
<1>10. Inv_Agree' BY <1>u DEF IndInv, Inv_Agree, AgreeLow, AgreeCut, CutVsHead, Agree,
                               Common, AllocFrom, InFlightS
<1>11. Inv_Session' BY <1>u DEF IndInv, Inv_Session, SessSel, SessBase, SessRev, SessFull,
                                 SessStandby, StandbyAt, Agree, Common, AllocFrom,
                                 AllocAfter, InFlightS
<1>. QED BY <1>1, <1>2, <1>4, <1>6, <1>7, <1>9, <1>10, <1>11 DEF IndInv

LEMMA I_Next == IndInv /\ [Next]_vars => IndInv'
<1>. SUFFICES ASSUME IndInv, [Next]_vars PROVE IndInv' OBVIOUS
<1>0. CASE UNCHANGED vars BY <1>0, I_Stutter
<1>a. CASE \E x \in Blocks : Submit(x) \/ Zombie(x) BY <1>a, I_Submit, I_Zombie
<1>b. CASE \E w \in pend : Drop(w) \/ Complete(w) \/ \E r \in R : Land(w, r)
  BY <1>b, I_Drop, I_Complete, I_LandS, I_LandD, RDistinct
<1>c. CASE FailBegin \/ FaultOut \/ MarkStale \/ Return
  BY <1>c, I_FailBegin, I_FaultOut, I_MarkStale, I_Return
<1>d. CASE CutBegin \/ CutRecord \/ CutAbort \/ Retire
  BY <1>d, I_CutBegin, I_CutRecord, I_CutAbort, I_Retire
<1>e. CASE \E r \in R : CutOne(r) \/ \E e \in Epochs : GC(r, e)
  BY <1>e, I_CutOneS, I_CutOneD, I_GCS, I_GCD, RDistinct
<1>f. CASE SelectBase \/ RevertD \/ BulkCopy \/ SessionFail \/ Chase
  BY <1>f, I_SelectBase, I_RevertD, I_BulkCopy, I_SessionFail, I_Chase
<1>g. CASE Unstage \/ Stage \/ StageAdmit \/ StageDefer
  BY <1>g, I_Unstage, I_Stage, I_StageAdmit, I_StageDefer
<1>. QED BY <1>0, <1>a, <1>b, <1>c, <1>d, <1>e, <1>f, <1>g DEF Next

LEMMA IndSafe == IndInv => Inv
<1>. SUFFICES ASSUME IndInv PROVE Inv OBVIOUS
<1>1. Inv_MemberAgrees
  <2>. SUFFICES ASSUME "D" \in members, dPath = "ok"
               PROVE  \A x \in Blocks : ~Pending(x) => head["D"][x] = head["S"][x]
    BY DEF Inv_MemberAgrees
  <2>1. HeadsTracked BY DEF IndInv, Inv_Phase, HeadsTracked
  <2>. QED BY <2>1 DEF IndInv, Inv_Heads
<1>. QED BY <1>1 DEF IndInv, Inv, Inv_AdmitFaithful

THEOREM Safety == Spec => []Inv
<1>1. Init => IndInv BY I_Init
<1>2. IndInv /\ [Next]_vars => IndInv' BY I_Next
<1>3. IndInv => Inv BY IndSafe
<1>. QED BY <1>1, <1>2, <1>3, PTL DEF Spec

=============================================================================
