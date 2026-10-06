---- MODULE NoRec ----
(* The control for Rec.tla: the same module with the RECURSIVE operator removed. *)
EXTENDS Naturals, TLAPS
VARIABLE x
Init == x = 0
THEOREM InitX == Init => x = 0
BY DEF Init
====
