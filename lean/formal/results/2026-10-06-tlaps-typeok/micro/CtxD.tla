---- MODULE CtxD ----
(* LeanP1Anc's 27-field Writer record: how an EXCEPT typing obligation fares. *)
EXTENDS Naturals, TLAPS
VARIABLES w, seq
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
THEOREM C7 == WriterInit \in Writer BY DEF WriterInit, Writer, Opt
THEOREM C6 == ASSUME NEW W \in Writer PROVE W.memo \in Nat BY DEF Writer
====
