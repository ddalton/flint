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
\* E18: E17's ScopeHolds trace found the third take: Consume adopts doc[p] at a
\* path whose LOCAL already holds the document's content (Converges).
TakeAt3(s, p) == TakeAt(s, p) \/ Converges(s, p)
S3Wide3 == \A s \in Writers, p \in Paths : TakeAt3(s, p) => ~BackR3(s, p)
TakeOwed3 == [][\A s \in Writers, p \in Paths :
                  (w[s].baseline[p] # Nil /\ doc[p] # Nil /\ w'[s].baseline[p] # w[s].baseline[p] /\ Takes(s, p))
                  => TakeAt3(s, p)]_vars
\* E19: NR3's unguarded strengthening.  E15's S3All failed only where the
\* tree's own commit installed a delete at p (cased, p among its deletes) and
\* the gateway renamed an older version back; Finish then drops the baseline.
\* A back position is only ever that; neither a take path's guard holds there.
Excuse(s, p) == /\ w[s].pc = "cased" /\ p \in w[s].deletes /\ w[s].inst[p] = Nil
                /\ w[s].local[p] # w[s].baseline[p] /\ ~Converges(s, p)
S3X == \A s \in Writers, p \in Paths : BackR3(s, p) => Excuse(s, p)
\* E19, the relation S3X needs to be inductive: a tree's baseline at p is
\* behind the document (doc derives from it), or a record names where it went
\* (a conflict, an acknowledged delete, an acknowledgement at another path),
\* or the tree's own commit installed the delete (cased, before Finish).
Rel(s, p) == w[s].baseline[p] # Nil =>
  \/ doc[p] # Nil /\ Derives(doc[p], w[s].baseline[p])
  \/ <<p, w[s].baseline[p]>> \in conflicts
  \/ \E d \in udel : d[1] = p /\ Derives(d[2], w[s].baseline[p])
  \/ \E a \in acked : a[1] # p /\ Derives(a[2], w[s].baseline[p])
  \/ w[s].pc = "cased" /\ p \in w[s].deletes /\ w[s].inst[p] = Nil
RelAll == \A s \in Writers, p \in Paths : Rel(s, p)
\* E19b: RelAll fails twice (E19): a restart after the tree's own installed
\* delete leaves a dead baseline (local Nil; Edit mints fresh, so never owed
\* again), and a cased tree's own install need not derive from its baseline
\* (the file was new when edited).  The relation, guarded to where a take
\* can happen: the tree holds its baseline unedited, or it installed doc[p].
RelG(s, p) == (w[s].baseline[p] # Nil /\ w[s].local[p] = w[s].baseline[p]) =>
  \/ doc[p] # Nil /\ Derives(doc[p], w[s].baseline[p])
  \/ <<p, w[s].baseline[p]>> \in conflicts
  \/ \E d \in udel : d[1] = p /\ Derives(d[2], w[s].baseline[p])
  \/ \E a \in acked : a[1] # p /\ Derives(a[2], w[s].baseline[p])
  \/ w[s].pc = "cased" /\ w[s].inst[p] = doc[p]
RelGAll == \A s \in Writers, p \in Paths : RelG(s, p)
\* E19c: RelG fails in FetchHolds (E19b): another tree's committed delete
\* leaves the path empty with only its tombstone naming the version.
RelG2(s, p) == (w[s].baseline[p] # Nil /\ w[s].local[p] = w[s].baseline[p]) =>
  \/ doc[p] # Nil /\ Derives(doc[p], w[s].baseline[p])
  \/ doc[p] = Nil /\ tomb[p] # Nil /\ Derives(tomb[p], w[s].baseline[p])
  \/ <<p, w[s].baseline[p]>> \in conflicts
  \/ \E d \in udel : d[1] = p /\ Derives(d[2], w[s].baseline[p])
  \/ \E a \in acked : a[1] # p /\ Derives(a[2], w[s].baseline[p])
  \/ w[s].pc = "cased" /\ w[s].inst[p] = doc[p]
RelG2All == \A s \in Writers, p \in Paths : RelG2(s, p)
\* E22: the fact S3Wide3's rename case needs.  A rename lands only on an
\* empty path; it steps a tree back only by bringing a STRICTLY OLDER version
\* of the tree's baseline from another path.  So: a take-able baseline at an
\* emptied path that no record exempts has no older version published anywhere.
Exempt(s, p) ==
  \/ <<p, w[s].baseline[p]>> \in conflicts
  \/ \E d \in udel : d[1] = p /\ Derives(d[2], w[s].baseline[p])
  \/ \E a \in acked : a[1] # p /\ Derives(a[2], w[s].baseline[p])
TakeAble(s, p) == w[s].local[p] = w[s].baseline[p] \/ (w[s].pc = "cased" /\ w[s].inst[p] = doc[p])
Older(b, h) == h # Nil /\ Derives(b, h) /\ Content(h) # Content(b)
NoOlder == \A s \in Writers, p \in Paths :
  (w[s].baseline[p] # Nil /\ doc[p] = Nil /\ TakeAble(s, p) /\ ~Exempt(s, p))
    => \A q \in Paths : ~Older(w[s].baseline[p], doc[q])
\* E29: NR3 FAILS on the model with E27's Edit base: the conflict at p names a
\* SUCCESSOR of the baseline (a save over it), not the baseline, and nothing
\* else does (a stale tree's commit over that save; then a delete and a rename
\* back).  NR5: a conflict is read as udel and acked are, by what derives from
\* the baseline (Back's <<p, baseline>> is the case k = baseline).
NamedAt(p, b) ==
  \/ \E c \in conflicts : c[1] = p /\ Derives(c[2], b)
  \/ \E d \in udel : d[1] = p /\ Derives(d[2], b)
  \/ \E a \in acked : a[1] # p /\ Derives(a[2], b)
BackR5(s, p) == /\ doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[p])
                /\ Content(doc[p]) # Content(w[s].baseline[p])
                /\ ~NamedAt(p, w[s].baseline[p])
NR5 == [][\A s \in Writers, p \in Paths : ~(BackR5(s, p) /\ Takes(s, p))]_vars
==============================================================================
