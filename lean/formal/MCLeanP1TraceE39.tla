------------------------- MODULE MCLeanP1TraceE39 ----------------------------
(***************************************************************************)
(* GATE COPY (2026-10-14), over LeanP1 and its claims NR5 and AN5, of      *)
(*   results/2026-10-07-tlaps-m5/MCLeanP1M5H6.tla                          *)
(* E39: E38's script (MCLeanP1M5H5), three paths: c committed at P1 and    *)
(* renamed to P2, a save h' over it, G elapses; h' renamed on to P3 (its   *)
(* tombstone at P2 names it), saves land in the empty P2 and P1 (both      *)
(* tombstones cleared).  A, its tree still at c on P1 (the fetch fails),   *)
(* commits a new file over P2's save and reaps c.  What names c: h''s ack  *)
(* at P3, off c's chain.                                                   *)
(***************************************************************************)
EXTENDS LeanP1
CONSTANTS WA, P1, P2, P3
VARIABLE k
St(A) == A /\ TookUpdate /\ RetUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>
Script(i) ==
  CASE i = 0  -> St(Checkout(WA))
    [] i = 1  -> St(Edit(WA, P1))
    [] i = 2  -> St(Consume(WA))
    [] i = 3  -> St(Scan(WA))
    [] i = 4  -> St(Upload(WA, P1))
    [] i = 5  -> St(Restart(WA))
    [] i = 6  -> St(Consume(WA))
    [] i = 7  -> St(Scan(WA))
    [] i = 8  -> St(Upload(WA, P1))
    [] i = 9  -> St(Claim(WA))
    [] i = 10 -> St(Verify(WA))
    [] i = 11 -> St(Install(WA))
    [] i = 12 -> St(Collect(WA))
    [] i = 13 -> St(Finish(WA))
    [] i = 14 -> St(GRename(P1, P2))
    [] i = 15 -> St(GPut(P2))
    [] i = 16 -> St(GCas(P2))
    [] i = 17 -> Age
    [] i = 18 -> St(GRename(P2, P3))
    [] i = 19 -> St(GPut(P2))
    [] i = 20 -> St(GCas(P2))
    [] i = 21 -> St(GPut(P1))
    [] i = 22 -> St(GCas(P1))
    [] i = 23 -> St(Edit(WA, P2))
    [] i = 24 -> St(Consume(WA))
    [] i = 25 -> St(Scan(WA))
    [] i = 26 -> St(Upload(WA, P2))
    [] i = 27 -> St(Claim(WA))
    [] i = 28 -> \E h \in Handles : Reap(WA, h)
    [] i = 29 -> St(Verify(WA))
    [] i = 30 -> St(Install(WA))
    [] OTHER  -> FALSE
HInit == Init /\ k = 0
HNext == Script(k) /\ k' = k + 1
HSpec == HInit /\ [][HNext]_<<vars, k>>
HIsNext == [][Next]_vars
HReach == k < 31
==============================================================================
