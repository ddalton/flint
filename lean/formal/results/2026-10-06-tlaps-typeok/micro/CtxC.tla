---- MODULE CtxC ----
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
C0W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C0(s) == w' = [w EXCEPT ![s] = C0W(s)]
C1W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C1(s) == w' = [w EXCEPT ![s] = C1W(s)]
C2W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C2(s) == w' = [w EXCEPT ![s] = C2W(s)]
C3W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C3(s) == w' = [w EXCEPT ![s] = C3W(s)]
C4W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C4(s) == w' = [w EXCEPT ![s] = C4W(s)]
C5W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C5(s) == w' = [w EXCEPT ![s] = C5W(s)]
C6W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C6(s) == w' = [w EXCEPT ![s] = C6W(s)]
C7W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C7(s) == w' = [w EXCEPT ![s] = C7W(s)]
C8W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C8(s) == w' = [w EXCEPT ![s] = C8W(s)]
C9W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C9(s) == w' = [w EXCEPT ![s] = C9W(s)]
C10W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C10(s) == w' = [w EXCEPT ![s] = C10W(s)]
C11W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C11(s) == w' = [w EXCEPT ![s] = C11W(s)]
C12W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C12(s) == w' = [w EXCEPT ![s] = C12W(s)]
C13W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C13(s) == w' = [w EXCEPT ![s] = C13W(s)]
C14W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C14(s) == w' = [w EXCEPT ![s] = C14W(s)]
C15W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C15(s) == w' = [w EXCEPT ![s] = C15W(s)]
C16W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C16(s) == w' = [w EXCEPT ![s] = C16W(s)]
C17W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C17(s) == w' = [w EXCEPT ![s] = C17W(s)]
C18W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C18(s) == w' = [w EXCEPT ![s] = C18W(s)]
C19W(s) == [w[s] EXCEPT !.pc = "idle", !.synced = seq, !.skipped = w[s].skipped \cup {}, !.adv = FALSE]
C19(s) == w' = [w EXCEPT ![s] = C19W(s)]
THEOREM C7 == WriterInit \in Writer BY DEF WriterInit, Writer, Opt
THEOREM C6 == ASSUME NEW W \in Writer PROVE W.memo \in Nat BY DEF Writer
====
