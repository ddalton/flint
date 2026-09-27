---------------------------- MODULE MCLeanP1Gate ----------------------------
(***************************************************************************)
(* The GATE's shape of LeanP1 (2026-09-27).  `LeanP1Holds` at its full     *)
(* bounds did not finish in six hours twice (373M distinct, `behind`;      *)
(* 997M, depth 29, queue still growing, the scan trigger); it runs as an   *)
(* opt-in deep job with checkpoints instead.  The gate checks the shipped  *)
(* rules at a bound it can DECIDE, as a state constraint, not an edit:     *)
(* `seq` stops at GateMaxSeq, and the reader ghost may be held off.        *)
(* Which bound is the gate's is chosen by measurement (the sizing runs in  *)
(* results/2026-09-27-leanp1-gate-shape/), and a bound is only the gate's  *)
(* once every mutation world still fires under it.                         *)
(***************************************************************************)
EXTENDS LeanP1

CONSTANTS GateMaxSeq, GateReader

GateBound ==
  /\ seq <= GateMaxSeq
  /\ GateReader \/ rdoc = [p \in Paths |-> Nil]
==============================================================================
