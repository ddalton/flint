---- MODULE MCAKXR ----
\* Step 1 (2026-10-17): the PRE-FIX model (3ed14442, no rename row) from AKXB's
\* last state (it broke BoundOK).  Does AK1 fail from here?
EXTENDS LeanP1, TLC
CONSTANTS p1, p2, A
Init18 ==
/\ live = {<<p1, 1>>, <<p2, 1>>, <<p2, 2>>}
/\ minted = {<<p1, 1>>, <<p2, 1>>, <<p2, 2>>}
/\ doc = (p1 :> Nil @@ p2 :> Nil)
/\ seq = 6
/\ tomb = (p1 :> <<p2, 2>> @@ p2 :> <<p2, 2>>)
/\ base = (<<p1, 1>> :> Nil @@ <<p1, 2>> :> Nil @@ <<p1, 3>> :> Nil @@ <<p1, 4>> :> Nil @@ <<p2, 1>> :> Nil @@ <<p2, 2>> :> <<p2, 1>> @@ <<p2, 3>> :> Nil @@ <<p2, 4>> :> Nil)
/\ acked = {<<p1, <<p2, 2>>>>, <<p2, <<p2, 2>>>>}
/\ conflicts = {}
/\ holder = "none"
/\ nextGen = 3
/\ ui = 0
/\ reqs = 4
/\ barriers = 2
/\ w = (A :> [pend |-> [id |-> 1, fold |-> 1, nonces |-> {1}], bound |-> [id |-> 1, fold |-> 1, wd |-> {}, doc |-> (p1 :> Nil @@ p2 :> Nil), inst |-> FALSE, seq |-> 6, clk |-> "sentinel"], car |-> [id |-> 0, dropped |-> {}, uncited |-> {}, paths |-> {}, deletes |-> {}, pub |-> (p1 :> Nil @@ p2 :> Nil)], dset |-> {p2}, moved |-> {}, decl |-> (p1 :> Nil @@ p2 :> <<p2, 2>>), deletes |-> {}, local |-> (p1 :> Nil @@ p2 :> <<p2, 2>>), baseline |-> (p1 :> Nil @@ p2 :> <<p2, 2>>), nt |-> 1, cFold |-> 0, pc |-> "idle", cId |-> 0, uploads |-> {}, snap |-> (p1 :> Nil @@ p2 :> Nil), inst |-> (p1 :> Nil @@ p2 :> Nil), gone |-> {}, lost |-> {}, upDone |-> {}, sStage |-> "none", sDrop |-> {}, sKeep |-> {}, st |-> "on", integrated |-> {1, 2}, verified |-> FALSE, retire |-> {}, collected |-> FALSE, adv |-> FALSE, synced |-> 6, derived |-> 5, skipped |-> {}, scope |-> {p1, p2}, sTgt |-> {}, sHeld |-> (p1 :> Nil @@ p2 :> Nil), unlinked |-> {}, memo |-> 0, rnow |-> 0, ack |-> [dropped |-> {}, inst |-> FALSE, nonces |-> {}, st |-> "none", seq |-> 0, src |-> "none"], clk |-> "none", iseq |-> 0, instd |-> FALSE, cN |-> {1}, aN |-> {}, capped |-> {}, dMint |-> (p1 :> 0 @@ p2 :> 3), stamps |-> {}, deposed |-> FALSE, nd |-> 0])
/\ took = (A :> (p1 :> {1} @@ p2 :> {1, 2}))
/\ gw = (p1 :> Nil @@ p2 :> Nil)
/\ mv = Nil
/\ udel = {<<p1, <<p1, 1>>>>, <<p2, <<p2, 2>>>>}
/\ restarts = 1
/\ syncs = 0
/\ regressed = FALSE
/\ rescopes = 0
/\ fails = 0
/\ upped = {<<p1, 1>>, <<p2, 1>>, <<p2, 2>>}
/\ copies = 0
/\ orig = (<<p1, 1>> :> Nil @@ <<p1, 2>> :> Nil @@ <<p1, 3>> :> Nil @@ <<p1, 4>> :> Nil @@ <<p2, 1>> :> Nil @@ <<p2, 2>> :> Nil @@ <<p2, 3>> :> Nil @@ <<p2, 4>> :> Nil)
/\ retiring = {<<p1, 1>>, <<p2, 1>>, <<p2, 2>>}
/\ aged = {}
/\ ages = 0
/\ rdoc = (p1 :> Nil @@ p2 :> Nil)
/\ rlag = TRUE
Spec18 == Init18 /\ [][Next]_vars
====
