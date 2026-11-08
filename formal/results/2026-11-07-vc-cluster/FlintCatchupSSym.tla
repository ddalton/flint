--------------------------- MODULE FlintCatchupSSym ---------------------------
(***************************************************************************)
(* SYMMETRY over the replica ids for FlintCatchupS (rev 13 or later: the   *)
(* ids may be model values).  Rebuilt 2026-11-07 for gate item 3; the      *)
(* c29 wrapper (34243d06) is not on main.                                  *)
(*                                                                         *)
(* The model uses replica order in two places: First (CoverageWalk FALSE   *)
(* only; every arm here has it TRUE) and the stamp read's CHOOSE in        *)
(* NewestS, which picks one of the maximal views.  The read is blind to    *)
(* ids when every maximal view agrees on the fields the read uses: seq,    *)
(* gen, size and mem (Core).  c29 first checked that the newest view is    *)
(* unique; that is too strict, since two maximal views may differ in the   *)
(* released view (rgen, rmem), which the read never consults.  NewestS's   *)
(* fields read are .seq, .gen, .size and .mem only.                        *)
(***************************************************************************)
EXTENDS FlintCatchupS, TLC

Perms == Permutations(Reps)

Maximal(q) == \A p \in VAtt : ~Newer(View(stamp[p]), View(stamp[q]))

\* every maximal view the read could choose agrees on the fields it reads
Inv_ReadUnique ==
  \A q1, q2 \in VAtt : Maximal(q1) /\ Maximal(q2) => Core(View(stamp[q1])) = Core(View(stamp[q2]))

\* c29's first form, too strict: the newest view itself is unique
Inv_ViewUnique ==
  \A q1, q2 \in VAtt : Maximal(q1) /\ Maximal(q2) => View(stamp[q1]) = View(stamp[q2])
==============================================================================
