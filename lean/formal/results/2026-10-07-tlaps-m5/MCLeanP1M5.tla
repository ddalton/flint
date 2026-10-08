----------------------------- MODULE MCLeanP1M5 -----------------------------
(***************************************************************************)
(* M5 exploration (docs/plans/lean-tlaps-inductive-proof-plan.md, M5):     *)
(* candidate strengthenings of Inv_AckedNamed, stated over LeanP1.tla       *)
(* UNCHANGED and TLC-checked before any becomes a TLAPS lemma               *)
(* (results/2026-10-07-tlaps-m5/NOTES.txt).                                *)
(*                                                                         *)
(* E1 (M5NoLive): Inv_AckedNamed WITHOUT its first disjunct (`h \in live`). *)
(* An acknowledged handle was cited by the CAS that acknowledged it, and   *)
(* every step that uncites one leaves a name (a derived save, a tombstone  *)
(* and an acked delete, a conflict, a rename's new path).  If E1 holds, the *)
(* sweep and the reaper -- which only shrink `live` -- cannot break the     *)
(* claim, which is the obligation the plan calls the real work.            *)
(* E1 is also checked against the claim's own disjuncts one at a time:     *)
(* M5Cited (still cited somewhere it may be named) as a probe expected to  *)
(* FAIL, so a holding E1 is not E1 holding for the trivial reason.         *)
(***************************************************************************)
EXTENDS LeanP1

AccountedNL(p, h) ==
  \/ Preserved(h)
  \/ \E q \in Paths :
       /\ q = p \/ (p = h[1] /\ <<q, h>> \in acked)
       /\ \/ doc[q] # Nil /\ Derives(doc[q], h)
          \/ tomb[q] # Nil /\ Derives(tomb[q], h)
          \/ \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
          \/ \E d \in udel : d[1] = q /\ Derives(d[2], h)
  \/ \E b \in acked : b[1] = p /\ Later(b[2], h)
M5NoLive == \A a \in acked : AccountedNL(a[1], a[2])

\* A probe: every acked handle is still cited.  Expected to FAIL (a delete).
M5CitedProbe == \A a \in acked : Cited(a[2])
\* A probe: no acked handle is a copy (generation > MaxMint).  If it FAILS,
\* the renamed-copy case is reachable in that world and M5NoLive covered it.
M5CopyAckedProbe == \A a \in acked : Gen(a[2]) <= MaxMint
(* E2 (M5NoBack): Inv_NoRegress's strengthening.  `regressed` is set only by *)
(* a consume or a sync that takes an owed path p with Back(s, p) in the     *)
(* state before the step.  So the claim follows from Back never holding at  *)
(* all, owed or not: no writer's baseline derives from a different-content  *)
(* published version without a conflict naming the baseline.               *)
M5NoBack == \A s \in Writers, p \in Paths : ~Back(s, p)
\* The weaker form the step needs (owed paths only).
M5NoBackOwed == \A s \in Writers, p \in Paths : Owed(s, p) => ~Back(s, p)
(* E6: Inv_NoRegress FAILS with three gateway removals (out/E6Rem3.out): a  *)
(* rename p1 -> p2 forks <<p1,1>>; the writer re-creates p1 as <<p1,2>> over *)
(* it (conflict <<p1, <<p1,1>>>>); the gateway DELETES <<p1,2>> (udel names  *)
(* it) and renames p2 back to p1; the writer's consume takes <<p1,1>> under  *)
(* a baseline that derives from it, and no CONFLICT names <<p1,2>>.  The     *)
(* acknowledged delete does.  Candidate restatement: Back exempts a          *)
(* baseline an acknowledged delete at p names.                              *)
BackU(s, p) == Back(s, p) /\ <<p, w[s].baseline[p]>> \notin udel
M5NoBackU == \A s \in Writers, p \in Paths : ~BackU(s, p)
\* ...and E6Rem3U FAILS too: a UI save <<p1,3>> over <<p1,2>> is what the
\* delete names.  General form: an acknowledged delete at p names a version
\* derived from the baseline.
BackD(s, p) == Back(s, p) /\ ~\E d \in udel : d[1] = p /\ Derives(d[2], w[s].baseline[p])
M5NoBackD == \A s \in Writers, p \in Paths : ~BackD(s, p)
\* E7: M5NoLive FAILS with three removals (out/E6Rem3-M5NoLive.out, depth
\* 10): a rename p1 -> p2 and back acks <<p2, <<p1,1>>>>; a fresh install at
\* p2 clears its tombstone; <<p1,1>> is cited at p1 -- only `live` names it
\* for the ack at p2.  Candidate: cited anywhere in place of live.
M5NoLiveC == \A a \in acked : Cited(a[2]) \/ AccountedNL(a[1], a[2])
==============================================================================
