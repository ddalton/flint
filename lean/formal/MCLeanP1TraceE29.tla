------------------------- MODULE MCLeanP1TraceE29 ----------------------------
(***************************************************************************)
(* GATE COPY (2026-10-14), over LeanP1 and its claims NR5 and AN5, of      *)
(*   results/2026-10-07-tlaps-m5/MCLeanP1M5H2.tla                          *)
(* E29: a second SCRIPTED behaviour (every step a Next step, with Next's   *)
(* frame), on the model with E27's Edit base.  The conflict that names B's *)
(* baseline's SUCCESSOR, not the baseline itself:                          *)
(*   o = <<P1, 1>> renamed P1 -> P2; A edits b from o and commits it into  *)
(*   the empty P1 (conflict <<P1, o>>), then restarts before Finish (its   *)
(*   baseline stays o); B takes b; the gateway saves b' over b; A deletes  *)
(*   its file and writes c (base: its baseline o) and commits it over b'   *)
(*   (foreign: conflict <<P1, b'>>); the gateway deletes c (udel <<P1, c>>,*)
(*   not derived from b) and renames o back; B steps b -> o.               *)
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
    [] i = 6  -> St(Upload(WA, P1))
    [] i = 7  -> St(Claim(WA))
    [] i = 8  -> St(Verify(WA))
    [] i = 9  -> St(Install(WA))
    [] i = 10 -> St(Restart(WA))
    [] i = 11 -> St(RPullRead(WB))
    [] i = 12 -> St(RPullSync(WB))
    [] i = 13 -> St(GPut(P1))
    [] i = 14 -> St(GCas(P1))
    [] i = 15 -> St(Delete(WA, P1))
    [] i = 16 -> St(Edit(WA, P1))
    [] i = 17 -> St(Consume(WA))
    [] i = 18 -> St(Scan(WA))
    [] i = 19 -> St(Upload(WA, P1))
    [] i = 20 -> St(Claim(WA))
    [] i = 21 -> St(Verify(WA))
    [] i = 22 -> St(Install(WA))
    [] i = 23 -> St(Collect(WA)) \/ St(Finish(WA))
    [] i = 24 -> St(Finish(WA)) \/ St(GDelete(P1))
    [] i = 25 -> St(GDelete(P1)) \/ St(GRename(P2, P1))
    [] i = 26 -> St(GRename(P2, P1)) \/ St(RPullRead(WB))
    [] i = 27 -> St(RPullRead(WB)) \/ St(RPullSync(WB))
    [] i = 28 -> St(RPullSync(WB))
    [] OTHER  -> FALSE
HInit == Init /\ k = 0
HNext == Script(k) /\ k' = k + 1
HSpec == HInit /\ [][HNext]_<<vars, k>>
HIsNext == [][Next]_vars
HReach == k < 29
==============================================================================
