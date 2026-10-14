---------------------------- MODULE MCLeanP1M5H5 -----------------------------
(***************************************************************************)
(* E38: a hand-found AN3 trace, SCRIPTED (E27's method: every step is a    *)
(* Next step, `k` counts them).  One writer.                               *)
(*   A edits k from the seed, uploads it, restarts, and re-uploads it as   *)
(*   the copy c; commits c at P1 (clean there from now on).  The gateway   *)
(*   renames P1 -> P2 (acked <<P2, c>>), a save h' lands over c at P2,     *)
(*   G elapses (c retired), a save h'' lands in the empty P1 (tombstone    *)
(*   cleared).  A's consume fails to fetch P1 (its tree keeps c there);    *)
(*   A makes a new file at P2 and commits it over h' (conflict <<P2, h'>>, *)
(*   naming h', not c); in its commit section A reaps c.  Nothing names c. *)
(***************************************************************************)
EXTENDS MCLeanP1M5D
CONSTANTS WA, P1, P2
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
    [] i = 18 -> St(GPut(P1))
    [] i = 19 -> St(GCas(P1))
    [] i = 20 -> St(Edit(WA, P2))
    [] i = 21 -> St(Consume(WA))
    [] i = 22 -> St(Scan(WA))
    [] i = 23 -> St(Upload(WA, P2))
    [] i = 24 -> St(Claim(WA))
    [] i = 25 -> \E h \in Handles : Reap(WA, h)
    [] i = 26 -> St(Verify(WA))
    [] i = 27 -> St(Install(WA))
    [] OTHER  -> FALSE
HInit == Init /\ k = 0
HNext == Script(k) /\ k' = k + 1
HSpec == HInit /\ [][HNext]_<<vars, k>>
HIsNext == [][Next]_vars
HReach == k < 28
==============================================================================
