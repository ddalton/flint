-------------------------- MODULE FlintCatchupTReachQ --------------------------
(***************************************************************************)
(* Availability probes for the quorum arms (option (a)).  soloAck: a write *)
(* was acked having landed on ONE leg; soloStage: a stage assembled a raid *)
(* of ONE leg.  Violated = reachable.  At WriteQuorum = AssemblyQuorum = 1 *)
(* (the code) both are reachable: the volume keeps acking writes and keeps *)
(* staging through a two-leg failure.  At 2 neither is: through a two-leg  *)
(* failure every write fails and no stage completes, until a leg returns.  *)
(***************************************************************************)
EXTENDS FlintCatchupT

VARIABLES soloAck, soloStage

QInit == Init /\ soloAck = FALSE /\ soloStage = FALSE
QNext == /\ Next
         /\ soloAck' = (soloAck \/ \E w \in pend : w \notin pend' /\ Cardinality(w.landed) = 1
                                                   /\ acked'[w.b] = w.v /\ acked[w.b] # w.v)
         /\ soloStage' = (soloStage \/ ~attached /\ attached' /\ Cardinality(members') = 1)
QSpec == QInit /\ [][QNext]_<<vars, soloAck, soloStage>>

Inv_NoSoloAck == ~soloAck
Inv_NoSoloStage == ~soloStage

=============================================================================
