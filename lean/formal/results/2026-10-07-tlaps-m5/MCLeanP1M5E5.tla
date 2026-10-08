------------------------------ MODULE MCLeanP1M5E5 ---------------------------
(* M5 E5 (results/2026-10-07-tlaps-m5/NOTES.txt). *)
EXTENDS MCLeanP1M5
(* E5: which of E1's disjuncts carry it.  Each variant drops one; one that *)
(* still holds marks a disjunct the proof need not maintain.               *)
NameAt(q, h, tombOK, tookOK, udelOK) ==
  \/ doc[q] # Nil /\ Derives(doc[q], h)
  \/ tombOK /\ tomb[q] # Nil /\ Derives(tomb[q], h)
  \/ tookOK /\ \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
  \/ udelOK /\ \E d \in udel : d[1] = q /\ Derives(d[2], h)
AccV(p, h, presOK, tombOK, tookOK, udelOK, laterOK) ==
  \/ presOK /\ Preserved(h)
  \/ \E q \in Paths : (q = p \/ (p = h[1] /\ <<q, h>> \in acked)) /\ NameAt(q, h, tombOK, tookOK, udelOK)
  \/ laterOK /\ \E b \in acked : b[1] = p /\ Later(b[2], h)
M5NoTomb   == \A a \in acked : AccV(a[1], a[2], TRUE, FALSE, TRUE, TRUE, TRUE)
M5NoTook   == \A a \in acked : AccV(a[1], a[2], TRUE, TRUE, FALSE, TRUE, TRUE)
M5NoUdel   == \A a \in acked : AccV(a[1], a[2], TRUE, TRUE, TRUE, FALSE, TRUE)
M5NoLater  == \A a \in acked : AccV(a[1], a[2], TRUE, TRUE, TRUE, TRUE, FALSE)
M5NoPres   == \A a \in acked : AccV(a[1], a[2], FALSE, TRUE, TRUE, TRUE, TRUE)
==============================================================================
