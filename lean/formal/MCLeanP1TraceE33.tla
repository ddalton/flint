------------------------- MODULE MCLeanP1TraceE33 ----------------------------
(***************************************************************************)
(* GATE COPY (2026-10-14), over LeanP1 and its claims NR5 and AN5, of      *)
(*   results/2026-10-07-tlaps-m5/MCLeanP1M5H4.tla                          *)
(* E33: E27's script, the second edit made over a DELETED file (local    *)
(* Nil) after the SCAN, before the install (E31: after the install).     *)
(* E27: one SCRIPTED behaviour of LeanP1 (every step is a Next step, with  *)
(* Next's frame), to check a hand-found NR3 trace too deep for BFS.  `k`   *)
(* counts the steps; step i may take only Script(i).                       *)
(*   b0 = <<P1, 1>> renamed P1 -> P2; A edits k1 from b0 and commits it    *)
(*   into the empty P1 (conflict <<P1, b0>>); before Finish A edits k2,    *)
(*   whose base is A's STALE baseline b0; B (a reader) takes k1; A commits *)
(*   k2 over k1 (not foreign: no record); the gateway deletes k2 (udel     *)
(*   <<P1, k2>>, not derived from k1) and renames b0 back; B takes b0.     *)
(***************************************************************************)
EXTENDS LeanP1
CONSTANTS WA, WB, P1, P2
VARIABLE k
St(A) == A /\ TookUpdate /\ RetUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>
Script(i) ==
  CASE i = 0  -> St(Checkout(WA))
    [] i = 1  -> St(Checkout(WB))
    [] i = 2  -> St(GRename(P1, P2))
    [] i = 3  -> St(Edit(WA, P1))
    [] i = 4  -> St(Consume(WA))
    [] i = 5  -> St(Scan(WA))
    [] i = 6  -> St(Delete(WA, P1))
    [] i = 7  -> St(Edit(WA, P1))
    [] i = 8  -> St(Upload(WA, P1))
    [] i = 9  -> St(Claim(WA))
    [] i = 10 -> St(Verify(WA))
    [] i = 11 -> St(Install(WA))
    [] i = 12 -> St(Collect(WA)) \/ St(Finish(WA))
    [] i = 13 -> St(Finish(WA)) \/ St(RPullRead(WB))
    [] i = 14 -> St(RPullRead(WB)) \/ St(RPullSync(WB))
    [] i = 15 -> St(RPullSync(WB)) \/ St(Consume(WA))
    [] i = 16 -> St(Consume(WA)) \/ St(Scan(WA))
    [] i = 17 -> St(Scan(WA)) \/ St(Upload(WA, P1))
    [] i = 18 -> St(Upload(WA, P1)) \/ St(Claim(WA))
    [] i = 19 -> St(Claim(WA)) \/ St(Verify(WA))
    [] i = 20 -> St(Verify(WA)) \/ St(Install(WA))
    [] i = 21 -> St(Install(WA)) \/ St(Collect(WA))
    [] i = 22 -> St(Collect(WA)) \/ St(Finish(WA))
    [] i = 23 -> St(Finish(WA)) \/ St(GDelete(P1))
    [] i = 24 -> St(GDelete(P1)) \/ St(GRename(P2, P1))
    [] i = 25 -> St(GRename(P2, P1)) \/ St(RPullRead(WB))
    [] i = 26 -> St(RPullRead(WB)) \/ St(RPullSync(WB))
    [] i = 27 -> St(RPullSync(WB))
    [] OTHER  -> FALSE
HInit == Init /\ k = 0
HNext == Script(k) /\ k' = k + 1
HSpec == HInit /\ [][HNext]_<<vars, k>>
\* Every scripted step is a Next step (checked as a property).
HIsNext == [][Next]_vars
\* Reached: the script's end.
\* The end: the step into k = 27 is the reader's take the trace is about
\* (the EditBase mutant breaks NR5 on exactly that step); the script's
\* last alternatives let it land one index early, so k = 28 is never reached.
HReach == k < 27
==============================================================================
