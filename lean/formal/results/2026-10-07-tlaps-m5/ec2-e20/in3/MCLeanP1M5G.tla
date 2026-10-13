---------------------------- MODULE MCLeanP1M5G ----------------------------
(* M5, E23: NR3 fails with two COMMITTING writers and gateway removals (E21  *)
(* W2C, d19): A's committed delete of p1 leaves only a tombstone, a rename    *)
(* brings an older version back, and B -- holding what A deleted -- takes it. *)
(* An explicit delete then an explicit rename: no silent regress, but no      *)
(* record names it.  udel records the GATEWAY's acknowledged deletes; `cdel`  *)
(* is its twin for a COMMIT's, as a history variable here (LeanP1.tla is not  *)
(* edited).  NR4 = NR3, also exempt when a commit deleted the baseline or a   *)
(* version derived from it at p.                                              *)
EXTENDS MCLeanP1M5R
VARIABLE cdel
CommitDeletes == {<<p, doc[p]>> : p \in {q \in Paths : doc[q] # Nil /\ doc'[q] = Nil}}
MCInit == Init /\ cdel = {}
MCNext == Next /\ cdel' = IF \E s \in Writers : Install(s) THEN cdel \cup CommitDeletes ELSE cdel
MCSpec == MCInit /\ [][MCNext]_<<vars, cdel>>
BackR4(s, p) == BackR3(s, p) /\ ~\E d \in cdel : d[1] = p /\ Derives(d[2], w[s].baseline[p])
NR4 == [][\A s \in Writers, p \in Paths : ~(BackR4(s, p) /\ Takes(s, p))]_<<vars, cdel>>
\* The calibration: NR3 under MCSpec must still fail in W2C, as under Spec.
NR3G == [][\A s \in Writers, p \in Paths : ~(BackR3(s, p) /\ Takes(s, p))]_<<vars, cdel>>
==============================================================================
