--------------------------- MODULE FlintCatchupTAnc ---------------------------
(***************************************************************************)
(* TLC/tla-mc only (tlapm refuses RECURSIVE): the ghost lineage anc of     *)
(* FlintCatchupT is the parent walk of every present snapshot, and empty   *)
(* for an absent one (so anc is a function of the rest of the state).      *)
(***************************************************************************)
EXTENDS FlintCatchupT

RECURSIVE AncRec(_, _, _)
AncRec(r, e, n) == IF e = 0 \/ n = 0 THEN {} ELSE {e} \cup AncRec(r, sPar[r][e], n - 1)

Inv_AncOK ==
  \A r \in R : \A e \in Epochs :
    anc[r][e] = IF e \in sPres[r] THEN AncRec(r, e, MaxEpoch) ELSE {}

=============================================================================
