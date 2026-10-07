---- MODULE Rec27 ----
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

\* One record, three fields, one of them a nested function update.
THEOREM R1 ==
  ASSUME NEW W \in Writer, NEW h \in Handles, NEW p \in Paths, NEW n \in Nat
  PROVE  [W EXCEPT !.pc = "consumed",
                   !.local = [q \in Paths |-> IF q = p THEN h ELSE @[q]],
                   !.synced = n] \in Writer
BY DEF Writer, Opt

\* A function of records, the path form the actions use.
THEOREM R2 ==
  ASSUME NEW w \in [Writers -> Writer], NEW s \in Writers,
         NEW h \in Handles, NEW p \in Paths, NEW n \in Nat
  PROVE  [w EXCEPT ![s].pc = "consumed", ![s].local[p] = h, ![s].synced = n] \in [Writers -> Writer]
BY DEF Writer, Opt

\* Field access through an EXCEPT on other fields.
THEOREM R3 ==
  ASSUME NEW w \in [Writers -> Writer], NEW s \in Writers, NEW t \in Writers
  PROVE  [w EXCEPT ![s].pc = "consumed"][t].uploads = w[t].uploads
BY DEF Writer

\* Reading a field's type out of membership.
THEOREM R4 ==
  ASSUME NEW W \in Writer, NEW p \in Paths, W.baseline[p] # Nil
  PROVE  W.baseline[p] \in Handles /\ W.baseline[p][2] \in Gens
BY DEF Writer, Opt, Handles
====
