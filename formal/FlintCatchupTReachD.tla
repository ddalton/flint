--------------------------- MODULE FlintCatchupTReachD ---------------------------
(***************************************************************************)
(* Reachability probes for the proof's session cases (TLC/tla-mc only):    *)
(* each must FAIL, its trace the witness.                                 *)
(***************************************************************************)
EXTENDS FlintCatchupT

\* a standby demoted by the chase (CoverSet empty): stale, base kept
R_NoDemote == ~\E r \in R : dState[r] = "stale" /\ reverted[r] # -1 /\ sb[r] = -1
\* ... and resumed from that base without a re-revert
R_NoResume == ~\E r \in R : dState[r] = "stale" /\ reverted[r] # -1 /\ sb[r] = reverted[r]
                              /\ head[r] # (IF reverted[r] = 0 THEN Zero ELSE sCon[r][reverted[r]])
=============================================================================
