-------------------------- MODULE FlintCatchupSSym --------------------------
(***************************************************************************)
(* SYMMETRY over the replica ids for FlintCatchupS (rev 13 lets the ids be *)
(* model values), the gate's item 3 (2026-10-31): a smaller state graph   *)
(* for the three-replica zombie arm.                                       *)
(*                                                                         *)
(* SYMMETRY is sound for a safety check only if no step tells the ids     *)
(* apart.  Every id-specific operator was audited: First (Min over ids) is *)
(* reached only with CoverageWalk FALSE (no S arm), Max/Min elsewhere are  *)
(* over epochs, no step orders or adds ids, and 0 (zx, zfreed, zLeg) is no *)
(* id.  The one CHOOSE over id-carrying values is NewestS; it is id-free   *)
(* when the newest view among VAtt is unique, which Inv_NewestUnique      *)
(* checks on the runs WITHOUT the symmetry.  Never with a PROPERTY: TLC   *)
(* and tla-mc may miss a lasso under SYMMETRY.                             *)
(***************************************************************************)
EXTENDS FlintCatchupS, TLC

Sym == Permutations(Reps)

NewestSet == {st \in {View(stamp[q]) : q \in VAtt} :
                \A q \in VAtt : ~Newer(View(stamp[q]), st)}
Inv_NewestUnique == VAtt # {} => Cardinality(NewestSet) = 1
=============================================================================
