---- MODULE Rec27c ----
(* LeanP1Anc's 27-field Writer record: how an EXCEPT typing obligation fares. *)
EXTENDS Naturals, TLAPS
CONSTANTS Nil, Paths, Gens, Writers
Handles == Paths \X Gens
Opt(S) == S \cup {Nil}
Writer ==
  [st : {"off", "on"},
   pc : {"idle", "consumed", "scanned", "claimed", "cased", "pulling"},
   local : [Paths -> Opt(Handles)],
   baseline : [Paths -> Opt(Handles)],
   integrated : SUBSET Gens,
   uploads : SUBSET Paths, deletes : SUBSET Paths,
   snap : [Paths -> Opt(Handles)],
   upDone : SUBSET Paths, gone : SUBSET Paths, verified : BOOLEAN,
   inst : [Paths -> Opt(Handles)],
   retire : SUBSET Handles,
   collected : BOOLEAN,
   adv : BOOLEAN,
   synced : Nat,
   derived : Nat,
   skipped : SUBSET Paths,
   scope : SUBSET Paths,
   sStage : {"none", "saved", "mid"},
   sTgt : SUBSET Paths, sDrop : SUBSET Paths, sKeep : SUBSET Paths,
   sHeld : [Paths -> Opt(Handles)],
   unlinked : SUBSET Paths,
   memo : Nat, rnow : Nat]


\* (i) thirteen clauses, constant values only.
THEOREM E1 ==
  ASSUME NEW W \in Writer
  PROVE  [W EXCEPT !.pc = "idle", !.adv = FALSE, !.uploads = {}, !.deletes = {},
            !.snap = [p \in Paths |-> Nil], !.upDone = {}, !.gone = {}, !.verified = FALSE,
            !.collected = FALSE, !.synced = 0, !.derived = 0, !.skipped = {}, !.sStage = "none"] \in Writer
BY DEF Writer, Opt
\* (ii) thirteen clauses, the four computed values opaque and typed.
THEOREM E2 ==
  ASSUME NEW W \in Writer, NEW bl \in [Paths -> Opt(Handles)], NEW sy \in Nat, NEW dv \in Nat, NEW sk \in SUBSET Paths
  PROVE  [W EXCEPT !.pc = "idle", !.baseline = bl, !.synced = sy, !.derived = dv, !.skipped = sk,
            !.adv = FALSE, !.uploads = {}, !.deletes = {}, !.snap = [p \in Paths |-> Nil],
            !.upDone = {}, !.gone = {}, !.verified = FALSE, !.collected = FALSE] \in Writer
BY DEF Writer, Opt
\* (iii) seven clauses, computed values inline (Consume's shape).
THEOREM E3 ==
  ASSUME NEW W \in Writer, NEW d \in [Paths -> Opt(Handles)], NEW tk \in SUBSET Paths, NEW n \in Nat, NEW G \in SUBSET Gens
  PROVE  [W EXCEPT !.pc = "consumed",
            !.local = [p \in Paths |-> IF p \in tk THEN d[p] ELSE W.local[p]],
            !.baseline = [p \in Paths |-> IF p \in tk THEN d[p] ELSE W.baseline[p]],
            !.integrated = W.integrated \cup G,
            !.synced = IF tk = {} THEN W.synced ELSE n,
            !.derived = IF tk = {} THEN 0 ELSE n,
            !.skipped = {p \in Paths \ tk : d[p] # W.baseline[p]}] \in Writer
BY DEF Writer, Opt
\* (iv) the same seven, values opaque.
THEOREM E4 ==
  ASSUME NEW W \in Writer, NEW lc \in [Paths -> Opt(Handles)], NEW bl \in [Paths -> Opt(Handles)],
         NEW G \in SUBSET Gens, NEW sy \in Nat, NEW dv \in Nat, NEW sk \in SUBSET Paths
  PROVE  [W EXCEPT !.pc = "consumed", !.local = lc, !.baseline = bl, !.integrated = G,
            !.synced = sy, !.derived = dv, !.skipped = sk] \in Writer
BY DEF Writer, Opt
\* (v) a chain: six clauses then seven more on the result.
THEOREM E5 ==
  ASSUME NEW W \in Writer, NEW bl \in [Paths -> Opt(Handles)], NEW sy \in Nat, NEW dv \in Nat, NEW sk \in SUBSET Paths
  PROVE  [W EXCEPT !.pc = "idle", !.baseline = bl, !.synced = sy, !.derived = dv, !.skipped = sk, !.adv = FALSE] \in Writer
BY DEF Writer, Opt
\* (vi) field reads through a 13-clause EXCEPT.
THEOREM E6 ==
  ASSUME NEW W \in Writer, NEW bl \in [Paths -> Opt(Handles)], NEW sy \in Nat, NEW dv \in Nat, NEW sk \in SUBSET Paths
  PROVE  LET R == [W EXCEPT !.pc = "idle", !.baseline = bl, !.synced = sy, !.derived = dv, !.skipped = sk,
            !.adv = FALSE, !.uploads = {}, !.deletes = {}, !.snap = [p \in Paths |-> Nil],
            !.upDone = {}, !.gone = {}, !.verified = FALSE, !.collected = FALSE]
         IN R.local = W.local /\ R.uploads = {} /\ R.baseline = bl
BY DEF Writer, Opt
====
