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
\* NR3: NR2, and exempt when the baseline -- or a version derived from it -- is
\* acknowledged at ANOTHER path: renamed away, not lost (E13 P3: p1 renamed to
\* p3 while its old version is renamed back to p1).
BackR3(s, p) == BackR2(s, p) /\ ~\E a \in acked : a[1] # p /\ Derives(a[2], w[s].baseline[p])
NR3 == [][\A s \in Writers, p \in Paths : ~(BackR3(s, p) /\ Takes(s, p))]_vars

(* M5 part C, NR3's strengthening candidates (E15).  NR3 forbids the step   *)
(* that takes doc[p] while BackR3 holds; the strongest state invariant that  *)
(* gives it is that no tree is ever in an unexempted back position.         *)
S3All == \A s \in Writers, p \in Paths : ~BackR3(s, p)
\* ...or only where the tree is owed the path (the step that takes doc[p]).
S3Owed == \A s \in Writers, p \in Paths : Owed(s, p) => ~BackR3(s, p)
\* E16: does every step that MOVES a tree's (non-Nil) baseline onto a non-Nil doc[p] take
\* an OWED path?  If so, NR3 follows from S3Owed; a counterexample names the
\* step that takes a path some other way, which the proof needs a case for.
TakeOwed == [][\A s \in Writers, p \in Paths :
                 (w[s].baseline[p] # Nil /\ doc[p] # Nil /\ w'[s].baseline[p] # w[s].baseline[p] /\ Takes(s, p))
                 => Owed(s, p)]_vars
\* E17: E16 found the other take: Finish moves a cased tree's baseline to what
\* it installed, which is doc[p] while nothing has replaced it -- a step
\* FORWARD.  The paths a tree may take doc[p] at, and NR3's strengthening
\* over them.
TakeAt(s, p) == Owed(s, p) \/ (w[s].pc = "cased" /\ w[s].inst[p] = doc[p])
S3Wide == \A s \in Writers, p \in Paths : TakeAt(s, p) => ~BackR3(s, p)
TakeOwed2 == [][\A s \in Writers, p \in Paths :
                  (w[s].baseline[p] # Nil /\ doc[p] # Nil /\ w'[s].baseline[p] # w[s].baseline[p] /\ Takes(s, p))
                  => TakeAt(s, p)]_vars
==============================================================================
