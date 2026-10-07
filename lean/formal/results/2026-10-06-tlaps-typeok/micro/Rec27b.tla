---- MODULE Rec27b ----
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

WriterInit ==
  [st |-> "off", pc |-> "idle",
   local |-> [p \in Paths |-> Nil], baseline |-> [p \in Paths |-> Nil],
   integrated |-> {},
   uploads |-> {}, deletes |-> {},
   snap |-> [p \in Paths |-> Nil], upDone |-> {}, gone |-> {},
   verified |-> FALSE, inst |-> [p \in Paths |-> Nil], retire |-> {},
   collected |-> FALSE, adv |-> FALSE, synced |-> 0, derived |-> 0, skipped |-> {},
   scope |-> {}, sStage |-> "none", sTgt |-> {}, sDrop |-> {}, sKeep |-> {},
   sHeld |-> [p \in Paths |-> Nil], unlinked |-> {}, memo |-> 0, rnow |-> 0]

\* All 27 fields at once, from membership.
THEOREM R5 ==
  ASSUME NEW W \in Writer
  PROVE  /\ W.st \in {"off", "on"}
         /\ W.pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
         /\ W.local \in [Paths -> Opt(Handles)] /\ W.baseline \in [Paths -> Opt(Handles)]
         /\ W.integrated \subseteq Gens
         /\ W.uploads \subseteq Paths /\ W.deletes \subseteq Paths
         /\ W.snap \in [Paths -> Opt(Handles)]
         /\ W.upDone \subseteq Paths /\ W.gone \subseteq Paths /\ W.verified \in BOOLEAN
         /\ W.inst \in [Paths -> Opt(Handles)]
         /\ W.retire \subseteq Handles
         /\ W.collected \in BOOLEAN /\ W.adv \in BOOLEAN
         /\ W.synced \in Nat /\ W.derived \in Nat
         /\ W.skipped \subseteq Paths /\ W.scope \subseteq Paths
         /\ W.sStage \in {"none", "saved", "mid"}
         /\ W.sTgt \subseteq Paths /\ W.sDrop \subseteq Paths /\ W.sKeep \subseteq Paths
         /\ W.sHeld \in [Paths -> Opt(Handles)]
         /\ W.unlinked \subseteq Paths
         /\ W.memo \in Nat /\ W.rnow \in Nat
BY DEF Writer
\* The same, one field.
THEOREM R6 == ASSUME NEW W \in Writer PROVE W.sHeld \in [Paths -> Opt(Handles)] /\ W.memo \in Nat
BY DEF Writer
\* The initial record is in the set.
THEOREM R7 == WriterInit \in Writer
BY DEF WriterInit, Writer, Opt
\* A full EXCEPT, thirteen fields, no @ (FinishW's shape).
THEOREM R8 ==
  ASSUME NEW W \in Writer, NEW d \in [Paths -> Opt(Handles)], NEW n \in Nat
  PROVE  [W EXCEPT !.pc = "idle",
            !.baseline = [p \in Paths |-> IF p \in W.uploads \cap W.upDone THEN W.snap[p]
                                          ELSE IF p \in W.deletes /\ W.inst[p] = Nil THEN Nil
                                          ELSE W.baseline[p]],
            !.synced = IF W.inst = d THEN n ELSE W.synced,
            !.derived = IF W.adv /\ W.inst = d THEN n ELSE W.derived,
            !.skipped = IF W.adv /\ W.inst = d
                          THEN W.skipped \ ((W.uploads \cap W.upDone) \cup {p \in W.deletes : W.inst[p] = Nil})
                          ELSE W.skipped,
            !.adv = FALSE,
            !.uploads = {}, !.deletes = {}, !.snap = [p \in Paths |-> Nil],
            !.upDone = {}, !.gone = {}, !.verified = FALSE,
            !.collected = FALSE] \in Writer
BY DEF Writer, Opt
====
