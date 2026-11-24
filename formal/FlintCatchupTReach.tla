-------------------------- MODULE FlintCatchupTReach --------------------------
(***************************************************************************)
(* Vacuity probe for FlintCatchupT: a history flag set when a standby is   *)
(* admitted (Standby -> InSync).  Inv_NoAdmission violated = admissions    *)
(* are reachable in that configuration, so its HOLD is not vacuous.        *)
(***************************************************************************)
EXTENDS FlintCatchupT

VARIABLE admitted

RInit == Init /\ admitted = FALSE
RNext == Next /\ admitted' = (admitted \/ \E r \in R : dState[r] = "standby" /\ dState'[r] = "insync")
RSpec == RInit /\ [][RNext]_<<vars, admitted>>

Inv_NoAdmission == ~admitted

=============================================================================
