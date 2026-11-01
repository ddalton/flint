------------------------- MODULE FlintCatchupTProof -------------------------
(***************************************************************************)
(* TLAPS proof of the per-epoch trust rule (FlintCatchupT, Policy =       *)
(* "trust") for EVERY bound -- any replicas, blocks, writes, epochs,      *)
(* failures and retention depth -- in the clean setting the design review *)
(* names (docs/plans/catchup-design-review-2026-10-28.md): the unstage    *)
(* capture, no consumer crash, no zombie move, a raid that fails a base   *)
(* at its first dropped write, a write acked on one landing, one attachable*)
(* leg enough to stage, the base-inclusive copy, and the cut's record     *)
(* write stamping only legs still in_sync (CutStampsAll = FALSE, the      *)
(* mark-race fix).  The other knobs are free.                             *)
(*                                                                         *)
(*   THEOREM Safety == Spec => [](Inv_Catchup /\ Inv_MembersHold)          *)
(*                                                                         *)
(* Model: rev 15 (cutGone, CutStampsAll, the trust design rules).        *)
(* The inductive invariant is FlintCatchupTInd's IndInv (checked part by  *)
(* part there with a model checker).                                      *)
(***************************************************************************)
EXTENDS FlintCatchupTInd, TLAPS, NaturalsInduction, FiniteSetTheorems

\* (the model's own unnamed ASSUMEs, restated so a proof can cite them)
ASSUME Const ==
  /\ MaxVer \in Nat /\ MaxEpoch \in Nat /\ MaxFails \in Nat
  /\ Retain \in Nat /\ Retain >= 1
  /\ Reps \subseteq Nat /\ 0 \notin Reps /\ IsFiniteSet(Reps) /\ Reps # {}

ASSUME Clean ==
  /\ Policy = "trust" /\ Capture = TRUE /\ Crash = FALSE /\ ZombieMoves = FALSE
  /\ ZombieWhere = "colocated" /\ RaidFailsAtDrop = TRUE /\ StageOracle = FALSE
  /\ WriteQuorum = 1 /\ AssemblyQuorum = 1 /\ BaseInclusive = TRUE
  /\ CutStampsAll = FALSE /\ PinRetention = TRUE
  /\ Backoff \in BOOLEAN /\ CoverageWalk \in BOOLEAN
  /\ MarkGate \in BOOLEAN /\ ReadFails \in BOOLEAN /\ LocalLegGuard \in BOOLEAN
  /\ FenceMark \in BOOLEAN /\ CatchupWaitsCut \in BOOLEAN /\ AssemblyMark \in BOOLEAN
  /\ StageDefer \in BOOLEAN /\ AckAfterMark \in BOOLEAN

-----------------------------------------------------------------------------
(* Min and Max (as in FlintCatchupProof).                                  *)

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

LEMMA NoRiskNat == NoRisk \in Nat /\ NoRisk = MaxEpoch + 1
BY Const DEF NoRisk

LEMMA RNat == R \subseteq Nat /\ 0 \notin R /\ IsFiniteSet(R)
BY Const DEF R

\* With one landing enough to ack and one leg enough to stage, the quorum
\* never binds.
LEMMA QuorumTrue == QuorumOK
BY Clean DEF QuorumOK

LEMMA OneLanding ==
  ASSUME NEW S \in SUBSET R
  PROVE  (Cardinality(S) >= WriteQuorum) <=> S # {}
<1>1. IsFiniteSet(S) BY RNat, FS_Subset
<1>2. Cardinality(S) \in Nat BY <1>1, FS_CardinalityType
<1>3. Cardinality(S) = 0 <=> S = {} BY <1>1, FS_EmptySet
<1>. QED BY <1>2, <1>3, Clean

-----------------------------------------------------------------------------
(* TypeOK, in groups: one obligation over all of TypeOK times z3 out.      *)

TyScalar ==
  /\ ver \in Vers /\ nextSeq \in 1..(MaxEpoch + 1) /\ cutK \in 0..MaxEpoch
  /\ attached \in BOOLEAN /\ fails \in 0..MaxFails
  /\ badAdmit \in BOOLEAN /\ badLoss \in BOOLEAN /\ zx \in R \cup {0}
TySets ==
  /\ recorded \in SUBSET Epochs /\ cutTgt \in SUBSET R /\ cutDone \in SUBSET R
  /\ cutGone \in SUBSET R /\ members \in SUBSET R
TyLeg ==
  /\ path \in [R -> {"ok", "failing", "out", "back"}]
  /\ dState \in [R -> {"insync", "stale", "standby"}]
  /\ mark \in [R -> 0..MaxEpoch] /\ reverted \in [R -> -1..MaxEpoch]
  /\ sb \in [R -> -1..MaxEpoch] /\ pin \in [R -> 0..MaxEpoch]
  /\ riskRec \in [R -> 0..NoRisk] /\ restamp \in [R -> BOOLEAN]
TyHead ==
  /\ head \in [R -> [Blocks -> Vers]] /\ hAl \in [R -> SUBSET Blocks]
  /\ hPar \in [R -> 0..MaxEpoch]
TySnap ==
  /\ sPres \in [R -> SUBSET Epochs]
  /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]]
  /\ sAl \in [R -> [Epochs -> SUBSET Blocks]]
  /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]]
  /\ anc \in [R -> [Epochs -> SUBSET Epochs]]
TyPend ==
  /\ pend \subseteq Write /\ acked \in [Blocks -> Vers]
  /\ ackedEver \in [Blocks -> SUBSET Vers]
TyTie ==
  /\ used \in [R -> SUBSET R] /\ tainted \in [R -> BOOLEAN]
  /\ floor \in [R -> -1..MaxEpoch] /\ mustRev \in [R -> BOOLEAN]
TyTrust ==
  /\ cfgAt \in [Epochs -> SUBSET R] /\ conf \in [Epochs -> BOOLEAN]
  /\ tr \in [R -> SUBSET Epochs] /\ htr \in [R -> BOOLEAN]
  /\ cutAck \in [R -> [Epochs -> [Blocks -> Vers]]]

Ty == TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust

LEMMA TypeOKSplit == TypeOK <=> Ty
BY DEF TypeOK, Ty, TyScalar, TySets, TyLeg, TyHead, TySnap, TyPend, TyTie, TyTrust
LEMMA TypeOKSplitP == TypeOK' <=> Ty'
BY DEF TypeOK, Ty, TyScalar, TySets, TyLeg, TyHead, TySnap, TyPend, TyTie, TyTrust
LEMMA Groups == TypeOK => TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust
BY TypeOKSplit DEF Ty
LEMMA GroupsP ==
  TyScalar' /\ TySets' /\ TyLeg' /\ TyHead' /\ TySnap' /\ TyPend' /\ TyTie' /\ TyTrust' => TypeOK'
BY TypeOKSplitP DEF Ty

\* A group an action leaves alone carries over.
LEMMA C_Scalar == TyScalar /\ UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>> => TyScalar'
BY DEF TyScalar
LEMMA C_Sets == TySets /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>> => TySets'
BY DEF TySets
LEMMA C_Leg == TyLeg /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>> => TyLeg'
BY DEF TyLeg
LEMMA C_Head == TyHead /\ UNCHANGED <<head, hAl, hPar>> => TyHead'
BY DEF TyHead
LEMMA C_Snap == TySnap /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>> => TySnap'
BY DEF TySnap
LEMMA C_Pend == TyPend /\ UNCHANGED <<pend, acked, ackedEver>> => TyPend'
BY DEF TyPend
LEMMA C_Tie == TyTie /\ UNCHANGED tieVars => TyTie'
BY DEF TyTie, tieVars
LEMMA C_Trust == TyTrust /\ UNCHANGED trustVars => TyTrust'
BY DEF TyTrust, trustVars

LEMMA T_Init == Init => TypeOK
<1>. SUFFICES ASSUME Init PROVE Ty BY TypeOKSplit
<1>1. TyScalar BY Const DEF Init, TyScalar, Vers
<1>2. TySets BY DEF Init, TySets
<1>3. TyLeg BY Const, NoRiskNat DEF Init, TyLeg
<1>4. TyHead BY Const DEF Init, TyHead, Zero, Vers
<1>5. TySnap BY Const DEF Init, TySnap, Zero, Vers, Epochs
<1>6. TyPend BY Const DEF Init, TyPend, Zero, Vers
<1>7. TyTie BY Const DEF Init, TyTie
<1>8. TyTrust BY Const DEF Init, TyTrust, Zero, Vers
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8 DEF Ty

\* The record's view of legs going stale keeps the leg and tie groups.
LEMMA T_StaleMarks ==
  ASSUME TyLeg, TyTie, NEW M, StaleMarks(M),
         UNCHANGED <<path, mark, reverted, sb, pin, riskRec, used, mustRev>>
  PROVE  TyLeg' /\ TyTie'
<1>1. UNCHANGED <<tainted, floor>> BY Clean DEF StaleMarks
<1>2. dState' \in [R -> {"insync", "stale", "standby"}] /\ restamp' \in [R -> BOOLEAN]
  BY DEF StaleMarks, TyLeg
<1>3. TyLeg' BY <1>2 DEF TyLeg
<1>4. TyTie' BY <1>1 DEF TyTie
<1>. QED BY <1>3, <1>4

LEMMA LatestType == ASSUME TyScalar PROVE Latest \in 0..MaxEpoch
BY Const DEF TyScalar, Latest

\* The memberships and number types a step leans on.
LEMMA Basics ==
  ASSUME TypeOK
  PROVE  /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
         /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
         /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
         /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
         /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                         /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                         /\ sb[q] \in Int /\ reverted[q] \in Int
<1>1. members \subseteq R /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs
  BY DEF TypeOK
<1>2. InSyncR \subseteq R /\ Healthy \subseteq R BY <1>1 DEF InSyncR, Healthy
<1>3. Bases \subseteq R BY <1>1, <1>2 DEF Bases
<1>4. Epochs \subseteq Nat /\ R \subseteq Nat /\ MaxEpoch \in Nat BY Const, RNat DEF Epochs
<1>5. nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat BY Const DEF TypeOK, Vers
<1>6. \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                   /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                   /\ sb[q] \in Int /\ reverted[q] \in Int
  <2>. SUFFICES ASSUME NEW q \in R
               PROVE  /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
    OBVIOUS
  <2>1. sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs BY DEF TypeOK
  <2>2. hPar[q] \in 0..MaxEpoch /\ mark[q] \in 0..MaxEpoch /\ pin[q] \in 0..MaxEpoch BY DEF TypeOK
  <2>3. sb[q] \in -1..MaxEpoch /\ reverted[q] \in -1..MaxEpoch BY DEF TypeOK
  <2>. QED BY <2>1, <2>2, <2>3, Const
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6

\* The numbers a step compares.
LEMMA Nats ==
  ASSUME TypeOK
  PROVE  /\ ver \in Nat /\ nextSeq \in Nat /\ cutK \in Nat
         /\ \A x \in Blocks : acked[x] \in Nat /\ ackedEver[x] \subseteq Nat
         /\ \A q \in R : \A x \in Blocks : head[q][x] \in Nat
         /\ \A q \in R : \A e \in Epochs : \A x \in Blocks : sCon[q][e][x] \in Nat /\ cutAck[q][e][x] \in Nat
         /\ \A w \in pend : /\ w.b \in Blocks /\ w.v \in Nat /\ w.v >= 1 /\ w.cutAt \in Nat
                            /\ w.to \subseteq R /\ w.landed \subseteq R /\ w.dropped \subseteq R
BY Const DEF TypeOK, TyScalar, TyHead, TySnap, TyPend, TyTrust, Vers, Write

LEMMA T_Submit ==
  ASSUME TypeOK, NEW x \in Blocks, Submit(x) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. Latest \in 0..MaxEpoch BY <1>0, LatestType
<1>u. UNCHANGED <<nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, restamp>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<acked, ackedEver>> /\ UNCHANGED tieVars /\ UNCHANGED trustVars
  BY DEF Submit
<1>1. TyScalar' BY <1>0, <1>u, Const DEF Submit, TyScalar, Vers
<1>2. TyLeg'
  <2>1. riskRec' \in [R -> 0..NoRisk]
    <3>. SUFFICES ASSUME NEW r \in R PROVE riskRec'[r] \in 0..NoRisk BY DEF Submit
    <3>1. riskRec[r] \in 0..NoRisk BY <1>0 DEF TyLeg
    <3>2. riskRec[r] \in Nat /\ Latest \in Nat BY <3>1, <1>a, NoRiskNat
    <3>3. Min({riskRec[r], Latest}) \in {riskRec[r], Latest} BY <3>2, Min2
    <3>4. Latest \in 0..NoRisk BY <1>a, NoRiskNat, Const
    <3>5. Min({riskRec[r], Latest}) \in 0..NoRisk BY <3>1, <3>3, <3>4
    <3>6. riskRec'[r] = IF r \in members THEN riskRec[r] ELSE Min({riskRec[r], Latest}) BY DEF Submit
    <3>. QED BY <3>1, <3>5, <3>6
  <2>. QED BY <1>0, <1>u, <2>1 DEF TyLeg
<1>3. TyPend'
  <2>1. ver + 1 \in 1..MaxVer BY <1>0, Const DEF Submit, TyScalar, Vers
  <2>2. nextSeq \in 1..(MaxEpoch + 1) BY <1>0 DEF TyScalar
  <2>3. members \in SUBSET R BY <1>0 DEF TySets
  <2>4. [b |-> x, v |-> ver + 1, to |-> members, landed |-> {}, dropped |-> {},
         recAt |-> Latest, cutAt |-> nextSeq] \in Write
    BY <1>a, <2>1, <2>2, <2>3 DEF Write
  <2>. QED BY <1>0, <1>u, <2>4 DEF Submit, TyPend
<1>. QED BY <1>0, <1>u, <1>1, <1>2, <1>3, C_Sets, C_Head, C_Snap, C_Tie, C_Trust, GroupsP

LEMMA T_Land ==
  ASSUME TypeOK, NEW w \in pend, NEW r \in R, Land(w, r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. w \in Write BY <1>0 DEF TyPend
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
      /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED tieVars /\ UNCHANGED trustVars
  BY DEF Land
<1>1. TyHead' BY <1>0, <1>a DEF Land, TyHead, Write, Vers
<1>2. TyPend' BY <1>0, <1>a DEF Land, TyPend, Write
<1>. QED BY <1>0, <1>u, <1>1, <1>2, C_Scalar, C_Sets, C_Leg, C_Snap, C_Tie, C_Trust, GroupsP

LEMMA T_Drop ==
  ASSUME TypeOK, NEW w \in pend, NEW r \in R, Drop(w, r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. w \in Write BY <1>0 DEF TyPend
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, restamp>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<acked, ackedEver>>
      /\ UNCHANGED tieVars /\ UNCHANGED trustVars
  BY DEF Drop
<1>1. TyLeg'
  <2>1. riskRec[r] \in 0..NoRisk BY <1>0 DEF TyLeg
  <2>2. riskRec[r] \in Nat /\ w.recAt \in Nat /\ w.recAt <= MaxEpoch
    BY <2>1, <1>a, Const, NoRiskNat DEF Write
  <2>3. Min({riskRec[r], w.recAt}) \in {riskRec[r], w.recAt} BY <2>2, Min2
  <2>5. Min({riskRec[r], w.recAt}) \in 0..NoRisk BY <2>1, <2>2, <2>3, NoRiskNat, Const
  <2>6. riskRec' = [riskRec EXCEPT ![r] = Min({riskRec[r], w.recAt})] BY DEF Drop
  <2>4. riskRec' \in [R -> 0..NoRisk] BY <1>0, <2>5, <2>6 DEF TyLeg
  <2>. QED BY <1>0, <1>u, <2>4 DEF TyLeg
<1>2. TySets' BY <1>0, Clean DEF Drop, TySets
<1>3. TyPend' BY <1>0, <1>a DEF Drop, TyPend, Write
<1>. QED BY <1>0, <1>u, <1>1, <1>2, <1>3, C_Scalar, C_Head, C_Snap, C_Tie, C_Trust, GroupsP

LEMMA T_Complete ==
  ASSUME TypeOK, NEW w \in pend, Complete(w) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. w \in Write BY <1>0 DEF TyPend
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED tieVars /\ UNCHANGED trustVars
  BY DEF Complete
<1>1. TyPend' BY <1>0, <1>a DEF Complete, TyPend, Write, Vers
<1>. QED BY <1>0, <1>u, <1>1, C_Scalar, C_Sets, C_Leg, C_Head, C_Snap, C_Tie, C_Trust, GroupsP

\* Path and the monitor: FailBegin, FaultOut, MarkStale, Return.
LEMMA T_Path ==
  ASSUME TypeOK, NEW r \in R, FailBegin(r) \/ FaultOut(r) \/ MarkStale(r) \/ Return(r)
  PROVE  TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>u. /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED trustVars
  BY DEF FailBegin, FaultOut, MarkStale, Return, trustVars
<1>s. TyScalar' /\ TySets' /\ TyLeg' /\ TyTie'
  <2>1. CASE FailBegin(r)
    <3>1. UNCHANGED <<ver, nextSeq, cutK, attached, badAdmit, badLoss, zx>>
          /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
          /\ UNCHANGED <<dState, mark, reverted, sb, pin, riskRec, restamp>> /\ UNCHANGED tieVars
      BY <2>1 DEF FailBegin
    <3>2. fails' \in 0..MaxFails BY <1>0, <2>1, Const DEF FailBegin, TyScalar
    <3>3. path' \in [R -> {"ok", "failing", "out", "back"}] BY <1>0, <2>1 DEF FailBegin, TyLeg
    <3>. QED BY <1>0, <3>1, <3>2, <3>3, C_Sets, C_Tie DEF TyScalar, TyLeg
  <2>2. CASE FaultOut(r)
    <3>1. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
          /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone>>
          /\ UNCHANGED <<dState, mark, reverted, sb, pin, riskRec, restamp>> /\ UNCHANGED tieVars
      BY <2>2 DEF FaultOut
    <3>2. members' \in SUBSET R BY <1>0, <2>2 DEF FaultOut, TySets
    <3>3. path' \in [R -> {"ok", "failing", "out", "back"}] BY <1>0, <2>2 DEF FaultOut, TyLeg
    <3>. QED BY <1>0, <3>1, <3>2, <3>3, C_Scalar, C_Tie DEF TySets, TyLeg
  <2>3. CASE MarkStale(r)
    <3>1. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
          /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
          /\ UNCHANGED <<path, mark, reverted, sb, pin, riskRec, used, mustRev>>
          /\ StaleMarks({r})
      BY <2>3 DEF MarkStale
    <3>2. TyLeg' /\ TyTie' BY <1>0, <3>1, T_StaleMarks
    <3>. QED BY <1>0, <3>1, <3>2, C_Scalar, C_Sets
  <2>4. CASE Return(r)
    <3>1. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
          /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
          /\ UNCHANGED <<dState, mark, reverted, sb, pin, riskRec, restamp>> /\ UNCHANGED tieVars
      BY <2>4 DEF Return
    <3>3. path' \in [R -> {"ok", "failing", "out", "back"}] BY <1>0, <2>4 DEF Return, TyLeg
    <3>. QED BY <1>0, <3>1, <3>3, C_Scalar, C_Sets, C_Tie DEF TyLeg
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
<1>. QED BY <1>0, <1>u, <1>s, C_Head, C_Snap, C_Pend, C_Trust, GroupsP

LEMMA T_Observe ==
  ASSUME TypeOK, NEW e \in Epochs, Observe(e) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED tieVars
      /\ UNCHANGED <<cfgAt, htr, cutAck>>
  BY DEF Observe
<1>1. TyTrust' BY <1>0, <1>u DEF Observe, TyTrust
<1>. QED BY <1>0, <1>u, <1>1, C_Scalar, C_Sets, C_Leg, C_Head, C_Snap, C_Pend, C_Tie, GroupsP

\* The scheduler's small steps.
LEMMA T_CutSmall ==
  ASSUME TypeOK, cutK # 0 => cutK = nextSeq,
         CutBegin \/ CutRecord \/ CutAbort \/ CutAbortRead \/ Retire
  PROVE  TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>u. /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED tieVars
  BY DEF CutBegin, CutRecord, CutAbort, CutAbortRead, Retire
<1>s. TyScalar' /\ TySets' /\ TyLeg' /\ TyTrust'
  <2>1. CASE CutBegin
    <3>1. UNCHANGED <<ver, nextSeq, attached, fails, badAdmit, badLoss, zx>>
          /\ UNCHANGED <<recorded, members>>
          /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
          /\ UNCHANGED trustVars
      BY <2>1 DEF CutBegin
    <3>2. cutK' \in 0..MaxEpoch BY <1>0, <2>1 DEF CutBegin, TyScalar
    <3>3. cutTgt' \in SUBSET R /\ cutDone' \in SUBSET R /\ cutGone' \in SUBSET R
      BY <2>1 DEF CutBegin, InSyncR
    <3>. QED BY <1>0, <3>1, <3>2, <3>3, C_Leg, C_Trust DEF TyScalar, TySets
  <2>2. CASE CutRecord
    <3>1. cutK \in Epochs /\ nextSeq = cutK BY <1>0, <2>2, Const DEF CutRecord, TyScalar, Epochs
    <3>2. UNCHANGED <<ver, attached, fails, badAdmit, badLoss, zx>> /\ UNCHANGED members
          /\ UNCHANGED <<path, dState, reverted, sb, pin, riskRec, restamp>>
          /\ UNCHANGED <<tr, htr, cutAck>>
      BY <2>2 DEF CutRecord
    <3>a. /\ recorded' = recorded \cup {cutK} /\ nextSeq' = nextSeq + 1
          /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
          /\ mark' = [q \in R |-> IF q \in cutTgt /\ (CutStampsAll \/ dState[q] = "insync")
                                  THEN cutK ELSE mark[q]]
          /\ cfgAt' = [cfgAt EXCEPT ![cutK] = cutTgt \cap members]
          /\ conf' = [conf EXCEPT ![cutK] = FALSE]
      BY <2>2, Clean DEF CutRecord
    <3>3. TyScalar'
      <4>1. nextSeq' \in 1..(MaxEpoch + 1) /\ cutK' \in 0..MaxEpoch BY <3>1, <3>a, Const DEF Epochs
      <4>. QED BY <1>0, <3>2, <4>1 DEF TyScalar
    <3>4. TySets' BY <1>0, <3>1, <3>2, <3>a DEF TySets
    <3>5. mark' \in [R -> 0..MaxEpoch] BY <1>0, <3>1, <3>a DEF TyLeg, Epochs
    <3>6. TyLeg' BY <1>0, <3>2, <3>5 DEF TyLeg
    <3>7. TyTrust'
      <4>1. cfgAt' \in [Epochs -> SUBSET R] BY <1>0, <3>1, <3>a DEF TyTrust, TySets
      <4>2. conf' \in [Epochs -> BOOLEAN] BY <1>0, <3>1, <3>a DEF TyTrust
      <4>. QED BY <1>0, <3>2, <4>1, <4>2 DEF TyTrust
    <3>. QED BY <3>3, <3>4, <3>6, <3>7
  <2>3. CASE CutAbort \/ CutAbortRead
    <3>1. UNCHANGED <<ver, nextSeq, attached, fails, badAdmit, badLoss, zx>>
          /\ UNCHANGED <<recorded, members>>
          /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
          /\ UNCHANGED trustVars
          /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
      BY <2>3 DEF CutAbort, CutAbortRead
    <3>2. TyScalar' /\ TySets' BY <1>0, <3>1, Const DEF TyScalar, TySets
    <3>. QED BY <1>0, <3>1, <3>2, C_Leg, C_Trust
  <2>4. CASE Retire
    <3>1. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
          /\ UNCHANGED <<cutTgt, cutDone, cutGone, members>>
          /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
          /\ UNCHANGED trustVars
      BY <2>4 DEF Retire
    <3>2. recorded' \in SUBSET Epochs BY <1>0, <2>4 DEF Retire, TySets
    <3>. QED BY <1>0, <3>1, <3>2, C_Scalar, C_Leg, C_Trust DEF TySets
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
<1>. QED BY <1>0, <1>u, <1>s, C_Head, C_Snap, C_Pend, C_Tie, GroupsP

\* A snapshot cut from r's head: its ancestor set is epochs.
LEMMA NewAncType ==
  ASSUME TyHead, TySnap, NEW r \in R, NEW e \in Epochs
  PROVE  NewAnc(r, e) \in SUBSET Epochs
<1>1. hPar[r] \in 0..MaxEpoch BY DEF TyHead
<1>2. hPar[r] # 0 => hPar[r] \in Epochs BY <1>1, Const DEF Epochs
<1>3. AncOf(r, hPar[r]) \in SUBSET Epochs BY <1>2 DEF AncOf, TySnap
<1>. QED BY <1>3 DEF NewAnc

LEMMA T_CutOne ==
  ASSUME TypeOK, NEW r \in R, CutOne(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. cutK \in Epochs BY <1>0, Const DEF CutOne, TyScalar, Epochs
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
      /\ UNCHANGED head /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED tieVars
      /\ UNCHANGED <<cfgAt, conf, htr>>
  BY DEF CutOne
<1>1. TySets' BY <1>0, <1>u DEF CutOne, TySets
<1>2. TyTrust' BY <1>0, <1>a, <1>u DEF CutOne, TyTrust, TyPend
<1>3. TyHead' /\ TySnap'
  <2>1. CASE cutK \in sPres[r]
    <3>1. UNCHANGED <<hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>> BY <2>1 DEF CutOne
    <3>. QED BY <1>0, <1>u, <3>1, C_Head, C_Snap
  <2>2. CASE cutK \notin sPres[r]
    <3>1. NewAnc(r, cutK) \in SUBSET Epochs BY <1>0, <1>a, NewAncType
    <3>a. /\ sPres' = [sPres EXCEPT ![r] = @ \cup {cutK}]
          /\ sCon' = [sCon EXCEPT ![r][cutK] = head[r]]
          /\ sAl' = [sAl EXCEPT ![r][cutK] = hAl[r]]
          /\ sPar' = [sPar EXCEPT ![r][cutK] = hPar[r]]
          /\ anc' = [anc EXCEPT ![r][cutK] = NewAnc(r, cutK)]
          /\ hPar' = [hPar EXCEPT ![r] = cutK]
          /\ hAl' = [hAl EXCEPT ![r] = {}]
      BY <2>2 DEF CutOne
    <3>2. TyHead' BY <1>0, <1>a, <1>u, <3>a DEF TyHead, Epochs
    <3>3. sPres' \in [R -> SUBSET Epochs] BY <1>0, <1>a, <3>a DEF TySnap
    <3>4. sCon' \in [R -> [Epochs -> [Blocks -> Vers]]] BY <1>0, <1>a, <3>a DEF TySnap, TyHead
    <3>5. sAl' \in [R -> [Epochs -> SUBSET Blocks]] BY <1>0, <1>a, <3>a DEF TySnap, TyHead
    <3>6. sPar' \in [R -> [Epochs -> 0..MaxEpoch]] BY <1>0, <1>a, <3>a DEF TySnap, TyHead
    <3>7. anc' \in [R -> [Epochs -> SUBSET Epochs]] BY <1>0, <1>a, <3>1, <3>a DEF TySnap
    <3>. QED BY <3>2, <3>3, <3>4, <3>5, <3>6, <3>7 DEF TySnap
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>0, <1>u, <1>1, <1>2, <1>3, C_Scalar, C_Leg, C_Pend, C_Tie, GroupsP

LEMMA T_GC ==
  ASSUME TypeOK, NEW r \in R, NEW e \in Epochs, GC(r, e) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, riskRec, restamp>>
      /\ UNCHANGED <<head, sCon>> /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED tieVars
      /\ UNCHANGED <<cfgAt, conf, htr>>
  BY DEF GC
<1>g. /\ sPres' = [sPres EXCEPT ![r] = @ \ {e}]
      /\ anc' = [anc EXCEPT ![r] = [c \in Epochs |-> IF c = e THEN {} ELSE anc[r][c] \ {e}]]
      /\ \/ /\ hAl' = [hAl EXCEPT ![r] = @ \cup sAl[r][e]]
            /\ hPar' = [hPar EXCEPT ![r] = sPar[r][e]]
            /\ UNCHANGED <<sAl, sPar>>
         \/ /\ sAl' = [sAl EXCEPT ![r] = [c \in Epochs |->
                         IF c \in Children(r, e) THEN sAl[r][c] \cup sAl[r][e] ELSE sAl[r][c]]]
            /\ sPar' = [sPar EXCEPT ![r] = [c \in Epochs |->
                         IF c \in Children(r, e) THEN sPar[r][e] ELSE sPar[r][c]]]
            /\ UNCHANGED <<hAl, hPar>>
  BY DEF GC
<1>1. TyHead' BY <1>0, <1>u, <1>g DEF TyHead, TySnap, Epochs
<1>2. TySnap'
  <2>1. sPres' \in [R -> SUBSET Epochs] BY <1>0, <1>g DEF TySnap
  <2>2. anc' \in [R -> [Epochs -> SUBSET Epochs]] BY <1>0, <1>g DEF TySnap
  <2>3. sAl' \in [R -> [Epochs -> SUBSET Blocks]] /\ sPar' \in [R -> [Epochs -> 0..MaxEpoch]]
    BY <1>0, <1>g DEF TySnap
  <2>. QED BY <1>0, <1>u, <2>1, <2>2, <2>3 DEF TySnap
<1>3. TyTrust' BY <1>0, <1>u, Const DEF GC, TyTrust, Zero, Vers
<1>. QED BY <1>0, <1>u, <1>1, <1>2, <1>3, C_Scalar, C_Sets, C_Leg, C_Pend, C_Tie, GroupsP

LEMMA T_SelectBase ==
  ASSUME TypeOK, NEW r \in R, SelectBase(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. recorded # {} => Min(recorded) \in recorded
  BY <1>0, MinProp DEF TySets, Epochs
<1>b. BaseCandidates(r) \subseteq Epochs BY <1>0 DEF BaseCandidates, TySets
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, riskRec, restamp>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED tieVars /\ UNCHANGED trustVars
  BY DEF SelectBase
<1>1. TyLeg'
  <2>1. sb'[r] \in 0..MaxEpoch /\ sb' = [sb EXCEPT ![r] = sb'[r]]
    BY <1>0, <1>b, Const DEF SelectBase, TyLeg, Epochs
  <2>2. sb' \in [R -> -1..MaxEpoch] BY <1>0, <2>1 DEF TyLeg
  <2>3. reverted' \in [R -> -1..MaxEpoch] BY <1>0 DEF SelectBase, TyLeg
  <2>4. pin' \in [R -> 0..MaxEpoch] BY <1>0, <1>a, <2>1 DEF SelectBase, TyLeg, TySets, Epochs
  <2>. QED BY <1>0, <1>u, <2>2, <2>3, <2>4 DEF TyLeg
<1>. QED BY <1>0, <1>u, <1>1, C_Scalar, C_Sets, C_Head, C_Snap, C_Pend, C_Tie, C_Trust, GroupsP

LEMMA T_SessionFail ==
  ASSUME TypeOK, NEW r \in R, SessionFail(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, dState, mark, pin, riskRec, restamp>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED tieVars /\ UNCHANGED trustVars
  BY DEF SessionFail
<1>1. TyLeg' BY <1>0, <1>u, Const DEF SessionFail, TyLeg
<1>. QED BY <1>0, <1>u, <1>1, C_Scalar, C_Sets, C_Head, C_Snap, C_Pend, C_Tie, C_Trust, GroupsP

LEMMA T_RevertD ==
  ASSUME TypeOK, NEW r \in R, RevertD(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. sb[r] \in 0..MaxEpoch BY <1>0 DEF RevertD, TyLeg
<1>b. (IF sb[r] = 0 THEN Zero ELSE sCon[r][sb[r]]) \in [Blocks -> Vers]
  <2>1. CASE sb[r] = 0 BY <2>1, Const DEF Zero, Vers
  <2>2. CASE sb[r] # 0 BY <1>0, <1>a, <2>2, Const DEF TySnap, Epochs
  <2>. QED BY <2>1, <2>2
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, members>>
      /\ UNCHANGED <<path, dState, mark, sb, pin, riskRec, restamp>>
      /\ UNCHANGED <<sCon, sAl, sPar>> /\ UNCHANGED <<pend, acked, ackedEver>>
      /\ UNCHANGED tieVars /\ UNCHANGED <<cfgAt, conf, htr>>
  BY Clean DEF RevertD
<1>1. TySets' BY <1>0, <1>u DEF RevertD, TySets
<1>2. TyLeg' BY <1>0, <1>a, <1>u DEF RevertD, TyLeg
<1>3. TyHead' BY <1>0, <1>a, <1>b DEF RevertD, TyHead
<1>4. TySnap' BY <1>0, <1>u DEF RevertD, TySnap, Epochs
<1>5. TyTrust' BY <1>0, <1>u, Const DEF RevertD, TyTrust, Zero, Vers
<1>. QED BY <1>0, <1>u, <1>1, <1>2, <1>3, <1>4, <1>5, C_Scalar, C_Pend, C_Tie, GroupsP

\* A copy's result is a head.
LEMMA AfterCopyType ==
  ASSUME TySnap, NEW start \in [Blocks -> Vers], NEW s \in R, NEW lo \in Int,
         NEW target \in Int, Lin(s) \subseteq Epochs
  PROVE  AfterCopy(start, s, lo, target) \in [Blocks -> Vers]
<1>. SUFFICES ASSUME NEW x \in Blocks,
                    {e \in CopyElems(s, lo, target) : x \in sAl[s][e]} # {}
             PROVE  sCon[s][Max({e \in CopyElems(s, lo, target) : x \in sAl[s][e]})][x] \in Vers
  BY DEF AfterCopy
<1>. DEFINE H == {e \in CopyElems(s, lo, target) : x \in sAl[s][e]}
<1>1. H \in SUBSET (0..MaxEpoch) BY Const DEF CopyElems, Epochs
<1>2. Max(H) \in H BY <1>1, Const, MaxProp
<1>3. Max(H) \in Epochs BY <1>2 DEF CopyElems
<1>. QED BY <1>3 DEF TySnap

LEMMA LinType == ASSUME TyHead, TySnap, NEW s \in R PROVE Lin(s) \subseteq Epochs
<1>1. hPar[s] \in 0..MaxEpoch BY DEF TyHead
<1>2. hPar[s] # 0 => hPar[s] \in Epochs BY <1>1, Const DEF Epochs
<1>. QED BY <1>2 DEF Lin, AncOf, TySnap

LEMMA T_CopyAlign ==
  ASSUME TyHead, TySnap, TyTrust, NEW r \in R, NEW s \in R,
         NEW from \in 0..MaxEpoch, NEW target \in Epochs, CopyAlign(r, s, from, target),
         UNCHANGED <<cfgAt, conf, htr>>
  PROVE  TyHead' /\ TySnap' /\ TyTrust'
<1>. DEFINE lo == IF BaseInclusive \/ from = 0 THEN from ELSE from + 1
<1>0. head[r] \in [Blocks -> Vers] /\ lo \in Int /\ target \in Int BY Const DEF TyHead, Epochs
<1>1. AfterCopy(head[r], s, lo, target) \in [Blocks -> Vers]
  BY <1>0, AfterCopyType, LinType
<1>2. hAl[r] \cup Copied(s, lo, target) \subseteq Blocks BY DEF Copied, TyHead
<1>3. TyHead' BY <1>1, <1>2 DEF CopyAlign, TyHead, Epochs
<1>4. CASE from = target \/ target \in sPres[r]
  <2>. QED BY <1>3, <1>4 DEF CopyAlign, TySnap, TyTrust
<1>5. CASE ~(from = target \/ target \in sPres[r])
  <2>1. NewAnc(r, target) \in SUBSET Epochs BY NewAncType
  <2>a. /\ sPres' = [sPres EXCEPT ![r] = @ \cup {target}]
        /\ sCon' = [sCon EXCEPT ![r][target] = AfterCopy(head[r], s, lo, target)]
        /\ sAl' = [sAl EXCEPT ![r][target] = hAl[r] \cup Copied(s, lo, target)]
        /\ sPar' = [sPar EXCEPT ![r][target] = hPar[r]]
        /\ anc' = [anc EXCEPT ![r][target] = NewAnc(r, target)]
        /\ tr' = [tr EXCEPT ![r] = @ \cup {target}]
        /\ cutAck' = [cutAck EXCEPT ![r][target] = cutAck[s][target]]
    BY <1>5, Clean DEF CopyAlign
  <2>2. TySnap'
    <3>1. sPres' \in [R -> SUBSET Epochs] BY <2>a DEF TySnap
    <3>2. sCon' \in [R -> [Epochs -> [Blocks -> Vers]]] BY <1>1, <2>a DEF TySnap
    <3>3. sAl' \in [R -> [Epochs -> SUBSET Blocks]] BY <1>2, <2>a DEF TySnap
    <3>4. sPar' \in [R -> [Epochs -> 0..MaxEpoch]] BY <2>a DEF TySnap, TyHead
    <3>5. anc' \in [R -> [Epochs -> SUBSET Epochs]] BY <2>1, <2>a DEF TySnap
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5 DEF TySnap
  <2>3. TyTrust' BY <2>a DEF TyTrust
  <2>. QED BY <1>3, <2>2, <2>3
<1>. QED BY <1>4, <1>5

\* A copy's source is a replica.
LEMMA CoverSetSub ==
  ASSUME NEW r \in R, NEW t, NEW b PROVE CoverSet(r, t, b) \subseteq R
<1>1. Sources(r) \subseteq R BY DEF Sources
<1>2. Sources(r) # {} => First(Sources(r)) \in Sources(r) BY <1>1, RNat, MinProp DEF First
<1>. QED BY <1>1, <1>2 DEF CoverSet

\* The trust target is a recorded epoch, or 0.
LEMMA TgtType ==
  ASSUME TySets PROVE Tgt \in {0} \cup recorded /\ (Tgt # 0 => Tgt \in Epochs)
<1>1. TrustedRec \subseteq recorded BY DEF TrustedRec
<1>2. TrustedRec # {} => Max(TrustedRec) \in TrustedRec
  BY <1>1, Const, MaxProp DEF TySets, Epochs
<1>. QED BY <1>1, <1>2, Clean DEF Tgt, TySets, Epochs

LEMMA T_BulkCopy ==
  ASSUME TypeOK, NEW r \in R, BulkCopy(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. Tgt \in Epochs BY <1>0, TgtType DEF BulkCopy
<1>b. sb[r] \in 0..MaxEpoch BY <1>0 DEF BulkCopy, TyLeg
<1>c. PICK s \in CoverSet(r, Tgt, sb[r]) :
        /\ CopyAlign(r, s, sb[r], Tgt)
        /\ used' = used
        /\ mark' = [mark EXCEPT ![r] = Tgt]
        /\ pin' = [pin EXCEPT ![r] = IF PinRetention THEN Tgt ELSE pin[r]]
  BY Clean DEF BulkCopy
<1>d. s \in R BY CoverSetSub
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, reverted, riskRec, restamp>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED <<tainted, floor, mustRev>>
      /\ UNCHANGED <<cfgAt, conf, htr>>
  BY DEF BulkCopy
<1>1. TyHead' /\ TySnap' /\ TyTrust' BY <1>0, <1>a, <1>b, <1>c, <1>d, <1>u, T_CopyAlign
<1>e. dState' = [dState EXCEPT ![r] = "standby"] /\ sb' = [sb EXCEPT ![r] = -1] BY DEF BulkCopy
<1>2. TyLeg' BY <1>0, <1>a, <1>c, <1>e, <1>u, Clean, Const DEF TyLeg, Epochs
<1>3. TyTie' BY <1>0, <1>c, <1>u DEF TyTie
<1>. QED BY <1>0, <1>u, <1>1, <1>2, <1>3, C_Scalar, C_Sets, C_Pend, GroupsP

LEMMA T_Chase ==
  ASSUME TypeOK, NEW r \in R, Chase(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>a. Tgt \in Epochs BY <1>0, TgtType DEF Chase
<1>b. mark[r] \in 0..MaxEpoch BY <1>0 DEF TyLeg
<1>u. UNCHANGED <<ver, nextSeq, cutK, attached, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone, members>>
      /\ UNCHANGED <<path, sb, riskRec>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED <<tainted, floor>>
      /\ UNCHANGED <<cfgAt, conf, htr>>
  BY DEF Chase
<1>1. CASE CoverSet(r, Tgt, mark[r]) # {}
  <2>1. PICK s \in CoverSet(r, Tgt, mark[r]) :
          /\ CopyAlign(r, s, mark[r], Tgt)
          /\ used' = used
          /\ mark' = [mark EXCEPT ![r] = Tgt]
          /\ pin' = [pin EXCEPT ![r] = IF PinRetention THEN Tgt ELSE pin[r]]
    BY <1>1, Clean DEF Chase
  <2>2. s \in R BY CoverSetSub
  <2>3. TyHead' /\ TySnap' /\ TyTrust' BY <1>0, <1>a, <1>b, <1>u, <2>1, <2>2, T_CopyAlign
  <2>4. UNCHANGED <<dState, reverted, restamp, mustRev>> BY <1>1 DEF Chase
  <2>5. TyLeg' BY <1>0, <1>a, <1>u, <2>1, <2>4 DEF TyLeg, Epochs
  <2>6. TyTie' BY <1>0, <1>u, <2>1, <2>4 DEF TyTie
  <2>. QED BY <1>0, <1>u, <2>3, <2>5, <2>6, C_Scalar, C_Sets, C_Pend, GroupsP
<1>2. CASE CoverSet(r, Tgt, mark[r]) = {}
  <2>1. /\ dState' = [dState EXCEPT ![r] = "stale"] /\ Demote(r)
        /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
        /\ UNCHANGED <<mark, pin, used>> /\ UNCHANGED trustVars
    BY <1>2 DEF Chase, trustVars
  <2>2. reverted' = [reverted EXCEPT ![r] = -1] /\ UNCHANGED <<restamp, mustRev>> BY <2>1, Clean DEF Demote
  <2>3. TyLeg' BY <1>0, <1>u, <2>1, <2>2, Const DEF TyLeg
  <2>4. TyTie' BY <1>0, <1>u, <2>1, <2>2 DEF TyTie
  <2>. QED BY <1>0, <1>u, <2>1, <2>3, <2>4, C_Scalar, C_Sets, C_Head, C_Snap, C_Pend, C_Trust, GroupsP
<1>. QED BY <1>1, <1>2

LEMMA T_Unstage ==
  ASSUME TypeOK, Unstage PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>1. StaleMarks({q \in InSyncR : q \notin members})
  BY Clean, QuorumTrue DEF Unstage
<1>u. UNCHANGED <<ver, nextSeq, cutK, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone>>
      /\ UNCHANGED <<path, mark, reverted, sb, pin, riskRec, used, mustRev>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED <<cfgAt, cutAck>>
  BY DEF Unstage
<1>2. TyLeg' /\ TyTie' BY <1>0, <1>1, <1>u, T_StaleMarks
<1>3. TyScalar' BY <1>0, <1>u DEF Unstage, TyScalar
<1>4. TySets' BY <1>0, <1>u DEF Unstage, TySets
<1>5. TyTrust' BY <1>0, <1>u, Clean DEF Unstage, TyTrust
<1>. QED BY <1>0, <1>u, <1>2, <1>3, <1>4, <1>5, C_Head, C_Snap, C_Pend, GroupsP

LEMMA T_Stage ==
  ASSUME TypeOK, Stage PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>1. StaleMarks(AsmExcl(Attachable)) BY DEF Stage
<1>u. UNCHANGED <<ver, nextSeq, cutK, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone>>
      /\ UNCHANGED <<mark, reverted, sb, pin, riskRec, used, mustRev>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED <<cfgAt, conf, tr, cutAck>>
  BY DEF Stage
<1>2. TyLeg' /\ TyTie'
  <2>1. path' \in [R -> {"ok", "failing", "out", "back"}] BY <1>0 DEF Stage, TyLeg
  <2>2. dState' \in [R -> {"insync", "stale", "standby"}] /\ restamp' \in [R -> BOOLEAN]
        /\ UNCHANGED <<tainted, floor>>
    BY <1>0, <1>1, Clean DEF StaleMarks, TyLeg
  <2>. QED BY <1>0, <1>u, <2>1, <2>2 DEF TyLeg, TyTie
<1>3. TyScalar' BY <1>0, <1>u DEF Stage, TyScalar
<1>4. TySets' BY <1>0, <1>u DEF Stage, TySets, Attachable, InSyncR
<1>5. TyTrust' BY <1>0, <1>u, Clean DEF Stage, TyTrust
<1>. QED BY <1>0, <1>u, <1>2, <1>3, <1>4, <1>5, C_Head, C_Snap, C_Pend, GroupsP

-----------------------------------------------------------------------------
(* StageAdmit's LET, named (trust: the final delta's source is any member  *)
(* of the assembly whose lineage covers the standby's mark).               *)

AdmK == nextSeq
AdmT == Attachable
AdmElems(r, s) == {e \in Lin(s) \cup {AdmK} : mark[r] <= e /\ e <= AdmK}
AdmAl(s, e) == IF e = AdmK THEN hAl[s] ELSE sAl[s][e]
AdmCon(s, e) == IF e = AdmK THEN head[s] ELSE sCon[s][e]
AdmHits(r, s, x) == {e \in AdmElems(r, s) : x \in AdmAl(s, e)}
AdmHead(r, s) == [x \in Blocks |-> IF AdmHits(r, s, x) = {} THEN head[r][x]
                                   ELSE AdmCon(s, Max(AdmHits(r, s, x)))[x]]
AdmAlD(r, s) == hAl[r] \cup {x \in Blocks : \E e \in AdmElems(r, s) : x \in AdmAl(s, e)}

\* The guard and the snapshot effects of StageAdmit(r) with source s.
AdmGuard(r, s) ==
  /\ ~attached /\ cutK = 0 /\ dState[r] = "standby" /\ path[r] = "back"
  /\ nextSeq <= MaxEpoch /\ recorded # {} /\ AdmT # {}
  /\ mark[r] \in tr[r]
  /\ \A t \in AdmT : AdmK \notin sPres[t]
  /\ AdmK \notin sPres[r]
  /\ s \in AdmT /\ LinCovers(s, mark[r])
AdmSnap(r, s) ==
  /\ sPres' = [q \in R |-> IF q \in AdmT \cup {r} THEN sPres[q] \cup {AdmK} ELSE sPres[q]]
  /\ sCon' = [q \in R |-> IF q \in AdmT THEN [sCon[q] EXCEPT ![AdmK] = head[q]]
                          ELSE IF q = r THEN [sCon[q] EXCEPT ![AdmK] = AdmHead(r, s)]
                          ELSE sCon[q]]
  /\ sAl' = [q \in R |-> IF q \in AdmT THEN [sAl[q] EXCEPT ![AdmK] = hAl[q]]
                         ELSE IF q = r THEN [sAl[q] EXCEPT ![AdmK] = AdmAlD(r, s)]
                         ELSE sAl[q]]
  /\ sPar' = [q \in R |-> IF q \in AdmT \cup {r} THEN [sPar[q] EXCEPT ![AdmK] = hPar[q]]
                          ELSE sPar[q]]
  /\ anc' = [q \in R |-> IF q \in AdmT \cup {r} THEN [anc[q] EXCEPT ![AdmK] = NewAnc(q, AdmK)]
                         ELSE anc[q]]
  /\ hPar' = [q \in R |-> IF q \in AdmT \cup {r} THEN AdmK ELSE hPar[q]]
  /\ hAl' = [q \in R |-> IF q \in AdmT \cup {r} THEN {} ELSE hAl[q]]
  /\ head' = [head EXCEPT ![r] = AdmHead(r, s)]
AdmRest(r, s) ==
  /\ badAdmit' = (badAdmit \/ AdmHead(r, s) # head[s])
  /\ badLoss' = (badLoss \/ \E x \in Blocks :
                    \/ head[s][x] >= acked[x] /\ AdmHead(r, s)[x] < acked[x]
                    \/ AdmHead(r, s)[x] \notin ackedEver[x] \cup {head[q][x] : q \in AdmT})
  /\ tr' = [q \in R |-> IF (q \in AdmT /\ htr[q]) \/ (q = r /\ htr[s])
                        THEN tr[q] \cup {AdmK} ELSE tr[q] \ {AdmK}]
  /\ cutAck' = [q \in R |-> IF q \in AdmT \cup {r} THEN [cutAck[q] EXCEPT ![AdmK] = acked]
                            ELSE cutAck[q]]
  /\ htr' = [q \in R |-> FALSE]
  /\ recorded' = recorded \cup {AdmK}
  /\ nextSeq' = nextSeq + 1
  /\ mark' = [q \in R |-> IF q \in AdmT \cup {r} THEN AdmK ELSE mark[q]]
  /\ members' = AdmT \cup {r}
  /\ path' = [q \in R |-> IF q \in AdmT \cup {r} THEN "ok" ELSE path[q]]
  /\ dState' = [q \in R |-> IF q = r THEN "insync"
                            ELSE IF q \in AsmExcl(AdmT) THEN "stale" ELSE dState[q]]
  /\ restamp' = [q \in R |-> IF q = r \/ q \in AsmExcl(AdmT) THEN FALSE ELSE restamp[q]]
  /\ reverted' = [reverted EXCEPT ![r] = -1]
  /\ pin' = [pin EXCEPT ![r] = 0]
  /\ attached' = TRUE
  /\ riskRec' = [riskRec EXCEPT ![r] = NoRisk]
  /\ UNCHANGED tieVars
  /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, cutGone, sb, pend, acked, fails, cfgAt, conf, zx, ackedEver>>

LEMMA StageAdmitFacts ==
  ASSUME NEW r \in R, StageAdmit(r)
  PROVE  \E s \in R : AdmGuard(r, s) /\ AdmSnap(r, s) /\ AdmRest(r, s)
<1>1. AssemblyQuorum = 1 /\ Policy = "trust" /\ BaseInclusive = TRUE BY Clean
<1>2. PICK s \in {q \in Attachable : LinCovers(q, mark[r])} :
        LET lineS == Lin(s) \cup {nextSeq}
            elems == {e \in lineS : mark[r] <= e /\ e <= nextSeq}
            alOf(e) == IF e = nextSeq THEN hAl[s] ELSE sAl[s][e]
            conOf(e) == IF e = nextSeq THEN head[s] ELSE sCon[s][e]
            dHead == [x \in Blocks |->
                       LET hits == {e \in elems : x \in alOf(e)}
                       IN IF hits = {} THEN head[r][x] ELSE conOf(Max(hits))[x]]
            dAl == hAl[r] \cup {x \in Blocks : \E e \in elems : x \in alOf(e)}
        IN /\ sPres' = [q \in R |-> IF q \in Attachable \cup {r} THEN sPres[q] \cup {nextSeq} ELSE sPres[q]]
           /\ sCon' = [q \in R |-> IF q \in Attachable THEN [sCon[q] EXCEPT ![nextSeq] = head[q]]
                                   ELSE IF q = r THEN [sCon[q] EXCEPT ![nextSeq] = dHead]
                                   ELSE sCon[q]]
           /\ sAl' = [q \in R |-> IF q \in Attachable THEN [sAl[q] EXCEPT ![nextSeq] = hAl[q]]
                                  ELSE IF q = r THEN [sAl[q] EXCEPT ![nextSeq] = dAl]
                                  ELSE sAl[q]]
           /\ sPar' = [q \in R |-> IF q \in Attachable \cup {r} THEN [sPar[q] EXCEPT ![nextSeq] = hPar[q]]
                                   ELSE sPar[q]]
           /\ anc' = [q \in R |-> IF q \in Attachable \cup {r} THEN [anc[q] EXCEPT ![nextSeq] = NewAnc(q, nextSeq)]
                                  ELSE anc[q]]
           /\ hPar' = [q \in R |-> IF q \in Attachable \cup {r} THEN nextSeq ELSE hPar[q]]
           /\ hAl' = [q \in R |-> IF q \in Attachable \cup {r} THEN {} ELSE hAl[q]]
           /\ head' = [head EXCEPT ![r] = dHead]
           /\ badAdmit' = (badAdmit \/ dHead # head[s])
           /\ badLoss' = (badLoss \/ \E x \in Blocks :
                             \/ head[s][x] >= acked[x] /\ dHead[x] < acked[x]
                             \/ dHead[x] \notin ackedEver[x] \cup {head[q][x] : q \in Attachable})
           /\ tr' = [q \in R |-> IF (q \in Attachable /\ htr[q]) \/ (q = r /\ htr[s])
                                 THEN tr[q] \cup {nextSeq} ELSE tr[q] \ {nextSeq}]
           /\ cutAck' = [q \in R |-> IF q \in Attachable \cup {r} THEN [cutAck[q] EXCEPT ![nextSeq] = acked]
                                     ELSE cutAck[q]]
           /\ htr' = [q \in R |-> FALSE]
  BY <1>1 DEF StageAdmit
<1>3. s \in R BY DEF Attachable, InSyncR
<1>4. AdmGuard(r, s) BY <1>1, <1>2, Clean DEF StageAdmit, AdmGuard, AdmT, AdmK
<1>5. AdmHead(r, s) = [x \in Blocks |->
                         LET hits == {e \in {e \in Lin(s) \cup {nextSeq} : mark[r] <= e /\ e <= nextSeq} :
                                        x \in (IF e = nextSeq THEN hAl[s] ELSE sAl[s][e])}
                         IN IF hits = {} THEN head[r][x]
                            ELSE (IF Max(hits) = nextSeq THEN head[s] ELSE sCon[s][Max(hits)])[x]]
  BY DEF AdmHead, AdmHits, AdmElems, AdmAl, AdmCon, AdmK
<1>6. AdmAlD(r, s) = hAl[r] \cup {x \in Blocks : \E e \in {e \in Lin(s) \cup {nextSeq} : mark[r] <= e /\ e <= nextSeq} :
                                     x \in (IF e = nextSeq THEN hAl[s] ELSE sAl[s][e])}
  BY DEF AdmAlD, AdmElems, AdmAl, AdmK
<1>7. AdmSnap(r, s) BY <1>2, <1>5, <1>6 DEF AdmSnap, AdmT, AdmK
<1>8. AdmRest(r, s) BY <1>1, <1>2, <1>5, Clean DEF StageAdmit, AdmRest, AdmT, AdmK, tieVars
<1>. QED BY <1>3, <1>4, <1>7, <1>8

LEMMA AdmHeadType ==
  ASSUME TyScalar, TyHead, TySnap, NEW r \in R, NEW s \in R, nextSeq <= MaxEpoch
  PROVE  AdmHead(r, s) \in [Blocks -> Vers] /\ AdmAlD(r, s) \subseteq Blocks
<1>1. AdmK \in Epochs BY DEF TyScalar, AdmK, Epochs
<1>2. Lin(s) \subseteq Epochs BY LinType
<1>3. \A x \in Blocks : AdmHits(r, s, x) \in SUBSET (0..MaxEpoch)
  BY <1>1, <1>2 DEF AdmHits, AdmElems, Epochs
<1>4. \A x \in Blocks : AdmHits(r, s, x) # {} => AdmCon(s, Max(AdmHits(r, s, x)))[x] \in Vers
  <2>. SUFFICES ASSUME NEW x \in Blocks, AdmHits(r, s, x) # {}
               PROVE  AdmCon(s, Max(AdmHits(r, s, x)))[x] \in Vers
    OBVIOUS
  <2>1. Max(AdmHits(r, s, x)) \in AdmHits(r, s, x) BY <1>3, MaxProp, Const
  <2>2. Max(AdmHits(r, s, x)) \in Epochs BY <1>1, <1>2, <2>1 DEF AdmHits, AdmElems
  <2>3. \A e \in Epochs : AdmCon(s, e) \in [Blocks -> Vers] BY DEF AdmCon, TySnap, TyHead
  <2>. QED BY <2>2, <2>3
<1>5. AdmHead(r, s) \in [Blocks -> Vers] BY <1>4 DEF AdmHead, TyHead
<1>6. \A e \in AdmElems(r, s) : AdmAl(s, e) \subseteq Blocks
  BY <1>1, <1>2 DEF AdmElems, AdmAl, TyHead, TySnap
<1>. QED BY <1>5, <1>6 DEF AdmAlD, TyHead

LEMMA T_StageAdmit ==
  ASSUME TypeOK, NEW r \in R, StageAdmit(r) PROVE TypeOK'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>1. PICK s \in R : AdmGuard(r, s) /\ AdmSnap(r, s) /\ AdmRest(r, s) BY StageAdmitFacts
<1>2. AdmK \in Epochs /\ nextSeq <= MaxEpoch BY <1>0, <1>1 DEF AdmGuard, TyScalar, AdmK, Epochs
<1>3. AdmHead(r, s) \in [Blocks -> Vers] /\ AdmAlD(r, s) \subseteq Blocks BY <1>0, <1>2, AdmHeadType
<1>4. AdmT \subseteq R BY DEF AdmT, Attachable, InSyncR
<1>5. \A q \in R : NewAnc(q, AdmK) \in SUBSET Epochs BY <1>0, <1>2, NewAncType
<1>r. /\ nextSeq' = nextSeq + 1 /\ attached' = TRUE /\ badAdmit' \in BOOLEAN /\ badLoss' \in BOOLEAN
      /\ UNCHANGED <<ver, cutK, fails, zx>>
      /\ mark' = [q \in R |-> IF q \in AdmT \cup {r} THEN AdmK ELSE mark[q]]
      /\ path' = [q \in R |-> IF q \in AdmT \cup {r} THEN "ok" ELSE path[q]]
      /\ dState' = [q \in R |-> IF q = r THEN "insync"
                                ELSE IF q \in AsmExcl(AdmT) THEN "stale" ELSE dState[q]]
      /\ restamp' = [q \in R |-> IF q = r \/ q \in AsmExcl(AdmT) THEN FALSE ELSE restamp[q]]
      /\ reverted' = [reverted EXCEPT ![r] = -1] /\ pin' = [pin EXCEPT ![r] = 0]
      /\ riskRec' = [riskRec EXCEPT ![r] = NoRisk] /\ UNCHANGED sb
  BY <1>1 DEF AdmRest
<1>s1. TyScalar' BY <1>0, <1>2, <1>r, Const DEF TyScalar, AdmK
<1>s2. TySets' BY <1>0, <1>1, <1>2, <1>4 DEF AdmRest, TySets
<1>s3. TyLeg'
  <2>1. mark' \in [R -> 0..MaxEpoch] BY <1>0, <1>2, <1>r DEF TyLeg, Epochs, AdmK
  <2>2. path' \in [R -> {"ok", "failing", "out", "back"}] /\ dState' \in [R -> {"insync", "stale", "standby"}]
        /\ restamp' \in [R -> BOOLEAN]
    BY <1>0, <1>r DEF TyLeg
  <2>3. reverted' \in [R -> -1..MaxEpoch] /\ pin' \in [R -> 0..MaxEpoch] /\ sb' \in [R -> -1..MaxEpoch]
    BY <1>0, <1>r, Const DEF TyLeg
  <2>4. riskRec' \in [R -> 0..NoRisk] BY <1>0, <1>r, NoRiskNat DEF TyLeg
  <2>. QED BY <2>1, <2>2, <2>3, <2>4 DEF TyLeg
<1>s4. TyHead' BY <1>0, <1>1, <1>2, <1>3 DEF AdmSnap, TyHead, Epochs
<1>s5. TySnap'
  <2>1. sPres' \in [R -> SUBSET Epochs] BY <1>0, <1>1, <1>2 DEF AdmSnap, TySnap
  <2>2. sCon' \in [R -> [Epochs -> [Blocks -> Vers]]] BY <1>0, <1>1, <1>2, <1>3 DEF AdmSnap, TySnap, TyHead
  <2>3. sAl' \in [R -> [Epochs -> SUBSET Blocks]] BY <1>0, <1>1, <1>2, <1>3 DEF AdmSnap, TySnap, TyHead
  <2>4. sPar' \in [R -> [Epochs -> 0..MaxEpoch]] BY <1>0, <1>1, <1>2 DEF AdmSnap, TySnap, TyHead
  <2>5. anc' \in [R -> [Epochs -> SUBSET Epochs]] BY <1>0, <1>1, <1>2, <1>5 DEF AdmSnap, TySnap
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF TySnap
<1>s6. TyPend' BY <1>0, <1>1 DEF AdmRest, TyPend
<1>s7. TyTie' BY <1>0, <1>1, C_Tie DEF AdmRest
<1>s8. TyTrust' BY <1>0, <1>1, <1>2 DEF AdmRest, TyTrust, TyPend
<1>. QED BY <1>s1, <1>s2, <1>s3, <1>s4, <1>s5, <1>s6, <1>s7, <1>s8, GroupsP

-----------------------------------------------------------------------------
(* A snapshot chain, in general: present set P, parents Par, ghost         *)
(* ancestors An, allocations Al, contents Con.  Everything the catch-up    *)
(* reads off a lineage follows from three facts: parents are older and     *)
(* present, An is the parent walk, and an unallocated block reads through. *)

\* (Up, ChainShape and ChainReads are FlintCatchupTInd's.)

LEMMA AncBound ==
  ASSUME NEW P, NEW Par, NEW An, ChainShape(P, Par, An)
  PROVE  \A e \in P : e \in An[e] /\ An[e] \subseteq P /\ \A f \in An[e] : f <= e
<1>. DEFINE Q(n) == n \in P => (n \in An[n] /\ An[n] \subseteq P /\ \A f \in An[n] : f <= n)
<1>1. \A n \in Nat : (\A m \in 0..(n-1) : Q(m)) => Q(n)
  <2>. SUFFICES ASSUME NEW n \in Nat, \A m \in 0..(n-1) : Q(m), n \in P
               PROVE  n \in An[n] /\ An[n] \subseteq P /\ \A f \in An[n] : f <= n
    OBVIOUS
  <2>1. Par[n] \in 0..MaxEpoch /\ Par[n] < n /\ (Par[n] # 0 => Par[n] \in P)
    BY DEF ChainShape
  <2>2. An[n] = {n} \cup (IF Par[n] = 0 THEN {} ELSE An[Par[n]]) BY DEF ChainShape
  <2>3. CASE Par[n] = 0 BY <2>2, <2>3 DEF ChainShape
  <2>4. CASE Par[n] # 0
    <3>1. Par[n] \in 0..(n-1) /\ Par[n] \in P BY <2>1, <2>4, Const
    <3>2. An[Par[n]] \subseteq P /\ \A f \in An[Par[n]] : f <= Par[n] BY <3>1
    <3>. QED BY <2>1, <2>2, <2>4, <3>1, <3>2 DEF ChainShape, Epochs
  <2>. QED BY <2>3, <2>4
<1>2. \A n \in Nat : Q(n)
  <2>. HIDE DEF Q
  <2>. SUFFICES ASSUME NEW n0 \in Nat, ~Q(n0) PROVE FALSE OBVIOUS
  <2>. DEFINE S == {n \in Nat : ~Q(n)}
  <2>1. S \in SUBSET Nat /\ S # {} OBVIOUS
  <2>2. Min(S) \in S /\ \A u \in S : Min(S) <= u BY <2>1, MinProp
  <2>3. \A m \in 0..(Min(S) - 1) : Q(m) BY <2>2
  <2>. QED BY <2>2, <2>3, <1>1
<1>. QED BY <1>2 DEF ChainShape, Epochs

\* A lineage's ancestors are its own prefix.
LEMMA AncPrefix ==
  ASSUME NEW P, NEW Par, NEW An, ChainShape(P, Par, An)
  PROVE  \A e \in P : \A f \in An[e] : An[f] = {g \in An[e] : g <= f}
<1>0. \A e \in P : e \in An[e] /\ An[e] \subseteq P /\ \A f \in An[e] : f <= e BY AncBound
<1>. DEFINE Q(n) == n \in P => \A f \in An[n] : An[f] = {g \in An[n] : g <= f}
<1>1. \A n \in Nat : (\A m \in 0..(n-1) : Q(m)) => Q(n)
  <2>. SUFFICES ASSUME NEW n \in Nat, \A m \in 0..(n-1) : Q(m), n \in P, NEW f \in An[n]
               PROVE  An[f] = {g \in An[n] : g <= f}
    OBVIOUS
  <2>1. Par[n] < n /\ (Par[n] # 0 => Par[n] \in P) /\ Par[n] \in 0..MaxEpoch BY DEF ChainShape
  <2>2. An[n] = {n} \cup (IF Par[n] = 0 THEN {} ELSE An[Par[n]]) BY DEF ChainShape
  <2>3. CASE f = n
    <3>1. \A g \in An[n] : g <= n BY <1>0
    <3>. QED BY <2>3, <3>1
  <2>4. CASE f # n
    <3>1. Par[n] # 0 /\ f \in An[Par[n]] BY <2>2, <2>4
    <3>2. Par[n] \in 0..(n-1) /\ Par[n] \in P BY <2>1, <3>1, Const
    <3>3. An[f] = {g \in An[Par[n]] : g <= f} BY <3>1, <3>2
    <3>4. f <= Par[n] /\ Par[n] < n BY <1>0, <2>1, <3>1, <3>2
    <3>5. f \in Nat /\ n \in Nat BY <1>0, <3>1, <3>2 DEF ChainShape, Epochs
    <3>6. ~(n <= f) BY <3>4, <3>5, <3>2
    <3>7. {g \in An[n] : g <= f} = {g \in An[Par[n]] : g <= f} BY <2>2, <3>1, <3>6
    <3>. QED BY <3>3, <3>7
  <2>. QED BY <2>3, <2>4
<1>2. \A n \in Nat : Q(n)
  <2>. HIDE DEF Q
  <2>. SUFFICES ASSUME NEW n0 \in Nat, ~Q(n0) PROVE FALSE OBVIOUS
  <2>. DEFINE S == {n \in Nat : ~Q(n)}
  <2>1. S \in SUBSET Nat /\ S # {} OBVIOUS
  <2>2. Min(S) \in S /\ \A u \in S : Min(S) <= u BY <2>1, MinProp
  <2>3. \A m \in 0..(Min(S) - 1) : Q(m) BY <2>2
  <2>. QED BY <2>2, <2>3, <1>1
<1>. QED BY <1>2 DEF ChainShape, Epochs

\* Reading block x at e: the newest ancestor at or above lo that allocated x,
\* or, with none, what lo's parent holds (0 when the walk ends).
LEMMA ReadThrough ==
  ASSUME NEW P, NEW Par, NEW An, NEW Al, NEW Con,
         ChainShape(P, Par, An), ChainReads(P, Par, Al, Con),
         NEW x \in Blocks
  PROVE  \A e \in P : \A lo \in An[e] :
           LET H == {f \in An[e] : lo <= f /\ x \in Al[f]}
           IN  /\ H = {} => Con[e][x] = Up(Par, Con, Par[lo], x)
               /\ H # {} => Con[e][x] = Con[Max(H)][x] /\ Max(H) \in H
<1>0. \A e \in P : e \in An[e] /\ An[e] \subseteq P /\ \A f \in An[e] : f <= e BY AncBound
<1>. DEFINE Q(n) == n \in P => \A lo \in An[n] :
                      LET H == {f \in An[n] : lo <= f /\ x \in Al[f]}
                      IN  /\ H = {} => Con[n][x] = Up(Par, Con, Par[lo], x)
                          /\ H # {} => Con[n][x] = Con[Max(H)][x] /\ Max(H) \in H
<1>1. \A n \in Nat : (\A m \in 0..(n-1) : Q(m)) => Q(n)
  <2>. SUFFICES ASSUME NEW n \in Nat, \A m \in 0..(n-1) : Q(m), n \in P, NEW lo \in An[n]
               PROVE  LET H == {f \in An[n] : lo <= f /\ x \in Al[f]}
                      IN  /\ H = {} => Con[n][x] = Up(Par, Con, Par[lo], x)
                          /\ H # {} => Con[n][x] = Con[Max(H)][x] /\ Max(H) \in H
    OBVIOUS
  <2>. DEFINE H == {f \in An[n] : lo <= f /\ x \in Al[f]}
  <2>0. An[n] \subseteq P /\ \A f \in An[n] : f <= n /\ n \in An[n] BY <1>0
  <2>a. P \subseteq 0..MaxEpoch BY Const DEF ChainShape, Epochs
  <2>b. H \subseteq 0..MaxEpoch /\ \A f \in H : f <= n BY <2>0, <2>a
  <2>1. Par[n] < n /\ (Par[n] # 0 => Par[n] \in P) /\ Par[n] \in 0..MaxEpoch BY DEF ChainShape
  <2>2. An[n] = {n} \cup (IF Par[n] = 0 THEN {} ELSE An[Par[n]]) BY DEF ChainShape
  <2>3. CASE x \in Al[n] /\ lo <= n
    <3>1. n \in H BY <2>0, <2>3
    <3>2. Max(H) = n
      <4>1. Max(H) \in H /\ \A u \in H : u <= Max(H) BY <2>b, <3>1, MaxProp, Const
      <4>. QED BY <4>1, <2>b, <3>1, <2>a
    <3>. QED BY <3>1, <3>2
  <2>4. CASE ~(x \in Al[n] /\ lo <= n)
    <3>0. lo <= n BY <2>0
    <3>1. x \notin Al[n] BY <2>4, <3>0
    <3>2. Con[n][x] = Up(Par, Con, Par[n], x) BY <3>1 DEF ChainReads
    <3>3. CASE lo = n
      <4>1. H = {} BY <3>1, <3>3, <2>0, <2>a
      <4>. QED BY <3>2, <3>3, <4>1
    <3>4. CASE lo # n
      <4>1. Par[n] # 0 /\ lo \in An[Par[n]] BY <2>2, <3>4
      <4>2. Par[n] \in 0..(n-1) /\ Par[n] \in P BY <2>1, <4>1, Const
      <4>3. H = {f \in An[Par[n]] : lo <= f /\ x \in Al[f]} BY <2>2, <3>1, <4>1
      <4>4. Con[n][x] = Con[Par[n]][x] BY <3>2, <4>1 DEF Up
      <4>. QED BY <4>1, <4>2, <4>3, <4>4
    <3>. QED BY <3>3, <3>4
  <2>. QED BY <2>3, <2>4
<1>2. \A n \in Nat : Q(n)
  <2>. HIDE DEF Q
  <2>. SUFFICES ASSUME NEW n0 \in Nat, ~Q(n0) PROVE FALSE OBVIOUS
  <2>. DEFINE S == {n \in Nat : ~Q(n)}
  <2>1. S \in SUBSET Nat /\ S # {} OBVIOUS
  <2>2. Min(S) \in S /\ \A u \in S : Min(S) <= u BY <2>1, MinProp
  <2>3. \A m \in 0..(Min(S) - 1) : Q(m) BY <2>2
  <2>. QED BY <2>2, <2>3, <1>1
<1>. QED BY <1>2 DEF ChainShape, Epochs

\* No ancestor allocated x: the walk ends at 0.
LEMMA ReadBottom ==
  ASSUME NEW P, NEW Par, NEW An, NEW Al, NEW Con,
         ChainShape(P, Par, An), ChainReads(P, Par, Al, Con),
         NEW x \in Blocks
  PROVE  \A e \in P : (\A f \in An[e] : x \notin Al[f]) => Con[e][x] = 0
<1>. DEFINE Q(n) == n \in P => ((\A f \in An[n] : x \notin Al[f]) => Con[n][x] = 0)
<1>1. \A n \in Nat : (\A m \in 0..(n-1) : Q(m)) => Q(n)
  <2>. SUFFICES ASSUME NEW n \in Nat, \A m \in 0..(n-1) : Q(m), n \in P,
                      \A f \in An[n] : x \notin Al[f]
               PROVE  Con[n][x] = 0
    OBVIOUS
  <2>1. Par[n] < n /\ (Par[n] # 0 => Par[n] \in P) /\ Par[n] \in 0..MaxEpoch BY DEF ChainShape
  <2>2. An[n] = {n} \cup (IF Par[n] = 0 THEN {} ELSE An[Par[n]]) BY DEF ChainShape
  <2>3. Con[n][x] = Up(Par, Con, Par[n], x) BY <2>2 DEF ChainReads
  <2>4. CASE Par[n] = 0 BY <2>3, <2>4 DEF Up
  <2>5. CASE Par[n] # 0
    <3>1. Par[n] \in 0..(n-1) /\ Par[n] \in P BY <2>1, <2>5, Const
    <3>2. \A f \in An[Par[n]] : x \notin Al[f] BY <2>2, <2>5
    <3>. QED BY <2>3, <2>5, <3>1, <3>2 DEF Up
  <2>. QED BY <2>4, <2>5
<1>2. \A n \in Nat : Q(n)
  <2>. HIDE DEF Q
  <2>. SUFFICES ASSUME NEW n0 \in Nat, ~Q(n0) PROVE FALSE OBVIOUS
  <2>. DEFINE S == {n \in Nat : ~Q(n)}
  <2>1. S \in SUBSET Nat /\ S # {} OBVIOUS
  <2>2. Min(S) \in S /\ \A u \in S : Min(S) <= u BY <2>1, MinProp
  <2>3. \A m \in 0..(Min(S) - 1) : Q(m) BY <2>2
  <2>. QED BY <2>2, <2>3, <1>1
<1>. QED BY <1>2 DEF ChainShape, Epochs

\* The ancestors at or above lo that allocated x; all of them.
RT(An, Al, e, lo, x) == {f \in An[e] : lo <= f /\ x \in Al[f]}
RTall(An, Al, e, x) == {f \in An[e] : x \in Al[f]}

\* A snapshot k cut on top of a chain (its parent h, the head's).
LEMMA ChainExtend ==
  ASSUME NEW P, NEW Par, NEW An, ChainShape(P, Par, An),
         NEW P2, NEW Par2, NEW An2, NEW k \in Epochs, NEW h \in 0..MaxEpoch,
         k \notin P, h < k, h # 0 => h \in P,
         P2 = P \cup {k}, Par2[k] = h, An2[k] = {k} \cup (IF h = 0 THEN {} ELSE An[h]),
         \A e \in P : Par2[e] = Par[e] /\ An2[e] = An[e]
  PROVE  ChainShape(P2, Par2, An2)
<1>1. \A e \in P : Par[e] # 0 => Par[e] \in P /\ Par[e] # k BY DEF ChainShape
<1>. QED BY <1>1 DEF ChainShape

LEMMA ChainReadsExtend ==
  ASSUME NEW P, NEW Par, NEW Al, NEW Con, NEW An, ChainShape(P, Par, An), ChainReads(P, Par, Al, Con),
         NEW P2, NEW Par2, NEW Al2, NEW Con2, NEW k \in Epochs, NEW h \in 0..MaxEpoch,
         k \notin P, h # 0 => h \in P,
         P2 = P \cup {k}, Par2[k] = h,
         \A x \in Blocks : x \notin Al2[k] => Con2[k][x] = Up(Par, Con, h, x),
         \A e \in P : Par2[e] = Par[e] /\ Al2[e] = Al[e] /\ Con2[e] = Con[e]
  PROVE  ChainReads(P2, Par2, Al2, Con2)
<1>1. \A e \in P : Par[e] # 0 => Par[e] \in P /\ Par[e] # k BY DEF ChainShape
<1>2. h # 0 => Con2[h] = Con[h] BY DEF ChainShape
<1>. QED BY <1>1, <1>2 DEF ChainReads, Up

\* ReadThrough at one e and lo, the set named (what callers apply).
LEMMA ReadAt ==
  ASSUME NEW P, NEW Par, NEW An, NEW Al, NEW Con,
         ChainShape(P, Par, An), ChainReads(P, Par, Al, Con),
         NEW x \in Blocks, NEW e \in P, NEW lo \in An[e]
  PROVE  /\ RT(An, Al, e, lo, x) = {} => Con[e][x] = Up(Par, Con, Par[lo], x)
         /\ RT(An, Al, e, lo, x) # {} =>
              /\ Max(RT(An, Al, e, lo, x)) \in RT(An, Al, e, lo, x)
              /\ Con[e][x] = Con[Max(RT(An, Al, e, lo, x))][x]
<1>1. \A ee \in P : \A ll \in An[ee] :
        LET H == {f \in An[ee] : ll <= f /\ x \in Al[f]}
        IN  /\ H = {} => Con[ee][x] = Up(Par, Con, Par[ll], x)
            /\ H # {} => Con[ee][x] = Con[Max(H)][x] /\ Max(H) \in H
  BY ReadThrough
<1>. QED BY <1>1 DEF RT

\* Reading through the whole walk: the newest ancestor that allocated x.
LEMMA ReadMax ==
  ASSUME NEW P, NEW Par, NEW An, NEW Al, NEW Con,
         ChainShape(P, Par, An), ChainReads(P, Par, Al, Con),
         NEW x \in Blocks, NEW e \in P, RTall(An, Al, e, x) # {}
  PROVE  Max(RTall(An, Al, e, x)) \in RTall(An, Al, e, x) /\ Con[e][x] = Con[Max(RTall(An, Al, e, x))][x]
<1>0. e \in An[e] /\ An[e] \subseteq P /\ \A f \in An[e] : f <= e BY AncBound
<1>1. An[e] \subseteq Nat /\ An[e] # {} BY <1>0, Const DEF ChainShape, Epochs
<1>2. Min(An[e]) \in An[e] /\ \A f \in An[e] : Min(An[e]) <= f BY <1>1, MinProp
<1>3. RT(An, Al, e, Min(An[e]), x) = RTall(An, Al, e, x) BY <1>2 DEF RT, RTall
<1>4. RT(An, Al, e, Min(An[e]), x) # {} =>
        /\ Max(RT(An, Al, e, Min(An[e]), x)) \in RT(An, Al, e, Min(An[e]), x)
        /\ Con[e][x] = Con[Max(RT(An, Al, e, Min(An[e]), x))][x]
  BY <1>2, ReadAt
<1>. QED BY <1>3, <1>4

\* Under the invariant, leg s's lineage is a snapshot chain, and the copy's
\* elements from lo through t are t's own ancestors from lo up.
LEMMA LegChain ==
  ASSUME I_Lineage, I_Chain, TypeOK, NEW s \in R
  PROVE  /\ ChainShape(sPres[s], sPar[s], anc[s])
         /\ ChainReads(sPres[s], sPar[s], sAl[s], sCon[s])
BY DEF I_Lineage, I_Chain

LEMMA LinPrefix ==
  ASSUME I_Lineage, I_Ranges, TypeOK, NEW s \in R, NEW t \in Lin(s)
  PROVE  /\ t \in sPres[s] /\ hPar[s] \in sPres[s]
         /\ anc[s][t] = {g \in Lin(s) : g <= t}
<1>1. hPar[s] # 0 /\ Lin(s) = anc[s][hPar[s]] BY DEF Lin, AncOf
<1>2. hPar[s] \in sPres[s] BY <1>1 DEF I_Ranges
<1>3. ChainShape(sPres[s], sPar[s], anc[s]) BY DEF I_Lineage
<1>4. anc[s][hPar[s]] \subseteq sPres[s] BY <1>2, <1>3, AncBound
<1>5. anc[s][t] = {g \in anc[s][hPar[s]] : g <= t} BY <1>1, <1>2, <1>3, AncPrefix
<1>. QED BY <1>1, <1>2, <1>4, <1>5

LEMMA LinSub ==
  ASSUME I_Lineage, I_Ranges, TypeOK, NEW s \in R PROVE Lin(s) \subseteq sPres[s]
<1>1. CASE hPar[s] = 0 BY <1>1 DEF Lin, AncOf
<1>2. CASE hPar[s] # 0
  <2>1. hPar[s] \in sPres[s] BY <1>2 DEF I_Ranges
  <2>2. ChainShape(sPres[s], sPar[s], anc[s]) BY DEF I_Lineage
  <2>. QED BY <1>2, <2>1, <2>2, AncBound DEF Lin, AncOf
<1>. QED BY <1>1, <1>2

\* A new snapshot k on leg q, its parent the head's parent: the chain stays
\* a chain (specialized to a leg, so a step instantiates only q and k).
LEMMA LegExtend ==
  ASSUME I_Lineage, I_Chain, I_Ranges, TypeOK, NEW q \in R, NEW k \in Epochs,
         k \notin sPres[q], hPar[q] < k,
         sPres'[q] = sPres[q] \cup {k}, sPar'[q][k] = hPar[q],
         anc'[q][k] = {k} \cup (IF hPar[q] = 0 THEN {} ELSE anc[q][hPar[q]]),
         \A e \in sPres[q] : /\ sPar'[q][e] = sPar[q][e] /\ anc'[q][e] = anc[q][e]
                            /\ sAl'[q][e] = sAl[q][e] /\ sCon'[q][e] = sCon[q][e],
         \A x \in Blocks : x \notin sAl'[q][k] => sCon'[q][k][x] = Up(sPar[q], sCon[q], hPar[q], x)
  PROVE  ChainShape(sPres'[q], sPar'[q], anc'[q]) /\ ChainReads(sPres'[q], sPar'[q], sAl'[q], sCon'[q])
<1>1. ChainShape(sPres[q], sPar[q], anc[q]) /\ ChainReads(sPres[q], sPar[q], sAl[q], sCon[q])
  BY DEF I_Lineage, I_Chain
<1>2. hPar[q] \in sPres[q] \cup {0} /\ hPar[q] \in 0..MaxEpoch BY DEF I_Ranges, TypeOK, TyHead
<1>3. \A e \in sPres[q] : /\ sPar[q][e] \in 0..MaxEpoch /\ sPar[q][e] < e
                         /\ (sPar[q][e] # 0 => sPar[q][e] \in sPres[q] /\ sPar[q][e] # k)
                         /\ anc[q][e] = {e} \cup (IF sPar[q][e] = 0 THEN {} ELSE anc[q][sPar[q][e]])
  BY <1>1 DEF ChainShape
<1>4. sPres[q] \subseteq Epochs BY DEF TypeOK, TySnap
<1>5. ChainShape(sPres'[q], sPar'[q], anc'[q])
  <2>1. sPres'[q] \subseteq Epochs BY <1>4
  <2>2. ASSUME NEW e \in sPres'[q]
        PROVE  /\ sPar'[q][e] \in 0..MaxEpoch /\ sPar'[q][e] < e
               /\ (sPar'[q][e] # 0 => sPar'[q][e] \in sPres'[q])
               /\ anc'[q][e] = {e} \cup (IF sPar'[q][e] = 0 THEN {} ELSE anc'[q][sPar'[q][e]])
    <3>1. CASE e = k
      <4>1. hPar[q] # 0 => anc'[q][hPar[q]] = anc[q][hPar[q]] BY <1>2
      <4>. QED BY <3>1, <1>2, <4>1
    <3>2. CASE e # k
      <4>1. e \in sPres[q] BY <3>2
      <4>2. sPar[q][e] # 0 => anc'[q][sPar[q][e]] = anc[q][sPar[q][e]] BY <4>1, <1>3
      <4>. QED BY <4>1, <4>2, <1>3
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2 DEF ChainShape
<1>6. ChainReads(sPres'[q], sPar'[q], sAl'[q], sCon'[q])
  <2>. SUFFICES ASSUME NEW e \in sPres'[q], NEW x \in Blocks, x \notin sAl'[q][e]
               PROVE  sCon'[q][e][x] = Up(sPar'[q], sCon'[q], sPar'[q][e], x)
    BY DEF ChainReads
  <2>1. CASE e = k
    <3>1. hPar[q] # 0 => sCon'[q][hPar[q]] = sCon[q][hPar[q]] BY <1>2
    <3>. QED BY <2>1, <3>1 DEF Up
  <2>2. CASE e # k
    <3>1. e \in sPres[q] /\ x \notin sAl[q][e] BY <2>2
    <3>2. sCon[q][e][x] = Up(sPar[q], sCon[q], sPar[q][e], x) BY <3>1, <1>1 DEF ChainReads
    <3>3. sPar[q][e] # 0 => sCon'[q][sPar[q][e]] = sCon[q][sPar[q][e]] BY <3>1, <1>3
    <3>. QED BY <3>1, <3>2, <3>3 DEF Up
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>5, <1>6

LEMMA CopyRead ==
  ASSUME I_Lineage, I_Chain, I_Ranges, TypeOK, NEW s \in R, NEW t \in Lin(s),
         NEW lo \in Nat, lo = 0 \/ (lo \in Lin(s) /\ lo <= t), NEW x \in Blocks
  PROVE  LET H == {e \in CopyElems(s, lo, t) : x \in sAl[s][e]}
         IN  /\ H # {} => Max(H) \in H /\ sCon[s][Max(H)][x] = sCon[s][t][x]
             /\ H = {} /\ lo # 0 => sCon[s][t][x] = Up(sPar[s], sCon[s], sPar[s][lo], x)
             /\ H = {} /\ lo = 0 => sCon[s][t][x] = 0
<1>. DEFINE H == {e \in CopyElems(s, lo, t) : x \in sAl[s][e]}
<1>0. ChainShape(sPres[s], sPar[s], anc[s]) /\ ChainReads(sPres[s], sPar[s], sAl[s], sCon[s])
  BY LegChain
<1>1. t \in sPres[s] /\ anc[s][t] = {g \in Lin(s) : g <= t} BY LinPrefix
<1>2. Lin(s) \subseteq Nat BY LinType DEF TypeOK, TyHead, TySnap, Epochs
<1>3. H = {f \in anc[s][t] : lo <= f /\ x \in sAl[s][f]} BY <1>1, <1>2 DEF CopyElems
<1>4. CASE lo # 0
  <2>1. lo \in anc[s][t] BY <1>1, <1>4
  <2>2. /\ RT(anc[s], sAl[s], t, lo, x) = {} => sCon[s][t][x] = Up(sPar[s], sCon[s], sPar[s][lo], x)
        /\ RT(anc[s], sAl[s], t, lo, x) # {} =>
              /\ Max(RT(anc[s], sAl[s], t, lo, x)) \in RT(anc[s], sAl[s], t, lo, x)
              /\ sCon[s][t][x] = sCon[s][Max(RT(anc[s], sAl[s], t, lo, x))][x]
    <3>1. ChainShape(sPres[s], sPar[s], anc[s]) /\ ChainReads(sPres[s], sPar[s], sAl[s], sCon[s]) BY <1>0
    <3>2. t \in sPres[s] /\ lo \in anc[s][t] BY <1>1, <2>1
    <3>. QED BY <3>1, <3>2, ReadAt
  <2>3. H = RT(anc[s], sAl[s], t, lo, x) BY <1>3 DEF RT
  <2>. QED BY <1>4, <2>2, <2>3
<1>5. CASE lo = 0
  <2>1. \A f \in anc[s][t] : 0 <= f BY <1>1, <1>2
  <2>2. H = {f \in anc[s][t] : x \in sAl[s][f]} BY <1>3, <1>5, <2>1
  <2>a. H = RTall(anc[s], sAl[s], t, x) BY <2>2 DEF RTall
  <2>b. ChainShape(sPres[s], sPar[s], anc[s]) /\ ChainReads(sPres[s], sPar[s], sAl[s], sCon[s]) /\ t \in sPres[s]
    BY <1>0, <1>1
  <2>3. H # {} => Max(H) \in H /\ sCon[s][t][x] = sCon[s][Max(H)][x]
    <3>1. RTall(anc[s], sAl[s], t, x) # {} =>
            Max(RTall(anc[s], sAl[s], t, x)) \in RTall(anc[s], sAl[s], t, x)
            /\ sCon[s][t][x] = sCon[s][Max(RTall(anc[s], sAl[s], t, x))][x]
      BY <2>b, ReadMax
    <3>. QED BY <3>1, <2>a
  <2>4. H = {} => sCon[s][t][x] = 0
    <3>1. \A e \in sPres[s] : (\A f \in anc[s][e] : x \notin sAl[s][f]) => sCon[s][e][x] = 0
      BY <2>b, ReadBottom
    <3>. QED BY <3>1, <2>b, <2>2
  <2>. QED BY <1>5, <2>3, <2>4
<1>. QED BY <1>4, <1>5

-----------------------------------------------------------------------------
(* Frames: a part of the invariant carries over when what it reads does.   *)

LEMMA F_Ranges ==
  I_Ranges /\ UNCHANGED <<cutK, nextSeq, attached, pend, members, zx, recorded, conf, sPres, tr, hPar>>
  => I_Ranges'
BY DEF I_Ranges
LEMMA F_Lineage == I_Lineage /\ UNCHANGED <<sPres, sPar, anc>> => I_Lineage'
BY DEF I_Lineage
LEMMA F_Chain == I_Chain /\ UNCHANGED <<sPres, sPar, sAl, sCon, hAl, head, hPar>> => I_Chain'
BY DEF I_Chain
LEMMA F_Pend == I_Pend /\ UNCHANGED <<pend, acked, ver, members, head, nextSeq, cutK>> => I_Pend'
BY DEF I_Pend
LEMMA F_Members == I_Members /\ UNCHANGED <<attached, members, dState, path>> => I_Members'
BY DEF I_Members, InSyncR, Healthy
LEMMA F_Hold == I_Hold /\ UNCHANGED <<attached, members, head, acked, dState>> => I_Hold'
BY DEF I_Hold, InSyncR
LEMMA F_Legal == I_Legal /\ UNCHANGED <<head, sPres, sCon, acked, ackedEver, pend>> => I_Legal'
BY DEF I_Legal, PendVal
LEMMA F_PendSeen == I_PendSeen /\ UNCHANGED <<pend, head, sPres, sCon>> => I_PendSeen'
BY DEF I_PendSeen
LEMMA F_AckedEver == I_AckedEver /\ UNCHANGED <<acked, ackedEver>> => I_AckedEver'
BY DEF I_AckedEver
LEMMA F_Vers == I_Vers /\ UNCHANGED <<acked, ver, head, sPres, sCon>> => I_Vers'
BY DEF I_Vers
LEMMA F_WHi == I_WHi /\ UNCHANGED <<sPres, sCon, cutAck>> => I_WHi'
BY DEF I_WHi
LEMMA F_HeadHi == I_HeadHi /\ UNCHANGED <<dState, reverted, mark, head, sPres, cutAck>> => I_HeadHi'
BY DEF I_HeadHi, CatchingUp, HiOf
LEMMA F_HeadLo == I_HeadLo /\ UNCHANGED <<dState, reverted, mark, head, sPres, sCon>> => I_HeadLo'
BY DEF I_HeadLo, CatchingUp, HiOf
LEMMA F_Reverted ==
  I_Reverted /\ UNCHANGED <<dState, reverted, sb, hAl, hPar, head, sCon, sPres, recorded>> => I_Reverted'
BY DEF I_Reverted, RevOK
LEMMA F_HeadPend == I_HeadPend /\ UNCHANGED <<dState, reverted, mark, head, pend>> => I_HeadPend'
BY DEF I_HeadPend, CatchingUp, HiOf
LEMMA F_Session ==
  I_Session /\ UNCHANGED <<dState, reverted, sb, mark, members, hPar, nextSeq, sPres, tr, pin,
                           recorded, path, cutK, cutTgt, cutDone, cutGone>>
  => I_Session'
BY DEF I_Session, SessionOK, CatchingUp
LEMMA F_TC == Inv_TrustComplete /\ UNCHANGED <<tr, sCon, cutAck>> => Inv_TrustComplete'
BY DEF Inv_TrustComplete
LEMMA F_Fresh ==
  I_Fresh /\ UNCHANGED <<attached, members, dState, sPres, nextSeq, conf, sCon, cutAck>> => I_Fresh'
BY DEF I_Fresh, Bases, InSyncR

\* A leg the step leaves alone keeps its per-leg facts.
LEMMA SessionOther ==
  ASSUME NEW q \in R, SessionOK(q),
         UNCHANGED <<members, nextSeq, recorded, cutK, cutTgt, cutDone, cutGone>>,
         dState'[q] = dState[q], reverted'[q] = reverted[q], sb'[q] = sb[q], mark'[q] = mark[q],
         hPar'[q] = hPar[q], sPres'[q] = sPres[q], tr'[q] = tr[q], pin'[q] = pin[q], path'[q] = path[q]
  PROVE  SessionOK(q)'
BY DEF SessionOK, CatchingUp
LEMMA SessionOtherG ==
  ASSUME NEW q \in R, SessionOK(q),
         UNCHANGED <<members, nextSeq, recorded, cutK, cutTgt, cutDone>>, cutGone \subseteq cutGone',
         dState'[q] = dState[q], reverted'[q] = reverted[q], sb'[q] = sb[q], mark'[q] = mark[q],
         hPar'[q] = hPar[q], sPres'[q] = sPres[q], tr'[q] = tr[q], pin'[q] = pin[q], path'[q] = path[q]
  PROVE  SessionOK(q)'
BY DEF SessionOK, CatchingUp
LEMMA RevOther ==
  ASSUME NEW q \in R, RevOK(q), UNCHANGED recorded,
         dState'[q] = dState[q], reverted'[q] = reverted[q], sb'[q] = sb[q], hAl'[q] = hAl[q],
         hPar'[q] = hPar[q], head'[q] = head[q], sCon'[q] = sCon[q], sPres'[q] = sPres[q]
  PROVE  RevOK(q)'
BY DEF RevOK

\* IndInv, part by part.
LEMMA IndSplit ==
  IndInv <=> /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
             /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
             /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
BY DEF IndInv
LEMMA IndSplitP ==
  IndInv' <=> /\ TypeOK' /\ I_Ranges' /\ I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Members' /\ I_Hold'
              /\ I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted'
              /\ I_HeadPend' /\ I_Session' /\ Inv_TrustComplete' /\ I_Fresh' /\ ~badLoss'
BY DEF IndInv

-----------------------------------------------------------------------------
(* The inductive invariant: Init, and what it gives the theorem.            *)

LEMMA I_Init == Init => IndInv
<1>. SUFFICES ASSUME Init PROVE IndInv OBVIOUS
<1>0. TypeOK BY T_Init
<1>a. R # {} /\ MaxEpoch \in Nat BY Const DEF R
<1>1. I_Ranges BY Const DEF Init, I_Ranges, Epochs
<1>2. I_Lineage BY DEF Init, I_Lineage, ChainShape, Epochs
<1>3. I_Chain BY DEF Init, I_Chain, ChainReads, Up, Zero
<1>4. I_Pend BY DEF Init, I_Pend
<1>5. I_Members
  <2>1. members = R /\ InSyncR = R /\ Healthy = R BY DEF Init, InSyncR, Healthy
  <2>. QED BY <1>a, <2>1 DEF I_Members
<1>6. I_Hold BY DEF Init, I_Hold, Zero
<1>7. I_Legal /\ I_AckedEver BY DEF Init, I_Legal, I_AckedEver, Zero, PendVal
<1>8. I_WHi /\ I_Fresh /\ Inv_TrustComplete BY DEF Init, I_WHi, I_Fresh, Inv_TrustComplete, Bases, InSyncR
<1>9. I_HeadHi /\ I_HeadLo /\ I_Reverted /\ I_Session /\ I_HeadPend
  BY DEF Init, I_HeadHi, I_HeadLo, I_Reverted, RevOK, I_Session, SessionOK, I_HeadPend, CatchingUp
<1>10. I_PendSeen /\ I_Vers BY Const DEF Init, I_PendSeen, I_Vers, Zero
<1>11. ~badLoss BY DEF Init
<1>. QED BY <1>0, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, IndSplit

\* What the invariant gives the theorem.
LEMMA I_Goal == IndInv => Goal
<1>. SUFFICES ASSUME IndInv PROVE Goal OBVIOUS
<1>1. Inv_AckedHeld
  <2>. SUFFICES ASSUME NEW x \in Blocks PROVE \E r \in R : dState[r] = "insync" /\ head[r][x] >= acked[x]
    BY DEF Inv_AckedHeld
  <2>1. CASE attached
    <3>1. PICK r \in Healthy : TRUE BY <2>1 DEF IndInv, I_Members
    <3>2. r \in members /\ r \in InSyncR BY <2>1, <3>1 DEF IndInv, I_Members, Healthy
    <3>. QED BY <2>1, <3>2 DEF IndInv, I_Hold, InSyncR
  <2>2. CASE ~attached
    <3>1. PICK r \in InSyncR : TRUE BY <2>2 DEF IndInv, I_Hold
    <3>. QED BY <2>2, <3>1 DEF IndInv, I_Hold, InSyncR
  <2>. QED BY <2>1, <2>2
<1>2. Inv_MembersHold
  <2>1. CASE attached BY <2>1 DEF IndInv, I_Hold, Inv_MembersHold
  <2>2. CASE ~attached BY <2>2 DEF IndInv, I_Ranges, Inv_MembersHold
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2 DEF IndInv, Goal, Inv_Catchup, Inv_AdmitNoLoss

LEMMA CoverSetFacts ==
  ASSUME NEW r \in R, NEW t, NEW b, NEW s \in CoverSet(r, t, b)
  PROVE  s \in Sources(r) /\ t \in Lin(s) /\ (b <= 0 \/ b \in Lin(s)) /\ t \in tr[s]
<1>1. Sources(r) \subseteq R BY DEF Sources
<1>2. Sources(r) # {} => First(Sources(r)) \in Sources(r) BY <1>1, RNat, MinProp DEF First
<1>. QED BY <1>2, Clean DEF CoverSet, Covers, LinCovers

\* The trust target: a recorded epoch, below nextSeq, that some in-sync leg trusts.
LEMMA TgtFacts ==
  ASSUME I_Ranges, TypeOK, Tgt # 0
  PROVE  Tgt \in recorded /\ Tgt \in Epochs /\ Tgt < nextSeq /\ Tgt >= 1
<1>1. Tgt \in recorded /\ Tgt \in Epochs BY TgtType DEF TypeOK, TySets
<1>2. nextSeq \in Nat /\ recorded \subseteq 1..(nextSeq - 1) BY DEF TypeOK, TyScalar, I_Ranges
<1>. QED BY <1>1, <1>2 DEF Epochs

\* A value on a leg that is a write in flight is that write's (one per block).
LEMMA PendOne ==
  ASSUME I_Pend, NEW w \in pend, NEW u \in pend, u.b = w.b PROVE u = w
BY DEF I_Pend

-----------------------------------------------------------------------------
(* The steps that touch only the path, the scheduler's bookkeeping, or the  *)
(* session's selection.                                                    *)

LEMMA I_Return ==
  ASSUME IndInv, NEW r \in R, Return(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF Return, trustVars
<1>p. path[r] = "out" /\ path' = [path EXCEPT ![r] = "back"] BY DEF Return
<1>1. TypeOK' BY T_Path DEF IndInv
<1>2. I_Ranges' BY <1>0, <1>T, <1>u, F_Ranges
<1>3. I_Lineage' /\ I_Chain' BY <1>0, <1>T, <1>u, F_Lineage, F_Chain
<1>4. I_Pend' /\ I_Hold' BY <1>0, <1>T, <1>u, F_Pend, F_Hold
<1>5. I_Members'
  <2>0. members \subseteq R /\ \A q \in R : (path'[q] = "ok") <=> (path[q] = "ok")
    BY <1>p, <1>0, <1>T, Groups DEF TyLeg, TySets
  <2>1. Healthy' = Healthy BY <2>0, <1>u DEF Healthy
  <2>. QED BY <1>0, <1>T, <1>u, <2>1 DEF I_Members, InSyncR
<1>6. I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' BY <1>0, <1>T, <1>u, F_Legal, F_PendSeen, F_AckedEver, F_Vers
<1>7. I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted' /\ I_HeadPend'
  BY <1>0, <1>T, <1>u, F_WHi, F_HeadHi, F_HeadLo, F_Reverted, F_HeadPend
<1>8. I_Session'
  <2>1. ~CatchingUp(r) BY <1>0, <1>T, <1>p DEF I_Session, SessionOK
  <2>2. CatchingUp(r)' = CatchingUp(r) BY <1>u DEF CatchingUp
  <2>3. \A q \in R \ {r} : path'[q] = path[q] BY <1>p, <1>0, <1>T, Groups DEF TyLeg
  <2>. QED BY <1>0, <1>T, <1>u, <2>1, <2>2, <2>3 DEF I_Session, SessionOK, CatchingUp
<1>9. Inv_TrustComplete' /\ I_Fresh' BY <1>0, <1>T, <1>u, F_TC, F_Fresh
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_FailBegin ==
  ASSUME IndInv, NEW r \in R, FailBegin(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF FailBegin, trustVars
<1>p. /\ path[r] = "ok" /\ r \in members /\ dState[r] = "insync" /\ Healthy \ {r} # {}
      /\ path' = [path EXCEPT ![r] = "failing"]
  BY DEF FailBegin
<1>1. TypeOK' BY T_Path DEF IndInv
<1>2. I_Ranges' /\ I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Hold'
  BY <1>0, <1>T, <1>u, F_Ranges, F_Lineage, F_Chain, F_Pend, F_Hold
<1>3. I_Members'
  <2>0. members \subseteq R /\ (path'[r] # "ok") /\ \A q \in R \ {r} : path'[q] = path[q]
    BY <1>p, <1>0, <1>T, Groups DEF TyLeg, TySets
  <2>1. Healthy' = Healthy \ {r} BY <2>0, <1>u, <1>p DEF Healthy
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1 DEF I_Members, InSyncR
<1>4. I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted'
      /\ I_HeadPend' /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Legal, F_PendSeen, F_AckedEver, F_Vers, F_WHi, F_HeadHi, F_HeadLo, F_Reverted,
     F_HeadPend, F_TC, F_Fresh
<1>5. I_Session'
  <2>1. ~CatchingUp(r) /\ CatchingUp(r)' = CatchingUp(r) BY <1>u, <1>p DEF CatchingUp
  <2>2. \A q \in R \ {r} : path'[q] = path[q] BY <1>p, <1>0, <1>T, Groups DEF TyLeg
  <2>. QED BY <1>0, <1>T, <1>u, <2>1, <2>2 DEF I_Session, SessionOK, CatchingUp
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_FaultOut ==
  ASSUME IndInv, NEW r \in R, FaultOut(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, attached, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF FaultOut, trustVars
<1>p. /\ path[r] = "failing" /\ path' = [path EXCEPT ![r] = "out"] /\ members' = members \ {r}
  BY DEF FaultOut
<1>1. TypeOK' BY T_Path DEF IndInv
<1>2. I_Lineage' /\ I_Chain' /\ I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' /\ I_WHi'
      /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted' /\ I_HeadPend' /\ Inv_TrustComplete'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Legal, F_PendSeen, F_AckedEver, F_Vers, F_WHi, F_HeadHi,
     F_HeadLo, F_Reverted, F_HeadPend, F_TC
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p DEF I_Ranges
<1>4. I_Pend' BY <1>0, <1>T, <1>u, <1>p DEF I_Pend
<1>5. I_Members'
  <2>0. members \subseteq R /\ path[r] # "ok" /\ path'[r] # "ok" /\ \A q \in R \ {r} : path'[q] = path[q]
    BY <1>p, <1>0, <1>T, Groups DEF TyLeg, TySets
  <2>1. Healthy' = Healthy BY <2>0, <1>p DEF Healthy
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1 DEF I_Members, InSyncR
<1>6. I_Hold' BY <1>0, <1>T, <1>u, <1>p DEF I_Hold, InSyncR
<1>7. I_Session'
  <2>1. ~CatchingUp(r) /\ CatchingUp(r)' = CatchingUp(r) BY <1>u, <1>p, <1>0, <1>T DEF CatchingUp, I_Session, SessionOK
  <2>2. \A q \in R \ {r} : path'[q] = path[q] BY <1>p, <1>0, <1>T, Groups DEF TyLeg
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1, <2>2 DEF I_Session, SessionOK, CatchingUp
<1>8. I_Fresh' BY <1>0, <1>T, <1>u, <1>p DEF I_Fresh, Bases, InSyncR
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_MarkStale ==
  ASSUME IndInv, NEW r \in R, MarkStale(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF MarkStale, trustVars
<1>p. /\ r \notin members /\ dState[r] = "insync" /\ attached
      /\ dState' = [dState EXCEPT ![r] = "stale"]
  <2>1. r \notin members /\ dState[r] = "insync" /\ attached BY Clean DEF MarkStale
  <2>2. dState' = [q \in R |-> IF q \in {r} THEN "stale" ELSE dState[q]] BY DEF MarkStale, StaleMarks
  <2>. QED BY <2>1, <2>2, <1>0, <1>T, Groups DEF TyLeg
<1>1. TypeOK' BY T_Path DEF IndInv
<1>2. I_Ranges' /\ I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers'
      /\ I_WHi' /\ Inv_TrustComplete'
  BY <1>0, <1>T, <1>u, F_Ranges, F_Lineage, F_Chain, F_Pend, F_Legal, F_PendSeen, F_AckedEver, F_Vers, F_WHi, F_TC
<1>a. \A q \in R \ {r} : dState'[q] = dState[q] BY <1>p, <1>0, <1>T, Groups DEF TyLeg
<1>r0. SessionOK(r) BY <1>0, <1>T DEF I_Session
<1>r1. reverted[r] = -1 /\ sb[r] = -1 BY <1>r0, <1>p DEF SessionOK
<1>r2. dState'[r] = "stale" BY <1>p, <1>0, <1>T, Groups DEF TyLeg
<1>b. ~CatchingUp(r) /\ ~CatchingUp(r)' BY <1>u, <1>p, <1>r1, <1>r2 DEF CatchingUp
<1>c. \A q \in R : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q)
  BY <1>a, <1>b, <1>u DEF CatchingUp, HiOf
<1>3. I_Members' BY <1>0, <1>T, <1>u, <1>p, <1>a DEF I_Members, InSyncR, Healthy
<1>4. I_Hold' BY <1>0, <1>T, <1>u, <1>p DEF I_Hold, InSyncR
<1>5. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend' BY <1>0, <1>T, <1>u, <1>c DEF I_HeadHi, I_HeadLo, I_HeadPend
<1>r. reverted[r] = -1 /\ sb[r] = -1 BY <1>0, <1>T, <1>p DEF I_Session, SessionOK
<1>6. I_Reverted'
  <2>1. \A q \in R \ {r} : RevOK(q)' BY <1>0, <1>T, <1>u, <1>a, RevOther DEF I_Reverted
  <2>2. RevOK(r)' BY <1>r, <1>u DEF RevOK
  <2>. QED BY <2>1, <2>2 DEF I_Reverted
<1>7. I_Session'
  <2>1. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>u, <1>a, SessionOther DEF I_Session
  <2>2. SessionOK(r)'
    <3>1. dState'[r] = "stale" BY <1>p, <1>0, <1>T, Groups DEF TyLeg
    <3>. QED BY <3>1, <1>r, <1>u, <1>b DEF SessionOK, CatchingUp
  <2>. QED BY <2>1, <2>2 DEF I_Session
<1>8. I_Fresh' BY <1>0, <1>T, <1>u, <1>p DEF I_Fresh, Bases, InSyncR
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_Observe ==
  ASSUME IndInv, NEW e \in Epochs, Observe(e) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, htr, cutAck>>
  BY DEF Observe
<1>p. /\ attached /\ ~conf[e] /\ e \in recorded
      /\ conf' = [conf EXCEPT ![e] = TRUE]
      /\ tr' = [q \in R |-> IF q \in cfgAt[e] \cap members /\ e \in sPres[q] THEN tr[q] \cup {e} ELSE tr[q]]
  BY Clean DEF Observe
<1>e. e < nextSeq BY <1>0, <1>T, <1>p DEF I_Ranges
<1>1. TypeOK' BY T_Observe DEF IndInv
<1>2. I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Members' /\ I_Hold' /\ I_Legal' /\ I_PendSeen'
      /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted' /\ I_HeadPend'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Pend, F_Members, F_Hold, F_Legal, F_PendSeen, F_AckedEver, F_Vers,
     F_WHi, F_HeadHi, F_HeadLo, F_Reverted, F_HeadPend
<1>3. I_Ranges'
  <2>1. \A q \in R : tr'[q] \subseteq sPres[q] \cap 1..(nextSeq - 1)
    BY <1>0, <1>T, <1>p, <1>e DEF I_Ranges, Epochs
  <2>2. \A f \in Epochs : ~conf'[f] => ~conf[f] BY <1>p, <1>0, <1>T, Groups DEF TyTrust
  <2>. QED BY <1>0, <1>T, <1>u, <2>1, <2>2 DEF I_Ranges
<1>4. Inv_TrustComplete'
  <2>. SUFFICES ASSUME NEW q \in R, NEW f \in tr'[q], NEW x \in Blocks
               PROVE  sCon[q][f][x] >= cutAck[q][f][x]
    BY <1>u DEF Inv_TrustComplete
  <2>1. CASE f \in tr[q] BY <2>1, <1>0, <1>T DEF Inv_TrustComplete
  <2>2. CASE f \notin tr[q]
    <3>1. f = e /\ q \in members /\ e \in sPres[q] BY <2>2, <1>p
    <3>. QED BY <3>1, <1>0, <1>T, <1>p DEF I_Fresh, Bases
  <2>. QED BY <2>1, <2>2
<1>5. I_Session'
  <2>1. \A q \in R : tr[q] \subseteq tr'[q] BY <1>p
  <2>. QED BY <1>0, <1>T, <1>u, <2>1 DEF I_Session, SessionOK, CatchingUp
<1>6. I_Fresh'
  <2>1. \A f \in Epochs : ~conf'[f] => ~conf[f] BY <1>p, <1>0, <1>T, Groups DEF TyTrust
  <2>2. \A q \in R : sPres[q] \subseteq Epochs BY <1>0, <1>T, Groups DEF TySnap
  <2>3. Bases' = Bases BY <1>u DEF Bases, InSyncR
  <2>. SUFFICES ASSUME NEW q \in Bases', NEW e \in sPres'[q], e >= nextSeq' \/ ~conf'[e], NEW x \in Blocks
               PROVE  sCon'[q][e][x] >= cutAck'[q][e][x]
    BY DEF I_Fresh
  <2>5. sPres' = sPres /\ nextSeq' = nextSeq BY <1>u
  <2>6. Bases \subseteq R BY <1>0, <1>T, Groups DEF Bases, InSyncR, TySets
  <2>4. q \in Bases /\ e \in sPres[q] /\ (e >= nextSeq \/ ~conf[e]) BY <2>1, <2>2, <2>3, <2>5, <2>6
  <2>. QED BY <2>4, <1>u, <1>0, <1>T DEF I_Fresh
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_CutBegin ==
  ASSUME IndInv, CutBegin PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<members, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF CutBegin, trustVars
<1>p. /\ attached /\ cutK = 0 /\ \A w \in pend : w.cutAt >= nextSeq
      /\ cutK' = nextSeq /\ cutTgt' = InSyncR /\ cutDone' = {} /\ cutGone' = {}
  BY DEF CutBegin
<1>1. TypeOK' BY <1>0, <1>T, T_CutSmall DEF I_Ranges
<1>2. I_Lineage' /\ I_Chain' /\ I_Members' /\ I_Hold' /\ I_Legal' /\ I_PendSeen'
      /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted' /\ I_HeadPend'
      /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Members, F_Hold, F_Legal, F_PendSeen, F_AckedEver, F_Vers,
     F_WHi, F_HeadHi, F_HeadLo, F_Reverted, F_HeadPend, F_TC, F_Fresh
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p DEF I_Ranges
<1>4. I_Pend' BY <1>0, <1>T, <1>u, <1>p DEF I_Pend
<1>5. I_Session'
  <2>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
  <2>1. CatchingUp(q) => q \notin cutTgt' BY <1>p DEF CatchingUp, InSyncR
  <2>2. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>. QED BY <2>1, <2>2, <1>u, <1>p DEF SessionOK, CatchingUp
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_CutAbort ==
  ASSUME IndInv, CutAbort \/ CutAbortRead PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<members, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
      /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
  BY DEF CutAbort, CutAbortRead, trustVars
<1>1. TypeOK' BY <1>0, <1>T, T_CutSmall DEF I_Ranges
<1>2. I_Lineage' /\ I_Chain' /\ I_Members' /\ I_Hold' /\ I_Legal' /\ I_PendSeen'
      /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted' /\ I_HeadPend'
      /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Members, F_Hold, F_Legal, F_PendSeen, F_AckedEver, F_Vers,
     F_WHi, F_HeadHi, F_HeadLo, F_Reverted, F_HeadPend, F_TC, F_Fresh
<1>3. I_Ranges' /\ I_Pend' /\ I_Session' BY <1>0, <1>T, <1>u DEF I_Ranges, I_Pend, I_Session, SessionOK, CatchingUp
<1>. QED BY <1>1, <1>2, <1>3, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_Retire ==
  ASSUME IndInv, Retire PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF Retire, trustVars
<1>p. /\ Cardinality(recorded) > Retain
      /\ \A q \in R : pin[q] # 0 => Min(recorded) < pin[q]
      /\ recorded' = recorded \ {Min(recorded)}
  BY Clean DEF Retire
<1>1. TypeOK' BY <1>0, <1>T, T_CutSmall DEF I_Ranges
<1>2. I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Members' /\ I_Hold' /\ I_Legal' /\ I_PendSeen'
      /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
      /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Pend, F_Members, F_Hold, F_Legal, F_PendSeen, F_AckedEver, F_Vers,
     F_WHi, F_HeadHi, F_HeadLo, F_HeadPend, F_TC, F_Fresh
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p DEF I_Ranges
<1>4. I_Reverted'
  <2>. SUFFICES ASSUME NEW q \in R PROVE RevOK(q)' BY DEF I_Reverted
  <2>1. RevOK(q) BY <1>0, <1>T DEF I_Reverted
  <2>2. recorded' \subseteq recorded BY <1>p
  <2>. QED BY <2>1, <2>2, <1>u DEF RevOK
<1>5. I_Session'
  <2>1. \A q \in R : pin[q] # 0 /\ pin[q] \in recorded => pin[q] \in recorded'
    <3>. SUFFICES ASSUME NEW q \in R, pin[q] # 0, pin[q] \in recorded PROVE pin[q] # Min(recorded)
      BY <1>p
    <3>. QED BY <1>p, <1>0, <1>T, Groups DEF TyLeg, TySets, Epochs
  <2>2. \A q \in R : dState[q] = "standby" => mark[q] \in recorded'
    BY <2>1, <1>0, <1>T DEF I_Session, SessionOK
  <2>3. \A q \in R : dState[q] = "stale" /\ sb[q] > 0 => sb[q] \in recorded'
    BY <2>1, <1>0, <1>T DEF I_Session, SessionOK
  <2>4. recorded' \subseteq recorded BY <1>p
  <2>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
  <2>5. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>6. dState[q] = "standby" => mark[q] \in recorded' BY <2>2
  <2>7. dState[q] = "stale" /\ sb[q] > 0 => sb[q] \in recorded' BY <2>3
  <2>8. recorded' # {}
    <3>1. Cardinality(recorded) > Retain /\ Retain >= 1 BY <1>p, Const
    <3>2. IsFiniteSet(1..MaxEpoch) BY Const, FS_Interval
    <3>3. recorded \in SUBSET (1..MaxEpoch) BY <1>0, <1>T, Groups DEF TySets, Epochs
    <3>4. IsFiniteSet(recorded) BY <3>2, <3>3, FS_Subset
    <3>5. Cardinality(recorded) \in Nat BY <3>4, FS_CardinalityType
    <3>. SUFFICES ASSUME recorded' = {} PROVE FALSE OBVIOUS
    <3>6. recorded \in SUBSET {Min(recorded)} BY <1>p
    <3>7. IsFiniteSet({Min(recorded)}) /\ Cardinality({Min(recorded)}) = 1 BY FS_Singleton
    <3>8. Cardinality(recorded) <= 1 BY <3>6, <3>7, FS_Subset
    <3>. QED BY <3>1, <3>5, <3>8, Const
  <2>. QED BY <2>4, <2>5, <2>6, <2>7, <2>8, <1>u DEF SessionOK, CatchingUp
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_SessionFail ==
  ASSUME IndInv, NEW r \in R, SessionFail(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF SessionFail, trustVars
<1>p. /\ dState[r] = "stale" /\ sb' = [sb EXCEPT ![r] = -1] /\ reverted' = [reverted EXCEPT ![r] = -1]
  BY DEF SessionFail
<1>a. \A q \in R \ {r} : sb'[q] = sb[q] /\ reverted'[q] = reverted[q] BY <1>p, <1>0, <1>T, Groups DEF TyLeg
<1>b. ~CatchingUp(r)' /\ \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q)
  BY <1>u, <1>p, <1>a, <1>0, <1>T, Groups DEF CatchingUp, HiOf, TyLeg
<1>1. TypeOK' BY T_SessionFail DEF IndInv
<1>2. I_Ranges' /\ I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Members' /\ I_Hold' /\ I_Legal'
      /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Ranges, F_Lineage, F_Chain, F_Pend, F_Members, F_Hold, F_Legal, F_PendSeen,
     F_AckedEver, F_Vers, F_WHi, F_TC, F_Fresh
<1>3. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend' BY <1>0, <1>T, <1>u, <1>b DEF I_HeadHi, I_HeadLo, I_HeadPend
<1>4. I_Reverted'
  <2>1. \A q \in R \ {r} : RevOK(q)' BY <1>0, <1>T, <1>u, <1>a, RevOther DEF I_Reverted
  <2>2. RevOK(r)'
    <3>1. reverted'[r] = -1 BY <1>p, <1>0, <1>T, Groups DEF TyLeg
    <3>. QED BY <3>1 DEF RevOK
  <2>. QED BY <2>1, <2>2 DEF I_Reverted
<1>5. I_Session'
  <2>1. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>u, <1>a, SessionOther DEF I_Session
  <2>2. SessionOK(r)'
    <3>1. sb'[r] = -1 /\ reverted'[r] = -1 /\ dState'[r] = "stale" BY <1>p, <1>u, <1>0, <1>T, Groups DEF TyLeg
    <3>. QED BY <3>1, <1>b, <1>u DEF SessionOK
  <2>. QED BY <2>1, <2>2 DEF I_Session
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_SelectBase ==
  ASSUME IndInv, NEW r \in R, SelectBase(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF SelectBase, trustVars
<1>g. dState[r] = "stale" /\ path[r] = "back" /\ sb[r] = -1 /\ recorded # {} BY DEF SelectBase
<1>r. reverted[r] = -1 BY <1>0, <1>T, <1>g DEF I_Session, SessionOK
<1>p. PICK b \in BaseCandidates(r) \cup {0} :
        LET b2 == IF CoverageWalk /\ b # 0 /\ CoverSet(r, Tgt, b) = {} THEN 0 ELSE b
        IN /\ sb' = [sb EXCEPT ![r] = b2]
           /\ reverted' = [reverted EXCEPT ![r] = IF reverted[r] = b2 /\ ~mustRev[r] THEN reverted[r] ELSE -1]
  BY DEF SelectBase
<1>q. PICK b2 \in BaseCandidates(r) \cup {0} :
        /\ sb' = [sb EXCEPT ![r] = b2] /\ reverted' = [reverted EXCEPT ![r] = -1]
  <2>1. BaseCandidates(r) \subseteq Epochs BY <1>0, <1>T, Groups DEF BaseCandidates, TySets
  <2>2. b \in Nat BY <2>1 DEF Epochs
  <2>. QED BY <1>p, <1>r, <2>2
<1>c. b2 # 0 => b2 \in recorded /\ b2 \in sPres[r] /\ b2 \in tr[r] BY <1>q, Clean DEF BaseCandidates
<1>n. b2 \in 0..(nextSeq - 1) /\ b2 # -1
  <2>1. recorded \subseteq 1..(nextSeq - 1) BY <1>0, <1>T DEF I_Ranges
  <2>2. nextSeq \in 1..(MaxEpoch + 1) BY <1>0, <1>T, Groups DEF TyScalar
  <2>. QED BY <1>c, <2>1, <2>2
<1>s. pin'[r] = (IF b2 = 0 THEN Min(recorded) ELSE b2) /\ \A q \in R \ {r} : pin'[q] = pin[q]
  BY <1>q, <1>0, <1>T, Clean, Groups DEF SelectBase, TyLeg
<1>a. \A q \in R \ {r} : sb'[q] = sb[q] /\ reverted'[q] = reverted[q] BY <1>q, <1>0, <1>T, Groups DEF TyLeg
<1>b. ~CatchingUp(r)' /\ ~CatchingUp(r) /\ \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q)
  BY <1>u, <1>q, <1>g, <1>r, <1>a, <1>0, <1>T, Groups DEF CatchingUp, HiOf, TyLeg
<1>1. TypeOK' BY T_SelectBase DEF IndInv
<1>2. I_Ranges' /\ I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Members' /\ I_Hold' /\ I_Legal'
      /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Ranges, F_Lineage, F_Chain, F_Pend, F_Members, F_Hold, F_Legal, F_PendSeen,
     F_AckedEver, F_Vers, F_WHi, F_TC, F_Fresh
<1>3. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend' BY <1>0, <1>T, <1>u, <1>b DEF I_HeadHi, I_HeadLo, I_HeadPend
<1>4. I_Reverted'
  <2>1. \A q \in R \ {r} : RevOK(q)' BY <1>0, <1>T, <1>u, <1>a, RevOther DEF I_Reverted
  <2>2. RevOK(r)'
    <3>1. reverted'[r] = -1 BY <1>q, <1>0, <1>T, Groups DEF TyLeg
    <3>. QED BY <3>1 DEF RevOK
  <2>. QED BY <2>1, <2>2 DEF I_Reverted
<1>5. I_Session'
  <2>0. sb'[r] = b2 BY <1>q, <1>0, <1>T, Groups DEF TyLeg
  <2>1. sb'[r] # -1 => dState'[r] = "stale" /\ sb'[r] < nextSeq' /\ recorded' # {}
    <3>1. dState'[r] = "stale" /\ nextSeq' = nextSeq /\ recorded' = recorded BY <1>g, <1>u
    <3>2. b2 < nextSeq BY <1>n, <1>T
    <3>. QED BY <2>0, <3>1, <3>2, <1>g
  <2>2. dState'[r] = "stale" /\ sb'[r] > 0 => pin'[r] = sb'[r] /\ sb'[r] \in recorded
    BY <1>q, <1>s, <1>c, <1>u, <1>0, <1>T, Groups DEF TyLeg
  <2>3. dState'[r] = "stale" /\ sb'[r] > 0 /\ reverted'[r] = -1 /\ sb'[r] \in sPres'[r] => sb'[r] \in tr'[r]
    BY <1>q, <1>c, <1>u, <1>0, <1>T, Groups DEF TyLeg
  <2>4. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>u, <1>a, <1>s, SessionOther DEF I_Session
  <2>5. SessionOK(r)'
    <3>1. dState'[r] = "stale" /\ reverted'[r] = -1 /\ ~CatchingUp(r)' BY <1>u, <1>g, <1>b, <1>q, <1>0, <1>T, Groups DEF TyLeg
    <3>2. sb'[r] # -1 /\ dState'[r] # "insync" /\ dState'[r] # "standby" BY <3>1, <1>q, <1>n, <1>0, <1>T, Groups DEF TyLeg
    <3>3. recorded' = recorded /\ nextSeq' = nextSeq /\ cutK' = cutK BY <1>u
    <3>. QED BY <3>1, <3>2, <3>3, <2>1, <2>2, <2>3 DEF SessionOK
  <2>. QED BY <2>4, <2>5 DEF I_Session
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>u, <1>0, <1>T, IndSplitP

-----------------------------------------------------------------------------
(* The data plane.                                                         *)

LEMMA I_Submit ==
  ASSUME IndInv, NEW x \in Blocks, Submit(x) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<acked, fails, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF Submit, trustVars
<1>. DEFINE nw == [b |-> x, v |-> ver + 1, to |-> members, landed |-> {},
                   dropped |-> {}, recAt |-> Latest, cutAt |-> nextSeq]
<1>p. /\ attached /\ ~Pending(x) /\ ver < MaxVer
      /\ ver' = ver + 1 /\ pend' = pend \cup {nw}
  BY DEF Submit
<1>v. ver \in Nat /\ nextSeq \in Nat BY <1>0, <1>T, Groups DEF TyScalar, Vers
<1>n. \A q \in R : head[q][x] # ver + 1 /\ \A e \in sPres[q] : sCon[q][e][x] # ver + 1
  BY <1>0, <1>T, <1>v, Groups DEF I_Vers, TyHead, TySnap, Vers
<1>1. TypeOK' BY T_Submit DEF IndInv
<1>2. I_Lineage' /\ I_Chain' /\ I_Members' /\ I_Hold' /\ I_AckedEver' /\ I_WHi' /\ I_HeadHi'
      /\ I_HeadLo' /\ I_Reverted' /\ I_Session' /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Members, F_Hold, F_AckedEver, F_WHi, F_HeadHi, F_HeadLo,
     F_Reverted, F_Session, F_TC, F_Fresh
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p DEF I_Ranges
<1>4. I_Pend'
  <2>0. TypeOK /\ I_Pend /\ I_Vers /\ I_Ranges BY <1>0, <1>T
  <2>N. /\ ver \in Nat /\ nextSeq \in Nat /\ acked[x] \in Nat
        /\ \A w \in pend : w.v \in Nat /\ w.cutAt \in Nat
    BY <2>0, Nats
  <2>1. ver + 1 > acked[x] BY <2>0, <2>N DEF I_Vers
  <2>2. \A w \in pend : w.b # x BY <1>p DEF Pending
  <2>3. cutK # 0 => nextSeq >= cutK BY <2>0, <2>N DEF I_Ranges
  <2>4. \A w1, w2 \in pend' : w1.b = w2.b => w1 = w2 BY <2>0, <2>2, <1>p DEF I_Pend
  <2>5. ASSUME NEW w \in pend'
        PROVE  /\ w.v > acked'[w.b] /\ w.v <= ver'
               /\ w.landed \cap w.dropped = {}
               /\ w.landed \cup w.dropped \subseteq w.to
               /\ members' \subseteq w.to \ w.dropped
               /\ \A q \in w.landed \cap members' : head'[q][w.b] = w.v
               /\ w.cutAt <= nextSeq' /\ w.cutAt >= nextSeq' - 1
               /\ cutK' # 0 => w.cutAt >= cutK'
    <3>1. CASE w = nw BY <3>1, <2>1, <2>3, <2>N, <1>p, <1>u
    <3>2. CASE w \in pend BY <3>2, <2>0, <2>N, <1>p, <1>u DEF I_Pend
    <3>. QED BY <3>1, <3>2, <1>p
  <2>. QED BY <2>4, <2>5 DEF I_Pend
<1>5. I_Legal'
  <2>1. \A y \in Blocks : PendVal(y) \subseteq PendVal(y)' BY <1>p DEF PendVal
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1 DEF I_Legal
<1>6. I_PendSeen' BY <1>0, <1>T, <1>u, <1>p, <1>n DEF I_PendSeen
<1>7. I_Vers'
  <2>1. ver \in Nat /\ (\A y \in Blocks : acked[y] \in Nat)
        /\ (\A q \in R : \A y \in Blocks : head[q][y] \in Nat)
        /\ (\A q \in R : \A e \in Epochs : \A y \in Blocks : sCon[q][e][y] \in Nat)
    BY <1>0, <1>T, Nats
  <2>2. \A q \in R : sPres[q] \subseteq Epochs BY <1>0, <1>T, Groups DEF TySnap
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1, <2>2 DEF I_Vers
<1>8. I_HeadPend' BY <1>0, <1>T, <1>u, <1>p, <1>n DEF I_HeadPend, CatchingUp, HiOf
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_Land ==
  ASSUME IndInv, NEW w \in pend, NEW r \in R, Land(w, r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF Land, trustVars
<1>. DEFINE w2 == [w EXCEPT !.landed = @ \cup {r}]
<1>p. /\ r \in w.to \ (w.landed \cup w.dropped) /\ path[r] \in {"ok", "failing"}
      /\ head' = [head EXCEPT ![r][w.b] = w.v] /\ hAl' = [hAl EXCEPT ![r] = @ \cup {w.b}]
      /\ pend' = (pend \ {w}) \cup {w2}
  BY DEF Land
<1>w. w \in Write /\ w2.b = w.b /\ w2.v = w.v /\ w2.cutAt = w.cutAt /\ w2.to = w.to
      /\ w2.dropped = w.dropped /\ w2.landed = w.landed \cup {r}
  BY <1>0, <1>T, Groups DEF TyPend, Write
<1>a. attached BY <1>0, <1>T DEF I_Ranges
<1>c. ~CatchingUp(r) BY <1>0, <1>T, <1>p DEF I_Session, SessionOK
<1>h. \A q \in R : \A y \in Blocks : head'[q][y] = IF q = r /\ y = w.b THEN w.v ELSE head[q][y]
  BY <1>p, <1>0, <1>T, Groups DEF TyHead
<1>v. \A y \in Blocks : PendVal(y)' = PendVal(y) BY <1>p, <1>w DEF PendVal
<1>1. TypeOK' BY T_Land DEF IndInv
<1>2. I_Lineage' /\ I_Members' /\ I_AckedEver' /\ I_WHi' /\ I_Reverted' /\ I_Session'
      /\ Inv_TrustComplete' /\ I_Fresh'
  <2>1. I_Reverted' BY <1>0, <1>T, <1>u, <1>h, <1>c, <1>p, Groups DEF I_Reverted, RevOK, CatchingUp, TyHead
  <2>2. I_Session' BY <1>0, <1>T, <1>u, <1>p DEF I_Session, SessionOK, CatchingUp
  <2>. QED BY <1>0, <1>T, <1>u, <2>1, <2>2, F_Lineage, F_Members, F_AckedEver, F_WHi, F_TC, F_Fresh
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p, <1>a DEF I_Ranges
<1>4. I_Chain'
  <2>1. \A q \in R : \A y \in Blocks : y \notin hAl'[q] => head'[q][y] = head[q][y] /\ y \notin hAl[q]
    BY <1>p, <1>h, <1>0, <1>T, Groups DEF TyHead
  <2>. QED BY <1>0, <1>T, <1>u, <2>1 DEF I_Chain
<1>5. I_Pend'
  <2>1. \A u \in pend' : u = w2 \/ (u \in pend /\ u # w) BY <1>p
  <2>2. \A u \in pend : u # w => u.b # w.b BY <1>0, <1>T DEF I_Pend
  <2>3. \A w1, u2 \in pend' : w1.b = u2.b => w1 = u2 BY <2>1, <2>2, <1>w, <1>0, <1>T DEF I_Pend
  <2>4. ASSUME NEW u \in pend'
        PROVE  /\ u.v > acked'[u.b] /\ u.v <= ver'
               /\ u.landed \cap u.dropped = {}
               /\ u.landed \cup u.dropped \subseteq u.to
               /\ members' \subseteq u.to \ u.dropped
               /\ \A q \in u.landed \cap members' : head'[q][u.b] = u.v
               /\ u.cutAt <= nextSeq' /\ u.cutAt >= nextSeq' - 1
               /\ cutK' # 0 => u.cutAt >= cutK'
    <3>1. CASE u = w2
      <4>1. \A q \in w2.landed \cap members : head'[q][w.b] = w.v
        <5>. SUFFICES ASSUME NEW q \in w2.landed \cap members PROVE head'[q][w.b] = w.v OBVIOUS
        <5>1. CASE q = r BY <5>1, <1>h, <1>0, <1>T, Groups DEF TyPend, Write
        <5>2. CASE q # r BY <5>2, <1>w, <1>h, <1>0, <1>T, Groups DEF I_Pend, TyPend, Write
        <5>. QED BY <5>1, <5>2
      <4>. QED BY <3>1, <4>1, <1>w, <1>p, <1>u, <1>0, <1>T DEF I_Pend
    <3>2. CASE u \in pend /\ u # w
      <4>1. u.b # w.b BY <3>2, <2>2
      <4>0. u.b \in Blocks BY <3>2, <1>0, <1>T, Groups DEF TyPend, Write
      <4>2. \A q \in R : head'[q][u.b] = head[q][u.b] BY <4>0, <4>1, <1>h
      <4>3. /\ u.v > acked[u.b] /\ u.v <= ver /\ u.landed \cap u.dropped = {}
            /\ u.landed \cup u.dropped \subseteq u.to /\ members \subseteq u.to \ u.dropped
            /\ \A q \in u.landed \cap members : head[q][u.b] = u.v
            /\ u.cutAt <= nextSeq /\ u.cutAt >= nextSeq - 1 /\ (cutK # 0 => u.cutAt >= cutK)
        BY <3>2, <1>0, <1>T DEF I_Pend
      <4>4. acked' = acked /\ ver' = ver /\ members' = members /\ nextSeq' = nextSeq /\ cutK' = cutK BY <1>u
      <4>5. members \subseteq R BY <1>0, <1>T, Groups DEF TySets
      <4>. QED BY <4>2, <4>3, <4>4, <4>5
    <3>. QED BY <3>1, <3>2, <2>1
  <2>. QED BY <2>3, <2>4 DEF I_Pend
<1>6. I_Hold' BY <1>0, <1>T, <1>u, <1>h, <1>a, Groups DEF I_Hold, I_Pend, TyHead, TyPend, Write, Vers
<1>7. I_Legal'
  <2>1. w.v \in PendVal(w.b) BY DEF PendVal
  <2>2. \A u \in pend : \E u2 \in pend' : u2.b = u.b /\ u2.v = u.v /\ u2.cutAt = u.cutAt BY <1>p, <1>w
  <2>. SUFFICES ASSUME NEW q \in R, NEW y \in Blocks
               PROVE  /\ head'[q][y] \in ackedEver'[y] \cup PendVal(y)'
                      /\ head'[q][y] <= acked'[y] \/ head'[q][y] \in PendVal(y)'
                      /\ \A e \in sPres'[q] : /\ sCon'[q][e][y] \in ackedEver'[y] \cup PendVal(y)'
                                              /\ \/ sCon'[q][e][y] <= acked'[y]
                                                 \/ \E u \in pend' : u.b = y /\ u.v = sCon'[q][e][y] /\ u.cutAt = e
    BY DEF I_Legal
  <2>3. CASE q = r /\ y = w.b BY <2>3, <1>h, <1>v, <2>1, <2>2, <1>u, <1>0, <1>T, <1>w DEF I_Legal
  <2>4. CASE ~(q = r /\ y = w.b) BY <2>4, <1>h, <1>v, <2>2, <1>u, <1>0, <1>T DEF I_Legal
  <2>. QED BY <2>3, <2>4
<1>8. I_PendSeen'
  <2>. SUFFICES ASSUME NEW u \in pend',
                      \E q \in R : head'[q][u.b] = u.v \/ \E e \in sPres'[q] : sCon'[q][e][u.b] = u.v
               PROVE  u.landed # {}
    BY DEF I_PendSeen
  <2>1. CASE u = w2 BY <2>1, <1>w
  <2>2. CASE u \in pend /\ u # w
    <3>1. u.b # w.b BY <2>2, <1>0, <1>T DEF I_Pend
    <3>0. u.b \in Blocks BY <2>2, <1>0, <1>T, Groups DEF TyPend, Write
    <3>2. \A q \in R : head'[q][u.b] = head[q][u.b] BY <3>0, <3>1, <1>h
    <3>. QED BY <2>2, <3>2, <1>u, <1>0, <1>T DEF I_PendSeen
  <2>. QED BY <2>1, <2>2, <1>p
<1>9. I_Vers' BY <1>0, <1>T, <1>u, <1>h DEF I_Vers, I_Pend
<1>10. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
  <2>0. \A q \in R \ {r} : head'[q] = head[q] BY <1>p, <1>0, <1>T, Groups DEF TyHead
  <2>1. \A q \in R : CatchingUp(q) => head'[q] = head[q] BY <2>0, <1>c
  <2>2. \A q \in R : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) BY <1>u DEF CatchingUp, HiOf
  <2>3. \A u \in pend' : \E u2 \in pend : u2.b = u.b /\ u2.v = u.v /\ u2.cutAt = u.cutAt BY <1>p, <1>w
  <2>. QED BY <1>0, <1>T, <1>u, <2>1, <2>2, <2>3 DEF I_HeadHi, I_HeadLo, I_HeadPend
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_Drop ==
  ASSUME IndInv, NEW w \in pend, NEW r \in R, Drop(w, r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<acked, fails, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF Drop, trustVars
<1>. DEFINE w2 == [w EXCEPT !.dropped = @ \cup {r}]
<1>p. /\ r \in w.to \ (w.landed \cup w.dropped) /\ path[r] \in {"failing", "out", "back"}
      /\ pend' = (pend \ {w}) \cup {w2} /\ members' = members \ {r}
  BY Clean DEF Drop
<1>w. w \in Write /\ w2.b = w.b /\ w2.v = w.v /\ w2.cutAt = w.cutAt /\ w2.to = w.to
      /\ w2.landed = w.landed /\ w2.dropped = w.dropped \cup {r}
  BY <1>0, <1>T, Groups DEF TyPend, Write
<1>a. attached BY <1>0, <1>T DEF I_Ranges
<1>v. \A y \in Blocks : PendVal(y)' = PendVal(y) BY <1>p, <1>w DEF PendVal
<1>e. \A u \in pend' : \E u2 \in pend : u2.b = u.b /\ u2.v = u.v /\ u2.cutAt = u.cutAt /\ u2.landed = u.landed
  BY <1>p, <1>w
<1>f. \A u \in pend : \E u2 \in pend' : u2.b = u.b /\ u2.v = u.v /\ u2.cutAt = u.cutAt /\ u2.landed = u.landed
  BY <1>p, <1>w
<1>1. TypeOK' BY T_Drop DEF IndInv
<1>2. I_Lineage' /\ I_Chain' /\ I_AckedEver' /\ I_Vers' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo'
      /\ I_Reverted' /\ Inv_TrustComplete'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_AckedEver, F_Vers, F_WHi, F_HeadHi, F_HeadLo, F_Reverted, F_TC
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p, <1>a DEF I_Ranges
<1>4. I_Pend'
  <2>1. \A u \in pend : u # w => u.b # w.b BY <1>0, <1>T DEF I_Pend
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <1>w, <2>1 DEF I_Pend
<1>5. I_Members'
  <2>1. Healthy' = Healthy BY <1>u, <1>p DEF Healthy
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1 DEF I_Members, InSyncR
<1>6. I_Hold' BY <1>0, <1>T, <1>u, <1>p DEF I_Hold, InSyncR
<1>7. I_Legal' /\ I_PendSeen' /\ I_HeadPend'
  BY <1>0, <1>T, <1>u, <1>v, <1>e, <1>f DEF I_Legal, I_PendSeen, I_HeadPend, CatchingUp, HiOf
<1>8. I_Session' BY <1>0, <1>T, <1>u, <1>p DEF I_Session, SessionOK, CatchingUp
<1>9. I_Fresh' BY <1>0, <1>T, <1>u, <1>p, <1>a DEF I_Fresh, Bases
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_Complete ==
  ASSUME IndInv, NEW w \in pend, Complete(w) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark, reverted, sb, pin>>
      /\ UNCHANGED <<fails, riskRec, badAdmit, badLoss, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, tr, htr, cutAck>>
  BY DEF Complete, trustVars
<1>w. w \in Write BY <1>0, <1>T, Groups DEF TyPend
<1>L. (Cardinality(w.landed) >= WriteQuorum) <=> w.landed # {} BY <1>w, OneLanding DEF Write
<1>p. /\ w.landed \cup w.dropped = w.to /\ pend' = pend \ {w}
      /\ acked' = IF w.landed # {} THEN [acked EXCEPT ![w.b] = w.v] ELSE acked
      /\ ackedEver' = IF w.landed # {} THEN [ackedEver EXCEPT ![w.b] = @ \cup {w.v}] ELSE ackedEver
  BY <1>L DEF Complete
<1>a. attached BY <1>0, <1>T DEF I_Ranges
<1>o. \A u \in pend : u # w => u.b # w.b BY <1>0, <1>T DEF I_Pend
<1>g. w.v > acked[w.b] /\ w.v \in Nat /\ acked[w.b] \in Nat BY <1>0, <1>T, <1>w, Groups DEF I_Pend, TyPend, Write, Vers
<1>m. members \subseteq w.landed BY <1>0, <1>T, <1>p DEF I_Pend
<1>1. TypeOK' BY T_Complete DEF IndInv
<1>2. I_Lineage' /\ I_Chain' /\ I_Members' /\ I_WHi' /\ I_HeadHi' /\ I_HeadLo' /\ I_Reverted'
      /\ I_Session' /\ Inv_TrustComplete' /\ I_Fresh'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Members, F_WHi, F_HeadHi, F_HeadLo, F_Reverted, F_Session, F_TC, F_Fresh
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p, <1>a DEF I_Ranges
<1>4. I_Pend'
  <2>1. \A u \in pend' : acked'[u.b] = acked[u.b] BY <1>p, <1>o, <1>0, <1>T, Groups DEF TyPend, Write
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1 DEF I_Pend
<1>5. I_Hold'
  <2>1. CASE w.landed # {}
    <3>1. \A q \in members : head[q][w.b] = w.v BY <1>0, <1>T, <1>m DEF I_Pend
    <3>. QED BY <1>0, <1>T, <1>u, <1>p, <1>g, <2>1, <3>1, <1>a, Groups DEF I_Hold, TyPend
  <2>2. CASE w.landed = {} BY <1>0, <1>T, <1>u, <1>p, <2>2 DEF I_Hold, InSyncR
  <2>. QED BY <2>1, <2>2
<1>6. I_AckedEver' /\ I_Vers'
  <2>1. w.v <= ver BY <1>0, <1>T DEF I_Pend
  <2>N. ver \in Nat /\ (\A y \in Blocks : acked[y] \in Nat /\ ackedEver[y] \subseteq Nat) /\ w.b \in Blocks
    BY <1>0, <1>T, Nats
  <2>2. CASE w.landed = {} BY <2>2, <1>p, <1>u, <1>0, <1>T, F_AckedEver, F_Vers
  <2>3. CASE w.landed # {}
    <3>1. /\ acked' = [acked EXCEPT ![w.b] = w.v]
          /\ ackedEver' = [ackedEver EXCEPT ![w.b] = @ \cup {w.v}]
      BY <2>3, <1>p
    <3>2. \A y \in Blocks : y # w.b => acked'[y] = acked[y] /\ ackedEver'[y] = ackedEver[y]
      BY <3>1, <1>0, <1>T, Groups DEF TyPend
    <3>3. acked'[w.b] = w.v /\ ackedEver'[w.b] = ackedEver[w.b] \cup {w.v} BY <3>1, <2>N, <1>0, <1>T, Groups DEF TyPend
    <3>4. I_AckedEver'
      <4>. SUFFICES ASSUME NEW y \in Blocks
                   PROVE  acked'[y] \in ackedEver'[y] /\ 0 \in ackedEver'[y] /\ \A v \in ackedEver'[y] : v <= acked'[y]
        BY DEF I_AckedEver
      <4>1. CASE y = w.b BY <4>1, <3>3, <1>g, <2>N, <1>0, <1>T DEF I_AckedEver
      <4>2. CASE y # w.b BY <4>2, <3>2, <1>0, <1>T DEF I_AckedEver
      <4>. QED BY <4>1, <4>2
    <3>5. I_Vers'
      <4>1. \A y \in Blocks : acked'[y] <= ver BY <3>2, <3>3, <2>1, <1>0, <1>T DEF I_Vers
      <4>. QED BY <4>1, <1>u, <1>0, <1>T DEF I_Vers
    <3>. QED BY <3>4, <3>5
  <2>. QED BY <2>2, <2>3
<1>7. I_Legal'
  <2>1. \A y \in Blocks : y # w.b => PendVal(y)' = PendVal(y) BY <1>p DEF PendVal
  <2>2. PendVal(w.b)' = {} /\ PendVal(w.b) = {w.v} BY <1>p, <1>o DEF PendVal
  <2>3. \A q \in R : (head[q][w.b] = w.v \/ \E e \in sPres[q] : sCon[q][e][w.b] = w.v) => w.landed # {}
    BY <1>0, <1>T DEF I_PendSeen
  <2>4. w.landed # {} => w.v \in ackedEver'[w.b] /\ acked'[w.b] = w.v BY <1>p, <1>0, <1>T, <1>w, Groups DEF TyPend, Write
  <2>5. \A y \in Blocks : y # w.b => acked'[y] = acked[y] /\ ackedEver'[y] = ackedEver[y]
    BY <1>p, <1>0, <1>T, <1>w, Groups DEF TyPend, Write
  <2>6. acked[w.b] <= acked'[w.b] /\ ackedEver[w.b] \subseteq ackedEver'[w.b]
    BY <1>p, <1>g, <1>0, <1>T, <1>w, Groups DEF TyPend, Write
  <2>7. \A u \in pend' : u \in pend /\ u.b # w.b BY <1>p, <1>o
  <2>. SUFFICES ASSUME NEW q \in R, NEW y \in Blocks
               PROVE  /\ head[q][y] \in ackedEver'[y] \cup PendVal(y)'
                      /\ head[q][y] <= acked'[y] \/ head[q][y] \in PendVal(y)'
                      /\ \A e \in sPres[q] : /\ sCon[q][e][y] \in ackedEver'[y] \cup PendVal(y)'
                                            /\ \/ sCon[q][e][y] <= acked'[y]
                                               \/ \E u \in pend' : u.b = y /\ u.v = sCon[q][e][y] /\ u.cutAt = e
    BY <1>u DEF I_Legal
  <2>N1. head[q][y] \in Nat /\ acked[y] \in Nat BY <1>0, <1>T, Nats
  <2>N2. acked'[y] \in Nat BY <1>1 DEF TypeOK, Vers
  <2>N3. \A e \in sPres[q] : sCon[q][e][y] \in Nat
    <3>1. sPres[q] \subseteq Epochs BY <1>0, <1>T, Groups DEF TySnap
    <3>. QED BY <3>1, <1>0, <1>T, Nats
  <2>N. head[q][y] \in Nat /\ acked[y] \in Nat /\ acked'[y] \in Nat /\ \A e \in sPres[q] : sCon[q][e][y] \in Nat
    BY <2>N1, <2>N2, <2>N3
  <2>8. CASE y # w.b
    <3>1. \A e \in sPres[q] : \A u \in pend : u.b = y /\ u.v = sCon[q][e][y] /\ u.cutAt = e => u \in pend'
      BY <2>8, <1>p
    <3>. QED BY <2>8, <2>1, <2>5, <3>1, <1>0, <1>T DEF I_Legal
  <2>9. CASE y = w.b
    <3>1. head[q][y] \in ackedEver[y] \cup {w.v} /\ (head[q][y] <= acked[y] \/ head[q][y] = w.v)
      BY <2>9, <2>2, <1>0, <1>T DEF I_Legal
    <3>2. \A e \in sPres[q] : sCon[q][e][y] \in ackedEver[y] \cup {w.v} /\ (sCon[q][e][y] <= acked[y] \/ sCon[q][e][y] = w.v)
      <4>. SUFFICES ASSUME NEW e \in sPres[q] PROVE sCon[q][e][y] \in ackedEver[y] \cup {w.v} /\ (sCon[q][e][y] <= acked[y] \/ sCon[q][e][y] = w.v)
        OBVIOUS
      <4>1. sCon[q][e][y] \in ackedEver[y] \cup PendVal(y) BY <1>0, <1>T DEF I_Legal
      <4>2. \/ sCon[q][e][y] <= acked[y]
            \/ \E u \in pend : u.b = y /\ u.v = sCon[q][e][y] /\ u.cutAt = e
        BY <1>0, <1>T DEF I_Legal
      <4>3. \A u \in pend : u.b = y => u = w BY <2>9, <1>o
      <4>. QED BY <4>1, <4>2, <4>3, <2>2, <2>9
    <3>3. head[q][y] = w.v => head[q][y] \in ackedEver'[y] /\ head[q][y] <= acked'[y] BY <2>3, <2>4, <2>9, <2>N
    <3>4. \A e \in sPres[q] : sCon[q][e][y] = w.v => sCon[q][e][y] \in ackedEver'[y] /\ sCon[q][e][y] <= acked'[y]
      BY <2>3, <2>4, <2>9, <2>N
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <2>6, <2>N, <2>9
  <2>. QED BY <2>8, <2>9
<1>8. I_PendSeen' BY <1>0, <1>T, <1>u, <1>p DEF I_PendSeen
<1>9. I_HeadPend' BY <1>0, <1>T, <1>u, <1>p DEF I_HeadPend, CatchingUp, HiOf
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>u, <1>0, <1>T, IndSplitP

-----------------------------------------------------------------------------
(* The revert (§5 step 0, or the full build's empty head with its reap).    *)

LEMMA I_RevertD ==
  ASSUME IndInv, NEW r \in R, RevertD(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>. DEFINE keep == IF sb[r] = 0 THEN {e \in sPres[r] : e < Min(recorded)}
                                  ELSE {e \in sPres[r] : e <= sb[r]}
<1>. DEFINE base == IF sb[r] = 0 THEN Zero ELSE sCon[r][sb[r]]
<1>so. SessionOK(r) /\ RevOK(r) BY <1>0, <1>T DEF I_Session, I_Reverted
<1>g. /\ dState[r] = "stale" /\ path[r] = "back" /\ sb[r] # -1 /\ reverted[r] = -1
      /\ (sb[r] > 0 => sb[r] \in sPres[r])
  BY DEF RevertD
<1>t. /\ sb[r] \in 0..MaxEpoch /\ recorded # {} /\ recorded \subseteq Nat
      /\ Min(recorded) \in recorded /\ \A e \in recorded : Min(recorded) <= e
  <2>1. sb[r] \in 0..MaxEpoch /\ recorded # {} BY <1>so, <1>g, <1>0, <1>T, Groups DEF SessionOK, TyLeg
  <2>2. recorded \subseteq Nat BY <1>0, <1>T, Groups DEF TySets, Epochs
  <2>. QED BY <2>1, <2>2, MinProp
<1>c. ChainShape(sPres[r], sPar[r], anc[r]) /\ ChainReads(sPres[r], sPar[r], sAl[r], sCon[r])
  BY <1>0, <1>T, LegChain
<1>k. /\ keep \subseteq sPres[r]
      /\ \A e \in keep : e <= sb[r] \/ (sb[r] = 0 /\ \A f \in recorded : e < f)
      /\ sb[r] > 0 => sb[r] \in keep
      /\ \A e \in keep : sPar[r][e] # 0 => sPar[r][e] \in keep
      /\ \A e \in sPres[r] : e > sb[r] /\ sb[r] > 0 => e \notin keep
  <2>1. sPres[r] \subseteq Nat BY <1>0, <1>T, Groups DEF TySnap, Epochs
  <2>2. \A e \in sPres[r] : sPar[r][e] < e /\ (sPar[r][e] # 0 => sPar[r][e] \in sPres[r]) /\ sPar[r][e] \in Nat
    BY <1>c DEF ChainShape
  <2>. QED BY <1>g, <1>t, <2>1, <2>2
<1>b. \A x \in Blocks : base[x] = Up(sPar[r], sCon[r], sb[r], x) /\ base[x] \in Nat
  BY <1>t, <1>g, <1>0, <1>T, Nats DEF Up, Zero, Epochs
<1>. HIDE DEF keep, base
<1>u. /\ UNCHANGED <<ver, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt, cutDone>>
      /\ UNCHANGED <<members, attached, path, dState, mark, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, htr>>
  BY Clean DEF RevertD
<1>p. /\ sPres' = [sPres EXCEPT ![r] = keep]
      /\ anc' = [anc EXCEPT ![r] = [c \in Epochs |-> IF c \in keep THEN anc[r][c] ELSE {}]]
      /\ tr' = [tr EXCEPT ![r] = @ \cap keep]
      /\ cutAck' = [cutAck EXCEPT ![r] = [c \in Epochs |-> IF c \in keep THEN @[c] ELSE Zero]]
      /\ head' = [head EXCEPT ![r] = base]
      /\ hAl' = [hAl EXCEPT ![r] = {}] /\ hPar' = [hPar EXCEPT ![r] = sb[r]]
      /\ reverted' = [reverted EXCEPT ![r] = sb[r]]
      /\ cutGone' = IF cutK # 0 /\ r \in cutTgt \ cutDone THEN cutGone \cup {r} ELSE cutGone
  BY Clean DEF RevertD, keep, base
<1>m. \A q \in R \ {r} : /\ sPres'[q] = sPres[q] /\ anc'[q] = anc[q] /\ tr'[q] = tr[q]
                          /\ cutAck'[q] = cutAck[q] /\ head'[q] = head[q] /\ hAl'[q] = hAl[q]
                          /\ hPar'[q] = hPar[q] /\ reverted'[q] = reverted[q]
  BY <1>p, <1>0, <1>T, Groups DEF TySnap, TyTrust, TyHead, TyLeg
<1>n. /\ sPres'[r] = keep /\ tr'[r] = tr[r] \cap keep /\ hAl'[r] = {} /\ hPar'[r] = sb[r]
      /\ head'[r] = base /\ reverted'[r] = sb[r]
      /\ \A c \in keep : anc'[r][c] = anc[r][c] /\ cutAck'[r][c] = cutAck[r][c]
      /\ \A c \in Epochs \ keep : anc'[r][c] = {}
  <2>1. /\ sPres'[r] = keep /\ tr'[r] = tr[r] \cap keep /\ hAl'[r] = {} /\ hPar'[r] = sb[r]
        /\ head'[r] = base /\ reverted'[r] = sb[r]
    BY <1>p, <1>0, <1>T, Groups DEF TySnap, TyTrust, TyHead, TyLeg
  <2>2. /\ anc'[r] = [c \in Epochs |-> IF c \in keep THEN anc[r][c] ELSE {}]
        /\ cutAck'[r] = [c \in Epochs |-> IF c \in keep THEN cutAck[r][c] ELSE Zero]
    BY <1>p, <1>0, <1>T, Groups DEF TySnap, TyTrust
  <2>3. keep \subseteq Epochs BY <1>k, <1>T
  <2>. QED BY <2>1, <2>2, <2>3
<1>s. r \notin members /\ r \notin InSyncR /\ ~CatchingUp(r)
  BY <1>0, <1>T, <1>g, <1>so DEF I_Members, I_Ranges, InSyncR, CatchingUp
<1>sP. \A q \in R : sPres'[q] \subseteq sPres[q] /\ tr'[q] \subseteq tr[q] /\ tr'[q] \subseteq sPres'[q]
               /\ \A c \in sPres'[q] : cutAck'[q][c] = cutAck[q][c]
  <2>1. \A q \in R : tr[q] \subseteq sPres[q] BY <1>0, <1>T DEF I_Ranges
  <2>. QED BY <1>m, <1>n, <1>k, <2>1
<1>1. TypeOK' BY T_RevertD DEF IndInv
<1>2. I_Ranges'
  <2>1. tr'[r] \subseteq sPres'[r] BY <1>n
  <2>2. hPar'[r] \in sPres'[r] \cup {0} BY <1>n, <1>k, <1>t
  <2>. QED BY <1>0, <1>T, <1>u, <1>m, <1>sP, <2>1, <2>2 DEF I_Ranges
<1>3. I_Lineage'
  <2>1. ChainShape(keep, sPar[r], anc'[r]) BY <1>c, <1>k, <1>n DEF ChainShape
  <2>. QED BY <1>0, <1>T, <1>m, <1>n, <1>u, <2>1 DEF I_Lineage
<1>4. I_Chain'
  <2>1. ChainReads(keep, sPar[r], sAl[r], sCon[r]) BY <1>c, <1>k DEF ChainReads, Up
  <2>2. \A x \in Blocks : head'[r][x] = Up(sPar[r], sCon[r], hPar'[r], x) BY <1>n, <1>b
  <2>. QED BY <1>0, <1>T, <1>m, <1>n, <1>u, <2>1, <2>2 DEF I_Chain
<1>5. I_Pend' /\ I_Members' /\ I_Hold'
  <2>0. members \subseteq R /\ InSyncR \subseteq R /\ \A q \in members : head'[q] = head[q]
    BY <1>0, <1>T, <1>m, <1>s, Groups DEF TySets, InSyncR
  <2>1. I_Pend'
    <3>. SUFFICES ASSUME NEW w \in pend
                 PROVE  \A q \in w.landed \cap members : head'[q][w.b] = w.v
      BY <1>0, <1>T, <1>u DEF I_Pend
    <3>. QED BY <2>0, <1>0, <1>T DEF I_Pend
  <2>2. I_Hold'
    <3>1. InSyncR' = InSyncR BY <1>u DEF InSyncR
    <3>2. \A q \in InSyncR : head'[q] = head[q] BY <1>m, <1>s, <2>0
    <3>. QED BY <1>0, <1>T, <1>u, <2>0, <3>1, <3>2 DEF I_Hold
  <2>. QED BY <1>0, <1>T, <1>u, <2>1, <2>2, F_Members
<1>bv. \A x \in Blocks : base[x] = 0 \/ (sb[r] \in sPres[r] /\ base[x] = sCon[r][sb[r]][x])
  <2>. SUFFICES ASSUME NEW x \in Blocks PROVE base[x] = 0 \/ (sb[r] \in sPres[r] /\ base[x] = sCon[r][sb[r]][x])
    OBVIOUS
  <2>1. CASE sb[r] = 0 BY <2>1, <1>b DEF Up
  <2>2. CASE sb[r] # 0 BY <2>2, <1>b, <1>g, <1>t DEF Up
  <2>. QED BY <2>1, <2>2
<1>6. I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers'
  <2>0. \A x \in Blocks : 0 \in ackedEver[x] /\ 0 <= acked[x] BY <1>0, <1>T, Nats DEF I_AckedEver
  <2>r. \A x \in Blocks : /\ head'[r][x] \in ackedEver[x] \cup PendVal(x)
                          /\ head'[r][x] <= acked[x] \/ head'[r][x] \in PendVal(x)
    <3>. SUFFICES ASSUME NEW x \in Blocks
                 PROVE  /\ head'[r][x] \in ackedEver[x] \cup PendVal(x)
                        /\ head'[r][x] <= acked[x] \/ head'[r][x] \in PendVal(x)
      OBVIOUS
    <3>1. CASE base[x] = 0 BY <3>1, <2>0, <1>n
    <3>2. CASE base[x] # 0
      <4>1. sb[r] \in sPres[r] /\ base[x] = sCon[r][sb[r]][x] BY <3>2, <1>bv
      <4>2. /\ sCon[r][sb[r]][x] \in ackedEver[x] \cup PendVal(x)
            /\ \/ sCon[r][sb[r]][x] <= acked[x]
               \/ \E w \in pend : w.b = x /\ w.v = sCon[r][sb[r]][x] /\ w.cutAt = sb[r]
        BY <4>1, <1>0, <1>T DEF I_Legal
      <4>. QED BY <4>1, <4>2, <1>n DEF PendVal
    <3>. QED BY <3>1, <3>2
  <2>2. I_Legal'
    <3>. SUFFICES ASSUME NEW q \in R, NEW x \in Blocks
                 PROVE  /\ head'[q][x] \in ackedEver[x] \cup PendVal(x)
                        /\ head'[q][x] <= acked[x] \/ head'[q][x] \in PendVal(x)
                        /\ \A e \in sPres'[q] : /\ sCon[q][e][x] \in ackedEver[x] \cup PendVal(x)
                                               /\ \/ sCon[q][e][x] <= acked[x]
                                                  \/ \E w \in pend : w.b = x /\ w.v = sCon[q][e][x] /\ w.cutAt = e
      BY <1>u DEF I_Legal, PendVal
    <3>1. \A e \in sPres'[q] : e \in sPres[q] BY <1>sP
    <3>2. CASE q = r BY <3>1, <3>2, <2>r, <1>0, <1>T DEF I_Legal
    <3>3. CASE q # r BY <3>1, <3>3, <1>m, <1>0, <1>T DEF I_Legal
    <3>. QED BY <3>2, <3>3
  <2>3. I_PendSeen'
    <3>. SUFFICES ASSUME NEW w \in pend,
                        \E q \in R : head'[q][w.b] = w.v \/ \E e \in sPres'[q] : sCon[q][e][w.b] = w.v
                 PROVE  w.landed # {}
      BY <1>u DEF I_PendSeen
    <3>1. w.b \in Blocks /\ w.v >= 1 BY <1>0, <1>T, Nats
    <3>2. head'[r][w.b] = w.v => sCon[r][sb[r]][w.b] = w.v /\ sb[r] \in sPres[r] BY <3>1, <1>bv, <1>n
    <3>3. \E q \in R : head[q][w.b] = w.v \/ \E e \in sPres[q] : sCon[q][e][w.b] = w.v
      BY <3>2, <1>m, <1>sP
    <3>. QED BY <3>3, <1>0, <1>T DEF I_PendSeen
  <2>4. I_Vers'
    <3>. SUFFICES ASSUME NEW q \in R, NEW x \in Blocks
                 PROVE  head'[q][x] <= ver /\ \A e \in sPres'[q] : sCon[q][e][x] <= ver
      BY <1>u, <1>0, <1>T DEF I_Vers
    <3>1. CASE q = r
      <4>1. base[x] = 0 \/ (sb[r] \in sPres[r] /\ base[x] = sCon[r][sb[r]][x]) BY <1>bv
      <4>2. ver \in Nat BY <1>0, <1>T, Nats
      <4>3. base[x] <= ver BY <4>1, <4>2, <1>0 DEF I_Vers
      <4>4. \A e \in sPres'[r] : sCon[r][e][x] <= ver BY <1>sP, <1>0 DEF I_Vers
      <4>. QED BY <3>1, <4>3, <4>4, <1>n
    <3>2. CASE q # r BY <3>2, <1>m, <1>sP, <1>0, <1>T DEF I_Vers
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <1>0, <1>T, <1>u, <2>2, <2>3, <2>4, F_AckedEver
<1>7. I_WHi' BY <1>0, <1>T, <1>u, <1>sP DEF I_WHi
<1>8. Inv_TrustComplete' BY <1>0, <1>T, <1>u, <1>sP DEF Inv_TrustComplete
<1>9. I_HeadHi' /\ I_HeadLo'
  <2>1. CatchingUp(r)' /\ HiOf(r)' = sb[r] BY <1>n, <1>g, <1>u DEF CatchingUp, HiOf
  <2>2. \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) BY <1>m, <1>u DEF CatchingUp, HiOf
  <2>3. \A q2 \in R : \A e2 \in sPres'[q2] : e2 > sb[r] => \A x \in Blocks : base[x] <= cutAck'[q2][e2][x]
    <3>. SUFFICES ASSUME NEW q2 \in R, NEW e2 \in sPres'[q2], e2 > sb[r], NEW x \in Blocks
                 PROVE  base[x] <= cutAck'[q2][e2][x]
      OBVIOUS
    <3>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <1>sP
    <3>2. e2 \in Epochs BY <3>1, <1>0, <1>T, Groups DEF TySnap
    <3>3. CASE sb[r] = 0 BY <3>3, <3>1, <3>2, <1>b, <1>0, <1>T, Nats DEF Up
    <3>4. CASE sb[r] # 0
      <4>1. sb[r] \in sPres[r] /\ base[x] = sCon[r][sb[r]][x] BY <3>4, <1>bv, <1>g, <1>t, <1>b DEF Up
      <4>. QED BY <4>1, <3>1, <1>0, <1>T DEF I_WHi
    <3>. QED BY <3>3, <3>4
  <2>4. \A q \in R : \A p \in sPres'[q] : p < sb[r] => \A x \in Blocks : sCon[q][p][x] <= base[x]
    <3>. SUFFICES ASSUME NEW q \in R, NEW p \in sPres'[q], p < sb[r], NEW x \in Blocks
                 PROVE  sCon[q][p][x] <= base[x]
      OBVIOUS
    <3>2. p \in sPres[q] /\ p \in Epochs BY <1>sP, <1>0, <1>T, Groups DEF TySnap
    <3>0. sb[r] > 0 BY <3>2, <1>t DEF Epochs
    <3>1. sb[r] > 0 /\ sb[r] \in sPres[r] /\ sb[r] \in tr[r] BY <3>0, <1>g, <1>t, <1>so DEF SessionOK
    <3>3. sCon[q][p][x] <= cutAck[r][sb[r]][x] BY <3>1, <3>2, <1>0, <1>T DEF I_WHi
    <3>4. cutAck[r][sb[r]][x] <= sCon[r][sb[r]][x] BY <3>1, <1>0, <1>T DEF Inv_TrustComplete
    <3>5. base[x] = sCon[r][sb[r]][x] BY <3>1, <1>b DEF Up
    <3>6. sCon[q][p][x] \in Nat /\ cutAck[r][sb[r]][x] \in Nat /\ sCon[r][sb[r]][x] \in Nat
      BY <3>1, <3>2, <1>0, <1>T, Nats, Groups DEF TySnap
    <3>. QED BY <3>3, <3>4, <3>5, <3>6
  <2>5. I_HeadHi'
    <3>1. \A q \in R \ {r} : CatchingUp(q)' => \A q2 \in R : \A e2 \in sPres'[q2] : e2 > HiOf(q)' =>
            \A x \in Blocks : head'[q][x] <= cutAck'[q2][e2][x]
      BY <1>0, <1>T, <1>m, <1>sP, <2>2 DEF I_HeadHi
    <3>. QED BY <3>1, <2>1, <2>3, <1>n DEF I_HeadHi
  <2>6. I_HeadLo'
    <3>1. \A q \in R \ {r} : CatchingUp(q)' => \A q1 \in R : \A p \in sPres'[q1] : p < HiOf(q)' =>
            \A x \in Blocks : sCon'[q1][p][x] <= head'[q][x]
      BY <1>0, <1>T, <1>m, <1>sP, <1>u, <2>2 DEF I_HeadLo
    <3>. QED BY <3>1, <2>1, <2>4, <1>n, <1>u DEF I_HeadLo
  <2>. QED BY <2>5, <2>6
<1>10. I_Reverted'
  <2>1. \A q \in R \ {r} : RevOK(q)' BY <1>0, <1>T, <1>m, <1>u, RevOther DEF I_Reverted
  <2>2. RevOK(r)'
    <3>1. head'[r] = (IF reverted'[r] = 0 THEN Zero ELSE sCon'[r][reverted'[r]])
      BY <1>n, <1>u, <1>t, <1>g DEF base
    <3>. QED BY <3>1, <1>n, <1>u, <1>k, <1>g DEF RevOK
  <2>. QED BY <2>1, <2>2 DEF I_Reverted
<1>11. I_HeadPend'
  <2>1. \A w \in pend : base[w.b] = w.v => w.cutAt <= sb[r]
    <3>. SUFFICES ASSUME NEW w \in pend, base[w.b] = w.v PROVE w.cutAt <= sb[r] OBVIOUS
    <3>1. w.v > acked[w.b] /\ w.v >= 1 /\ w.b \in Blocks BY <1>0, <1>T, Nats DEF I_Pend
    <3>2. sb[r] \in sPres[r] /\ sCon[r][sb[r]][w.b] = w.v BY <3>1, <1>bv
    <3>a. sCon[r][sb[r]][w.b] <= acked[w.b] \/ \E u \in pend : u.b = w.b /\ u.v = sCon[r][sb[r]][w.b] /\ u.cutAt = sb[r]
      BY <3>1, <3>2, <1>0 DEF I_Legal
    <3>b. acked[w.b] \in Nat /\ w.v \in Nat BY <3>1, <1>0, Nats
    <3>3. \E u \in pend : u.b = w.b /\ u.v = w.v /\ u.cutAt = sb[r]
      BY <3>a, <3>b, <3>1, <3>2
    <3>4. PICK u \in pend : u.b = w.b /\ u.v = w.v /\ u.cutAt = sb[r] BY <3>3
    <3>5. u = w BY <3>4, <1>0, <1>T DEF I_Pend
    <3>6. sb[r] \in Int BY <1>t
    <3>. QED BY <3>4, <3>5, <3>6
  <2>2. \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) BY <1>m, <1>u DEF CatchingUp, HiOf
  <2>3. HiOf(r)' = sb[r] BY <1>n, <1>u, <1>g DEF HiOf
  <2>. QED BY <1>0, <1>T, <1>m, <1>n, <1>u, <2>1, <2>2, <2>3 DEF I_HeadPend
<1>12. I_Session'
  <2>0. cutGone \subseteq cutGone' BY <1>p
  <2>1. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>m, <1>u, <2>0, SessionOtherG DEF I_Session
  <2>2. SessionOK(r)'
    <3>1. cutK # 0 /\ r \in cutTgt => r \in cutDone \cup cutGone' BY <1>p
    <3>2. nextSeq \notin keep
      <4>1. sb[r] < nextSeq BY <1>so, <1>g DEF SessionOK
      <4>2. \A f \in recorded : f <= nextSeq - 1 BY <1>0 DEF I_Ranges
      <4>3. keep \subseteq Nat BY <1>k, <1>T
      <4>4. \A e \in keep : e < nextSeq BY <4>1, <4>2, <4>3, <1>k, <1>t, <1>T
      <4>. QED BY <4>4, <1>T
    <3>. QED BY <1>so, <1>n, <1>u, <1>g, <3>1, <3>2 DEF SessionOK, CatchingUp
  <2>. QED BY <2>1, <2>2 DEF I_Session
<1>13. I_Fresh'
  <2>1. Bases' = Bases /\ r \notin Bases BY <1>u, <1>s DEF Bases, InSyncR
  <2>. SUFFICES ASSUME NEW q \in Bases, NEW c \in sPres'[q], c >= nextSeq \/ ~conf[c], NEW x \in Blocks
               PROVE  sCon[q][c][x] >= cutAck'[q][c][x]
    BY <2>1, <1>u DEF I_Fresh
  <2>2. q \in R \ {r} BY <2>1, <1>T
  <2>3. c \in sPres[q] /\ cutAck'[q] = cutAck[q] BY <2>2, <1>m
  <2>. QED BY <2>3, <1>0 DEF I_Fresh
<1>. QED BY <1>0, <1>T, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13,
            <1>u, IndSplitP

-----------------------------------------------------------------------------
(* The scheduler's cut of one leg.                                         *)

LEMMA I_CutOne ==
  ASSUME IndInv, NEW r \in R, CutOne(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, recorded, nextSeq, cutK, cutTgt, cutGone, members, attached>>
      /\ UNCHANGED <<path, dState, mark, reverted, sb, pin, pend, acked, fails, riskRec>>
      /\ UNCHANGED <<badAdmit, badLoss, ackedEver, restamp, zx, cfgAt, conf, htr>>
  BY DEF CutOne
<1>p. /\ cutK # 0 /\ r \in cutTgt \ cutDone /\ r \notin cutGone
      /\ cutDone' = cutDone \cup {r} /\ tr' = [tr EXCEPT ![r] = @ \ {cutK}]
  BY Clean DEF CutOne
<1>N. /\ nextSeq \in Nat /\ cutK \in Nat
      /\ \A w \in pend : w.cutAt \in Nat /\ w.b \in Blocks /\ w.v \in Nat
      /\ \A q \in R : sPres[q] \subseteq Epochs /\ Epochs \subseteq Nat
  BY <1>0, <1>T, Nats, Const, Groups DEF TySnap, Epochs
<1>k. /\ cutK = nextSeq /\ cutK \in Epochs /\ attached
      /\ \A w \in pend : w.cutAt = cutK
  <2>1. cutK = nextSeq /\ attached BY <1>p, <1>0, <1>T DEF I_Ranges
  <2>2. \A w \in pend : w.cutAt >= cutK /\ w.cutAt <= nextSeq BY <1>p, <1>0, <1>T DEF I_Pend
  <2>3. cutK \in Epochs BY <1>p, <1>0, <1>T, Const, Groups DEF TyScalar, Epochs
  <2>. QED BY <2>1, <2>2, <2>3, <1>N
<1>c. ~CatchingUp(r)
  <2>1. SessionOK(r) BY <1>0, <1>T DEF I_Session
  <2>. QED BY <2>1, <1>p DEF SessionOK
<1>h. \A q \in R : CatchingUp(q) => HiOf(q) < nextSeq /\ HiOf(q) \in Nat
  <2>. SUFFICES ASSUME NEW q \in R, CatchingUp(q) PROVE HiOf(q) < nextSeq /\ HiOf(q) \in Nat OBVIOUS
  <2>1. SessionOK(q) /\ RevOK(q) BY <1>0, <1>T DEF I_Session, I_Reverted
  <2>2. mark[q] \in 0..MaxEpoch /\ reverted[q] \in -1..MaxEpoch /\ sb[q] \in -1..MaxEpoch BY <1>0, <1>T, Groups DEF TyLeg
  <2>. QED BY <2>1, <2>2, <1>N DEF SessionOK, RevOK, CatchingUp, HiOf
<1>a. \A q \in R : \A e \in sPres[q] : e <= nextSeq BY <1>0, <1>T DEF I_Ranges
<1>tr. \A q \in R : tr'[q] \subseteq tr[q] /\ (q # r => tr'[q] = tr[q]) BY <1>p, <1>0, <1>T, Groups DEF TyTrust
<1>so. \A q \in R : SessionOK(q) BY <1>0, <1>T DEF I_Session
<1>ss. \A q \in R : dState[q] = "stale" /\ sb[q] > 0 => sb[q] # cutK /\ sb[q] < nextSeq
  BY <1>so, <1>k, <1>N DEF SessionOK
<1>1. TypeOK' BY T_CutOne DEF IndInv
<1>2. I_Pend' /\ I_Members' /\ I_Hold' /\ I_AckedEver' /\ I_HeadPend'
  BY <1>0, <1>T, <1>u, F_Pend, F_Members, F_Hold, F_AckedEver, F_HeadPend
<1>3. CASE cutK \in sPres[r]
  <2>u. UNCHANGED <<sPres, sCon, sAl, sPar, hPar, hAl, anc, cutAck>> BY <1>3 DEF CutOne
  <2>1. I_Lineage' /\ I_Chain' /\ I_Legal' /\ I_PendSeen' /\ I_Vers' /\ I_WHi' /\ I_HeadHi'
        /\ I_HeadLo' /\ I_Reverted'
    BY <1>0, <1>T, <1>u, <2>u, F_Lineage, F_Chain, F_Legal, F_PendSeen, F_Vers, F_WHi, F_HeadHi,
       F_HeadLo, F_Reverted
  <2>2. I_Ranges' BY <1>0, <1>T, <1>u, <2>u, <1>tr DEF I_Ranges
  <2>3. Inv_TrustComplete' BY <1>0, <1>T, <2>u, <1>tr DEF Inv_TrustComplete
  <2>4. I_Session'
    <3>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
    <3>1. SessionOK(q) BY <1>so
    <3>2. dState[q] = "stale" /\ sb[q] > 0 /\ reverted[q] = -1 /\ sb[q] \in sPres[q] => sb[q] \in tr'[q]
      BY <3>1, <1>ss, <1>p, <1>0, <1>T, Groups DEF SessionOK, TyTrust
    <3>. QED BY <3>1, <3>2, <1>u, <2>u, <1>p DEF SessionOK, CatchingUp
  <2>5. I_Fresh' BY <1>0, <1>T, <1>u, <2>u DEF I_Fresh, Bases, InSyncR
  <2>. QED BY <1>0, <1>T, <1>1, <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <1>u, IndSplitP
<1>4. CASE cutK \notin sPres[r]
  <2>s. /\ sPres' = [sPres EXCEPT ![r] = @ \cup {cutK}]
        /\ sCon' = [sCon EXCEPT ![r][cutK] = head[r]]
        /\ sAl' = [sAl EXCEPT ![r][cutK] = hAl[r]]
        /\ sPar' = [sPar EXCEPT ![r][cutK] = hPar[r]]
        /\ anc' = [anc EXCEPT ![r][cutK] = NewAnc(r, cutK)]
        /\ hPar' = [hPar EXCEPT ![r] = cutK] /\ hAl' = [hAl EXCEPT ![r] = {}]
        /\ cutAck' = [cutAck EXCEPT ![r][cutK] = acked]
    BY <1>4, Clean DEF CutOne
  <2>hp. hPar[r] \in 0..(cutK - 1) /\ (hPar[r] # 0 => hPar[r] \in sPres[r])
    <3>1. hPar[r] \in sPres[r] \cup {0} BY <1>0, <1>T DEF I_Ranges
    <3>2. hPar[r] \in 0..MaxEpoch BY <1>0, <1>T, Groups DEF TyHead
    <3>3. hPar[r] # cutK BY <3>1, <1>4, <1>p
    <3>. QED BY <3>1, <3>2, <3>3, <1>a, <1>k, <1>N
  <2>m. \A q \in R \ {r} : /\ sPres'[q] = sPres[q] /\ sCon'[q] = sCon[q] /\ sAl'[q] = sAl[q]
                            /\ sPar'[q] = sPar[q] /\ anc'[q] = anc[q] /\ hPar'[q] = hPar[q]
                            /\ hAl'[q] = hAl[q] /\ cutAck'[q] = cutAck[q]
    BY <2>s, <1>0, <1>T, Groups DEF TySnap, TyHead, TyTrust
  <2>o. \A e \in sPres[r] : /\ sCon'[r][e] = sCon[r][e] /\ sAl'[r][e] = sAl[r][e]
                             /\ sPar'[r][e] = sPar[r][e] /\ anc'[r][e] = anc[r][e]
                             /\ cutAck'[r][e] = cutAck[r][e]
    BY <2>s, <1>4, <1>k, <1>0, <1>T, Groups DEF TySnap, TyTrust
  <2>n. /\ sCon'[r][cutK] = head[r] /\ sAl'[r][cutK] = hAl[r] /\ sPar'[r][cutK] = hPar[r]
        /\ anc'[r][cutK] = NewAnc(r, cutK) /\ cutAck'[r][cutK] = acked /\ sPres'[r] = sPres[r] \cup {cutK}
        /\ hPar'[r] = cutK /\ hAl'[r] = {}
    BY <2>s, <1>k, <1>0, <1>T, Groups DEF TySnap, TyTrust, TyHead
  <2>na. NewAnc(r, cutK) = {cutK} \cup (IF hPar[r] = 0 THEN {} ELSE anc[r][hPar[r]]) BY DEF NewAnc, AncOf
  <2>1. I_Ranges'
    <3>0. cutK \in 1..nextSeq BY <1>k, <1>N DEF Epochs
    <3>a. sPres[r] \subseteq 1..nextSeq BY <1>0 DEF I_Ranges
    <3>1. sPres'[r] \subseteq 1..nextSeq /\ hPar'[r] \in sPres'[r] BY <3>0, <3>a, <2>n
    <3>. QED BY <1>0, <1>T, <1>u, <1>tr, <2>m, <2>n, <3>1 DEF I_Ranges
  <2>rd. \A x \in Blocks : x \notin sAl'[r][cutK] => sCon'[r][cutK][x] = Up(sPar[r], sCon[r], hPar[r], x)
    BY <2>n, <1>0, <1>T DEF I_Chain
  <2>LE. ChainShape(sPres'[r], sPar'[r], anc'[r]) /\ ChainReads(sPres'[r], sPar'[r], sAl'[r], sCon'[r])
    <3>1. anc'[r][cutK] = {cutK} \cup (IF hPar[r] = 0 THEN {} ELSE anc[r][hPar[r]]) BY <2>n, <2>na
    <3>2. hPar[r] < cutK BY <2>hp, <1>k, <1>N
    <3>. QED BY <1>0, <1>T, <1>k, <1>4, <2>n, <2>o, <2>rd, <3>1, <3>2, LegExtend
  <2>2. I_Lineage'
    <3>1. ChainShape(sPres'[r], sPar'[r], anc'[r]) BY <2>LE
    <3>2. \A e \in Epochs \ sPres'[r] : anc'[r][e] = {} BY <2>s, <2>n, <1>0, <1>T, Groups DEF TySnap, I_Lineage
    <3>. QED BY <1>0, <1>T, <2>m, <3>1, <3>2 DEF I_Lineage
  <2>3. I_Chain'
    <3>1. ChainReads(sPres'[r], sPar'[r], sAl'[r], sCon'[r])
      <4>1. ChainShape(sPres[r], sPar[r], anc[r]) /\ ChainReads(sPres[r], sPar[r], sAl[r], sCon[r])
        BY <1>0, <1>T, LegChain
      <4>. QED BY <2>LE
    <3>2. \A x \in Blocks : x \notin hAl'[r] => head'[r][x] = Up(sPar'[r], sCon'[r], hPar'[r], x)
      BY <2>n, <1>u, <1>k, <1>N DEF Up, Epochs
    <3>. QED BY <1>0, <1>T, <1>u, <2>m, <3>1, <3>2 DEF I_Chain
  <2>pv. \A x \in Blocks : \/ head[r][x] <= acked[x]
                           \/ \E w \in pend : w.b = x /\ w.v = head[r][x] /\ w.cutAt = cutK
    <3>. SUFFICES ASSUME NEW x \in Blocks, ~(head[r][x] <= acked[x])
                 PROVE  \E w \in pend : w.b = x /\ w.v = head[r][x] /\ w.cutAt = cutK
      OBVIOUS
    <3>1. head[r][x] \in PendVal(x) BY <1>0, <1>T DEF I_Legal
    <3>. QED BY <3>1, <1>k DEF PendVal
  <2>4. I_Legal' /\ I_PendSeen' /\ I_Vers'
    <3>1. I_Legal'
      <4>. SUFFICES ASSUME NEW q \in R, NEW x \in Blocks
                   PROVE  /\ head[q][x] \in ackedEver[x] \cup PendVal(x)
                          /\ head[q][x] <= acked[x] \/ head[q][x] \in PendVal(x)
                          /\ \A e \in sPres'[q] : /\ sCon'[q][e][x] \in ackedEver[x] \cup PendVal(x)
                                                 /\ \/ sCon'[q][e][x] <= acked[x]
                                                    \/ \E w \in pend : w.b = x /\ w.v = sCon'[q][e][x] /\ w.cutAt = e
        BY <1>u DEF I_Legal, PendVal
      <4>1. CASE q # r BY <4>1, <2>m, <1>0, <1>T DEF I_Legal
      <4>2. CASE q = r BY <4>2, <2>n, <2>o, <2>pv, <1>0, <1>T DEF I_Legal
      <4>. QED BY <4>1, <4>2
    <3>2. I_PendSeen' BY <1>0, <1>T, <1>u, <2>m, <2>n, <2>o DEF I_PendSeen
    <3>3. I_Vers' BY <1>0, <1>T, <1>u, <2>m, <2>n, <2>o DEF I_Vers
    <3>. QED BY <3>1, <3>2, <3>3
  <2>5. I_WHi'
    <3>1. \A q1 \in R : \A e1 \in sPres[q1] : e1 < cutK => \A x \in Blocks : sCon[q1][e1][x] <= acked[x]
      <4>. SUFFICES ASSUME NEW q1 \in R, NEW e1 \in sPres[q1], e1 < cutK, NEW x \in Blocks
                   PROVE  sCon[q1][e1][x] <= acked[x]
        OBVIOUS
      <4>1. \A w \in pend : w.cutAt # e1 BY <1>k, <1>N
      <4>. QED BY <4>1, <1>0, <1>T DEF I_Legal
    <3>2. \A q \in R : \A e \in sPres'[q] : e <= cutK
      <4>. SUFFICES ASSUME NEW q \in R, NEW e \in sPres'[q] PROVE e <= cutK OBVIOUS
      <4>1. CASE q = r BY <4>1, <2>n, <1>a, <1>k, <1>N
      <4>2. CASE q # r BY <4>2, <2>m, <1>a, <1>k
      <4>. QED BY <4>1, <4>2
    <3>. SUFFICES ASSUME NEW q1 \in R, NEW q2 \in R, NEW e1 \in sPres'[q1], NEW e2 \in sPres'[q2], e1 < e2,
                        NEW x \in Blocks
                 PROVE  sCon'[q1][e1][x] <= cutAck'[q2][e2][x]
      BY DEF I_WHi
    <3>3. CASE e2 = cutK /\ q2 = r
      <4>1. e1 \in sPres[q1] /\ sCon'[q1][e1] = sCon[q1][e1] BY <3>3, <2>m, <2>n, <2>o, <1>N
      <4>. QED BY <4>1, <3>1, <3>3, <2>n
    <3>4. CASE ~(e2 = cutK /\ q2 = r)
      <4>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <3>4, <2>m, <2>n, <2>o
      <4>2. e1 # cutK BY <4>1, <3>2, <1>N
      <4>3. e1 \in sPres[q1] /\ sCon'[q1][e1] = sCon[q1][e1] BY <4>2, <2>m, <2>n, <2>o
      <4>. QED BY <4>1, <4>3, <1>0, <1>T DEF I_WHi
    <3>. QED BY <3>3, <3>4
  <2>6. I_HeadHi' /\ I_HeadLo'
    <3>1. \A q \in R : CatchingUp(q) => \A x \in Blocks : head[q][x] <= acked[x]
      <4>. SUFFICES ASSUME NEW q \in R, CatchingUp(q), NEW x \in Blocks PROVE head[q][x] <= acked[x]
        OBVIOUS
      <4>1. \A w \in pend : head[q][x] = w.v /\ w.b = x => w.cutAt <= HiOf(q) BY <1>0, <1>T DEF I_HeadPend
      <4>2. ~(head[q][x] \in PendVal(x)) BY <4>1, <1>h, <1>k, <1>N DEF PendVal
      <4>. QED BY <4>2, <1>0, <1>T DEF I_Legal
    <3>2. \A q \in R : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) BY <1>u DEF CatchingUp, HiOf
    <3>3. \A q \in R : CatchingUp(q) => q # r BY <1>c
    <3>4. I_HeadHi'
      <4>. SUFFICES ASSUME NEW q \in R, CatchingUp(q), NEW q2 \in R, NEW e2 \in sPres'[q2], e2 > HiOf(q),
                          NEW x \in Blocks
                   PROVE  head[q][x] <= cutAck'[q2][e2][x]
        BY <3>2, <1>u DEF I_HeadHi
      <4>1. CASE e2 = cutK /\ q2 = r BY <4>1, <3>1, <2>n
      <4>2. CASE ~(e2 = cutK /\ q2 = r)
        <5>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <4>2, <2>m, <2>n, <2>o
        <5>. QED BY <5>1, <1>0, <1>T DEF I_HeadHi
      <4>. QED BY <4>1, <4>2
    <3>5. I_HeadLo'
      <4>. SUFFICES ASSUME NEW q \in R, CatchingUp(q), NEW q1 \in R, NEW p \in sPres'[q1], p < HiOf(q),
                          NEW x \in Blocks
                   PROVE  sCon'[q1][p][x] <= head[q][x]
        BY <3>2, <1>u DEF I_HeadLo
      <4>1. p # cutK \/ q1 # r BY <1>h, <1>k, <1>N
      <4>2. p \in sPres[q1] /\ sCon'[q1][p] = sCon[q1][p] BY <4>1, <2>m, <2>n, <2>o
      <4>. QED BY <4>2, <1>0, <1>T DEF I_HeadLo
    <3>. QED BY <3>4, <3>5
  <2>7. I_Reverted'
    <3>1. \A q \in R \ {r} : RevOK(q)' BY <1>0, <1>T, <1>u, <2>m, RevOther DEF I_Reverted
    <3>2. RevOK(r)' BY <1>c, <1>u DEF RevOK, CatchingUp
    <3>. QED BY <3>1, <3>2 DEF I_Reverted
  <2>8. Inv_TrustComplete'
    <3>. SUFFICES ASSUME NEW q \in R, NEW e \in tr'[q], NEW x \in Blocks
                 PROVE  sCon'[q][e][x] >= cutAck'[q][e][x]
      BY DEF Inv_TrustComplete
    <3>1. e \in tr[q] /\ (q = r => e # cutK) BY <1>p, <1>tr, <1>0, <1>T, Groups DEF TyTrust
    <3>2. e \in sPres[q] BY <3>1, <1>0, <1>T DEF I_Ranges
    <3>3. sCon'[q][e] = sCon[q][e] /\ cutAck'[q][e] = cutAck[q][e] BY <3>1, <3>2, <2>m, <2>o
    <3>. QED BY <3>1, <3>3, <1>0, <1>T DEF Inv_TrustComplete
  <2>9. I_Session'
    <3>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
    <3>1. SessionOK(q) BY <1>so
    <3>2. dState[q] = "stale" /\ sb[q] > 0 /\ reverted[q] = -1 /\ sb[q] \in sPres'[q] => sb[q] \in tr'[q] /\ sb[q] \in sPres[q]
      BY <3>1, <1>ss, <1>p, <2>m, <2>n, <1>0, <1>T, Groups DEF SessionOK, TyTrust
    <3>3. CASE q = r BY <3>1, <3>2, <3>3, <1>c, <1>u, <1>p, <2>n DEF SessionOK, CatchingUp
    <3>4. CASE q # r BY <3>1, <3>2, <3>4, <1>u, <1>p, <2>m, <1>tr DEF SessionOK, CatchingUp
    <3>. QED BY <3>3, <3>4
  <2>10. I_Fresh'
    <3>. SUFFICES ASSUME NEW q \in Bases, NEW e \in sPres'[q], e >= nextSeq \/ ~conf[e], NEW x \in Blocks
                 PROVE  sCon'[q][e][x] >= cutAck'[q][e][x]
      BY <1>u DEF I_Fresh, Bases, InSyncR
    <3>1. CASE q = r /\ e = cutK
      <4>1. head[r][x] >= acked[x] BY <3>1, <1>0, <1>T, <1>k DEF I_Hold, Bases
      <4>. QED BY <4>1, <2>n, <3>1
    <3>2. CASE ~(q = r /\ e = cutK)
      <4>0. q \in R BY <1>T
      <4>1. CASE q = r
        <5>1. e \in sPres[r] BY <4>1, <3>2, <2>n
        <5>. QED BY <5>1, <4>1, <2>o, <1>0 DEF I_Fresh
      <4>2. CASE q # r BY <4>0, <4>2, <2>m, <1>0 DEF I_Fresh
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <1>0, <1>T, <1>1, <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <1>u, IndSplitP
<1>. QED BY <1>3, <1>4

LEMMA I_CutRecord ==
  ASSUME IndInv, CutRecord PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, members, attached>>
      /\ UNCHANGED <<path, dState, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss>>
      /\ UNCHANGED <<ackedEver, restamp, zx, tr, htr, cutAck>>
  BY DEF CutRecord
<1>p. /\ cutK # 0 /\ cutDone = cutTgt
      /\ recorded' = recorded \cup {cutK} /\ nextSeq' = nextSeq + 1
      /\ mark' = [q \in R |-> IF q \in cutTgt /\ dState[q] = "insync" THEN cutK ELSE mark[q]]
      /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
      /\ cfgAt' = [cfgAt EXCEPT ![cutK] = cutTgt \cap members]
      /\ conf' = [conf EXCEPT ![cutK] = FALSE]
  BY Clean DEF CutRecord
<1>k. cutK = nextSeq /\ cutK \in Epochs /\ attached /\ nextSeq \in Nat
  BY <1>p, <1>0, <1>T, Const, Groups DEF I_Ranges, TyScalar, Epochs
<1>c. \A q \in R : CatchingUp(q) => mark'[q] = mark[q] BY <1>p DEF CatchingUp
<1>h. \A q \in R : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) BY <1>u, <1>c DEF CatchingUp, HiOf
<1>1. TypeOK' BY <1>0, <1>T, T_CutSmall DEF I_Ranges
<1>2. I_Lineage' /\ I_Chain' /\ I_Members' /\ I_Hold' /\ I_Legal' /\ I_PendSeen' /\ I_AckedEver'
      /\ I_Vers' /\ I_WHi' /\ Inv_TrustComplete'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Members, F_Hold, F_Legal, F_PendSeen, F_AckedEver, F_Vers,
     F_WHi, F_TC
<1>3. I_Ranges'
  <2>1. \A e \in Epochs : ~conf'[e] => attached /\ e < nextSeq'
    BY <1>p, <1>k, <1>0, <1>T, Groups DEF I_Ranges, TyTrust
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <1>k, <2>1 DEF I_Ranges
<1>4. I_Pend'
  <2>1. \A w \in pend : w.cutAt >= cutK /\ w.cutAt <= nextSeq /\ w.cutAt \in Nat
    BY <1>0, <1>T, <1>p, Nats DEF I_Pend
  <2>2. \A w \in pend : w.cutAt <= nextSeq' /\ w.cutAt >= nextSeq' - 1 BY <2>1, <1>p, <1>k
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>2 DEF I_Pend
<1>5. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend' BY <1>0, <1>T, <1>u, <1>h DEF I_HeadHi, I_HeadLo, I_HeadPend
<1>6. I_Reverted'
  <2>. SUFFICES ASSUME NEW q \in R PROVE RevOK(q)' BY DEF I_Reverted
  <2>0. RevOK(q) /\ SessionOK(q) BY <1>0, <1>T DEF I_Reverted, I_Session
  <2>1. CASE dState[q] = "stale" /\ reverted[q] # -1
    <3>1. cutK \notin sPres[q] BY <2>0, <2>1, <1>k DEF SessionOK, CatchingUp
    <3>2. \A e \in sPres[q] : e < cutK BY <3>1, <1>0, <1>T, <1>k, Groups DEF I_Ranges, TySnap, Epochs
    <3>. QED BY <2>0, <2>1, <3>2, <1>u, <1>p DEF RevOK
  <2>2. CASE ~(dState[q] = "stale" /\ reverted[q] # -1) BY <2>2, <1>u DEF RevOK
  <2>. QED BY <2>1, <2>2
<1>7. I_Session'
  <2>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
  <2>0. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>1. CatchingUp(q) => \A e \in sPres[q] : e < cutK
    <3>1. CatchingUp(q) => cutK \notin sPres[q] BY <2>0, <1>k DEF SessionOK
    <3>. QED BY <3>1, <1>0, <1>T, <1>k, Groups DEF I_Ranges, TySnap, Epochs
  <2>2. CatchingUp(q) => nextSeq' \notin sPres[q] BY <1>0, <1>T, <1>k, <1>p DEF I_Ranges
  <2>3. dState[q] = "standby" => mark'[q] = mark[q] BY <1>p
  <2>4. mark[q] \in Nat /\ sb[q] \in Int BY <1>0, <1>T, Groups DEF TyLeg
  <2>. QED BY <2>0, <2>1, <2>2, <2>3, <2>4, <1>u, <1>p, <1>k DEF SessionOK, CatchingUp
<1>8. I_Fresh'
  <2>1. \A e \in Epochs : (e >= nextSeq' \/ ~conf'[e]) => (e >= nextSeq \/ ~conf[e])
    BY <1>p, <1>k, <1>0, <1>T, Groups DEF TyTrust, Epochs
  <2>. QED BY <1>0, <1>T, <1>u, <2>1 DEF I_Fresh, Bases, InSyncR
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>u, <1>0, <1>T, IndSplitP

LEMMA I_GC ==
  ASSUME IndInv, NEW r \in R, NEW e \in Epochs, GC(r, e) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>. DEFINE kids == Children(r, e)
<1>u. /\ UNCHANGED <<ver, head, sCon, recorded, nextSeq, cutK, cutTgt, cutDone, cutGone>>
      /\ UNCHANGED <<members, attached, path, dState, mark, reverted, sb, pin, pend, acked>>
      /\ UNCHANGED <<fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx, cfgAt, conf, htr>>
  BY DEF GC
<1>g. /\ recorded # {} /\ e \in sPres[r] /\ e < Min(recorded)
      /\ Cardinality(kids) + (IF hPar[r] = e THEN 1 ELSE 0) <= 1
  BY DEF GC
<1>t. /\ recorded \subseteq Nat /\ Min(recorded) \in recorded /\ \A f \in recorded : Min(recorded) <= f
      /\ \A f \in recorded : e < f
  <2>1. recorded \subseteq Nat BY <1>0, <1>T, Groups DEF TySets, Epochs
  <2>2. Min(recorded) \in recorded /\ \A f \in recorded : Min(recorded) <= f BY <2>1, <1>g, MinProp
  <2>. QED BY <2>1, <2>2, <1>g DEF Epochs
<1>c. ChainShape(sPres[r], sPar[r], anc[r]) /\ ChainReads(sPres[r], sPar[r], sAl[r], sCon[r])
  BY <1>0, <1>T, LegChain
<1>ab. \A c \in sPres[r] : \A f \in anc[r][c] : f <= c BY <1>c, AncBound
<1>sh. /\ sPres[r] \subseteq Epochs /\ Epochs \subseteq Nat
       /\ \A c \in sPres[r] : sPar[r][c] \in 0..MaxEpoch /\ sPar[r][c] < c /\ (sPar[r][c] # 0 => sPar[r][c] \in sPres[r])
       /\ \A c \in sPres[r] : anc[r][c] = {c} \cup (IF sPar[r][c] = 0 THEN {} ELSE anc[r][sPar[r][c]])
       /\ \A c \in sPres[r] : \A f \in anc[r][c] : f <= c
       /\ \A c \in Epochs \ sPres[r] : anc[r][c] = {}
  <2>1. sPres[r] \subseteq Epochs /\ Epochs \subseteq Nat BY <1>T
  <2>2. \A c \in sPres[r] : sPar[r][c] \in 0..MaxEpoch /\ sPar[r][c] < c /\ (sPar[r][c] # 0 => sPar[r][c] \in sPres[r])
    BY <1>c DEF ChainShape
  <2>3. \A c \in sPres[r] : anc[r][c] = {c} \cup (IF sPar[r][c] = 0 THEN {} ELSE anc[r][sPar[r][c]])
    BY <1>c DEF ChainShape
  <2>4. \A c \in Epochs \ sPres[r] : anc[r][c] = {} BY <1>0 DEF I_Lineage
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <1>ab
<1>kf. /\ kids = {c \in sPres[r] : sPar[r][c] = e}
       /\ hPar[r] = e => kids = {}
  <2>1. kids = {c \in sPres[r] : sPar[r][c] = e} BY DEF Children
  <2>2. IsFiniteSet(kids)
    <3>1. IsFiniteSet(1..MaxEpoch) BY Const, FS_Interval
    <3>. QED BY <3>1, <2>1, <1>sh, FS_Subset DEF Epochs
  <2>3. Cardinality(kids) \in Nat BY <2>2, FS_CardinalityType
  <2>4. hPar[r] = e => Cardinality(kids) = 0 BY <1>g, <2>3
  <2>. QED BY <2>1, <2>2, <2>4, FS_EmptySet
<1>p. /\ sPres' = [sPres EXCEPT ![r] = @ \ {e}]
      /\ anc' = [anc EXCEPT ![r] = [c \in Epochs |-> IF c = e THEN {} ELSE anc[r][c] \ {e}]]
      /\ tr' = [tr EXCEPT ![r] = @ \ {e}]
      /\ cutAck' = [cutAck EXCEPT ![r][e] = Zero]
      /\ IF hPar[r] = e
           THEN /\ hAl' = [hAl EXCEPT ![r] = @ \cup sAl[r][e]]
                /\ hPar' = [hPar EXCEPT ![r] = sPar[r][e]]
                /\ UNCHANGED <<sAl, sPar>>
           ELSE /\ sAl' = [sAl EXCEPT ![r] = [c \in Epochs |->
                            IF c \in kids THEN sAl[r][c] \cup sAl[r][e] ELSE sAl[r][c]]]
                /\ sPar' = [sPar EXCEPT ![r] = [c \in Epochs |->
                            IF c \in kids THEN sPar[r][e] ELSE sPar[r][c]]]
                /\ UNCHANGED <<hAl, hPar>>
  BY Clean DEF GC
<1>m. \A q \in R \ {r} : /\ sPres'[q] = sPres[q] /\ anc'[q] = anc[q] /\ tr'[q] = tr[q]
                          /\ cutAck'[q] = cutAck[q] /\ hPar'[q] = hPar[q] /\ hAl'[q] = hAl[q]
                          /\ sAl'[q] = sAl[q] /\ sPar'[q] = sPar[q]
  BY <1>p, <1>0, <1>T, Groups DEF TySnap, TyTrust, TyHead
<1>n. /\ sPres'[r] = sPres[r] \ {e} /\ tr'[r] = tr[r] \ {e}
      /\ \A c \in Epochs : anc'[r][c] = IF c = e THEN {} ELSE anc[r][c] \ {e}
      /\ \A c \in sPres'[r] : cutAck'[r][c] = cutAck[r][c]
  BY <1>p, <1>0, <1>T, Groups DEF TySnap, TyTrust
<1>hk. hPar[r] = e =>
         /\ hPar'[r] = sPar[r][e] /\ hAl'[r] = hAl[r] \cup sAl[r][e]
         /\ \A c \in sPres'[r] : sPar'[r][c] = sPar[r][c] /\ sAl'[r][c] = sAl[r][c]
  BY <1>p, <1>0, <1>T, Groups DEF TyHead, TySnap
<1>nk. hPar[r] # e =>
         /\ hPar'[r] = hPar[r] /\ hAl'[r] = hAl[r]
         /\ \A c \in sPres'[r] : sPar'[r][c] = IF sPar[r][c] = e THEN sPar[r][e] ELSE sPar[r][c]
         /\ \A c \in sPres'[r] : sAl'[r][c] = IF sPar[r][c] = e THEN sAl[r][c] \cup sAl[r][e] ELSE sAl[r][c]
  BY <1>p, <1>kf, <1>n, <1>0, <1>T, Groups DEF TyHead, TySnap
<1>sP. /\ \A q \in R : sPres'[q] \subseteq sPres[q] /\ tr'[q] \subseteq tr[q] /\ tr'[q] \subseteq sPres'[q]
       /\ \A q \in R : \A c \in sPres'[q] : cutAck'[q][c] = cutAck[q][c]
  <2>1. \A q \in R : tr[q] \subseteq sPres[q] BY <1>0, <1>T DEF I_Ranges
  <2>. QED BY <1>m, <1>n, <2>1
<1>1. TypeOK' BY T_GC DEF IndInv
<1>2. I_Pend' /\ I_Members' /\ I_Hold' /\ I_AckedEver' /\ I_HeadPend'
  BY <1>0, <1>T, <1>u, F_Pend, F_Members, F_Hold, F_AckedEver, F_HeadPend
<1>3. I_Lineage'
  <2>1. ChainShape(sPres'[r], sPar'[r], anc'[r])
    <3>1. sPres'[r] \subseteq Epochs BY <1>n, <1>sh
    <3>2. \A c \in sPres'[r] : sPar'[r][c] \in 0..MaxEpoch /\ sPar'[r][c] < c
                               /\ (sPar'[r][c] # 0 => sPar'[r][c] \in sPres'[r])
      <4>. SUFFICES ASSUME NEW c \in sPres'[r]
                   PROVE  sPar'[r][c] \in 0..MaxEpoch /\ sPar'[r][c] < c /\ (sPar'[r][c] # 0 => sPar'[r][c] \in sPres'[r])
        OBVIOUS
      <4>1. c \in sPres[r] /\ c # e BY <1>n
      <4>2. CASE hPar[r] = e
        <5>1. sPar[r][c] # e BY <4>1, <4>2, <1>kf
        <5>. QED BY <5>1, <4>1, <1>hk, <4>2, <1>sh, <1>n
      <4>3. CASE hPar[r] # e
        <5>1. CASE sPar[r][c] = e BY <5>1, <4>1, <4>3, <1>nk, <1>sh, <1>g, <1>n
        <5>2. CASE sPar[r][c] # e BY <5>2, <4>1, <4>3, <1>nk, <1>sh, <1>n
        <5>. QED BY <5>1, <5>2
      <4>. QED BY <4>2, <4>3
    <3>3. \A c \in sPres'[r] : anc'[r][c] = {c} \cup (IF sPar'[r][c] = 0 THEN {} ELSE anc'[r][sPar'[r][c]])
      <4>. SUFFICES ASSUME NEW c \in sPres'[r]
                   PROVE  anc'[r][c] = {c} \cup (IF sPar'[r][c] = 0 THEN {} ELSE anc'[r][sPar'[r][c]])
        OBVIOUS
      <4>1. c \in sPres[r] /\ c # e /\ c \in Epochs BY <1>n, <1>sh
      <4>2. anc'[r][c] = anc[r][c] \ {e} BY <4>1, <1>n
      <4>3. \A f \in sPres[r] : sPar[r][f] # 0 /\ sPar[r][f] # e => anc'[r][sPar[r][f]] = anc[r][sPar[r][f]] \ {e}
        BY <1>n, <1>sh
      <4>4. CASE sPar[r][c] = e
        <5>1. hPar[r] # e /\ sPar'[r][c] = sPar[r][e] BY <4>4, <4>1, <1>kf, <1>nk
        <5>2. anc[r][c] = {c} \cup anc[r][e] /\ anc[r][e] = {e} \cup (IF sPar[r][e] = 0 THEN {} ELSE anc[r][sPar[r][e]])
          BY <4>4, <4>1, <1>sh, <1>g
        <5>3. sPar[r][e] # 0 => e \notin anc[r][sPar[r][e]] /\ sPar[r][e] # e /\ sPar[r][e] \in sPres[r]
          BY <1>sh, <1>g
        <5>4. sPar[r][e] # 0 => anc'[r][sPar[r][e]] = anc[r][sPar[r][e]] \ {e} BY <5>3, <1>n, <1>sh
        <5>. QED BY <4>2, <5>1, <5>2, <5>3, <5>4, <4>1
      <4>5. CASE sPar[r][c] # e
        <5>1. sPar'[r][c] = sPar[r][c] BY <4>5, <4>1, <1>hk, <1>nk, <1>n
        <5>2. anc[r][c] = {c} \cup (IF sPar[r][c] = 0 THEN {} ELSE anc[r][sPar[r][c]]) BY <4>1, <1>sh
        <5>. QED BY <4>2, <4>3, <4>5, <5>1, <5>2, <4>1
      <4>. QED BY <4>4, <4>5
    <3>. QED BY <3>1, <3>2, <3>3 DEF ChainShape
  <2>2. \A c \in Epochs \ sPres'[r] : anc'[r][c] = {} BY <1>n, <1>sh
  <2>. QED BY <1>0, <1>T, <1>m, <2>1, <2>2 DEF I_Lineage
<1>4. I_Chain'
  <2>1. ChainReads(sPres'[r], sPar'[r], sAl'[r], sCon'[r])
    <3>. SUFFICES ASSUME NEW c \in sPres'[r], NEW x \in Blocks, x \notin sAl'[r][c]
                 PROVE  sCon[r][c][x] = Up(sPar'[r], sCon[r], sPar'[r][c], x)
      BY <1>u DEF ChainReads
    <3>1. c \in sPres[r] /\ c # e BY <1>n
    <3>2. CASE hPar[r] # e /\ sPar[r][c] = e
      <4>1. x \notin sAl[r][c] /\ x \notin sAl[r][e] /\ sPar'[r][c] = sPar[r][e] BY <3>2, <3>1, <1>nk, <1>n
      <4>2. sCon[r][c][x] = sCon[r][e][x] BY <4>1, <3>1, <3>2, <1>c, <1>g DEF ChainReads, Up, Epochs
      <4>3. sCon[r][e][x] = Up(sPar[r], sCon[r], sPar[r][e], x) BY <4>1, <1>c, <1>g DEF ChainReads
      <4>. QED BY <4>1, <4>2, <4>3 DEF Up
    <3>3. CASE ~(hPar[r] # e /\ sPar[r][c] = e)
      <4>1. sPar[r][c] # e BY <3>3, <3>1, <1>kf
      <4>2. sPar'[r][c] = sPar[r][c] /\ sAl'[r][c] = sAl[r][c] BY <4>1, <3>1, <1>hk, <1>nk, <1>n
      <4>. QED BY <4>2, <3>1, <1>c DEF ChainReads, Up
    <3>. QED BY <3>2, <3>3
  <2>2. \A x \in Blocks : x \notin hAl'[r] => head[r][x] = Up(sPar'[r], sCon[r], hPar'[r], x)
    <3>. SUFFICES ASSUME NEW x \in Blocks, x \notin hAl'[r] PROVE head[r][x] = Up(sPar'[r], sCon[r], hPar'[r], x)
      OBVIOUS
    <3>0. head[r][x] = Up(sPar[r], sCon[r], hPar[r], x) \/ x \in hAl[r] BY <1>0, <1>T DEF I_Chain
    <3>1. CASE hPar[r] = e
      <4>1. x \notin hAl[r] /\ x \notin sAl[r][e] /\ hPar'[r] = sPar[r][e] BY <3>1, <1>hk
      <4>2. head[r][x] = sCon[r][e][x] BY <3>0, <4>1, <3>1, <1>g DEF Up, Epochs
      <4>. QED BY <4>1, <4>2, <1>c, <1>g DEF ChainReads, Up
    <3>2. CASE hPar[r] # e BY <3>0, <3>2, <1>nk DEF Up
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <1>0, <1>T, <1>m, <1>u, <2>1, <2>2 DEF I_Chain
<1>5. I_Ranges'
  <2>1. hPar'[r] \in sPres'[r] \cup {0}
    <3>1. CASE hPar[r] = e BY <3>1, <1>hk, <1>sh, <1>g, <1>n
    <3>2. CASE hPar[r] # e BY <3>2, <1>nk, <1>n, <1>0, <1>T DEF I_Ranges
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <1>0, <1>T, <1>u, <1>m, <1>sP, <2>1 DEF I_Ranges
<1>6. I_Legal' /\ I_PendSeen' /\ I_Vers' /\ I_WHi' /\ Inv_TrustComplete' /\ I_Fresh'
  <2>1. I_Legal' BY <1>0, <1>T, <1>u, <1>sP DEF I_Legal, PendVal
  <2>2. I_PendSeen' BY <1>0, <1>T, <1>u, <1>sP DEF I_PendSeen
  <2>3. I_Vers' BY <1>0, <1>T, <1>u, <1>sP DEF I_Vers
  <2>4. I_WHi' BY <1>0, <1>T, <1>u, <1>sP DEF I_WHi
  <2>5. Inv_TrustComplete'
    <3>. SUFFICES ASSUME NEW q \in R, NEW c \in tr'[q], NEW x \in Blocks PROVE sCon[q][c][x] >= cutAck'[q][c][x]
      BY <1>u DEF Inv_TrustComplete
    <3>1. c \in tr[q] /\ c \in sPres'[q] BY <1>sP
    <3>. QED BY <3>1, <1>sP, <1>0 DEF Inv_TrustComplete
  <2>6. I_Fresh'
    <3>1. Bases' = Bases BY <1>u DEF Bases, InSyncR
    <3>. SUFFICES ASSUME NEW q \in Bases, NEW c \in sPres'[q], c >= nextSeq \/ ~conf[c], NEW x \in Blocks
                 PROVE  sCon[q][c][x] >= cutAck'[q][c][x]
      BY <3>1, <1>u DEF I_Fresh
    <3>2. c \in sPres[q] /\ cutAck'[q][c] = cutAck[q][c] /\ q \in R BY <1>sP, <1>T
    <3>. QED BY <3>2, <1>0 DEF I_Fresh
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6
<1>7. I_HeadHi' /\ I_HeadLo'
  <2>1. \A q \in R : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) BY <1>u DEF CatchingUp, HiOf
  <2>. QED BY <1>0, <1>T, <1>u, <1>sP, <2>1 DEF I_HeadHi, I_HeadLo
<1>8. I_Reverted' /\ I_Session'
  <2>0. \A q \in R : SessionOK(q) /\ RevOK(q) BY <1>0, <1>T DEF I_Session, I_Reverted
  <2>1. \A q \in R : dState[q] = "stale" /\ sb[q] > 0 => sb[q] # e /\ sb[q] \in recorded BY <2>0, <1>t DEF SessionOK
  <2>2. \A q \in R : dState[q] = "standby" => mark[q] # e /\ mark[q] \in recorded BY <2>0, <1>t DEF SessionOK
  <2>3. dState[r] = "stale" /\ reverted[r] # -1 => hPar[r] # e
    <3>. SUFFICES ASSUME dState[r] = "stale", reverted[r] # -1 PROVE hPar[r] # e OBVIOUS
    <3>1. hPar[r] = reverted[r] /\ sb[r] = reverted[r] BY <2>0 DEF RevOK
    <3>2. reverted[r] = 0 \/ reverted[r] > 0 BY <1>0, <1>T, Groups DEF TyLeg
    <3>. QED BY <3>1, <3>2, <2>1, <1>sh, <1>g
  <2>4. I_Reverted'
    <3>1. \A q \in R \ {r} : RevOK(q)' BY <2>0, <1>m, <1>u, RevOther
    <3>2. RevOK(r)'
      <4>1. CASE dState[r] = "stale" /\ reverted[r] # -1
        <5>1. hPar'[r] = hPar[r] /\ hAl'[r] = hAl[r] BY <4>1, <2>3, <1>nk
        <5>2. reverted[r] > 0 => reverted[r] # e BY <2>0, <2>1, <4>1 DEF RevOK
        <5>. QED BY <2>0, <4>1, <5>1, <5>2, <1>n, <1>u DEF RevOK
      <4>2. CASE ~(dState[r] = "stale" /\ reverted[r] # -1) BY <4>2, <1>u DEF RevOK
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <3>1, <3>2 DEF I_Reverted
  <2>5. I_Session'
    <3>1. \A q \in R \ {r} : SessionOK(q)' BY <2>0, <1>m, <1>u, SessionOther
    <3>2. SessionOK(r)'
      <4>1. dState[r] = "standby" => hPar'[r] <= mark[r]
        <5>. SUFFICES ASSUME dState[r] = "standby" PROVE hPar'[r] <= mark[r] OBVIOUS
        <5>1. hPar[r] <= mark[r] BY <2>0 DEF SessionOK
        <5>2. CASE hPar[r] = e BY <5>1, <5>2, <1>hk, <1>sh, <1>g, <1>0, <1>T, Groups DEF TyLeg
        <5>3. CASE hPar[r] # e BY <5>1, <5>3, <1>nk
        <5>. QED BY <5>2, <5>3
      <4>2. dState[r] = "stale" /\ sb[r] > 0 /\ reverted[r] = -1 /\ sb[r] \in sPres'[r] => sb[r] \in tr'[r]
        BY <2>0, <2>1, <1>n DEF SessionOK
      <4>. QED BY <2>0, <4>1, <4>2, <1>n, <1>u DEF SessionOK, CatchingUp
    <3>. QED BY <3>1, <3>2 DEF I_Session
  <2>. QED BY <2>4, <2>5
<1>. QED BY <1>0, <1>T, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>u, IndSplitP

-----------------------------------------------------------------------------
(* The copy (copy_chain_and_align): what a base-inclusive copy of source   *)
(* s's lineage from the leg's head epoch H0 through a trusted target t     *)
(* leaves on the leg.  Shared by BulkCopy (H0 = the base) and Chase (H0 =  *)
(* the mark).                                                              *)

NH(r, s, H0, t) == AfterCopy(head[r], s, H0, t)

LEMMA CopyFacts ==
  ASSUME IndInv, NEW r \in R, NEW s \in R, NEW t \in Epochs, NEW H0 \in 0..MaxEpoch,
         CatchingUp(r), HiOf(r) = H0, H0 <= t,
         t \in Lin(s), t \in tr[s], H0 = 0 \/ H0 \in Lin(s)
  PROVE  /\ \A x \in Blocks : \/ x \in Copied(s, H0, t) /\ NH(r, s, H0, t)[x] = sCon[s][t][x]
                              \/ x \notin Copied(s, H0, t) /\ NH(r, s, H0, t)[x] = head[r][x]
                                 /\ sCon[s][t][x] <= head[r][x]
         /\ \A x \in Blocks : NH(r, s, H0, t)[x] >= cutAck[s][t][x]
         /\ \A q \in R : \A p \in sPres[q] : p < t =>
              \A x \in Blocks : sCon[q][p][x] <= NH(r, s, H0, t)[x]
         /\ \A q2 \in R : \A e2 \in sPres[q2] : e2 > t =>
              \A x \in Blocks : NH(r, s, H0, t)[x] <= cutAck[q2][e2][x]
         /\ H0 < t => \A x \in Blocks : head[r][x] <= cutAck[s][t][x]
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_WHi /\ I_HeadHi /\ I_HeadLo
      /\ Inv_TrustComplete
  BY DEF IndInv
<1>l. Lin(s) \subseteq Epochs /\ t \in sPres[s] BY <1>0, LinType, LinPrefix, Groups
<1>c. ChainShape(sPres[s], sPar[s], anc[s]) BY <1>0, LegChain
<1>tc. \A x \in Blocks : cutAck[s][t][x] <= sCon[s][t][x] BY <1>0 DEF Inv_TrustComplete
<1>v. \A x \in Blocks : head[r][x] \in Nat /\ sCon[s][t][x] \in Nat /\ cutAck[s][t][x] \in Nat
  BY <1>0, <1>l, Groups DEF TyHead, TySnap, TyTrust, Vers
<1>1. ASSUME NEW x \in Blocks
      PROVE  \/ x \in Copied(s, H0, t) /\ NH(r, s, H0, t)[x] = sCon[s][t][x]
             \/ x \notin Copied(s, H0, t) /\ NH(r, s, H0, t)[x] = head[r][x] /\ sCon[s][t][x] <= head[r][x]
  <2>. DEFINE Hx == {e \in CopyElems(s, H0, t) : x \in sAl[s][e]}
  <2>a. H0 \in Nat BY Const
  <2>b. H0 = 0 \/ (H0 \in Lin(s) /\ H0 <= t) OBVIOUS
  <2>r. /\ Hx # {} => Max(Hx) \in Hx /\ sCon[s][Max(Hx)][x] = sCon[s][t][x]
        /\ Hx = {} /\ H0 # 0 => sCon[s][t][x] = Up(sPar[s], sCon[s], sPar[s][H0], x)
        /\ Hx = {} /\ H0 = 0 => sCon[s][t][x] = 0
    BY <1>0, <2>a, <2>b, CopyRead
  <2>d. NH(r, s, H0, t)[x] = IF Hx = {} THEN head[r][x] ELSE sCon[s][Max(Hx)][x]
    BY Clean DEF NH, AfterCopy
  <2>e. x \in Copied(s, H0, t) <=> Hx # {} BY DEF Copied
  <2>1. CASE Hx # {} BY <2>1, <2>r, <2>d, <2>e
  <2>2. CASE Hx = {}
    <3>1. sCon[s][t][x] <= head[r][x]
      <4>1. CASE H0 = 0 BY <4>1, <2>2, <2>r, <1>v
      <4>2. CASE H0 # 0
        <5>1. H0 \in sPres[s] BY <4>2, <1>0, LinSub
        <5>2. sPar[s][H0] < H0 /\ (sPar[s][H0] # 0 => sPar[s][H0] \in sPres[s]) BY <5>1, <1>c DEF ChainShape
        <5>3. CASE sPar[s][H0] = 0 BY <5>3, <4>2, <2>2, <2>r, <1>v DEF Up
        <5>4. CASE sPar[s][H0] # 0
          <6>1. sCon[s][sPar[s][H0]][x] <= head[r][x] BY <5>2, <5>4, <1>0 DEF I_HeadLo
          <6>. QED BY <6>1, <5>4, <4>2, <2>2, <2>r DEF Up
        <5>. QED BY <5>3, <5>4
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <2>2, <2>d, <2>e, <3>1
  <2>. QED BY <2>1, <2>2
<1>2. \A x \in Blocks : NH(r, s, H0, t)[x] >= cutAck[s][t][x] BY <1>1, <1>tc, <1>v
<1>3. \A q \in R : \A p \in sPres[q] : p < t => \A x \in Blocks : sCon[q][p][x] <= NH(r, s, H0, t)[x]
  <2>. SUFFICES ASSUME NEW q \in R, NEW p \in sPres[q], p < t, NEW x \in Blocks
               PROVE  sCon[q][p][x] <= NH(r, s, H0, t)[x]
    OBVIOUS
  <2>1. sCon[q][p][x] <= cutAck[s][t][x] BY <1>0, <1>l DEF I_WHi
  <2>0. p \in Epochs BY <1>0, Groups DEF TySnap
  <2>2. sCon[q][p][x] \in Nat BY <2>0, <1>0, Nats
  <2>. QED BY <1>1, <1>tc, <1>v, <2>1, <2>2
<1>4. \A q2 \in R : \A e2 \in sPres[q2] : e2 > t => \A x \in Blocks : NH(r, s, H0, t)[x] <= cutAck[q2][e2][x]
  <2>. SUFFICES ASSUME NEW q2 \in R, NEW e2 \in sPres[q2], e2 > t, NEW x \in Blocks
               PROVE  NH(r, s, H0, t)[x] <= cutAck[q2][e2][x]
    OBVIOUS
  <2>1. sCon[s][t][x] <= cutAck[q2][e2][x] BY <1>0, <1>l DEF I_WHi
  <2>0. e2 \in Nat /\ t \in Nat /\ H0 \in Nat /\ e2 > HiOf(r)
    <3>1. e2 \in Epochs BY <1>0, Groups DEF TySnap
    <3>. QED BY <3>1, Const DEF Epochs
  <2>2. head[r][x] <= cutAck[q2][e2][x] BY <2>0, <1>0 DEF I_HeadHi
  <2>. QED BY <1>1, <2>1, <2>2
<1>5. H0 < t => \A x \in Blocks : head[r][x] <= cutAck[s][t][x]
  <2>. SUFFICES ASSUME H0 < t, NEW x \in Blocks PROVE head[r][x] <= cutAck[s][t][x] OBVIOUS
  <2>1. t > HiOf(r) OBVIOUS
  <2>. QED BY <2>1, <1>0, <1>l DEF I_HeadHi
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5

\* What a copy step leaves for the parts that read contents, when the copy
\* cuts a new snapshot t on r (r's head becomes that snapshot).
LEMMA CopySnap ==
  ASSUME IndInv, NEW r \in R, NEW s \in R, NEW t \in Epochs, NEW H0 \in 0..MaxEpoch, NEW N,
         s # r, t \in sPres[s], t \in tr[s], t < nextSeq, t \notin sPres[r], H0 < t,
         CatchingUp(r), HiOf(r) = H0,
         \A x \in Blocks : \/ N[x] = sCon[s][t][x]
                           \/ N[x] = head[r][x] /\ sCon[s][t][x] <= head[r][x],
         \A x \in Blocks : N[x] >= cutAck[s][t][x],
         \A q \in R : \A p \in sPres[q] : p < t => \A x \in Blocks : sCon[q][p][x] <= N[x],
         \A q2 \in R : \A e2 \in sPres[q2] : e2 > t => \A x \in Blocks : N[x] <= cutAck[q2][e2][x],
         \A x \in Blocks : head[r][x] <= cutAck[s][t][x],
         \A x \in Blocks : \A w \in pend : N[x] = w.v /\ w.b = x => w.cutAt = t,
         sPres'[r] = sPres[r] \cup {t}, sCon'[r][t] = N, cutAck'[r][t] = cutAck[s][t],
         tr'[r] = tr[r] \cup {t}, head'[r] = N,
         \A e \in sPres[r] : sCon'[r][e] = sCon[r][e] /\ cutAck'[r][e] = cutAck[r][e],
         \A q \in R \ {r} : /\ sPres'[q] = sPres[q] /\ sCon'[q] = sCon[q] /\ cutAck'[q] = cutAck[q]
                           /\ tr'[q] = tr[q] /\ head'[q] = head[q],
         CatchingUp(r)', HiOf(r)' = t,
         \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q),
         UNCHANGED <<pend, acked, ackedEver, ver>>
  PROVE  /\ I_WHi' /\ Inv_TrustComplete' /\ I_Legal' /\ I_PendSeen' /\ I_Vers'
         /\ I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
<1>0. /\ TypeOK /\ I_Legal /\ I_PendSeen /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo
      /\ I_HeadPend /\ Inv_TrustComplete /\ I_Pend
  BY DEF IndInv
<1>N. /\ \A q \in R : sPres[q] \subseteq Epochs
      /\ Epochs \subseteq Nat /\ t \in Nat /\ H0 \in Nat
      /\ \A q \in R : \A e \in Epochs : \A x \in Blocks : sCon[q][e][x] \in Nat /\ cutAck[q][e][x] \in Nat
      /\ \A q \in R : \A x \in Blocks : head[q][x] \in Nat
      /\ \A x \in Blocks : acked[x] \in Nat
  <2>1. \A q \in R : sPres[q] \subseteq Epochs BY <1>0, Groups DEF TySnap
  <2>2. Epochs \subseteq Nat /\ t \in Nat /\ H0 \in Nat BY Const DEF Epochs
  <2>3. /\ \A q \in R : \A e \in Epochs : \A x \in Blocks : sCon[q][e][x] \in Nat /\ cutAck[q][e][x] \in Nat
        /\ \A q \in R : \A x \in Blocks : head[q][x] \in Nat
        /\ \A x \in Blocks : acked[x] \in Nat
    BY <1>0, Nats
  <2>. QED BY <2>1, <2>2, <2>3
<1>NV. \A x \in Blocks : N[x] \in Nat BY <1>N
<1>tc. \A x \in Blocks : cutAck[s][t][x] <= sCon[s][t][x] BY <1>0 DEF Inv_TrustComplete
<1>1. I_WHi'
  <2>. SUFFICES ASSUME NEW q1 \in R, NEW q2 \in R, NEW e1 \in sPres'[q1], NEW e2 \in sPres'[q2],
                      e1 < e2, NEW x \in Blocks
               PROVE  sCon'[q1][e1][x] <= cutAck'[q2][e2][x]
    BY DEF I_WHi
  <2>1. CASE q2 = r /\ e2 = t
    <3>1. e1 \in sPres[q1] /\ sCon'[q1][e1] = sCon[q1][e1] BY <2>1
    <3>. QED BY <3>1, <2>1, <1>0 DEF I_WHi
  <2>2. CASE q1 = r /\ e1 = t /\ ~(q2 = r /\ e2 = t)
    <3>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <2>2
    <3>. QED BY <3>1, <2>2
  <2>3. CASE ~(q2 = r /\ e2 = t) /\ ~(q1 = r /\ e1 = t)
    <3>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <2>3
    <3>2. e1 \in sPres[q1] /\ sCon'[q1][e1] = sCon[q1][e1] BY <2>3
    <3>. QED BY <3>1, <3>2, <1>0 DEF I_WHi
  <2>. QED BY <2>1, <2>2, <2>3
<1>2. Inv_TrustComplete'
  <2>. SUFFICES ASSUME NEW q \in R, NEW e \in tr'[q], NEW x \in Blocks
               PROVE  sCon'[q][e][x] >= cutAck'[q][e][x]
    BY DEF Inv_TrustComplete
  <2>1. CASE q = r /\ e = t
    <3>1. sCon'[q][e][x] = N[x] /\ cutAck'[q][e][x] = cutAck[s][t][x] BY <2>1
    <3>2. N[x] >= cutAck[s][t][x] OBVIOUS
    <3>. QED BY <3>1, <3>2
  <2>2. CASE ~(q = r /\ e = t)
    <3>1. e \in tr[q] /\ e \in sPres[q] BY <2>2, <1>0 DEF I_Ranges, IndInv
    <3>2. sCon'[q][e] = sCon[q][e] /\ cutAck'[q][e] = cutAck[q][e] BY <3>1, <2>2
    <3>. QED BY <3>1, <3>2, <1>0 DEF Inv_TrustComplete
  <2>. QED BY <2>1, <2>2
<1>3. I_Legal'
  <2>. SUFFICES ASSUME NEW q \in R, NEW x \in Blocks
               PROVE  /\ head'[q][x] \in ackedEver[x] \cup PendVal(x)
                      /\ head'[q][x] <= acked[x] \/ head'[q][x] \in PendVal(x)
                      /\ \A e \in sPres'[q] : /\ sCon'[q][e][x] \in ackedEver[x] \cup PendVal(x)
                                             /\ \/ sCon'[q][e][x] <= acked[x]
                                                \/ \E w \in pend : w.b = x /\ w.v = sCon'[q][e][x] /\ w.cutAt = e
    BY DEF I_Legal, PendVal
  <2>0. /\ sCon[s][t][x] \in ackedEver[x] \cup PendVal(x)
        /\ head[r][x] \in ackedEver[x] \cup PendVal(x)
        /\ head[r][x] <= acked[x] \/ head[r][x] \in PendVal(x)
        /\ sCon[s][t][x] <= acked[x] \/ sCon[s][t][x] \in PendVal(x)
    BY <1>0 DEF I_Legal, PendVal
  <2>n. /\ N[x] \in ackedEver[x] \cup PendVal(x)
        /\ N[x] <= acked[x] \/ N[x] \in PendVal(x)
        /\ N[x] <= acked[x] \/ \E w \in pend : w.b = x /\ w.v = N[x] /\ w.cutAt = t
    <3>1. N[x] \in ackedEver[x] \cup PendVal(x) /\ (N[x] <= acked[x] \/ N[x] \in PendVal(x)) BY <2>0
    <3>2. N[x] \in PendVal(x) => \E w \in pend : w.b = x /\ w.v = N[x] /\ w.cutAt = t BY DEF PendVal
    <3>. QED BY <3>1, <3>2
  <2>1. CASE q = r
    <3>1. \A e \in sPres'[r] : e = t \/ (e \in sPres[r] /\ sCon'[r][e] = sCon[r][e]) OBVIOUS
    <3>. QED BY <2>1, <2>n, <3>1, <1>0 DEF I_Legal
  <2>2. CASE q # r BY <2>2, <1>0 DEF I_Legal
  <2>. QED BY <2>1, <2>2
<1>4. I_PendSeen'
  <2>. SUFFICES ASSUME NEW w \in pend,
                      \E q \in R : head'[q][w.b] = w.v \/ \E e \in sPres'[q] : sCon'[q][e][w.b] = w.v
               PROVE  w.landed # {}
    BY DEF I_PendSeen
  <2>1. w.b \in Blocks BY <1>0, Nats
  <2>2. N[w.b] = w.v => (sCon[s][t][w.b] = w.v \/ head[r][w.b] = w.v) BY <2>1
  <2>3. \E q \in R : head[q][w.b] = w.v \/ \E e \in sPres[q] : sCon[q][e][w.b] = w.v BY <2>2
  <2>. QED BY <2>3, <1>0 DEF I_PendSeen
<1>5. I_Vers'
  <2>1. \A x \in Blocks : N[x] <= ver BY <1>0 DEF I_Vers
  <2>. SUFFICES ASSUME NEW q \in R, NEW x \in Blocks
               PROVE  head'[q][x] <= ver /\ \A e \in sPres'[q] : sCon'[q][e][x] <= ver
    BY <1>0 DEF I_Vers
  <2>2. CASE q = r BY <2>1, <2>2, <1>0 DEF I_Vers
  <2>3. CASE q # r BY <2>3, <1>0 DEF I_Vers
  <2>. QED BY <2>2, <2>3
<1>6. I_HeadHi'
  <2>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW q2 \in R, NEW e2 \in sPres'[q2], e2 > HiOf(q)',
                      NEW x \in Blocks
               PROVE  head'[q][x] <= cutAck'[q2][e2][x]
    BY DEF I_HeadHi
  <2>1. CASE q = r
    <3>1. ~(q2 = r /\ e2 = t) /\ e2 > t BY <2>1
    <3>2. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <3>1
    <3>. QED BY <2>1, <3>1, <3>2
  <2>2. CASE q # r
    <3>0. CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ head'[q] = head[q] BY <2>2
    <3>1. CASE q2 = r /\ e2 = t BY <3>0, <3>1, <1>0 DEF I_HeadHi
    <3>2. CASE ~(q2 = r /\ e2 = t)
      <4>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <3>2
      <4>. QED BY <3>0, <4>1, <1>0 DEF I_HeadHi
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2
<1>7. I_HeadLo'
  <2>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW q1 \in R, NEW p \in sPres'[q1], p < HiOf(q)',
                      NEW x \in Blocks
               PROVE  sCon'[q1][p][x] <= head'[q][x]
    BY DEF I_HeadLo
  <2>1. CASE q = r
    <3>1. ~(q1 = r /\ p = t) /\ p < t BY <2>1
    <3>2. p \in sPres[q1] /\ sCon'[q1][p] = sCon[q1][p] BY <3>1
    <3>. QED BY <2>1, <3>1, <3>2
  <2>2. CASE q # r
    <3>0. CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ head'[q] = head[q] BY <2>2
    <3>1. CASE q1 = r /\ p = t
      <4>1. sCon[s][t][x] <= head[q][x] BY <3>0, <3>1, <1>0 DEF I_HeadLo
      <4>2. head[r][x] <= sCon[s][t][x] BY <1>tc, <1>N
      <4>. QED BY <3>0, <3>1, <4>1, <4>2, <1>N
    <3>2. CASE ~(q1 = r /\ p = t)
      <4>1. p \in sPres[q1] /\ sCon'[q1][p] = sCon[q1][p] BY <3>2
      <4>. QED BY <3>0, <4>1, <1>0 DEF I_HeadLo
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2
<1>8. I_HeadPend'
  <2>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW w \in pend, head'[q][w.b] = w.v
               PROVE  w.cutAt <= HiOf(q)'
    BY DEF I_HeadPend
  <2>0. w.b \in Blocks /\ w.cutAt \in Nat BY <1>0, Nats
  <2>1. CASE q = r BY <2>0, <2>1, <1>N
  <2>2. CASE q # r BY <2>2, <1>0 DEF I_HeadPend
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8

\* A copy onto the base epoch itself (no new snapshot): only r's head moves.
LEMMA CopyHeadOnly ==
  ASSUME IndInv, NEW r \in R, NEW s \in R, NEW t \in Epochs, NEW N,
         t \in sPres[s], CatchingUp(r), HiOf(r) = t,
         \A x \in Blocks : N[x] = sCon[s][t][x] \/ N[x] = head[r][x],
         \A q \in R : \A p \in sPres[q] : p < t => \A x \in Blocks : sCon[q][p][x] <= N[x],
         \A q2 \in R : \A e2 \in sPres[q2] : e2 > t => \A x \in Blocks : N[x] <= cutAck[q2][e2][x],
         \A x \in Blocks : \A w \in pend : N[x] = w.v /\ w.b = x => w.cutAt = t,
         head'[r] = N, \A q \in R \ {r} : head'[q] = head[q],
         CatchingUp(r)', HiOf(r)' = t,
         \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q),
         UNCHANGED <<sPres, sCon, cutAck, pend, acked, ackedEver, ver>>
  PROVE  /\ I_Legal' /\ I_PendSeen' /\ I_Vers' /\ I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
<1>0. /\ TypeOK /\ I_Legal /\ I_PendSeen /\ I_Vers /\ I_HeadHi /\ I_HeadLo /\ I_HeadPend /\ I_Pend
  BY DEF IndInv
<1>1. I_Legal'
  <2>. SUFFICES ASSUME NEW q \in R, NEW x \in Blocks
               PROVE  /\ head'[q][x] \in ackedEver[x] \cup PendVal(x)
                      /\ head'[q][x] <= acked[x] \/ head'[q][x] \in PendVal(x)
    BY <1>0 DEF I_Legal, PendVal
  <2>0. /\ sCon[s][t][x] \in ackedEver[x] \cup PendVal(x)
        /\ sCon[s][t][x] <= acked[x] \/ sCon[s][t][x] \in PendVal(x)
    BY <1>0 DEF I_Legal, PendVal
  <2>1. CASE q = r BY <2>0, <2>1, <1>0 DEF I_Legal
  <2>2. CASE q # r BY <2>2, <1>0 DEF I_Legal
  <2>. QED BY <2>1, <2>2
<1>2. I_PendSeen'
  <2>. SUFFICES ASSUME NEW w \in pend,
                      \E q \in R : head'[q][w.b] = w.v \/ \E e \in sPres[q] : sCon[q][e][w.b] = w.v
               PROVE  w.landed # {}
    BY DEF I_PendSeen
  <2>1. w.b \in Blocks BY <1>0, Nats
  <2>. QED BY <2>1, <1>0 DEF I_PendSeen
<1>3. I_Vers'
  <2>1. \A x \in Blocks : N[x] <= ver BY <1>0 DEF I_Vers
  <2>. QED BY <2>1, <1>0 DEF I_Vers
<1>4. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
  <2>1. I_HeadHi'
    <3>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW q2 \in R, NEW e2 \in sPres[q2], e2 > HiOf(q)',
                        NEW x \in Blocks
                 PROVE  head'[q][x] <= cutAck[q2][e2][x]
      BY DEF I_HeadHi
    <3>1. CASE q = r BY <3>1
    <3>2. CASE q # r BY <3>2, <1>0 DEF I_HeadHi
    <3>. QED BY <3>1, <3>2
  <2>2. I_HeadLo'
    <3>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW q1 \in R, NEW p \in sPres[q1], p < HiOf(q)',
                        NEW x \in Blocks
                 PROVE  sCon[q1][p][x] <= head'[q][x]
      BY DEF I_HeadLo
    <3>1. CASE q = r BY <3>1
    <3>2. CASE q # r BY <3>2, <1>0 DEF I_HeadLo
    <3>. QED BY <3>1, <3>2
  <2>3. I_HeadPend'
    <3>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW w \in pend, head'[q][w.b] = w.v
                 PROVE  w.cutAt <= HiOf(q)'
      BY DEF I_HeadPend
    <3>0. w.b \in Blocks /\ w.cutAt \in Nat /\ t \in Nat BY <1>0, Nats, Const DEF Epochs
    <3>1. CASE q = r BY <3>0, <3>1
    <3>2. CASE q # r BY <3>2, <1>0 DEF I_HeadPend
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2, <1>3, <1>4

LEMMA I_BulkCopy ==
  ASSUME IndInv, NEW r \in R, BulkCopy(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>g. /\ dState[r] = "stale" /\ path[r] = "back" /\ reverted[r] # -1 /\ sb[r] = reverted[r]
      /\ Tgt # 0 /\ sb[r] <= Tgt
      /\ dState' = [dState EXCEPT ![r] = "standby"] /\ sb' = [sb EXCEPT ![r] = -1]
  BY Clean DEF BulkCopy
<1>. DEFINE t == Tgt
<1>. DEFINE b == sb[r]
<1>s. PICK s \in CoverSet(r, t, b) :
        /\ CopyAlign(r, s, b, t)
        /\ mark' = [mark EXCEPT ![r] = t] /\ pin' = [pin EXCEPT ![r] = t]
  BY Clean DEF BulkCopy
<1>u. /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path>>
      /\ UNCHANGED <<reverted, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
      /\ UNCHANGED <<cfgAt, conf, htr>>
  BY DEF BulkCopy
<1>cf. s \in Sources(r) /\ t \in Lin(s) /\ (b <= 0 \/ b \in Lin(s)) /\ t \in tr[s] BY <1>s, CoverSetFacts
<1>t. t \in recorded /\ t \in Epochs /\ t < nextSeq /\ t >= 1 BY <1>0, <1>T, <1>g, TgtFacts
<1>b. b \in 0..MaxEpoch /\ b <= t /\ (b = 0 \/ b \in Lin(s)) BY <1>0, <1>T, <1>g, <1>cf, Groups DEF TyLeg
<1>cu. CatchingUp(r) /\ HiOf(r) = b BY <1>g DEF CatchingUp, HiOf
<1>ro. RevOK(r) /\ SessionOK(r) BY <1>0, <1>T DEF I_Reverted, I_Session
<1>rv. /\ hAl[r] = {} /\ hPar[r] = b /\ (b > 0 => b \in sPres[r])
       /\ \A e \in sPres[r] : e <= b \/ (b = 0 /\ \A f \in recorded : e < f)
  BY <1>ro, <1>g DEF RevOK
<1>sv. s \in R /\ s # r /\ t \in sPres[s] /\ s \in InSyncR BY <1>cf, <1>0, <1>T, LinSub DEF Sources, InSyncR
<1>N. /\ t \in Nat /\ b \in Nat /\ nextSeq \in Nat
      /\ \A w \in pend : w.cutAt \in Nat /\ w.b \in Blocks /\ w.v \in Nat
      /\ \A x \in Blocks : acked[x] \in Nat /\ head[r][x] \in Nat /\ sCon[s][t][x] \in Nat
  BY <1>0, <1>T, <1>t, <1>b, <1>sv, Nats, Const DEF Epochs
<1>F. /\ \A x \in Blocks : \/ x \in Copied(s, b, t) /\ NH(r, s, b, t)[x] = sCon[s][t][x]
                           \/ x \notin Copied(s, b, t) /\ NH(r, s, b, t)[x] = head[r][x]
                              /\ sCon[s][t][x] <= head[r][x]
      /\ \A x \in Blocks : NH(r, s, b, t)[x] >= cutAck[s][t][x]
      /\ \A q \in R : \A p \in sPres[q] : p < t => \A x \in Blocks : sCon[q][p][x] <= NH(r, s, b, t)[x]
      /\ \A q2 \in R : \A e2 \in sPres[q2] : e2 > t => \A x \in Blocks : NH(r, s, b, t)[x] <= cutAck[q2][e2][x]
      /\ b < t => \A x \in Blocks : head[r][x] <= cutAck[s][t][x]
  BY <1>t, <1>b, <1>cu, <1>cf, <1>sv, CopyFacts
<1>lo. (IF BaseInclusive \/ b = 0 THEN b ELSE b + 1) = b BY Clean
<1>pv. \A x \in Blocks : \A w \in pend : NH(r, s, b, t)[x] = w.v /\ w.b = x => w.cutAt = t
  <2>. SUFFICES ASSUME NEW x \in Blocks, NEW w \in pend, NH(r, s, b, t)[x] = w.v, w.b = x
               PROVE  w.cutAt = t
    OBVIOUS
  <2>1. w.cutAt >= nextSeq - 1 /\ w.cutAt <= nextSeq /\ w.v > acked[x] BY <1>0, <1>T DEF I_Pend
  <2>2. CASE x \in Copied(s, b, t)
    <3>1. sCon[s][t][x] = w.v BY <2>2, <1>F
    <3>2. ~(sCon[s][t][x] <= acked[x]) BY <3>1, <2>1, <1>N
    <3>3. \E u \in pend : u.b = x /\ u.v = w.v /\ u.cutAt = t BY <3>1, <3>2, <1>sv, <1>0, <1>T DEF I_Legal
    <3>. QED BY <3>3, <1>0, <1>T, PendOne
  <2>3. CASE x \notin Copied(s, b, t)
    <3>1. head[r][x] = w.v BY <2>3, <1>F
    <3>2. w.cutAt <= b BY <3>1, <1>cu, <1>0, <1>T DEF I_HeadPend
    <3>. QED BY <3>2, <2>1, <1>b, <1>t, <1>N
  <2>. QED BY <2>2, <2>3
<1>ms. /\ mark'[r] = t /\ pin'[r] = t /\ dState'[r] = "standby" /\ sb'[r] = -1
       /\ \A q \in R \ {r} : mark'[q] = mark[q] /\ pin'[q] = pin[q] /\ dState'[q] = dState[q] /\ sb'[q] = sb[q]
  BY <1>s, <1>g, <1>0, <1>T, Groups DEF TyLeg
<1>cu2. /\ CatchingUp(r)' /\ HiOf(r)' = t
        /\ \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q)
  BY <1>ms, <1>u DEF CatchingUp, HiOf
<1>1. TypeOK' BY T_BulkCopy DEF IndInv
<1>2. I_Pend' /\ I_Members' /\ I_Hold' /\ I_AckedEver'
  <2>1. r \notin members /\ r \notin InSyncR BY <1>0, <1>T, <1>g DEF I_Members, I_Ranges, InSyncR
  <2>2. InSyncR' = InSyncR BY <1>ms, <1>g DEF InSyncR
  <2>3. \A q \in R \ {r} : head'[q] = head[q] BY <1>s, <1>0, <1>T, Groups DEF CopyAlign, TyHead
  <2>4. \A q \in members : head'[q] = head[q] BY <2>1, <2>3, <1>T
  <2>4b. \A q \in InSyncR : head'[q] = head[q] BY <2>1, <2>3, <1>T
  <2>5. I_Pend'
    <3>. SUFFICES ASSUME NEW w \in pend
                 PROVE  \A q \in w.landed \cap members : head'[q][w.b] = w.v
      BY <1>0, <1>T, <1>u DEF I_Pend
    <3>. QED BY <2>4, <1>0, <1>T DEF I_Pend
  <2>6. I_Members' BY <1>0, <1>T, <1>u, <2>2 DEF I_Members, Healthy
  <2>7. I_Hold' BY <1>0, <1>u, <2>2, <2>4, <2>4b DEF I_Hold
  <2>. QED BY <1>0, <1>T, <1>u, <2>5, <2>6, <2>7, F_AckedEver
<1>rest. /\ I_Reverted' /\ I_Fresh'
  <2>1. I_Reverted'
    <3>1. \A q \in R \ {r} : RevOK(q)'
      <4>1. \A q \in R \ {r} : hAl'[q] = hAl[q] /\ hPar'[q] = hPar[q] /\ head'[q] = head[q]
                              /\ sCon'[q] = sCon[q] /\ sPres'[q] = sPres[q]
        BY <1>s, <1>0, <1>T, Groups DEF CopyAlign, TyHead, TySnap
      <4>. QED BY <4>1, <1>ms, <1>u, <1>0, <1>T, RevOther DEF I_Reverted
    <3>2. RevOK(r)' BY <1>ms DEF RevOK
    <3>. QED BY <3>1, <3>2 DEF I_Reverted
  <2>2. I_Fresh'
    <3>1. Bases' = Bases /\ r \notin Bases BY <1>ms, <1>u, <1>g, <1>0, <1>T DEF Bases, InSyncR, I_Members, I_Ranges
    <3>2. \A q \in Bases : sPres'[q] = sPres[q] /\ sCon'[q] = sCon[q] /\ cutAck'[q] = cutAck[q]
      BY <3>1, <1>s, <1>0, <1>T, Groups DEF CopyAlign, TySnap, TyTrust
    <3>. QED BY <3>1, <3>2, <1>u, <1>0, <1>T DEF I_Fresh
  <2>. QED BY <2>1, <2>2
<1>3. CASE b = t
  <2>c. /\ head' = [head EXCEPT ![r] = NH(r, s, b, t)]
        /\ hAl' = [hAl EXCEPT ![r] = hAl[r] \cup Copied(s, b, t)]
        /\ UNCHANGED <<sPres, sCon, sAl, sPar, hPar, anc, tr, cutAck>>
    BY <1>3, <1>s, <1>lo DEF CopyAlign, NH
  <2>h. \A q \in R \ {r} : head'[q] = head[q] /\ hAl'[q] = hAl[q] BY <2>c, <1>0, <1>T, Groups DEF TyHead
  <2>hr. head'[r] = NH(r, s, b, t) /\ hAl'[r] = Copied(s, b, t) BY <2>c, <1>rv, <1>0, <1>T, Groups DEF TyHead
  <2>1. I_Ranges' /\ I_Lineage' /\ I_WHi' /\ Inv_TrustComplete'
    BY <1>0, <1>T, <1>u, <2>c, F_Lineage, F_WHi, F_TC DEF I_Ranges
  <2>2. I_Chain'
    <3>1. \A x \in Blocks : x \notin hAl'[r] => head'[r][x] = Up(sPar[r], sCon[r], hPar[r], x)
      <4>. SUFFICES ASSUME NEW x \in Blocks, x \notin hAl'[r] PROVE head'[r][x] = Up(sPar[r], sCon[r], hPar[r], x)
        OBVIOUS
      <4>1. NH(r, s, b, t)[x] = head[r][x] BY <2>hr, <1>F
      <4>. QED BY <4>1, <2>hr, <1>rv, <1>0, <1>T DEF I_Chain
    <3>. QED BY <1>0, <1>T, <2>c, <2>h, <3>1 DEF I_Chain
  <2>3. I_Legal' /\ I_PendSeen' /\ I_Vers' /\ I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
    <3>1. \A x \in Blocks : NH(r, s, b, t)[x] = sCon[s][t][x] \/ NH(r, s, b, t)[x] = head[r][x] BY <1>F
    <3>. QED BY <1>sv, <1>3, <1>cu, <1>F, <1>pv, <2>hr, <2>h, <1>cu2, <2>c, <1>u, <3>1, <1>t, CopyHeadOnly
  <2>4. I_Session'
    <3>1. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>u, <1>ms, <2>c, <2>h, SessionOther DEF I_Session
    <3>2. SessionOK(r)'
      <4>1. \A e \in sPres[r] : e <= t \/ \A f \in recorded : e < f BY <1>rv, <1>3
      <4>2. r \notin members /\ nextSeq \notin sPres[r] BY <1>ro, <1>0, <1>T, <1>g DEF SessionOK, CatchingUp, I_Members, I_Ranges, InSyncR
      <4>. QED BY <1>ms, <1>t, <1>rv, <1>3, <4>1, <4>2, <2>c, <1>u, <1>g, <1>cu, <1>cu2, <1>ro DEF SessionOK, CatchingUp
    <3>. QED BY <3>1, <3>2 DEF I_Session
  <2>. QED BY <1>0, <1>T, <1>1, <1>2, <1>rest, <2>1, <2>2, <2>3, <2>4, <1>u, IndSplitP
<1>4. CASE b # t
  <2>0. b < t /\ t \notin sPres[r]
    <3>1. b < t BY <1>4, <1>b, <1>N
    <3>2. t \notin sPres[r] BY <3>1, <1>rv, <1>t, <1>N
    <3>. QED BY <3>1, <3>2
  <2>c. /\ head' = [head EXCEPT ![r] = NH(r, s, b, t)]
        /\ sPres' = [sPres EXCEPT ![r] = @ \cup {t}]
        /\ sCon' = [sCon EXCEPT ![r][t] = NH(r, s, b, t)]
        /\ sAl' = [sAl EXCEPT ![r][t] = hAl[r] \cup Copied(s, b, t)]
        /\ sPar' = [sPar EXCEPT ![r][t] = hPar[r]]
        /\ anc' = [anc EXCEPT ![r][t] = NewAnc(r, t)]
        /\ hPar' = [hPar EXCEPT ![r] = t] /\ hAl' = [hAl EXCEPT ![r] = {}]
        /\ tr' = [tr EXCEPT ![r] = @ \cup {t}]
        /\ cutAck' = [cutAck EXCEPT ![r][t] = cutAck[s][t]]
    BY <1>4, <2>0, <1>s, <1>lo, Clean DEF CopyAlign, NH
  <2>m. \A q \in R \ {r} : /\ head'[q] = head[q] /\ sPres'[q] = sPres[q] /\ sCon'[q] = sCon[q]
                            /\ sAl'[q] = sAl[q] /\ sPar'[q] = sPar[q] /\ anc'[q] = anc[q]
                            /\ hPar'[q] = hPar[q] /\ hAl'[q] = hAl[q] /\ tr'[q] = tr[q]
                            /\ cutAck'[q] = cutAck[q]
    BY <2>c, <1>0, <1>T, Groups DEF TyHead, TySnap, TyTrust
  <2>o. \A e \in sPres[r] : /\ sCon'[r][e] = sCon[r][e] /\ sAl'[r][e] = sAl[r][e]
                             /\ sPar'[r][e] = sPar[r][e] /\ anc'[r][e] = anc[r][e]
                             /\ cutAck'[r][e] = cutAck[r][e]
    BY <2>c, <2>0, <1>t, <1>0, <1>T, Groups DEF TySnap, TyTrust
  <2>n. /\ sPres'[r] = sPres[r] \cup {t} /\ sCon'[r][t] = NH(r, s, b, t)
        /\ sAl'[r][t] = Copied(s, b, t) /\ sPar'[r][t] = b /\ anc'[r][t] = NewAnc(r, t)
        /\ hPar'[r] = t /\ hAl'[r] = {} /\ head'[r] = NH(r, s, b, t) /\ tr'[r] = tr[r] \cup {t}
        /\ cutAck'[r][t] = cutAck[s][t]
    BY <2>c, <1>rv, <1>t, <1>0, <1>T, Groups DEF TyHead, TySnap, TyTrust
  <2>1. I_Ranges'
    <3>1. sPres'[r] \subseteq 1..nextSeq /\ tr'[r] \subseteq sPres'[r] \cap 1..(nextSeq - 1) /\ hPar'[r] \in sPres'[r]
      BY <2>n, <1>t, <1>0, <1>T DEF I_Ranges, Epochs
    <3>. QED BY <1>0, <1>T, <1>u, <2>m, <3>1 DEF I_Ranges
  <2>rd. \A x \in Blocks : x \notin sAl'[r][t] => sCon'[r][t][x] = Up(sPar[r], sCon[r], hPar[r], x)
    <3>. SUFFICES ASSUME NEW x \in Blocks, x \notin sAl'[r][t] PROVE sCon'[r][t][x] = Up(sPar[r], sCon[r], hPar[r], x)
      OBVIOUS
    <3>1. NH(r, s, b, t)[x] = head[r][x] BY <2>n, <1>F
    <3>. QED BY <3>1, <2>n, <1>rv, <1>0, <1>T DEF I_Chain
  <2>LE. ChainShape(sPres'[r], sPar'[r], anc'[r]) /\ ChainReads(sPres'[r], sPar'[r], sAl'[r], sCon'[r])
    <3>1. anc'[r][t] = {t} \cup (IF hPar[r] = 0 THEN {} ELSE anc[r][hPar[r]]) BY <2>n, <1>rv DEF NewAnc, AncOf
    <3>2. hPar[r] < t /\ sPar'[r][t] = hPar[r] BY <2>0, <2>n, <1>rv
    <3>. QED BY <1>0, <1>T, <1>t, <2>0, <2>n, <2>o, <2>rd, <3>1, <3>2, LegExtend
  <2>2. I_Lineage'
    <3>1. ChainShape(sPres'[r], sPar'[r], anc'[r]) BY <2>LE
    <3>2. \A e \in Epochs \ sPres'[r] : anc'[r][e] = {} BY <2>c, <2>n, <1>0, <1>T, Groups DEF TySnap, I_Lineage
    <3>. QED BY <1>0, <1>T, <2>m, <3>1, <3>2 DEF I_Lineage
  <2>3. I_Chain'
    <3>1. ChainReads(sPres'[r], sPar'[r], sAl'[r], sCon'[r])
      <4>. QED BY <2>LE
    <3>2. \A x \in Blocks : head'[r][x] = Up(sPar'[r], sCon'[r], hPar'[r], x) BY <2>n, <1>t DEF Up
    <3>. QED BY <1>0, <1>T, <2>m, <3>1, <3>2 DEF I_Chain
  <2>4. /\ I_WHi' /\ Inv_TrustComplete' /\ I_Legal' /\ I_PendSeen' /\ I_Vers'
        /\ I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
    <3>1. \A x \in Blocks : \/ NH(r, s, b, t)[x] = sCon[s][t][x]
                            \/ NH(r, s, b, t)[x] = head[r][x] /\ sCon[s][t][x] <= head[r][x]
      BY <1>F
    <3>2. \A x \in Blocks : head[r][x] <= cutAck[s][t][x] BY <1>F, <2>0
    <3>. QED BY <1>sv, <1>t, <1>b, <2>0, <1>cu, <1>cf, <3>1, <3>2, <1>F, <1>pv, <2>n, <2>o, <2>m,
                <1>cu2, <1>u, CopySnap
  <2>5. I_Session'
    <3>1. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>u, <1>ms, <2>m, SessionOther DEF I_Session
    <3>2. SessionOK(r)'
      <4>1. \A e \in sPres'[r] : e <= t \/ \A f \in recorded : e < f BY <1>rv, <2>n, <2>0, <1>b, <1>N, <1>T
      <4>2. r \notin members /\ nextSeq \notin sPres'[r]
        <5>1. r \notin members BY <1>0, <1>T, <1>g DEF I_Members, I_Ranges, InSyncR
        <5>2. nextSeq \notin sPres[r] BY <1>ro, <1>cu DEF SessionOK
        <5>3. t # nextSeq BY <1>t, <1>N
        <5>. QED BY <5>1, <5>2, <5>3, <2>n
      <4>. QED BY <1>ms, <1>t, <2>n, <4>1, <4>2, <1>u, <1>g, <1>cu, <1>cu2, <1>ro DEF SessionOK, CatchingUp
    <3>. QED BY <3>1, <3>2 DEF I_Session
  <2>. QED BY <1>0, <1>T, <1>1, <1>2, <1>rest, <2>1, <2>2, <2>3, <2>4, <2>5, <1>u, IndSplitP
<1>. QED BY <1>3, <1>4

LEMMA I_Chase ==
  ASSUME IndInv, NEW r \in R, Chase(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>. DEFINE t == Tgt
<1>. DEFINE b == mark[r]
<1>g. /\ dState[r] = "standby" /\ path[r] = "back" /\ Tgt # 0 /\ t > b
  BY DEF Chase
<1>u. /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path>>
      /\ UNCHANGED <<sb, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
      /\ UNCHANGED <<cfgAt, conf, htr>>
  BY DEF Chase
<1>t. t \in recorded /\ t \in Epochs /\ t < nextSeq /\ t >= 1 BY <1>0, <1>T, <1>g, TgtFacts
<1>so. SessionOK(r) BY <1>0, <1>T DEF I_Session
<1>sb. /\ b >= 1 /\ b \in recorded /\ hPar[r] <= b /\ hPar[r] \in sPres[r] \cup {0}
       /\ \A e \in sPres[r] : e <= b \/ \A f \in recorded : e < f
       /\ r \notin members /\ sb[r] = -1 /\ b \in 0..MaxEpoch /\ hPar[r] \in 0..MaxEpoch
  BY <1>so, <1>g, <1>0, <1>T, Groups DEF SessionOK, I_Ranges, TyLeg, TyHead
<1>5. CASE CoverSet(r, t, b) = {}
  <2>c. /\ dState' = [dState EXCEPT ![r] = "stale"] /\ reverted' = [reverted EXCEPT ![r] = -1]
        /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, anc, mark, pin, tr, cutAck>>
    BY <1>5, Clean DEF Chase, Demote
  <2>a. \A q \in R \ {r} : dState'[q] = dState[q] /\ reverted'[q] = reverted[q] BY <2>c, <1>0, <1>T, Groups DEF TyLeg
  <2>r. dState'[r] = "stale" /\ reverted'[r] = -1 BY <2>c, <1>0, <1>T, Groups DEF TyLeg
  <2>b. ~CatchingUp(r)' /\ \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q)
    BY <2>c, <2>a, <2>r, <1>u DEF CatchingUp, HiOf
  <2>1. TypeOK' BY T_Chase DEF IndInv
  <2>2. I_Ranges' /\ I_Lineage' /\ I_Chain' /\ I_Pend' /\ I_Legal' /\ I_PendSeen' /\ I_AckedEver'
        /\ I_Vers' /\ I_WHi' /\ Inv_TrustComplete'
    BY <1>0, <1>T, <1>u, <2>c, F_Ranges, F_Lineage, F_Chain, F_Pend, F_Legal, F_PendSeen, F_AckedEver,
       F_Vers, F_WHi, F_TC
  <2>3. I_Members' /\ I_Hold' /\ I_Fresh'
    <3>1. InSyncR' = InSyncR BY <2>a, <2>r, <1>g DEF InSyncR
    <3>2. Bases' = Bases BY <3>1, <1>u DEF Bases
    <3>. QED BY <1>0, <1>T, <1>u, <2>c, <3>1, <3>2 DEF I_Members, I_Hold, I_Fresh, Healthy
  <2>4. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend' BY <1>0, <1>T, <1>u, <2>c, <2>b DEF I_HeadHi, I_HeadLo, I_HeadPend
  <2>5. I_Reverted'
    <3>1. \A q \in R \ {r} : RevOK(q)' BY <1>0, <1>T, <2>a, <2>c, <1>u, RevOther DEF I_Reverted
    <3>2. RevOK(r)' BY <2>r DEF RevOK
    <3>. QED BY <3>1, <3>2 DEF I_Reverted
  <2>6. I_Session'
    <3>1. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>u, <2>a, <2>c, SessionOther DEF I_Session
    <3>2. SessionOK(r)' BY <2>r, <2>b, <1>sb, <1>u DEF SessionOK
    <3>. QED BY <3>1, <3>2 DEF I_Session
  <2>. QED BY <1>0, <1>T, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <1>u, <2>c, IndSplitP
<1>6. CASE CoverSet(r, t, b) # {}
  <2>s. PICK s \in CoverSet(r, t, b) :
          /\ CopyAlign(r, s, b, t)
          /\ mark' = [mark EXCEPT ![r] = t] /\ pin' = [pin EXCEPT ![r] = t]
          /\ UNCHANGED <<dState, reverted, restamp>>
    BY <1>6, Clean DEF Chase
  <2>cf. s \in Sources(r) /\ t \in Lin(s) /\ (b <= 0 \/ b \in Lin(s)) /\ t \in tr[s] BY <2>s, CoverSetFacts
  <2>sv. s \in R /\ s # r /\ t \in sPres[s] BY <2>cf, <1>0, <1>T, LinSub DEF Sources
  <2>b. b \in 0..MaxEpoch /\ b <= t /\ (b = 0 \/ b \in Lin(s)) BY <1>sb, <1>g, <2>cf
  <2>cu. CatchingUp(r) /\ HiOf(r) = b BY <1>g DEF CatchingUp, HiOf
  <2>F. /\ \A x \in Blocks : \/ x \in Copied(s, b, t) /\ NH(r, s, b, t)[x] = sCon[s][t][x]
                             \/ x \notin Copied(s, b, t) /\ NH(r, s, b, t)[x] = head[r][x]
                                /\ sCon[s][t][x] <= head[r][x]
        /\ \A x \in Blocks : NH(r, s, b, t)[x] >= cutAck[s][t][x]
        /\ \A q \in R : \A p \in sPres[q] : p < t => \A x \in Blocks : sCon[q][p][x] <= NH(r, s, b, t)[x]
        /\ \A q2 \in R : \A e2 \in sPres[q2] : e2 > t => \A x \in Blocks : NH(r, s, b, t)[x] <= cutAck[q2][e2][x]
        /\ b < t => \A x \in Blocks : head[r][x] <= cutAck[s][t][x]
    BY <1>t, <2>b, <2>cu, <2>cf, <2>sv, CopyFacts
  <2>N. /\ t \in Nat /\ b \in Nat /\ nextSeq \in Nat /\ hPar[r] \in Nat
        /\ \A w \in pend : w.cutAt \in Nat /\ w.b \in Blocks /\ w.v \in Nat
        /\ \A x \in Blocks : acked[x] \in Nat /\ sCon[s][t][x] \in Nat
    BY <1>0, <1>T, <1>t, <1>sb, <2>sv, Nats, Const DEF Epochs
  <2>lo. (IF BaseInclusive \/ b = 0 THEN b ELSE b + 1) = b BY Clean
  <2>0. b < t /\ t \notin sPres[r] /\ hPar[r] < t
    <3>1. b < t /\ hPar[r] < t BY <1>g, <1>sb, <2>N
    <3>2. t \notin sPres[r] BY <3>1, <1>sb, <1>t, <2>N
    <3>. QED BY <3>1, <3>2
  <2>pv. \A x \in Blocks : \A w \in pend : NH(r, s, b, t)[x] = w.v /\ w.b = x => w.cutAt = t
    <3>. SUFFICES ASSUME NEW x \in Blocks, NEW w \in pend, NH(r, s, b, t)[x] = w.v, w.b = x
                 PROVE  w.cutAt = t
      OBVIOUS
    <3>1. w.cutAt >= nextSeq - 1 /\ w.cutAt <= nextSeq /\ w.v > acked[x] BY <1>0, <1>T DEF I_Pend
    <3>2. CASE x \in Copied(s, b, t)
      <4>1. sCon[s][t][x] = w.v BY <3>2, <2>F
      <4>2. ~(sCon[s][t][x] <= acked[x]) BY <4>1, <3>1, <2>N
      <4>3. \E u \in pend : u.b = x /\ u.v = w.v /\ u.cutAt = t BY <4>1, <4>2, <2>sv, <1>0, <1>T DEF I_Legal
      <4>. QED BY <4>3, <1>0, <1>T, PendOne
    <3>3. CASE x \notin Copied(s, b, t)
      <4>1. head[r][x] = w.v BY <3>3, <2>F
      <4>2. w.cutAt <= b BY <4>1, <2>cu, <1>0, <1>T DEF I_HeadPend
      <4>. QED BY <4>2, <3>1, <2>0, <1>t, <2>N
    <3>. QED BY <3>2, <3>3
  <2>c. /\ head' = [head EXCEPT ![r] = NH(r, s, b, t)]
        /\ sPres' = [sPres EXCEPT ![r] = @ \cup {t}]
        /\ sCon' = [sCon EXCEPT ![r][t] = NH(r, s, b, t)]
        /\ sAl' = [sAl EXCEPT ![r][t] = hAl[r] \cup Copied(s, b, t)]
        /\ sPar' = [sPar EXCEPT ![r][t] = hPar[r]]
        /\ anc' = [anc EXCEPT ![r][t] = NewAnc(r, t)]
        /\ hPar' = [hPar EXCEPT ![r] = t] /\ hAl' = [hAl EXCEPT ![r] = {}]
        /\ tr' = [tr EXCEPT ![r] = @ \cup {t}]
        /\ cutAck' = [cutAck EXCEPT ![r][t] = cutAck[s][t]]
    BY <2>0, <2>s, <2>lo, Clean DEF CopyAlign, NH
  <2>m. \A q \in R \ {r} : /\ head'[q] = head[q] /\ sPres'[q] = sPres[q] /\ sCon'[q] = sCon[q]
                            /\ sAl'[q] = sAl[q] /\ sPar'[q] = sPar[q] /\ anc'[q] = anc[q]
                            /\ hPar'[q] = hPar[q] /\ hAl'[q] = hAl[q] /\ tr'[q] = tr[q]
                            /\ cutAck'[q] = cutAck[q]
    BY <2>c, <1>0, <1>T, Groups DEF TyHead, TySnap, TyTrust
  <2>o. \A e \in sPres[r] : /\ sCon'[r][e] = sCon[r][e] /\ sAl'[r][e] = sAl[r][e]
                             /\ sPar'[r][e] = sPar[r][e] /\ anc'[r][e] = anc[r][e]
                             /\ cutAck'[r][e] = cutAck[r][e]
    BY <2>c, <2>0, <1>t, <1>0, <1>T, Groups DEF TySnap, TyTrust
  <2>n. /\ sPres'[r] = sPres[r] \cup {t} /\ sCon'[r][t] = NH(r, s, b, t)
        /\ sAl'[r][t] = hAl[r] \cup Copied(s, b, t) /\ sPar'[r][t] = hPar[r] /\ anc'[r][t] = NewAnc(r, t)
        /\ hPar'[r] = t /\ hAl'[r] = {} /\ head'[r] = NH(r, s, b, t) /\ tr'[r] = tr[r] \cup {t}
        /\ cutAck'[r][t] = cutAck[s][t]
    BY <2>c, <1>t, <1>0, <1>T, Groups DEF TyHead, TySnap, TyTrust
  <2>ms. /\ mark'[r] = t /\ pin'[r] = t
         /\ \A q \in R \ {r} : mark'[q] = mark[q] /\ pin'[q] = pin[q]
    BY <2>s, <1>0, <1>T, Groups DEF TyLeg
  <2>cu2. /\ CatchingUp(r)' /\ HiOf(r)' = t
          /\ \A q \in R \ {r} : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q)
    BY <2>s, <2>ms, <1>g, <1>u DEF CatchingUp, HiOf
  <2>1. TypeOK' BY T_Chase DEF IndInv
  <2>2. I_Pend' /\ I_Members' /\ I_Hold' /\ I_AckedEver'
    <3>1. r \notin InSyncR /\ InSyncR' = InSyncR BY <1>g, <2>s DEF InSyncR
    <3>2. \A q \in members : head'[q] = head[q] BY <1>sb, <2>m, <1>T
    <3>2b. \A q \in InSyncR : head'[q] = head[q] BY <3>1, <2>m, <1>T
    <3>3. I_Pend'
      <4>. SUFFICES ASSUME NEW w \in pend
                   PROVE  \A q \in w.landed \cap members : head'[q][w.b] = w.v
        BY <1>0, <1>T, <1>u DEF I_Pend
      <4>. QED BY <3>2, <1>0, <1>T DEF I_Pend
    <3>4. I_Members' BY <1>0, <1>T, <1>u, <3>1 DEF I_Members, Healthy
    <3>5. I_Hold' BY <1>0, <1>u, <3>1, <3>2, <3>2b DEF I_Hold
    <3>. QED BY <1>0, <1>T, <1>u, <3>3, <3>4, <3>5, F_AckedEver
  <2>3. I_Ranges'
    <3>1. sPres'[r] \subseteq 1..nextSeq /\ tr'[r] \subseteq sPres'[r] \cap 1..(nextSeq - 1) /\ hPar'[r] \in sPres'[r]
      BY <2>n, <1>t, <1>0, <1>T DEF I_Ranges, Epochs
    <3>. QED BY <1>0, <1>T, <1>u, <2>m, <3>1 DEF I_Ranges
  <2>rd. \A x \in Blocks : x \notin sAl'[r][t] => sCon'[r][t][x] = Up(sPar[r], sCon[r], hPar[r], x)
    <3>. SUFFICES ASSUME NEW x \in Blocks, x \notin sAl'[r][t] PROVE sCon'[r][t][x] = Up(sPar[r], sCon[r], hPar[r], x)
      OBVIOUS
    <3>1. x \notin hAl[r] /\ x \notin Copied(s, b, t) BY <2>n
    <3>2. NH(r, s, b, t)[x] = head[r][x] BY <3>1, <2>F
    <3>. QED BY <3>1, <3>2, <2>n, <1>0, <1>T DEF I_Chain
  <2>LE. ChainShape(sPres'[r], sPar'[r], anc'[r]) /\ ChainReads(sPres'[r], sPar'[r], sAl'[r], sCon'[r])
    <3>1. anc'[r][t] = {t} \cup (IF hPar[r] = 0 THEN {} ELSE anc[r][hPar[r]]) BY <2>n DEF NewAnc, AncOf
    <3>. QED BY <1>0, <1>T, <1>t, <2>0, <2>n, <2>o, <2>rd, <3>1, LegExtend
  <2>4. I_Lineage'
    <3>1. ChainShape(sPres'[r], sPar'[r], anc'[r]) BY <2>LE
    <3>2. \A e \in Epochs \ sPres'[r] : anc'[r][e] = {} BY <2>c, <2>n, <1>0, <1>T, Groups DEF TySnap, I_Lineage
    <3>. QED BY <1>0, <1>T, <2>m, <3>1, <3>2 DEF I_Lineage
  <2>5. I_Chain'
    <3>1. ChainReads(sPres'[r], sPar'[r], sAl'[r], sCon'[r])
      <4>. QED BY <2>LE
    <3>2. \A x \in Blocks : head'[r][x] = Up(sPar'[r], sCon'[r], hPar'[r], x) BY <2>n, <1>t DEF Up
    <3>. QED BY <1>0, <1>T, <2>m, <3>1, <3>2 DEF I_Chain
  <2>6. /\ I_WHi' /\ Inv_TrustComplete' /\ I_Legal' /\ I_PendSeen' /\ I_Vers'
        /\ I_HeadHi' /\ I_HeadLo' /\ I_HeadPend'
    <3>1. \A x \in Blocks : \/ NH(r, s, b, t)[x] = sCon[s][t][x]
                            \/ NH(r, s, b, t)[x] = head[r][x] /\ sCon[s][t][x] <= head[r][x]
      BY <2>F
    <3>2. \A x \in Blocks : head[r][x] <= cutAck[s][t][x] BY <2>F, <2>0
    <3>. QED BY <2>sv, <1>t, <2>b, <2>0, <2>cu, <2>cf, <3>1, <3>2, <2>F, <2>pv, <2>n, <2>o, <2>m,
                <2>cu2, <1>u, CopySnap
  <2>7. I_Reverted'
    <3>1. \A q \in R \ {r} : RevOK(q)' BY <1>0, <1>T, <2>m, <2>s, <1>u, RevOther DEF I_Reverted
    <3>2. RevOK(r)' BY <1>g, <2>s DEF RevOK
    <3>. QED BY <3>1, <3>2 DEF I_Reverted
  <2>8. I_Session'
    <3>1. \A q \in R \ {r} : SessionOK(q)' BY <1>0, <1>T, <1>u, <2>ms, <2>m, <2>s, SessionOther DEF I_Session
    <3>2. SessionOK(r)'
      <4>1. \A e \in sPres'[r] : e <= t \/ \A f \in recorded : e < f BY <1>sb, <2>n, <2>0, <2>N, <2>b, <1>T
      <4>2. nextSeq \notin sPres'[r] BY <1>so, <1>g, <2>n, <1>t DEF SessionOK, CatchingUp
      <4>. QED BY <2>ms, <1>t, <2>n, <4>1, <4>2, <1>u, <1>g, <1>sb, <1>so, <2>s, <2>cu2 DEF SessionOK, CatchingUp
    <3>. QED BY <3>1, <3>2 DEF I_Session
  <2>9. I_Fresh'
    <3>1. Bases' = Bases /\ r \notin Bases BY <2>s, <1>u, <1>g, <1>sb DEF Bases, InSyncR
    <3>2. \A q \in Bases : sPres'[q] = sPres[q] /\ sCon'[q] = sCon[q] /\ cutAck'[q] = cutAck[q] BY <3>1, <2>m, <1>T
    <3>. QED BY <3>1, <3>2, <1>u, <1>0, <1>T DEF I_Fresh
  <2>. QED BY <1>0, <1>T, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <1>u, IndSplitP
<1>. QED BY <1>5, <1>6

-----------------------------------------------------------------------------
(* The consumer: NodeUnstage with the capture, and NodeStage.               *)

LEMMA I_Unstage ==
  ASSUME IndInv, Unstage PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>. DEFINE M == {q \in InSyncR : q \notin members}
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, path, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx, cfgAt, cutAck>>
  BY DEF Unstage
<1>p. /\ attached /\ pend = {} /\ cutK = 0
      /\ attached' = FALSE /\ members' = {}
      /\ dState' = [q \in R |-> IF q \in M THEN "stale" ELSE dState[q]]
      /\ conf' = [e \in Epochs |-> TRUE]
      /\ tr' = [q \in R |-> tr[q] \cup {e \in Epochs : ~conf[e] /\ q \in cfgAt[e] \cap members /\ e \in sPres[q]}]
  BY Clean, QuorumTrue DEF Unstage, StaleMarks
<1>m. members \subseteq InSyncR /\ Healthy # {} BY <1>0, <1>T, <1>p DEF I_Members
<1>i. InSyncR' = InSyncR \cap members BY <1>p, <1>m, <1>0, <1>T, Groups DEF InSyncR, TyLeg, TySets
<1>c. \A q \in R : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ ~(q \in M /\ CatchingUp(q))
  <2>. SUFFICES ASSUME NEW q \in R
               PROVE  CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ ~(q \in M /\ CatchingUp(q))
    OBVIOUS
  <2>1. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>2. CASE q \in M
    <3>1. dState[q] = "insync" /\ reverted[q] = -1 /\ dState'[q] = "stale" BY <2>1, <2>2, <1>p DEF SessionOK, InSyncR
    <3>. QED BY <3>1, <1>u DEF CatchingUp, HiOf
  <2>3. CASE q \notin M BY <2>3, <1>p, <1>u DEF CatchingUp, HiOf
  <2>. QED BY <2>2, <2>3
<1>1. TypeOK' BY T_Unstage DEF IndInv
<1>2. I_Lineage' /\ I_Chain' /\ I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' /\ I_WHi'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Legal, F_PendSeen, F_AckedEver, F_Vers, F_WHi
<1>3. I_Ranges'
  <2>1. \A q \in R : tr'[q] \subseteq sPres[q] \cap 1..(nextSeq - 1)
    <3>. SUFFICES ASSUME NEW q \in R, NEW e \in tr'[q] PROVE e \in sPres[q] /\ e \in 1..(nextSeq - 1) OBVIOUS
    <3>1. CASE e \in tr[q] BY <3>1, <1>0, <1>T DEF I_Ranges
    <3>2. CASE e \notin tr[q]
      <4>1. e \in Epochs /\ ~conf[e] /\ e \in sPres[q] BY <3>2, <1>p
      <4>2. e < nextSeq BY <4>1, <1>0, <1>T DEF I_Ranges
      <4>. QED BY <4>1, <4>2, <1>T, Const DEF Epochs
    <3>. QED BY <3>1, <3>2
  <2>2. \A e \in Epochs : conf'[e] BY <1>p
  <2>. QED BY <1>0, <1>T, <1>u, <1>p, <2>1, <2>2 DEF I_Ranges
<1>4. I_Pend' /\ I_Members' BY <1>p, <1>u DEF I_Pend, I_Members
<1>5. I_Hold'
  <2>1. InSyncR' # {}
    <3>1. PICK h \in Healthy : TRUE BY <1>m
    <3>. QED BY <3>1, <1>m, <1>i DEF Healthy
  <2>2. \A q \in InSyncR' : \A x \in Blocks : head[q][x] >= acked[x] BY <1>0, <1>T, <1>p, <1>i DEF I_Hold
  <2>. QED BY <1>p, <1>u, <2>1, <2>2 DEF I_Hold
<1>6. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend' BY <1>0, <1>T, <1>u, <1>c DEF I_HeadHi, I_HeadLo, I_HeadPend
<1>7. I_Reverted'
  <2>. SUFFICES ASSUME NEW q \in R PROVE RevOK(q)' BY DEF I_Reverted
  <2>1. CASE q \in M BY <2>1, <1>p, <1>0, <1>T, <1>u DEF I_Session, SessionOK, RevOK, InSyncR
  <2>2. CASE q \notin M BY <2>2, <1>p, <1>0, <1>T, <1>u DEF I_Reverted, RevOK
  <2>. QED BY <2>1, <2>2
<1>8. I_Session'
  <2>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
  <2>0. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>t. tr[q] \subseteq tr'[q] BY <1>p
  <2>1. CASE q \in M
    <3>1. reverted[q] = -1 /\ sb[q] = -1 /\ dState'[q] = "stale" /\ ~CatchingUp(q)'
      BY <2>1, <2>0, <1>p, <1>c DEF SessionOK, InSyncR
    <3>. QED BY <3>1, <1>u DEF SessionOK, CatchingUp
  <2>2. CASE q \notin M
    <3>1. dState'[q] = dState[q] BY <2>2, <1>p
    <3>2. dState[q] = "standby" => q \notin members' BY <1>p
    <3>. QED BY <2>0, <2>t, <3>1, <3>2, <1>u, <1>p, <1>c DEF SessionOK, CatchingUp
  <2>. QED BY <2>1, <2>2
<1>9. Inv_TrustComplete'
  <2>. SUFFICES ASSUME NEW q \in R, NEW e \in tr'[q], NEW x \in Blocks
               PROVE  sCon[q][e][x] >= cutAck[q][e][x]
    BY <1>u DEF Inv_TrustComplete
  <2>1. CASE e \in tr[q] BY <2>1, <1>0, <1>T DEF Inv_TrustComplete
  <2>2. CASE e \notin tr[q]
    <3>1. ~conf[e] /\ q \in members /\ e \in sPres[q] BY <2>2, <1>p
    <3>. QED BY <3>1, <1>0, <1>T, <1>p DEF I_Fresh, Bases
  <2>. QED BY <2>1, <2>2
<1>10. I_Fresh'
  <2>. SUFFICES ASSUME NEW q \in Bases', NEW e \in sPres[q], e >= nextSeq \/ ~conf'[e], NEW x \in Blocks
               PROVE  sCon[q][e][x] >= cutAck[q][e][x]
    BY <1>u DEF I_Fresh
  <2>a. q \in R BY <1>p, <1>i, <1>T DEF Bases
  <2>0. e \in Epochs BY <2>a, <1>T
  <2>1. q \in members /\ e >= nextSeq BY <2>0, <1>p, <1>i DEF Bases
  <2>. QED BY <2>1, <1>0, <1>T, <1>p DEF I_Fresh, Bases
<1>. QED BY <1>0, <1>T, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>u, IndSplitP

LEMMA I_Stage ==
  ASSUME IndInv, Stage PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>. DEFINE T == Attachable
<1>. DEFINE M == AsmExcl(T)
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, mark, reverted, sb, pin>>
      /\ UNCHANGED <<pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx, cfgAt, conf, tr, cutAck>>
  BY DEF Stage
<1>p. /\ ~attached /\ T # {}
      /\ attached' = TRUE /\ members' = T
      /\ path' = [q \in R |-> IF q \in T THEN "ok" ELSE path[q]]
      /\ dState' = [q \in R |-> IF q \in M THEN "stale" ELSE dState[q]]
  BY Clean DEF Stage, StaleMarks
<1>r. ~attached => pend = {} /\ cutK = 0 /\ members = {} /\ \A e \in Epochs : conf[e]
  BY <1>0, <1>T DEF I_Ranges
<1>z. zx = 0 BY <1>0, <1>T DEF I_Ranges
<1>t. T = {q \in InSyncR : path[q] # "out"} /\ T \subseteq InSyncR /\ InSyncR \subseteq R
  BY <1>z, Clean, RNat DEF Attachable, Reachable, InSyncR
<1>x. M \subseteq InSyncR \ T BY Clean DEF AsmExcl
<1>i. T \subseteq InSyncR' BY <1>p, <1>t, <1>x DEF InSyncR
<1>c. \A q \in R : CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ ~(q \in M /\ CatchingUp(q))
      /\ (CatchingUp(q) => q \notin T)
  <2>. SUFFICES ASSUME NEW q \in R
               PROVE  /\ CatchingUp(q)' = CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ ~(q \in M /\ CatchingUp(q))
                      /\ (CatchingUp(q) => q \notin T)
    OBVIOUS
  <2>1. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>2. CASE q \in M
    <3>1. dState[q] = "insync" /\ reverted[q] = -1 /\ dState'[q] = "stale" BY <2>1, <2>2, <1>p, <1>x DEF SessionOK, InSyncR
    <3>. QED BY <3>1, <1>u DEF CatchingUp, HiOf
  <2>3. CASE q \notin M BY <2>3, <1>p, <1>u, <1>t DEF CatchingUp, HiOf, InSyncR
  <2>. QED BY <2>2, <2>3
<1>1. TypeOK' BY T_Stage DEF IndInv
<1>2. I_Lineage' /\ I_Chain' /\ I_Legal' /\ I_PendSeen' /\ I_AckedEver' /\ I_Vers' /\ I_WHi'
      /\ Inv_TrustComplete'
  BY <1>0, <1>T, <1>u, F_Lineage, F_Chain, F_Legal, F_PendSeen, F_AckedEver, F_Vers, F_WHi, F_TC
<1>3. I_Ranges' BY <1>0, <1>T, <1>u, <1>p, <1>r DEF I_Ranges
<1>4. I_Pend' BY <1>p, <1>r, <1>u DEF I_Pend
<1>5. I_Members'
  <2>1. Healthy' = T BY <1>p, <1>t DEF Healthy
  <2>. QED BY <1>p, <1>i, <2>1 DEF I_Members
<1>6. I_Hold' BY <1>0, <1>T, <1>p, <1>u, <1>t, <1>r DEF I_Hold, InSyncR
<1>7. I_HeadHi' /\ I_HeadLo' /\ I_HeadPend' BY <1>0, <1>T, <1>u, <1>c DEF I_HeadHi, I_HeadLo, I_HeadPend
<1>8. I_Reverted'
  <2>. SUFFICES ASSUME NEW q \in R PROVE RevOK(q)' BY DEF I_Reverted
  <2>1. CASE q \in M BY <2>1, <1>p, <1>0, <1>T, <1>u, <1>x DEF I_Session, SessionOK, RevOK, InSyncR
  <2>2. CASE q \notin M BY <2>2, <1>p, <1>0, <1>T, <1>u DEF I_Reverted, RevOK
  <2>. QED BY <2>1, <2>2
<1>9. I_Session'
  <2>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
  <2>0. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>1. CASE q \in M
    <3>1. reverted[q] = -1 /\ sb[q] = -1 /\ dState'[q] = "stale" /\ ~CatchingUp(q)'
      BY <2>1, <2>0, <1>p, <1>c, <1>x DEF SessionOK, InSyncR
    <3>. QED BY <3>1, <1>u DEF SessionOK, CatchingUp
  <2>2. CASE q \notin M
    <3>1. dState'[q] = dState[q] BY <2>2, <1>p
    <3>2. dState[q] = "standby" => q \notin members' /\ path'[q] = path[q] BY <1>p, <1>t DEF InSyncR
    <3>3. CatchingUp(q) => path'[q] = path[q] BY <1>p, <1>c
    <3>. QED BY <2>0, <3>1, <3>2, <3>3, <1>u, <1>p, <1>c DEF SessionOK, CatchingUp
  <2>. QED BY <2>1, <2>2
<1>10. I_Fresh'
  <2>. SUFFICES ASSUME NEW q \in Bases', NEW e \in sPres[q], e >= nextSeq \/ ~conf[e], NEW x \in Blocks
               PROVE  sCon[q][e][x] >= cutAck[q][e][x]
    BY <1>u DEF I_Fresh
  <2>a. q \in R BY <1>p, <1>t DEF Bases
  <2>0. e \in Epochs BY <2>a, <1>T
  <2>1. q \in InSyncR /\ e >= nextSeq BY <2>0, <1>p, <1>t, <1>r DEF Bases
  <2>. QED BY <2>1, <1>0, <1>T, <1>p DEF I_Fresh, Bases
<1>. QED BY <1>0, <1>T, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>u, IndSplitP

-----------------------------------------------------------------------------
(* Stage admission: the final delta from the standby's mark through the    *)
(* source's head holds every acked write.  For a block the source wrote    *)
(* from the mark on, the delta is the source's head; for any other block   *)
(* the source's head reads an epoch older than the mark, which the         *)
(* standby's head already holds (HeadLo).                                  *)

LEMMA AdmHold ==
  ASSUME IndInv, NEW r \in R, NEW s \in R, AdmGuard(r, s), NEW x \in Blocks
  PROVE  /\ AdmHead(r, s)[x] >= acked[x]
         /\ AdmHead(r, s)[x] = head[s][x] \/ AdmHead(r, s)[x] = head[r][x]
         /\ \/ AdmHead(r, s)[x] = head[s][x]
            \/ \E e \in sPres[s] : AdmHead(r, s)[x] = sCon[s][e][x]
            \/ AdmHead(r, s)[x] = head[r][x]
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>g. /\ ~attached /\ dState[r] = "standby" /\ s \in AdmT /\ LinCovers(s, mark[r])
      /\ AdmK \notin sPres[s] /\ AdmK = nextSeq
  BY DEF AdmGuard, AdmK
<1>d. pend = {} BY <1>0, <1>g DEF I_Ranges
<1>si. s \in InSyncR BY <1>g, <1>0, Clean DEF AdmT, Attachable, I_Ranges
<1>hs. head[s][x] >= acked[x] /\ head[s][x] \in Nat /\ acked[x] \in Nat /\ head[r][x] \in Nat
  BY <1>0, <1>g, <1>si, Groups DEF I_Hold, TyHead, TyPend, Vers
<1>m. mark[r] >= 1 /\ HiOf(r) = mark[r] /\ CatchingUp(r) BY <1>0, <1>g DEF I_Session, SessionOK, HiOf, CatchingUp
<1>mn. mark[r] \in 0..MaxEpoch /\ nextSeq \in 1..(MaxEpoch + 1) /\ MaxEpoch \in Nat
  BY <1>0, Const, Groups DEF TyLeg, TyScalar
<1>l. mark[r] \in Lin(s) BY <1>g, <1>m, <1>mn DEF LinCovers
<1>h. hPar[s] # 0 /\ hPar[s] \in sPres[s] /\ Lin(s) = anc[s][hPar[s]]
  BY <1>l, <1>0 DEF Lin, AncOf, I_Ranges
<1>c. ChainShape(sPres[s], sPar[s], anc[s]) /\ ChainReads(sPres[s], sPar[s], sAl[s], sCon[s])
  BY <1>0, LegChain
<1>sp. /\ sPres[s] \subseteq 1..(nextSeq - 1) /\ Lin(s) \subseteq sPres[s]
       /\ AdmK \in Epochs /\ Lin(s) \subseteq Nat
  <2>1. sPres[s] \subseteq 1..nextSeq BY <1>0 DEF I_Ranges
  <2>2. Lin(s) \subseteq sPres[s] BY <1>0, LinSub
  <2>3. AdmK \in Epochs
    <3>1. nextSeq <= MaxEpoch BY DEF AdmGuard
    <3>. QED BY <3>1, <1>mn, <1>g DEF Epochs
  <2>. QED BY <2>1, <2>2, <2>3, <1>g, <1>mn DEF Epochs
<1>. DEFINE Hs == {e \in Lin(s) : mark[r] <= e /\ x \in sAl[s][e]}
<1>1. CASE x \in hAl[s]
  <2>0. mark[r] < nextSeq /\ mark[r] \in Nat /\ nextSeq \in Nat BY <1>0, <1>g, <1>mn DEF I_Session, SessionOK
  <2>1. AdmK \in AdmHits(r, s, x) BY <2>0, <1>1, <1>g DEF AdmHits, AdmElems, AdmAl
  <2>2. AdmHits(r, s, x) \subseteq 0..MaxEpoch /\ \A e \in AdmHits(r, s, x) : e <= AdmK
    <3>1. AdmElems(r, s) \subseteq Lin(s) \cup {AdmK} BY DEF AdmElems
    <3>2. Lin(s) \cup {AdmK} \subseteq Epochs BY <1>sp, <1>mn DEF Epochs
    <3>. QED BY <3>1, <3>2 DEF AdmHits, AdmElems, Epochs
  <2>3. Max(AdmHits(r, s, x)) = AdmK
    <3>1. Max(AdmHits(r, s, x)) \in AdmHits(r, s, x) /\ \A u \in AdmHits(r, s, x) : u <= Max(AdmHits(r, s, x))
      BY <2>1, <2>2, MaxProp, Const
    <3>. QED BY <3>1, <2>1, <2>2, <1>g, <1>0, Groups DEF TyScalar
  <2>4. AdmHead(r, s)[x] = head[s][x] BY <2>1, <2>3 DEF AdmHead, AdmCon
  <2>. QED BY <2>4, <1>hs
<1>2. CASE x \notin hAl[s]
  <2>0. head[s][x] = sCon[s][hPar[s]][x] BY <1>2, <1>h, <1>0 DEF I_Chain, Up
  <2>1. AdmHits(r, s, x) = Hs
    <3>1. \A e \in Lin(s) : e < AdmK /\ e # AdmK /\ e \in Nat BY <1>sp, <1>g, <1>mn
    <3>2. AdmK \notin {e \in Lin(s) \cup {AdmK} : mark[r] <= e /\ e <= AdmK /\ x \in AdmAl(s, e)} BY <1>2 DEF AdmAl
    <3>. QED BY <3>1, <3>2 DEF AdmHits, AdmElems, AdmAl
  <2>2. /\ Hs = {} => sCon[s][hPar[s]][x] = Up(sPar[s], sCon[s], sPar[s][mark[r]], x)
        /\ Hs # {} => Max(Hs) \in Hs /\ sCon[s][hPar[s]][x] = sCon[s][Max(Hs)][x]
    <3>1. mark[r] \in anc[s][hPar[s]] BY <1>l, <1>h
    <3>2. RT(anc[s], sAl[s], hPar[s], mark[r], x) = Hs BY <1>h DEF RT
    <3>3. /\ RT(anc[s], sAl[s], hPar[s], mark[r], x) = {} =>
              sCon[s][hPar[s]][x] = Up(sPar[s], sCon[s], sPar[s][mark[r]], x)
          /\ RT(anc[s], sAl[s], hPar[s], mark[r], x) # {} =>
              /\ Max(RT(anc[s], sAl[s], hPar[s], mark[r], x)) \in RT(anc[s], sAl[s], hPar[s], mark[r], x)
              /\ sCon[s][hPar[s]][x] = sCon[s][Max(RT(anc[s], sAl[s], hPar[s], mark[r], x))][x]
      <4>1. hPar[s] \in sPres[s] /\ mark[r] \in anc[s][hPar[s]] BY <1>h, <3>1
      <4>. QED BY <1>c, <4>1, ReadAt
    <3>. QED BY <3>2, <3>3
  <2>3. CASE Hs # {}
    <3>1. Max(Hs) # AdmK /\ Max(Hs) \in sPres[s] BY <2>2, <2>3, <1>sp, <1>g
    <3>2. AdmHead(r, s)[x] = sCon[s][Max(Hs)][x] BY <2>1, <2>3, <3>1 DEF AdmHead, AdmCon
    <3>. QED BY <3>1, <3>2, <2>0, <2>2, <2>3, <1>hs
  <2>4. CASE Hs = {}
    <3>1. AdmHead(r, s)[x] = head[r][x] BY <2>1, <2>4 DEF AdmHead
    <3>2. head[s][x] <= head[r][x]
      <4>1. mark[r] \in sPres[s] /\ sPar[s][mark[r]] < mark[r]
            /\ (sPar[s][mark[r]] # 0 => sPar[s][mark[r]] \in sPres[s])
        BY <1>l, <1>sp, <1>c DEF ChainShape
      <4>2. CASE sPar[s][mark[r]] = 0 BY <4>2, <2>0, <2>2, <2>4, <1>hs DEF Up
      <4>3. CASE sPar[s][mark[r]] # 0
        <5>1. sCon[s][sPar[s][mark[r]]][x] <= head[r][x] BY <4>1, <4>3, <1>m, <1>0 DEF I_HeadLo
        <5>. QED BY <5>1, <4>3, <2>0, <2>2, <2>4 DEF Up
      <4>. QED BY <4>2, <4>3
    <3>. QED BY <3>1, <3>2, <1>hs
  <2>. QED BY <2>3, <2>4
<1>. QED BY <1>1, <1>2

LEMMA I_StageAdmit ==
  ASSUME IndInv, NEW r \in R, StageAdmit(r) PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>s. PICK s \in R : AdmGuard(r, s) /\ AdmSnap(r, s) /\ AdmRest(r, s) BY StageAdmitFacts
<1>. DEFINE T == AdmT
<1>. DEFINE k == AdmK
<1>. DEFINE dH == AdmHead(r, s)
<1>. DEFINE U == T \cup {r}
<1>g. /\ ~attached /\ cutK = 0 /\ dState[r] = "standby" /\ path[r] = "back"
      /\ nextSeq <= MaxEpoch /\ T # {} /\ \A q \in T : k \notin sPres[q] /\ k \notin sPres[r]
      /\ s \in T /\ k = nextSeq
  BY <1>s DEF AdmGuard, AdmK
<1>r. pend = {} /\ members = {} /\ \A e \in Epochs : conf[e] BY <1>0, <1>T, <1>g DEF I_Ranges
<1>z. zx = 0 BY <1>0, <1>T DEF I_Ranges
<1>t. /\ T \subseteq InSyncR /\ r \notin T /\ T \subseteq R /\ U \subseteq R /\ InSyncR \subseteq R
      /\ \A q \in T : dState[q] = "insync"
  BY <1>g, <1>z, Clean, RNat DEF AdmT, Attachable, InSyncR
<1>k. k \in Epochs /\ k \in Nat /\ nextSeq \in Nat BY <1>g, <1>0, <1>T, Const, Groups DEF TyScalar, Epochs
<1>kU. \A q \in U : k \notin sPres[q] BY <1>g
<1>sp. \A q \in R : sPres[q] \subseteq 1..nextSeq /\ sPres[q] \subseteq Epochs
  BY <1>0, <1>T, Groups DEF I_Ranges, TySnap
<1>hp. \A q \in U : hPar[q] \in 0..(k - 1) /\ (hPar[q] # 0 => hPar[q] \in sPres[q])
  <2>. SUFFICES ASSUME NEW q \in U PROVE hPar[q] \in 0..(k - 1) /\ (hPar[q] # 0 => hPar[q] \in sPres[q]) OBVIOUS
  <2>1. hPar[q] \in sPres[q] \cup {0} /\ q \in R BY <1>0, <1>T, <1>t DEF I_Ranges
  <2>2. hPar[q] # k BY <2>1, <1>kU, <1>k DEF Epochs
  <2>. QED BY <2>1, <2>2, <1>sp, <1>k, <1>g
<1>H. \A x \in Blocks : /\ dH[x] >= acked[x]
                        /\ \/ dH[x] = head[s][x]
                           \/ \E e \in sPres[s] : dH[x] = sCon[s][e][x]
                           \/ dH[x] = head[r][x]
  BY <1>s, AdmHold
<1>L. \A q \in R : \A x \in Blocks : /\ head[q][x] <= acked[x] /\ head[q][x] \in ackedEver[x]
                                    /\ \A e \in sPres[q] : sCon[q][e][x] <= acked[x] /\ sCon[q][e][x] \in ackedEver[x]
  BY <1>0, <1>T, <1>r DEF I_Legal, PendVal
<1>dl. \A x \in Blocks : dH[x] <= acked[x] /\ dH[x] \in ackedEver[x] /\ dH[x] \in Nat
  <2>1. dH \in [Blocks -> Vers] BY <1>0, <1>T, <1>g, AdmHeadType, Groups
  <2>. QED BY <2>1, <1>H, <1>L, <1>t DEF Vers
<1>rest. /\ badAdmit' = (badAdmit \/ dH # head[s])
         /\ badLoss' = (badLoss \/ \E x \in Blocks :
                          \/ head[s][x] >= acked[x] /\ dH[x] < acked[x]
                          \/ dH[x] \notin ackedEver[x] \cup {head[q][x] : q \in T})
         /\ tr' = [q \in R |-> IF (q \in T /\ htr[q]) \/ (q = r /\ htr[s]) THEN tr[q] \cup {k} ELSE tr[q] \ {k}]
         /\ cutAck' = [q \in R |-> IF q \in U THEN [cutAck[q] EXCEPT ![k] = acked] ELSE cutAck[q]]
         /\ recorded' = recorded \cup {k} /\ nextSeq' = nextSeq + 1
         /\ mark' = [q \in R |-> IF q \in U THEN k ELSE mark[q]]
         /\ members' = U
         /\ path' = [q \in R |-> IF q \in U THEN "ok" ELSE path[q]]
         /\ dState' = [q \in R |-> IF q = r THEN "insync" ELSE IF q \in AsmExcl(T) THEN "stale" ELSE dState[q]]
         /\ reverted' = [reverted EXCEPT ![r] = -1] /\ pin' = [pin EXCEPT ![r] = 0]
         /\ attached' = TRUE
         /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, cutGone, sb, pend, acked, fails, cfgAt, conf, zx, ackedEver>>
  BY <1>s DEF AdmRest
<1>snap. /\ sPres' = [q \in R |-> IF q \in U THEN sPres[q] \cup {k} ELSE sPres[q]]
         /\ sCon' = [q \in R |-> IF q \in T THEN [sCon[q] EXCEPT ![k] = head[q]]
                                 ELSE IF q = r THEN [sCon[q] EXCEPT ![k] = dH] ELSE sCon[q]]
         /\ sAl' = [q \in R |-> IF q \in T THEN [sAl[q] EXCEPT ![k] = hAl[q]]
                                ELSE IF q = r THEN [sAl[q] EXCEPT ![k] = AdmAlD(r, s)] ELSE sAl[q]]
         /\ sPar' = [q \in R |-> IF q \in U THEN [sPar[q] EXCEPT ![k] = hPar[q]] ELSE sPar[q]]
         /\ anc' = [q \in R |-> IF q \in U THEN [anc[q] EXCEPT ![k] = NewAnc(q, k)] ELSE anc[q]]
         /\ hPar' = [q \in R |-> IF q \in U THEN k ELSE hPar[q]]
         /\ hAl' = [q \in R |-> IF q \in U THEN {} ELSE hAl[q]]
         /\ head' = [head EXCEPT ![r] = dH]
  BY <1>s DEF AdmSnap
<1>ex. AsmExcl(T) \subseteq InSyncR \ T /\ r \notin AsmExcl(T)
  BY Clean, <1>g DEF AsmExcl, InSyncR
<1>7. \A q \in R \ U : /\ sPres'[q] = sPres[q] /\ sCon'[q] = sCon[q] /\ sAl'[q] = sAl[q]
                       /\ sPar'[q] = sPar[q] /\ anc'[q] = anc[q] /\ hPar'[q] = hPar[q]
                       /\ hAl'[q] = hAl[q] /\ head'[q] = head[q] /\ cutAck'[q] = cutAck[q]
                       /\ mark'[q] = mark[q] /\ path'[q] = path[q] /\ pin'[q] = pin[q]
                       /\ reverted'[q] = reverted[q]
  BY <1>snap, <1>rest, <1>0, <1>T, Groups DEF TyHead, TyLeg
<1>8. ASSUME NEW q \in U
      PROVE  /\ sPres'[q] = sPres[q] \cup {k} /\ hPar'[q] = k /\ hAl'[q] = {}
             /\ sPar'[q][k] = hPar[q] /\ anc'[q][k] = NewAnc(q, k)
             /\ cutAck'[q][k] = acked /\ mark'[q] = k /\ path'[q] = "ok"
             /\ \A e \in sPres[q] : /\ sCon'[q][e] = sCon[q][e] /\ sAl'[q][e] = sAl[q][e]
                                   /\ sPar'[q][e] = sPar[q][e] /\ anc'[q][e] = anc[q][e]
                                   /\ cutAck'[q][e] = cutAck[q][e]
  <2>1. q \in R BY <1>t
  <2>2. \A e \in sPres[q] : e # k BY <1>kU
  <2>. QED BY <2>1, <2>2, <1>snap, <1>rest, <1>k, <1>0, <1>T, Groups DEF TySnap, TyTrust
<1>9. /\ \A q \in T : sCon'[q][k] = head[q] /\ sAl'[q][k] = hAl[q] /\ head'[q] = head[q]
      /\ sCon'[r][k] = dH /\ sAl'[r][k] = AdmAlD(r, s) /\ head'[r] = dH
  BY <1>snap, <1>t, <1>k, <1>0, <1>T, Groups DEF TySnap, TyHead
<1>tr. \A q \in R : k \notin tr[q] BY <1>0, <1>T, <1>g, <1>k DEF I_Ranges
<1>lq. ASSUME NEW q \in U
      PROVE  ChainShape(sPres'[q], sPar'[q], anc'[q]) /\ ChainReads(sPres'[q], sPar'[q], sAl'[q], sCon'[q])
  <2>1. anc'[q][k] = {k} \cup (IF hPar[q] = 0 THEN {} ELSE anc[q][hPar[q]]) BY <1>8 DEF NewAnc, AncOf
  <2>2. hPar[q] < k /\ k \notin sPres[q] /\ q \in R BY <1>hp, <1>kU, <1>t, <1>k
  <2>3. \A x \in Blocks : x \notin sAl'[q][k] => sCon'[q][k][x] = Up(sPar[q], sCon[q], hPar[q], x)
    <3>1. \A x \in Blocks : x \notin hAl[q] => head[q][x] = Up(sPar[q], sCon[q], hPar[q], x) BY <1>0, <1>T, <2>2 DEF I_Chain
    <3>2. CASE q \in T BY <3>2, <1>9, <3>1
    <3>3. CASE q = r
      <4>. SUFFICES ASSUME NEW x \in Blocks, x \notin AdmAlD(r, s) PROVE dH[x] = head[r][x] /\ x \notin hAl[r]
        BY <3>3, <1>9, <3>1
      <4>1. x \notin hAl[r] /\ AdmHits(r, s, x) = {} BY DEF AdmAlD, AdmHits
      <4>. QED BY <4>1 DEF AdmHead
    <3>. QED BY <3>2, <3>3
  <2>. QED BY <1>0, <1>T, <1>k, <1>8, <2>1, <2>2, <2>3, LegExtend
<1>1. TypeOK' BY T_StageAdmit DEF IndInv
<1>2. ~badLoss'
  <2>1. \A x \in Blocks : ~(head[s][x] >= acked[x] /\ dH[x] < acked[x])
    <3>. SUFFICES ASSUME NEW x \in Blocks PROVE ~(head[s][x] >= acked[x] /\ dH[x] < acked[x]) OBVIOUS
    <3>1. acked[x] \in Nat BY <1>0, <1>T, Nats
    <3>. QED BY <3>1, <1>H, <1>dl
  <2>. QED BY <1>0, <1>T, <1>rest, <1>dl, <2>1
<1>3. I_Pend' /\ I_PendSeen' /\ I_HeadPend' BY <1>rest, <1>r DEF I_Pend, I_PendSeen, I_HeadPend
<1>4. I_AckedEver' BY <1>0, <1>T, <1>rest, F_AckedEver
<1>5. I_Members'
  <2>1. U \subseteq InSyncR' BY <1>rest, <1>t, <1>ex DEF InSyncR
  <2>2. Healthy' = U BY <1>rest, <1>t DEF Healthy
  <2>. QED BY <1>rest, <2>1, <2>2 DEF I_Members
<1>6. I_Hold'
  <2>1. \A q \in T : \A x \in Blocks : head'[q][x] >= acked[x] BY <1>0, <1>T, <1>g, <1>t, <1>9 DEF I_Hold
  <2>2. \A x \in Blocks : head'[r][x] >= acked[x] BY <1>H, <1>9
  <2>. QED BY <1>rest, <2>1, <2>2 DEF I_Hold
<1>10. I_Ranges'
  <2>1. \A q \in R : tr'[q] \subseteq sPres'[q] \cap 1..(nextSeq' - 1)
    <3>. SUFFICES ASSUME NEW q \in R, NEW e \in tr'[q] PROVE e \in sPres'[q] /\ e \in 1..(nextSeq' - 1) OBVIOUS
    <3>1. CASE e = k /\ q \in U BY <3>1, <1>8, <1>k, <1>g, <1>rest DEF Epochs
    <3>2. CASE ~(e = k /\ q \in U)
      <4>1. e \in tr[q] BY <3>2, <1>rest
      <4>2. e \in sPres[q] /\ e \in 1..(nextSeq - 1) BY <4>1, <1>0, <1>T DEF I_Ranges
      <4>3. e \in sPres'[q] BY <4>2, <1>7, <1>8
      <4>. QED BY <4>2, <4>3, <1>rest, <1>k
    <3>. QED BY <3>1, <3>2
  <2>2. \A q \in R : sPres'[q] \subseteq 1..nextSeq' /\ hPar'[q] \in sPres'[q] \cup {0}
    <3>. SUFFICES ASSUME NEW q \in R PROVE sPres'[q] \subseteq 1..nextSeq' /\ hPar'[q] \in sPres'[q] \cup {0} OBVIOUS
    <3>1. CASE q \in U BY <3>1, <1>8, <1>sp, <1>k, <1>g, <1>rest DEF Epochs
    <3>2. CASE q \notin U BY <3>2, <1>7, <1>sp, <1>k, <1>rest, <1>0, <1>T DEF I_Ranges
    <3>. QED BY <3>1, <3>2
  <2>3. recorded' \subseteq 1..(nextSeq' - 1) BY <1>rest, <1>0, <1>T, <1>k, <1>g DEF I_Ranges, Epochs
  <2>. QED BY <1>rest, <1>r, <1>z, <1>g, <2>1, <2>2, <2>3 DEF I_Ranges
<1>11. I_Lineage'
  <2>. SUFFICES ASSUME NEW q \in R
               PROVE  /\ ChainShape(sPres'[q], sPar'[q], anc'[q])
                      /\ \A e \in Epochs \ sPres'[q] : anc'[q][e] = {}
    BY DEF I_Lineage
  <2>1. CASE q \in U
    <3>1. ChainShape(sPres[q], sPar[q], anc[q]) BY <1>0, <1>T, LegChain, <1>t, <2>1
    <3>2. anc'[q][k] = {k} \cup (IF hPar[q] = 0 THEN {} ELSE anc[q][hPar[q]]) BY <1>8, <2>1 DEF NewAnc, AncOf
    <3>3. ChainShape(sPres'[q], sPar'[q], anc'[q]) BY <2>1, <1>lq
    <3>4. \A e \in Epochs \ sPres'[q] : anc'[q][e] = {}
      <4>. SUFFICES ASSUME NEW e \in Epochs \ sPres'[q] PROVE anc'[q][e] = {} OBVIOUS
      <4>1. e # k /\ e \notin sPres[q] BY <1>8, <2>1
      <4>2. anc'[q][e] = anc[q][e] BY <4>1, <1>snap, <2>1, <1>t, <1>0, <1>T, Groups DEF TySnap
      <4>. QED BY <4>1, <4>2, <1>0, <1>T, <1>t, <2>1 DEF I_Lineage
    <3>. QED BY <3>3, <3>4
  <2>2. CASE q \notin U BY <2>2, <1>7, <1>0, <1>T DEF I_Lineage
  <2>. QED BY <2>1, <2>2
<1>12. I_Chain'
  <2>. SUFFICES ASSUME NEW q \in R
               PROVE  /\ ChainReads(sPres'[q], sPar'[q], sAl'[q], sCon'[q])
                      /\ \A x \in Blocks : x \notin hAl'[q] => head'[q][x] = Up(sPar'[q], sCon'[q], hPar'[q], x)
    BY DEF I_Chain
  <2>1. CASE q \in U
    <3>1. ChainShape(sPres[q], sPar[q], anc[q]) /\ ChainReads(sPres[q], sPar[q], sAl[q], sCon[q])
      BY <1>0, <1>T, LegChain, <1>t, <2>1
    <3>2. \A x \in Blocks : x \notin hAl[q] => head[q][x] = Up(sPar[q], sCon[q], hPar[q], x) BY <1>0, <1>T, <1>t, <2>1 DEF I_Chain
    <3>4. \A x \in Blocks : x \notin sAl'[q][k] => sCon'[q][k][x] = Up(sPar[q], sCon[q], hPar[q], x)
      <4>1. CASE q \in T BY <4>1, <1>9, <3>2
      <4>2. CASE q = r
        <5>. SUFFICES ASSUME NEW x \in Blocks, x \notin AdmAlD(r, s) PROVE dH[x] = head[r][x] /\ x \notin hAl[r]
          BY <4>2, <1>9, <3>2
        <5>1. x \notin hAl[r] /\ AdmHits(r, s, x) = {} BY DEF AdmAlD, AdmHits
        <5>. QED BY <5>1 DEF AdmHead
      <4>. QED BY <4>1, <4>2, <2>1
    <3>5. ChainReads(sPres'[q], sPar'[q], sAl'[q], sCon'[q]) BY <2>1, <1>lq
    <3>6. \A x \in Blocks : head'[q][x] = sCon'[q][k][x] BY <1>9, <2>1
    <3>. QED BY <3>5, <3>6, <1>8, <2>1, <1>k DEF Up, Epochs
  <2>2. CASE q \notin U BY <2>2, <1>7, <1>0, <1>T DEF I_Chain
  <2>. QED BY <2>1, <2>2
<1>13. I_Legal' /\ I_Vers'
  <2>1. \A q \in R : \A x \in Blocks : head'[q][x] <= acked[x] /\ head'[q][x] \in ackedEver[x]
    <3>. SUFFICES ASSUME NEW q \in R, NEW x \in Blocks PROVE head'[q][x] <= acked[x] /\ head'[q][x] \in ackedEver[x] OBVIOUS
    <3>1. CASE q = r BY <3>1, <1>9, <1>dl
    <3>2. CASE q # r BY <3>2, <1>snap, <1>L, <1>0, <1>T, Groups DEF TyHead
    <3>. QED BY <3>1, <3>2
  <2>2. \A q \in R : \A e \in sPres'[q] : \A x \in Blocks : sCon'[q][e][x] <= acked[x] /\ sCon'[q][e][x] \in ackedEver[x]
    <3>. SUFFICES ASSUME NEW q \in R, NEW e \in sPres'[q], NEW x \in Blocks
                 PROVE  sCon'[q][e][x] <= acked[x] /\ sCon'[q][e][x] \in ackedEver[x]
      OBVIOUS
    <3>1. CASE q \in U /\ e = k BY <3>1, <1>9, <1>L, <1>dl, <1>t
    <3>2. CASE q \in U /\ e # k BY <3>2, <1>8, <1>L
    <3>3. CASE q \notin U BY <3>3, <1>7, <1>L
    <3>. QED BY <3>1, <3>2, <3>3
  <2>3. \A x \in Blocks : acked[x] <= ver BY <1>0, <1>T DEF I_Vers
  <2>4. I_Legal' BY <2>1, <2>2, <1>rest DEF I_Legal, PendVal
  <2>5. I_Vers'
    <3>1. \A x \in Blocks : acked[x] \in Nat BY <1>0, <1>T, Nats
    <3>. QED BY <2>1, <2>2, <2>3, <3>1, <1>rest, <1>dl, <1>0, <1>T, Nats DEF I_Vers
  <2>. QED BY <2>4, <2>5
<1>14. I_WHi'
  <2>. SUFFICES ASSUME NEW q1 \in R, NEW q2 \in R, NEW e1 \in sPres'[q1], NEW e2 \in sPres'[q2], e1 < e2,
                      NEW x \in Blocks
               PROVE  sCon'[q1][e1][x] <= cutAck'[q2][e2][x]
    BY DEF I_WHi
  <2>0. e1 \in Nat /\ e2 \in Nat /\ e1 <= k /\ e2 <= k
    <3>1. \A q \in R : sPres'[q] \subseteq 1..k
      <4>. SUFFICES ASSUME NEW q \in R PROVE sPres'[q] \subseteq 1..k OBVIOUS
      <4>1. CASE q \in U BY <4>1, <1>8, <1>sp, <1>g, <1>k DEF Epochs
      <4>2. CASE q \notin U BY <4>2, <1>7, <1>sp, <1>g
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <3>1
  <2>1. e1 # k BY <2>0
  <2>2. e1 \in sPres[q1] /\ sCon'[q1][e1] = sCon[q1][e1] BY <2>1, <1>7, <1>8
  <2>3. CASE q2 \in U /\ e2 = k
    <3>1. sCon[q1][e1][x] <= acked[x] BY <2>2, <1>L
    <3>. QED BY <3>1, <2>2, <2>3, <1>8
  <2>4. CASE ~(q2 \in U /\ e2 = k)
    <3>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <2>4, <1>7, <1>8
    <3>. QED BY <3>1, <2>2, <1>0, <1>T DEF I_WHi
  <2>. QED BY <2>3, <2>4
<1>c. \A q \in R \ U : /\ CatchingUp(q)' = (CatchingUp(q) /\ q \notin AsmExcl(T))
                       /\ (CatchingUp(q) => q \notin AsmExcl(T) /\ HiOf(q)' = HiOf(q))
  <2>. SUFFICES ASSUME NEW q \in R \ U
               PROVE  /\ CatchingUp(q)' = (CatchingUp(q) /\ q \notin AsmExcl(T))
                      /\ (CatchingUp(q) => q \notin AsmExcl(T) /\ HiOf(q)' = HiOf(q))
    OBVIOUS
  <2>1. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>2. CASE q \in AsmExcl(T)
    <3>1. dState[q] = "insync" /\ reverted[q] = -1 /\ dState'[q] = "stale" /\ reverted'[q] = -1
      BY <2>1, <2>2, <1>ex, <1>rest, <1>7 DEF SessionOK, InSyncR
    <3>. QED BY <3>1 DEF CatchingUp, HiOf
  <2>3. CASE q \notin AsmExcl(T)
    <3>1. dState'[q] = dState[q] /\ reverted'[q] = reverted[q] /\ mark'[q] = mark[q] BY <2>3, <1>rest, <1>7
    <3>. QED BY <3>1, <2>3 DEF CatchingUp, HiOf
  <2>. QED BY <2>2, <2>3
<1>u2. ~CatchingUp(r)' /\ \A q \in T : ~CatchingUp(q)'
  BY <1>rest, <1>t, <1>ex DEF CatchingUp
<1>15. I_HeadHi' /\ I_HeadLo'
  <2>1. \A q \in R \ U : CatchingUp(q) => HiOf(q) < k /\ HiOf(q) \in Int
    <3>. SUFFICES ASSUME NEW q \in R \ U, CatchingUp(q) PROVE HiOf(q) < k /\ HiOf(q) \in Int OBVIOUS
    <3>1. SessionOK(q) /\ RevOK(q) BY <1>0, <1>T DEF I_Session, I_Reverted
    <3>2. mark[q] \in Int /\ reverted[q] \in Int /\ sb[q] \in Int BY <1>0, <1>T, Groups DEF TyLeg
    <3>. QED BY <3>1, <3>2, <1>g DEF SessionOK, RevOK, CatchingUp, HiOf
  <2>2. I_HeadHi'
    <3>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW q2 \in R, NEW e2 \in sPres'[q2], e2 > HiOf(q)',
                        NEW x \in Blocks
                 PROVE  head'[q][x] <= cutAck'[q2][e2][x]
      BY DEF I_HeadHi
    <3>0. q \in R \ U /\ CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ head'[q] = head[q] BY <1>u2, <1>c, <1>7
    <3>1. CASE q2 \in U /\ e2 = k
      <4>1. head[q][x] <= acked[x] BY <1>L
      <4>. QED BY <4>1, <3>0, <3>1, <1>8
    <3>2. CASE ~(q2 \in U /\ e2 = k)
      <4>1. e2 \in sPres[q2] /\ cutAck'[q2][e2] = cutAck[q2][e2] BY <3>2, <1>7, <1>8
      <4>. QED BY <4>1, <3>0, <1>0, <1>T DEF I_HeadHi
    <3>. QED BY <3>1, <3>2
  <2>3. I_HeadLo'
    <3>. SUFFICES ASSUME NEW q \in R, CatchingUp(q)', NEW q1 \in R, NEW p \in sPres'[q1], p < HiOf(q)',
                        NEW x \in Blocks
                 PROVE  sCon'[q1][p][x] <= head'[q][x]
      BY DEF I_HeadLo
    <3>0. q \in R \ U /\ CatchingUp(q) /\ HiOf(q)' = HiOf(q) /\ head'[q] = head[q] BY <1>u2, <1>c, <1>7
    <3>1. p # k BY <3>0, <2>1, <1>k
    <3>2. p \in sPres[q1] /\ sCon'[q1][p] = sCon[q1][p] BY <3>1, <1>7, <1>8
    <3>. QED BY <3>0, <3>2, <1>0, <1>T DEF I_HeadLo
  <2>. QED BY <2>2, <2>3
<1>16. I_Reverted'
  <2>. SUFFICES ASSUME NEW q \in R PROVE RevOK(q)' BY DEF I_Reverted
  <2>1. CASE q \in U BY <2>1, <1>rest, <1>t, <1>ex DEF RevOK, InSyncR
  <2>2. CASE q \in AsmExcl(T)
    <3>1. q \notin U /\ reverted'[q] = reverted[q] BY <2>2, <1>ex, <1>7
    <3>2. reverted[q] = -1 BY <2>2, <1>ex, <1>0, <1>T DEF I_Session, SessionOK, InSyncR
    <3>. QED BY <3>1, <3>2 DEF RevOK
  <2>3. CASE q \notin U /\ q \notin AsmExcl(T)
    <3>1. RevOK(q) /\ SessionOK(q) BY <1>0, <1>T DEF I_Reverted, I_Session
    <3>2. dState'[q] = dState[q] /\ reverted'[q] = reverted[q] BY <2>3, <1>rest, <1>7
    <3>3. dState[q] = "stale" /\ reverted[q] # -1 => \A e \in sPres[q] : e < k
      BY <3>1, <1>sp, <1>g, <1>k DEF SessionOK, CatchingUp
    <3>. QED BY <3>1, <3>2, <3>3, <2>3, <1>7, <1>rest DEF RevOK
  <2>. QED BY <2>1, <2>2, <2>3
<1>17. I_Session'
  <2>. SUFFICES ASSUME NEW q \in R PROVE SessionOK(q)' BY DEF I_Session
  <2>0. SessionOK(q) BY <1>0, <1>T DEF I_Session
  <2>1. CASE q = r
    <3>a. dState'[r] = "insync" /\ reverted'[r] = -1 BY <1>rest, <1>0, Groups DEF TyLeg
    <3>b. sb'[r] = -1 BY <1>rest, <2>0, <2>1, <1>g DEF SessionOK
    <3>1. dState'[r] = "insync" /\ reverted'[r] = -1 /\ sb'[r] = -1 BY <3>a, <3>b
    <3>. QED BY <3>1, <2>1, <1>u2 DEF SessionOK
  <2>2. CASE q \in T
    <3>1. dState'[q] = "insync" /\ reverted'[q] = reverted[q] /\ sb'[q] = sb[q] /\ dState[q] = "insync"
      BY <2>2, <1>rest, <1>t, <1>ex, <1>0, <1>T, Groups DEF TyLeg
    <3>. QED BY <3>1, <2>0, <2>2, <1>u2 DEF SessionOK
  <2>3. CASE q \in AsmExcl(T)
    <3>1. dState'[q] = "stale" /\ reverted'[q] = -1 /\ sb'[q] = -1 /\ ~CatchingUp(q)'
      BY <2>3, <2>0, <1>rest, <1>ex, <1>7, <1>c DEF SessionOK, InSyncR
    <3>. QED BY <3>1 DEF SessionOK
  <2>4. CASE q \notin U /\ q \notin AsmExcl(T)
    <3>1. dState'[q] = dState[q] /\ reverted'[q] = reverted[q] /\ sb'[q] = sb[q] /\ mark'[q] = mark[q]
          /\ pin'[q] = pin[q] /\ path'[q] = path[q] /\ hPar'[q] = hPar[q] /\ sPres'[q] = sPres[q]
          /\ tr'[q] = tr[q]
      BY <2>4, <1>rest, <1>7, <1>tr, <1>0, <1>T, Groups DEF TyTrust
    <3>2. CatchingUp(q) => \A e \in sPres[q] : e < k BY <2>0, <1>sp, <1>g, <1>k DEF SessionOK
    <3>3. CatchingUp(q)' = CatchingUp(q) BY <2>4, <1>c DEF CatchingUp
    <3>4. dState[q] = "standby" => q \notin U BY <2>4
    <3>5. /\ CatchingUp(q) => nextSeq' \notin sPres[q]
          /\ recorded \subseteq recorded' /\ nextSeq < nextSeq' /\ cutK' = 0 /\ members' = U
      BY <1>sp, <1>rest, <1>k, <1>g
    <3>6. dState[q] = "standby" =>
            /\ \A e \in sPres[q] : e <= mark[q] \/ \A f \in recorded' : e < f
            /\ mark[q] < nextSeq' /\ mark[q] \in recorded' /\ q \notin members'
      <4>1. dState[q] = "standby" => CatchingUp(q) BY DEF CatchingUp
      <4>2. dState[q] = "standby" => /\ mark[q] < nextSeq /\ mark[q] \in recorded
                                    /\ \A e \in sPres[q] : e <= mark[q] \/ \A f \in recorded : e < f
        BY <2>0 DEF SessionOK
      <4>3. sPres[q] \subseteq Nat /\ mark[q] \in Nat /\ nextSeq' = nextSeq + 1 /\ recorded' = recorded \cup {k}
        BY <1>T, <1>rest
      <4>. QED BY <4>1, <4>2, <4>3, <3>2, <3>4, <3>5, <1>k
    <3>7. sb[q] # -1 => sb[q] < nextSeq' /\ recorded' # {} BY <2>0, <3>5, <1>rest, <1>T DEF SessionOK
    <3>. QED BY <2>0, <3>1, <3>3, <3>5, <3>6, <3>7, <1>rest DEF SessionOK, CatchingUp
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
<1>18. Inv_TrustComplete'
  <2>. SUFFICES ASSUME NEW q \in R, NEW e \in tr'[q], NEW x \in Blocks
               PROVE  sCon'[q][e][x] >= cutAck'[q][e][x]
    BY DEF Inv_TrustComplete
  <2>1. CASE e = k /\ q \in U
    <3>1. sCon'[q][k][x] >= acked[x]
      <4>1. CASE q \in T BY <4>1, <1>9, <1>0, <1>T, <1>g, <1>t DEF I_Hold
      <4>2. CASE q = r BY <4>2, <1>9, <1>H
      <4>. QED BY <4>1, <4>2, <2>1
    <3>. QED BY <3>1, <1>8, <2>1
  <2>2. CASE ~(e = k /\ q \in U)
    <3>1. e \in tr[q] BY <2>2, <1>rest
    <3>2. e \in sPres[q] BY <3>1, <1>0, <1>T DEF I_Ranges
    <3>3. sCon'[q][e] = sCon[q][e] /\ cutAck'[q][e] = cutAck[q][e] BY <3>2, <1>7, <1>8
    <3>. QED BY <3>1, <3>3, <1>0, <1>T DEF Inv_TrustComplete
  <2>. QED BY <2>1, <2>2
<1>19. I_Fresh'
  <2>. SUFFICES ASSUME NEW q \in Bases', NEW e \in sPres'[q], e >= nextSeq' \/ ~conf'[e], NEW x \in Blocks
               PROVE  sCon'[q][e][x] >= cutAck'[q][e][x]
    BY DEF I_Fresh
  <2>1. \A qq \in R : \A ee \in sPres'[qq] : ee \in Epochs /\ ee < nextSeq'
    BY <1>7, <1>8, <1>sp, <1>g, <1>k, <1>rest
  <2>2. Bases' \subseteq R BY <1>rest, <1>t DEF Bases
  <2>3. q \in R /\ e \in Epochs /\ e < nextSeq' BY <2>1, <2>2
  <2>4. conf'[e] BY <2>3, <1>r, <1>g, <1>rest
  <2>5. ~(e >= nextSeq') BY <2>3, <1>T, <1>rest, <1>k
  <2>. QED BY <2>4, <2>5
<1>. QED BY <1>0, <1>T, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>10, <1>11, <1>12, <1>13, <1>14, <1>15,
            <1>16, <1>17, <1>18, <1>19, IndSplitP

-----------------------------------------------------------------------------
(* Stuttering, the steps the clean setting rules out, and the theorem.     *)

LEMMA I_Stutter == ASSUME IndInv, UNCHANGED vars PROVE IndInv'
<1>0. /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
      /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
      /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss
  BY IndSplit
<1>T. /\ members \subseteq R /\ InSyncR \subseteq R /\ Healthy \subseteq R /\ Bases \subseteq R
      /\ cutTgt \subseteq R /\ cutDone \subseteq R /\ cutGone \subseteq R
      /\ recorded \subseteq Epochs /\ Epochs \subseteq Nat /\ R \subseteq Nat
      /\ nextSeq \in Nat /\ cutK \in Nat /\ ver \in Nat /\ MaxEpoch \in Nat
      /\ \A q \in R : /\ sPres[q] \subseteq Epochs /\ tr[q] \subseteq Epochs
                      /\ hPar[q] \in Nat /\ mark[q] \in Nat /\ pin[q] \in Nat
                      /\ sb[q] \in Int /\ reverted[q] \in Int
  BY <1>0, Basics
<1>u. /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq>>
      /\ UNCHANGED <<cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark>>
      /\ UNCHANGED <<reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver>>
      /\ UNCHANGED <<used, tainted, floor, mustRev, restamp, cfgAt, conf, tr, htr, zx, cutAck>>
  BY DEF vars
<1>1. TypeOK' BY <1>0, <1>T, <1>u, Groups, GroupsP, C_Scalar, C_Sets, C_Leg, C_Head, C_Snap, C_Pend, C_Tie, C_Trust
             DEF tieVars, trustVars
<1>. QED BY <1>0, <1>T, <1>u, <1>1, IndSplitP, F_Ranges, F_Lineage, F_Chain, F_Pend, F_Members, F_Hold,
            F_Legal, F_PendSeen, F_AckedEver, F_Vers, F_WHi, F_HeadHi, F_HeadLo, F_Reverted,
            F_HeadPend, F_Session, F_TC, F_Fresh

\* The zombie, the crash and the tie's refusal cannot step in the clean setting.
LEMMA Disabled ==
  ASSUME IndInv
  PROVE  /\ \A r \in R : \A x \in Blocks : ~Zombie(r, x)
         /\ ~CrashUnstage /\ ~ZombieMove /\ ~ZombieMark /\ ~ZombieHygiene
         /\ \A r \in R : ~AdmitRefuse(r)
<1>1. zx = 0 BY DEF IndInv, I_Ranges
<1>2. \A r \in R : r # 0 BY RNat
<1>3. \A r \in R : \A x \in Blocks : ~Zombie(r, x) BY <1>1, <1>2, Clean DEF Zombie
<1>. QED BY <1>1, <1>3, Clean DEF CrashUnstage, ZombieMove, ZombieMark, ZombieHygiene, AdmitRefuse

LEMMA I_Next == IndInv /\ [Next]_vars => IndInv'
<1>. SUFFICES ASSUME IndInv, [Next]_vars PROVE IndInv' OBVIOUS
<1>d. /\ \A r \in R : \A x \in Blocks : ~Zombie(r, x)
      /\ ~CrashUnstage /\ ~ZombieMove /\ ~ZombieMark /\ ~ZombieHygiene
      /\ \A r \in R : ~AdmitRefuse(r)
  BY Disabled
<1>0. CASE UNCHANGED vars BY <1>0, I_Stutter
<1>1. CASE \E x \in Blocks : Submit(x) BY <1>1, I_Submit
<1>2. CASE \E w \in pend : Complete(w) \/ \E r \in R : Land(w, r) \/ Drop(w, r)
  BY <1>2, I_Complete, I_Land, I_Drop
<1>3. CASE \E r \in R : \E x \in Blocks : Zombie(r, x) BY <1>3, <1>d
<1>4. CASE \E r \in R : FailBegin(r) \/ FaultOut(r) \/ MarkStale(r) \/ Return(r)
  BY <1>4, I_FailBegin, I_FaultOut, I_MarkStale, I_Return
<1>5. CASE \E e \in Epochs : Observe(e) BY <1>5, I_Observe
<1>6. CASE CutBegin \/ CutRecord \/ CutAbort \/ CutAbortRead \/ Retire
  BY <1>6, I_CutBegin, I_CutRecord, I_CutAbort, I_Retire
<1>7. CASE \E r \in R : CutOne(r) \/ \E e \in Epochs : GC(r, e) BY <1>7, I_CutOne, I_GC
<1>8. CASE \E r \in R : SelectBase(r) \/ RevertD(r) \/ BulkCopy(r) \/ SessionFail(r) \/ Chase(r)
  BY <1>8, I_SelectBase, I_RevertD, I_BulkCopy, I_SessionFail, I_Chase
<1>9. CASE Unstage \/ CrashUnstage \/ Stage \/ ZombieMove \/ ZombieMark \/ ZombieHygiene
  BY <1>9, <1>d, I_Unstage, I_Stage
<1>10. CASE \E r \in R : StageAdmit(r) \/ AdmitRefuse(r) BY <1>10, <1>d, I_StageAdmit
<1>. QED BY <1>0, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF Next

THEOREM Safety == Spec => [](Inv_Catchup /\ Inv_MembersHold)
<1>1. Init => IndInv BY I_Init
<1>2. IndInv /\ [Next]_vars => IndInv' BY I_Next
<1>3. IndInv => Inv_Catchup /\ Inv_MembersHold BY I_Goal DEF Goal
<1>. QED BY <1>1, <1>2, <1>3, PTL DEF Spec

=============================================================================
