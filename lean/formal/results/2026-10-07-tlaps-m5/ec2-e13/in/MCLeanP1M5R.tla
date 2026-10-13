----------------------------- MODULE MCLeanP1M5R -----------------------------
(***************************************************************************)
(* M5, part C's groundwork: candidate RESTATEMENTS of Inv_AckedNamed and   *)
(* Inv_NoRegress, which are false as stated once the gateway may remove    *)
(* three times (NOTES.txt E6, E8).  Stated over LeanP1.tla UNCHANGED; the  *)
(* restatement itself is flint-46's (LeanP1.tla).  Each candidate must     *)
(* (a) hold in the three-removal worlds and the gate's, and (b) still FAIL *)
(* wherever the original fails for a real loss (a rule off, a mutation) -- *)
(* a widening checked only where it holds rules out nothing.               *)
(***************************************************************************)
EXTENDS LeanP1

(* Inv_AckedNamed.  The original names an acked <<p, h>> by a record at p,  *)
(* or -- only when p is h's own path -- at a path h was later acked at.     *)
(* E8: renamed away and back, then deleted at its own path; the ack at the  *)
(* path it passed through is named only by the delete AT ITS OWN PATH.      *)
AccountedAt(p, h, Q) ==
  \/ h \in live
  \/ Preserved(h)
  \/ \E q \in Q :
       \/ doc[q] # Nil /\ Derives(doc[q], h)
       \/ tomb[q] # Nil /\ Derives(tomb[q], h)
       \/ \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
       \/ \E d \in udel : d[1] = q /\ Derives(d[2], h)
  \/ \E b \in acked : b[1] = p /\ Later(b[2], h)
\* The original, through AccountedAt (a calibration: must match Inv_AckedNamed).
OrigPaths(p, h) == {p} \cup (IF p = h[1] THEN {q \in Paths : <<q, h>> \in acked} ELSE {})
AN0 == \A a \in acked : AccountedAt(a[1], a[2], OrigPaths(a[1], a[2]))
\* AN1: the version's whole rename chain -- its own path and every path it
\* was acknowledged at -- for every ack, not only the one at its own path.
Chain(p, h) == {p, h[1]} \cup {q \in Paths : <<q, h>> \in acked}
AN1 == \A a \in acked : AccountedAt(a[1], a[2], Chain(a[1], a[2]))
\* AN2: anywhere (the weakest: some record names it).
AN2 == \A a \in acked : AccountedAt(a[1], a[2], Paths)

\* AN3: AN1, and the later-ack clause along the chain too (E10: renamed to p2
\* and back, then a later save acked at p1; the stale ack at p2 is accounted
\* by the later ack at p1, a path on the version's chain).
AccountedChain(p, h) ==
  \/ AccountedAt(p, h, Chain(p, h))
  \/ \E b \in acked : b[1] \in Chain(p, h) /\ Later(b[2], h)
AN3 == \A a \in acked : AccountedChain(a[1], a[2])

(* Inv_NoRegress, as an ACTION: a tree takes the document's version at p   *)
(* (its baseline moves to doc[p]) while that steps BACK.  NR0 is the       *)
(* original's exemption (a conflict names the baseline); a calibration:    *)
(* it must fail exactly where Inv_NoRegress does.                          *)
Takes(s, p) == w'[s].baseline[p] = doc[p]
NR0 == [][\A s \in Writers, p \in Paths : ~(Back(s, p) /\ Takes(s, p))]_vars
\* NR1: also exempt when the gateway's ACKNOWLEDGED DELETE at p retired the
\* baseline itself (E6: delete <<p1,2>>, then a rename back restores <<p1,1>>).
BackR1(s, p) == Back(s, p) /\ ~\E d \in udel : d[1] = p /\ d[2] = w[s].baseline[p]
NR1 == [][\A s \in Writers, p \in Paths : ~(BackR1(s, p) /\ Takes(s, p))]_vars
\* NR2: ...or a version derived from the baseline (a save over it, then the
\* delete) -- M5NoBackU's trace, as a claim rather than a strengthening.
BackR2(s, p) == Back(s, p) /\ ~\E d \in udel : d[1] = p /\ Derives(d[2], w[s].baseline[p])
NR2 == [][\A s \in Writers, p \in Paths : ~(BackR2(s, p) /\ Takes(s, p))]_vars
==============================================================================
