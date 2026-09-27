---------------------------- MODULE MCLeanP1Like ----------------------------
(***************************************************************************)
(* LeanP1 cut down to LeanCore's shape, for one comparison: the distinct   *)
(* states LeanP1 needs at LeanCoreHoldsSmall's bounds (49,030,962,         *)
(* results/2026-09-24-l125-gate/).  What LeanCore never had is held off:   *)
(* the reader ghost never loads, the retire age is off in the cfg          *)
(* (RetireAge = FALSE, MaxAges = 0, MaxCopies = 0), and `seq` stops at     *)
(* LeanCore's MaxSeq = 3.  As a state constraint, not an edit: LeanP1.tla  *)
(* is what the gate checks.                                                *)
(***************************************************************************)
EXTENDS LeanP1

LikeLeanCore ==
  /\ rdoc = [p \in Paths |-> Nil]
  /\ seq <= 3
==============================================================================
