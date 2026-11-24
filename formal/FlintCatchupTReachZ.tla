------------------------- MODULE FlintCatchupTReachZ -------------------------
(***************************************************************************)
(* Vacuity probe for the Hygiene arms: was[r] marks a leg some zombie once *)
(* held; zadm is set when such a leg is admitted (Standby -> InSync).      *)
(* Inv_NoZombieLegAdmitted violated = the zombie's former leg is rebuilt   *)
(* and admitted in that configuration, so a HOLD there is not vacuous.     *)
(***************************************************************************)
EXTENDS FlintCatchupT

VARIABLES was, zadm

ZInit == Init /\ was = [r \in R |-> FALSE] /\ zadm = FALSE
ZNext == /\ Next
         /\ was' = [r \in R |-> was[r] \/ zx' = r]
         /\ zadm' = (zadm \/ \E r \in R : was[r] /\ dState[r] = "standby" /\ dState'[r] = "insync")
ZSpec == ZInit /\ [][ZNext]_<<vars, was, zadm>>

Inv_NoZombieLegAdmitted == ~zadm

=============================================================================
