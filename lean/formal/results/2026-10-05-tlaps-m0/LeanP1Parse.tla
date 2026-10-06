---- MODULE LeanP1Parse ----
(* M0: does tlapm parse LeanP1.tla as written (two RECURSIVE operators)? One trivial theorem. *)
EXTENDS LeanP1, TLAPS
THEOREM InitMv == Init => mv = Nil
BY DEF Init
THEOREM InitHolder == Init => holder = "none"
BY DEF Init
====
