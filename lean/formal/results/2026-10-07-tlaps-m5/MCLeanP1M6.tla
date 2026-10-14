------------------------------ MODULE MCLeanP1M6 ------------------------------
(***************************************************************************)
(* M6 (the plan's item 7): Prop_AgentWorkKept's strengthening as state     *)
(* invariants for TLC (E42).  What the agent wrote and nobody PUT is in    *)
(* no baseline, no recorded uncite, no copy's bytes; a copy is PUT.        *)
(***************************************************************************)
EXTENDS LeanP1
Bar == {"scanned", "claimed", "cased"}
BU == \A s \in Writers, p \in Paths : w[s].baseline[p] = Nil \/ w[s].baseline[p] \in upped
SU == \A s \in Writers, p \in Paths : w[s].sHeld[p] = Nil \/ w[s].sHeld[p] \in upped
OU == \A m \in minted : orig[m] # Nil => orig[m] \in upped
OC == \A m \in minted : orig[m] # Nil => m \in upped
UpSnap == \A s \in Writers : w[s].pc \in Bar => \A p \in w[s].upDone : w[s].snap[p] \in upped
J6 == BU /\ SU /\ OU /\ OC /\ UpSnap
==============================================================================
