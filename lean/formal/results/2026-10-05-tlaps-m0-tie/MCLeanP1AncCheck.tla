-------------------------- MODULE MCLeanP1AncCheck ---------------------------
(***************************************************************************)
(* TLC ONLY (tlapm never sees this module): LeanP1Anc.tla's restated        *)
(* `Derives` and `Supersedes` against LeanP1.tla's RECURSIVE originals,    *)
(* state by state, and the ghost against the base chain it claims to       *)
(* compute.  A world that holds these three on top of the gate's cfg, with *)
(* the gate's exact distinct count, ties the copy to the original.         *)
(***************************************************************************)
EXTENDS LeanP1Anc

RECURSIVE DerivesRec(_, _)
DerivesRec(k, h) == \/ k = h
                    \/ k # Nil /\ h # Nil /\ Content(k) = Content(h)
                    \/ k # Nil /\ base[k] # Nil /\ DerivesRec(base[k], h)

RECURSIVE SupersedesRec(_, _, _)
SupersedesRec(k, h, p) ==
  \/ k = h
  \/ k # Nil /\ h # Nil /\ Content(k) = Content(h)
  \/ k # Nil /\ base[k] # Nil /\ SupersedesRec(base[k], h, p)
  \/ <<p, k>> \in acked /\ <<p, h>> \in acked /\ Later(k, h)

\* The ghost IS the base chain.
AncSound == \A h \in Handles : anc[h] = AncOf(base[h])
\* The restated operators agree with the recursive ones on every pair.
DerivesEquiv == \A k, h \in Opt(Handles) : Derives(k, h) <=> DerivesRec(k, h)
SupersedesEquiv ==
  \A k, h \in Opt(Handles), p \in Paths : Supersedes(k, h, p) <=> SupersedesRec(k, h, p)
==============================================================================
