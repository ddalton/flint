---- MODULE Asm ----
(* Is an UNNAMED module-level ASSUME usable without being cited? *)
EXTENDS Naturals, TLAPS
CONSTANT K
ASSUME K \in Nat
THEOREM T1 == K + 1 \in Nat OBVIOUS
THEOREM T2 == K + 1 \in Nat BY SMT
THEOREM T3 == K + 1 \in Nat BY Zenon
====
