---- MODULE Rec ----
(* Minimal repro: a module that merely CONTAINS a RECURSIVE operator, never used by the theorem. *)
EXTENDS Naturals, TLAPS
VARIABLE x
RECURSIVE Sum(_)
Sum(n) == IF n = 0 THEN 0 ELSE n + Sum(n - 1)
Init == x = 0
THEOREM InitX == Init => x = 0
BY DEF Init
====
