------------------------ MODULE LeanScanRacyProof ----------------------------
(***************************************************************************)
(* TLAPS proof of LeanScanRacy's refinement, for every bound:             *)
(*   THEOREM NoMissedChange == Spec => []Inv_NoMissedChange               *)
(* with the rule as shipped (RacyClean, MeasureAtRead, ClearOutsideWindow *)
(* all TRUE) and any Window >= 1 tick. TLC checked it at 3,438 and        *)
(* 156,048 distinct; this removes the bounds (writes, ticks, window).     *)
(*                                                                         *)
(* The invariant alone is not inductive. What makes it so:                 *)
(*   - a write stamps the current tick, so mtime <= clock;                *)
(*   - an upload in flight read its bytes at tick `at`: either the file   *)
(*     is still those bytes, or a later write stamped mtime >= at         *)
(*     (ReadOK);                                                           *)
(*   - an entry NOT flagged racy was recorded (or cleared) at least a     *)
(*     window after its mtime, so a write since then has a strictly later *)
(*     mtime and changes the stat (BaseOK).                               *)
(* A racy entry needs no clause: the rule itself compares the bytes.      *)
(*                                                                         *)
(* Controls (results/2026-10-20-tlaps-racy): each mutation, put in Shipped *)
(* in place of its TRUE, must fail the step that relies on it.             *)
(***************************************************************************)
EXTENDS LeanScanRacy, TLAPS

ASSUME Const ==
  /\ MaxVer \in Nat /\ MaxTime \in Nat
  /\ Window \in Nat \ {0}

ASSUME Shipped ==
  /\ RacyClean = TRUE
  /\ MeasureAtRead = TRUE
  /\ ClearOutsideWindow = TRUE

ReadOK ==
  read.on => /\ read.size \in Sizes
             /\ read.mtime <= read.at /\ read.at <= clock
             /\ \/ ver = read.ver
                \/ read.at <= mtime

BaseOK ==
  ~base.racy => /\ base.mtime + Window <= clock
                /\ \/ ver = base.ver
                   \/ base.mtime < mtime

Ind == TypeOK /\ mtime <= clock /\ ReadOK /\ BaseOK

LEMMA IndInit == Init => Ind
  BY Const, Shipped DEF Init, Ind, TypeOK, ReadOK, BaseOK, NoRead

LEMMA IndNext == Ind /\ [Next]_vars => Ind'
<1> SUFFICES ASSUME Ind, [Next]_vars PROVE Ind'
  OBVIOUS
<1> USE Const, Shipped DEF Ind, TypeOK, ReadOK, BaseOK
<1>1. CASE Write
  BY <1>1 DEF Write
<1>2. CASE Tick
  BY <1>2 DEF Tick
<1>3. CASE ReadUpload
  BY <1>3 DEF ReadUpload
<1>4. CASE Record
  BY <1>4 DEF Record, IsRacy, NoRead
<1>5. CASE ClearRacy
  BY <1>5 DEF ClearRacy, IsRacy, StatChanged
<1>6. CASE UNCHANGED vars
  BY <1>6 DEF vars
<1> QED
  BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF Next

LEMMA IndSafe == Ind => Inv_NoMissedChange
  BY Shipped DEF Ind, TypeOK, BaseOK, Inv_NoMissedChange, OracleDirty,
                 CodeDirty, StatChanged

THEOREM NoMissedChange == Spec => []Inv_NoMissedChange
<1>1. Spec => []Ind
  BY IndInit, IndNext, PTL DEF Spec
<1> QED
  BY <1>1, IndSafe, PTL
==============================================================================
