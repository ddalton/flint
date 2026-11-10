-------------------------- MODULE FlintCatchupSRSym --------------------------
(***************************************************************************)
(* SYMMETRY over the replica ids for FlintCatchupSR rev 18 (a202f0f8), for *)
(* gate item 7 at three replicas: FlintCatchupSSym (9f274f80, the          *)
(* 2026-11-07 cluster batch) on SR.                                        *)
(*                                                                         *)
(* Rev 18 adds no use of replica order: its per-leg state is functions on  *)
(* R, its quantifiers range over R or members, and its Min and Max are     *)
(* over lvol sizes.  The order is used where rev 17 uses it: First         *)
(* (CoverageWalk FALSE only; every arm here has it TRUE) and the stamp     *)
(* read's CHOOSE in NewestS.  Rev 18's VAtt also needs the stamp readable  *)
(* at the leg's tail (Readable), and Inv_ReadUnique ranges over that VAtt. *)
(***************************************************************************)
EXTENDS FlintCatchupSR, TLC

Perms == Permutations(Reps)

Maximal(q) == \A p \in VAtt : ~Newer(View(stamp[p]), View(stamp[q]))

\* every maximal view the read could choose agrees on the fields it reads
Inv_ReadUnique ==
  \A q1, q2 \in VAtt : Maximal(q1) /\ Maximal(q2) => Core(View(stamp[q1])) = Core(View(stamp[q2]))
==============================================================================
