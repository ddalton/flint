---- MODULE LeanP1AncParse ----
(* M0: tlapm must accept LeanP1Anc.tla where it refused LeanP1.tla; the same two trivial theorems. *)
EXTENDS LeanP1Anc, TLAPS
THEOREM InitMv == Init => mv = Nil
BY DEF Init
THEOREM InitHolder == Init => holder = "none"
BY DEF Init
====
