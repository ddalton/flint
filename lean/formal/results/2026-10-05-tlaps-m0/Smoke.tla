---- MODULE Smoke ----
(* M0 smoke test: a one-variable inductive invariant through every stage of a tlapm proof. *)
EXTENDS Naturals, TLAPS
VARIABLE x
Init == x = 0
Next == x' = x + 1
Spec == Init /\ [][Next]_x
Inv == x \in Nat
THEOREM Safety == Spec => []Inv
<1>1. Init => Inv BY DEF Init, Inv
<1>2. Inv /\ [Next]_x => Inv' BY DEF Inv, Next
<1>. QED BY <1>1, <1>2, PTL DEF Spec
====
