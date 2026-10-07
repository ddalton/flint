---- MODULE Bool ----
(* Is an equality a BOOLEAN to the backends? *)
EXTENDS Naturals, TLAPS
THEOREM B1 == \A a, b \in Nat : (a = b) \in BOOLEAN OBVIOUS
THEOREM B2 == \A a, b \in Nat : (a = b) \in BOOLEAN BY SMT
THEOREM B3 == \A a, b \in Nat : (a = b) \in BOOLEAN BY Zenon
THEOREM B4 == \A a, b \in Nat : [adv |-> (a = b)] \in [adv : BOOLEAN] OBVIOUS
THEOREM B5 == \A S \in SUBSET Nat : (S = {}) \in BOOLEAN OBVIOUS
====
