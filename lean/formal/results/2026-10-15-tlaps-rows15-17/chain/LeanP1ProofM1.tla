---------------------------- MODULE LeanP1ProofM1 ----------------------------
EXTENDS LeanP1ProofM0

------------------------------------------------------------------------------
(* M1: Inv_OneHolder, Prop_DeleteSettles, Prop_NarrowNeverDeletes.          *)

CC == {"claimed", "cased"}
\* I1: the holder is in its commit section, and on.
Holder == holder # "none" => w[holder].pc \in CC /\ w[holder].st = "on"
\* Row 17: a deposed writer is in its commit section and is not the holder
\* (a takeover moved the lease; the straggler never holds it again until
\* Finish, Restart or the fence clears the mark).
Dep == \A s \in Writers : w[s].deposed => w[s].pc \in CC /\ holder # s
\* I6: at the CAS every delete landed (DeleteWinsPreserved: a delete over a
\* foreign change applies).
Cased == \A s \in Writers : w[s].pc = "cased" => \A p \in w[s].deletes : w[s].inst[p] = Nil
\* The scan's two sets are disjoint; what the verify withheld is an upload.
Mine == \A s \in Writers : w[s].deletes \cap w[s].uploads = {} /\ w[s].gone \subseteq w[s].uploads
\* From the scan to the finish, no path the barrier publishes or deletes is
\* one a rescope unlinked (such a path is clean: I7).
Ups == \A s \in Writers : w[s].pc \in {"scanned", "claimed", "cased"} =>
         (w[s].uploads \cup w[s].deletes) \cap w[s].unlinked = {}
\* A writer that is off has never run: every step but Checkout needs On(s).
Off == \A s \in Writers : w[s].st = "off" => w[s] = WriterInit
\* I7a: the barrier runs with no rescope in flight.
R1 == \A s \in Writers : w[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w[s].sStage = "none"
\* I7b: a path a rescope unlinked is clean until the agent touches it.
R2 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage = "none") => w[s].local[p] = w[s].baseline[p]
\* I7b with a rescope in flight: clean, or uncited by the first half --
\* dropped and not kept, its baseline gone, its bytes those the intent
\* recorded (so the second half unlinks it).
R3 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage \in {"saved", "mid"}) =>
          \/ w[s].local[p] = w[s].baseline[p]
          \/ /\ p \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[p] = Nil
             /\ w[s].local[p] # Nil /\ w[s].sHeld[p] = w[s].local[p]
\* Between the halves, every path the first half dropped is uncited.
R4 == \A s \in Writers : w[s].sStage = "mid" => \A p \in w[s].sDrop \ w[s].sKeep : w[s].baseline[p] = Nil
\* A reader's pull never spans the halves.
R5 == \A s \in Writers : w[s].pc = "pulling" => w[s].sStage \in {"none", "saved"}
Rescope == R1 /\ R2 /\ R3 /\ R4 /\ R5
M1 == Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope
IndM1 == IndTypeOK /\ M1
\* Prop_NarrowNeverDeletes's step formula.
NarrowOK == \A s \in Writers :
              (w[s].pc = "consumed" /\ w'[s].pc = "scanned") => w'[s].deletes \cap w[s].unlinked = {}

------------------------------------------------------------------------------
(* One lemma per conjunct: what a step's new tree R must satisfy.           *)

\* The step writes one tree: what every tree becomes.
LEMMA WriteAny ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R]
  PROVE  \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t]
BY DEF TypeOK

\* A step that writes no tree and keeps the holder keeps M1.
LEMMA M1Keep ==
  ASSUME M1, w' = w, holder' = holder
  PROVE  M1' /\ NarrowOK
<1>1. Inv_OneHolder' /\ Holder' /\ Dep' BY DEF M1, Inv_OneHolder, Holder, Dep
<1>2. Cased' /\ Mine' /\ Ups' /\ Off' BY DEF M1, Cased, Mine, Ups, Off
<1>3. R1' /\ R2' /\ R3' /\ R4' /\ R5' BY DEF M1, Rescope, R1, R2, R3, R4, R5
<1>4. NarrowOK BY DEF NarrowOK
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF M1, Rescope

\* The holder's three conjuncts after a step that keeps the holder.
LEMMA HolderWrite ==
  ASSUME Inv_OneHolder, Holder, Dep, holder \in Writers \cup {"none"},
         NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         holder' = holder,
         (R.pc \in CC) <=> (w[s].pc \in CC),
         R.deposed = w[s].deposed,
         w[s].st = "on" => R.st = "on"
  PROVE  Inv_OneHolder' /\ Holder' /\ Dep'
<1>1. Inv_OneHolder' BY DEF Inv_OneHolder, CC
<1>2. Holder' BY DEF Holder, CC
<1>3. Dep' BY DEF Dep, CC
<1>. QED BY <1>1, <1>2, <1>3

LEMMA CasedWrite ==
  ASSUME Cased, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc = "cased" => \A p \in R.deletes : R.inst[p] = Nil
  PROVE  Cased'
BY DEF Cased

LEMMA MineWrite ==
  ASSUME Mine, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.deletes \cap R.uploads = {} /\ R.gone \subseteq R.uploads
  PROVE  Mine'
BY DEF Mine

LEMMA UpsWrite ==
  ASSUME Ups, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc \in {"scanned", "claimed", "cased"} => (R.uploads \cup R.deletes) \cap R.unlinked = {}
  PROVE  Ups'
BY DEF Ups

LEMMA OffWrite ==
  ASSUME Off, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.st = "off" => R = WriterInit
  PROVE  Off'
BY DEF Off

\* Rescope after a step that writes R.
LEMMA RescopeWrite ==
  ASSUME Rescope, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc \in {"consumed", "scanned", "claimed", "cased"} => R.sStage = "none",
         \A p \in Paths : (p \in R.unlinked /\ R.sStage = "none") => R.local[p] = R.baseline[p],
         \A p \in Paths : (p \in R.unlinked /\ R.sStage \in {"saved", "mid"}) =>
           \/ R.local[p] = R.baseline[p]
           \/ /\ p \in R.sDrop \ R.sKeep /\ R.baseline[p] = Nil
              /\ R.local[p] # Nil /\ R.sHeld[p] = R.local[p],
         R.sStage = "mid" => \A p \in R.sDrop \ R.sKeep : R.baseline[p] = Nil,
         R.pc = "pulling" => R.sStage \in {"none", "saved"}
  PROVE  Rescope'
<1>1. R1' BY DEF Rescope, R1
<1>2. R2' BY DEF Rescope, R2
<1>3. R3' BY DEF Rescope, R3
<1>4. R4' BY DEF Rescope, R4
<1>5. R5' BY DEF Rescope, R5
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Rescope

\* Rescope after a step that keeps the rescope fields, except that the
\* unlinked set may shrink and the tree may change off it.
LEMMA RescopeShrink ==
  ASSUME Rescope, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.sStage = w[s].sStage, R.unlinked \subseteq w[s].unlinked,
         \A q \in R.unlinked : R.local[q] = w[s].local[q],
         R.baseline = w[s].baseline, R.sDrop = w[s].sDrop, R.sKeep = w[s].sKeep, R.sHeld = w[s].sHeld,
         R.pc \in {"consumed", "scanned", "claimed", "cased"} => R.sStage = "none",
         R.pc = "pulling" => R.sStage \in {"none", "saved"}
  PROVE  Rescope'
<1>1. \A p \in Paths : (p \in R.unlinked /\ R.sStage = "none") => R.local[p] = R.baseline[p]
  BY DEF Rescope, R2
<1>2. \A p \in Paths : (p \in R.unlinked /\ R.sStage \in {"saved", "mid"}) =>
        \/ R.local[p] = R.baseline[p]
        \/ /\ p \in R.sDrop \ R.sKeep /\ R.baseline[p] = Nil
           /\ R.local[p] # Nil /\ R.sHeld[p] = R.local[p]
  BY DEF Rescope, R3
<1>3. R.sStage = "mid" => \A p \in R.sDrop \ R.sKeep : R.baseline[p] = Nil BY DEF Rescope, R4
<1>. QED BY <1>1, <1>2, <1>3, RescopeWrite

\* Prop_NarrowNeverDeletes's step formula after a step that writes R.
LEMMA NarrowWrite ==
  ASSUME NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         (w[s].pc = "consumed" /\ R.pc = "scanned") => R.deletes \cap w[s].unlinked = {}
  PROVE  NarrowOK
BY DEF NarrowOK

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M1 == Init => IndM1
<1>. SUFFICES ASSUME Init PROVE IndM1 OBVIOUS
<1>1. IndTypeOK BY Init_TypeOK
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>2a. holder = "none" BY DEF Init
<1>3. \A s \in Writers : /\ w[s].st = "off" /\ w[s].pc = "idle"
                         /\ w[s].deletes = {} /\ w[s].uploads = {} /\ w[s].gone = {}
                         /\ w[s].unlinked = {} /\ w[s].sStage = "none" /\ w[s].deposed = FALSE
  BY <1>2 DEF WriterInit
<1>4. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off
  BY <1>2, <1>2a, <1>3, NoneWriter DEF Inv_OneHolder, Holder, Dep, Cased, Mine, Ups, Off
<1>5. Rescope BY <1>3 DEF Rescope, R1, R2, R3, R4, R5
<1>. QED BY <1>1, <1>4, <1>5 DEF IndM1, M1

------------------------------------------------------------------------------
(* Steps that write no tree.                                                *)

LEMMA GPut_M1 ==
  ASSUME IndM1, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GPut_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GPut
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA GCas_M1 ==
  ASSUME IndM1, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GCas_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GCas
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA GRename_M1 ==
  ASSUME IndM1, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GRename_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GRename
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

\* Never enabled (mv = Nil).
LEMMA GRenameFinish_M1 ==
  ASSUME IndM1, GRenameFinish, Frame
  PROVE  IndM1' /\ NarrowOK
BY DEF IndM1, IndTypeOK, GRenameFinish

LEMMA GDelete_M1 ==
  ASSUME IndM1, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GDelete_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GDelete
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA Sweep_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY Sweep_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF Sweep
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA Age_M1 ==
  ASSUME IndM1, Age, UNCHANGED anc
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY Age_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF Age
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA Reap_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY Reap_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF Reap
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA RLoad_M1 ==
  ASSUME IndM1, RLoad, UNCHANGED anc
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY RLoad_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF RLoad
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

------------------------------------------------------------------------------
(* The agent.                                                               *)

LEMMA Edit_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Edit_TypeOK
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>h. holder' = holder BY DEF Edit
<1>g. On(s) BY DEF Edit
<1>2. \A t \in Writers : w'[t] = IF t = s THEN EditW(s, p) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ EditW(s, p).st = w[s].st
      /\ EditW(s, p).pc = w[s].pc
      /\ EditW(s, p).deletes = w[s].deletes
      /\ EditW(s, p).inst = w[s].inst
      /\ EditW(s, p).uploads = w[s].uploads
      /\ EditW(s, p).gone = w[s].gone
      /\ EditW(s, p).unlinked = w[s].unlinked \ {p}
      /\ EditW(s, p).sStage = w[s].sStage
      /\ EditW(s, p).local = [w[s].local EXCEPT ![p] = <<p, nextGen>>]
      /\ EditW(s, p).baseline = w[s].baseline
      /\ EditW(s, p).sDrop = w[s].sDrop
      /\ EditW(s, p).sKeep = w[s].sKeep
      /\ EditW(s, p).sHeld = w[s].sHeld
      /\ EditW(s, p).deposed = w[s].deposed
  BY DEF EditW
<1>4a. (EditW(s, p).pc \in CC <=> w[s].pc \in CC) /\ EditW(s, p).deposed = w[s].deposed /\ (w[s].st = "on" => EditW(s, p).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. EditW(s, p).pc = "cased" => \A q \in EditW(s, p).deletes : EditW(s, p).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. EditW(s, p).deletes \cap EditW(s, p).uploads = {} /\ EditW(s, p).gone \subseteq EditW(s, p).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. EditW(s, p).pc \in {"scanned", "claimed", "cased"} => (EditW(s, p).uploads \cup EditW(s, p).deletes) \cap EditW(s, p).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. EditW(s, p).st = "off" => EditW(s, p) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ EditW(s, p).sStage = w[s].sStage /\ EditW(s, p).unlinked \subseteq w[s].unlinked
       /\ \A q \in EditW(s, p).unlinked : EditW(s, p).local[q] = w[s].local[q]
       /\ EditW(s, p).baseline = w[s].baseline /\ EditW(s, p).sDrop = w[s].sDrop /\ EditW(s, p).sKeep = w[s].sKeep /\ EditW(s, p).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (EditW(s, p).pc \in {"consumed", "scanned", "claimed", "cased"} => EditW(s, p).sStage = "none")
       /\ (EditW(s, p).pc = "pulling" => EditW(s, p).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ EditW(s, p).pc = "scanned") => EditW(s, p).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Delete_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Delete_TypeOK
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete
<1>h. holder' = holder BY DEF Delete, bucket
<1>g. On(s) BY DEF Delete
<1>2. \A t \in Writers : w'[t] = IF t = s THEN DeleteW(s, p) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ DeleteW(s, p).st = w[s].st
      /\ DeleteW(s, p).pc = w[s].pc
      /\ DeleteW(s, p).deletes = w[s].deletes
      /\ DeleteW(s, p).inst = w[s].inst
      /\ DeleteW(s, p).uploads = w[s].uploads
      /\ DeleteW(s, p).gone = w[s].gone
      /\ DeleteW(s, p).unlinked = w[s].unlinked \ {p}
      /\ DeleteW(s, p).sStage = w[s].sStage
      /\ DeleteW(s, p).local = [w[s].local EXCEPT ![p] = Nil]
      /\ DeleteW(s, p).baseline = w[s].baseline
      /\ DeleteW(s, p).sDrop = w[s].sDrop
      /\ DeleteW(s, p).sKeep = w[s].sKeep
      /\ DeleteW(s, p).sHeld = w[s].sHeld
      /\ DeleteW(s, p).deposed = w[s].deposed
  BY DEF DeleteW
<1>4a. (DeleteW(s, p).pc \in CC <=> w[s].pc \in CC) /\ DeleteW(s, p).deposed = w[s].deposed /\ (w[s].st = "on" => DeleteW(s, p).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. DeleteW(s, p).pc = "cased" => \A q \in DeleteW(s, p).deletes : DeleteW(s, p).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. DeleteW(s, p).deletes \cap DeleteW(s, p).uploads = {} /\ DeleteW(s, p).gone \subseteq DeleteW(s, p).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. DeleteW(s, p).pc \in {"scanned", "claimed", "cased"} => (DeleteW(s, p).uploads \cup DeleteW(s, p).deletes) \cap DeleteW(s, p).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. DeleteW(s, p).st = "off" => DeleteW(s, p) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ DeleteW(s, p).sStage = w[s].sStage /\ DeleteW(s, p).unlinked \subseteq w[s].unlinked
       /\ \A q \in DeleteW(s, p).unlinked : DeleteW(s, p).local[q] = w[s].local[q]
       /\ DeleteW(s, p).baseline = w[s].baseline /\ DeleteW(s, p).sDrop = w[s].sDrop /\ DeleteW(s, p).sKeep = w[s].sKeep /\ DeleteW(s, p).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (DeleteW(s, p).pc \in {"consumed", "scanned", "claimed", "cased"} => DeleteW(s, p).sStage = "none")
       /\ (DeleteW(s, p).pc = "pulling" => DeleteW(s, p).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ DeleteW(s, p).pc = "scanned") => DeleteW(s, p).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Checkout_M1 ==
  ASSUME IndM1, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Checkout_TypeOK
<1>i. w[s] = WriterInit BY <1>m DEF Off, Checkout
<1>j. /\ w[s].pc = "idle" /\ w[s].unlinked = {} /\ w[s].sStage = "none"
      /\ w[s].deletes = {} /\ w[s].uploads = {} /\ w[s].gone = {} /\ w[s].sDrop = {}
  BY <1>i DEF WriterInit
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>h. holder' = holder BY DEF Checkout, bucket
<1>g. w[s].st = "off" BY DEF Checkout
<1>2. \A t \in Writers : w'[t] = IF t = s THEN CheckoutW(s, T) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ CheckoutW(s, T).st = "on"
      /\ CheckoutW(s, T).pc = w[s].pc
      /\ CheckoutW(s, T).deletes = w[s].deletes
      /\ CheckoutW(s, T).inst = doc
      /\ CheckoutW(s, T).uploads = w[s].uploads
      /\ CheckoutW(s, T).gone = w[s].gone
      /\ CheckoutW(s, T).unlinked = w[s].unlinked
      /\ CheckoutW(s, T).sStage = w[s].sStage
      /\ CheckoutW(s, T).local = CheckoutHeld(s, T)
      /\ CheckoutW(s, T).baseline = CheckoutHeld(s, T)
      /\ CheckoutW(s, T).sDrop = w[s].sDrop
      /\ CheckoutW(s, T).sKeep = w[s].sKeep
      /\ CheckoutW(s, T).sHeld = w[s].sHeld
      /\ CheckoutW(s, T).deposed = w[s].deposed
  BY DEF CheckoutW
<1>4a. (CheckoutW(s, T).pc \in CC <=> w[s].pc \in CC) /\ CheckoutW(s, T).deposed = w[s].deposed /\ (w[s].st = "on" => CheckoutW(s, T).st = "on") BY <1>3
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. CheckoutW(s, T).pc = "cased" => \A q \in CheckoutW(s, T).deletes : CheckoutW(s, T).inst[q] = Nil BY <1>3, <1>j
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. CheckoutW(s, T).deletes \cap CheckoutW(s, T).uploads = {} /\ CheckoutW(s, T).gone \subseteq CheckoutW(s, T).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. CheckoutW(s, T).pc \in {"scanned", "claimed", "cased"} => (CheckoutW(s, T).uploads \cup CheckoutW(s, T).deletes) \cap CheckoutW(s, T).unlinked = {} BY <1>3, <1>j
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. CheckoutW(s, T).st = "off" => CheckoutW(s, T) = WriterInit BY <1>3
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope' BY <1>m, <1>2, <1>3, <1>j, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ CheckoutW(s, T).pc = "scanned") => CheckoutW(s, T).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Consume_M1 ==
  ASSUME IndM1, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Consume_TypeOK
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF Consume
<1>h. holder' = holder BY DEF Consume
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>ck
  <2>h. holder' = holder BY DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF Consume
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN ConsumeCheapW(s, ck) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ ConsumeCheapW(s, ck).st = w[s].st
        /\ ConsumeCheapW(s, ck).pc = "consumed"
        /\ ConsumeCheapW(s, ck).deletes = w[s].deletes
        /\ ConsumeCheapW(s, ck).inst = w[s].inst
        /\ ConsumeCheapW(s, ck).uploads = w[s].uploads
        /\ ConsumeCheapW(s, ck).gone = w[s].gone
        /\ ConsumeCheapW(s, ck).unlinked = w[s].unlinked
        /\ ConsumeCheapW(s, ck).sStage = w[s].sStage
        /\ ConsumeCheapW(s, ck).local = w[s].local
        /\ ConsumeCheapW(s, ck).baseline = w[s].baseline
        /\ ConsumeCheapW(s, ck).sDrop = w[s].sDrop
        /\ ConsumeCheapW(s, ck).sKeep = w[s].sKeep
        /\ ConsumeCheapW(s, ck).sHeld = w[s].sHeld
        /\ ConsumeCheapW(s, ck).deposed = w[s].deposed
    BY DEF ConsumeCheapW
  <2>4a. (ConsumeCheapW(s, ck).pc \in CC <=> w[s].pc \in CC) /\ ConsumeCheapW(s, ck).deposed = w[s].deposed /\ (w[s].st = "on" => ConsumeCheapW(s, ck).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. ConsumeCheapW(s, ck).pc = "cased" => \A q \in ConsumeCheapW(s, ck).deletes : ConsumeCheapW(s, ck).inst[q] = Nil BY <2>3
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. ConsumeCheapW(s, ck).deletes \cap ConsumeCheapW(s, ck).uploads = {} /\ ConsumeCheapW(s, ck).gone \subseteq ConsumeCheapW(s, ck).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. ConsumeCheapW(s, ck).pc \in {"scanned", "claimed", "cased"} => (ConsumeCheapW(s, ck).uploads \cup ConsumeCheapW(s, ck).deletes) \cap ConsumeCheapW(s, ck).unlinked = {} BY <2>3
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. ConsumeCheapW(s, ck).st = "off" => ConsumeCheapW(s, ck) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ ConsumeCheapW(s, ck).sStage = w[s].sStage /\ ConsumeCheapW(s, ck).unlinked \subseteq w[s].unlinked
         /\ \A q \in ConsumeCheapW(s, ck).unlinked : ConsumeCheapW(s, ck).local[q] = w[s].local[q]
         /\ ConsumeCheapW(s, ck).baseline = w[s].baseline /\ ConsumeCheapW(s, ck).sDrop = w[s].sDrop /\ ConsumeCheapW(s, ck).sKeep = w[s].sKeep /\ ConsumeCheapW(s, ck).sHeld = w[s].sHeld
    BY <2>3, <1>f
  <2>9b. /\ (ConsumeCheapW(s, ck).pc \in {"consumed", "scanned", "claimed", "cased"} => ConsumeCheapW(s, ck).sStage = "none")
         /\ (ConsumeCheapW(s, ck).pc = "pulling" => ConsumeCheapW(s, ck).sStage \in {"none", "saved"})
    BY <2>3, <2>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ ConsumeCheapW(s, ck).pc = "scanned") => ConsumeCheapW(s, ck).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>ck
  <2>1. w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <2>0
  <2>h. holder' = holder BY DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF Consume
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN ConsumeW(s, fail, ck) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ ConsumeW(s, fail, ck).st = w[s].st
        /\ ConsumeW(s, fail, ck).pc = "consumed"
        /\ ConsumeW(s, fail, ck).deletes = w[s].deletes
        /\ ConsumeW(s, fail, ck).inst = w[s].inst
        /\ ConsumeW(s, fail, ck).uploads = w[s].uploads
        /\ ConsumeW(s, fail, ck).gone = w[s].gone
        /\ ConsumeW(s, fail, ck).unlinked = w[s].unlinked
        /\ ConsumeW(s, fail, ck).sStage = w[s].sStage
        /\ ConsumeW(s, fail, ck).local = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]
        /\ ConsumeW(s, fail, ck).baseline = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]]
        /\ ConsumeW(s, fail, ck).sDrop = w[s].sDrop
        /\ ConsumeW(s, fail, ck).sKeep = w[s].sKeep
        /\ ConsumeW(s, fail, ck).sHeld = w[s].sHeld
        /\ ConsumeW(s, fail, ck).deposed = w[s].deposed
    BY DEF ConsumeW
  <2>4a. (ConsumeW(s, fail, ck).pc \in CC <=> w[s].pc \in CC) /\ ConsumeW(s, fail, ck).deposed = w[s].deposed /\ (w[s].st = "on" => ConsumeW(s, fail, ck).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. ConsumeW(s, fail, ck).pc = "cased" => \A q \in ConsumeW(s, fail, ck).deletes : ConsumeW(s, fail, ck).inst[q] = Nil BY <2>3
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. ConsumeW(s, fail, ck).deletes \cap ConsumeW(s, fail, ck).uploads = {} /\ ConsumeW(s, fail, ck).gone \subseteq ConsumeW(s, fail, ck).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. ConsumeW(s, fail, ck).pc \in {"scanned", "claimed", "cased"} => (ConsumeW(s, fail, ck).uploads \cup ConsumeW(s, fail, ck).deletes) \cap ConsumeW(s, fail, ck).unlinked = {} BY <2>3
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. ConsumeW(s, fail, ck).st = "off" => ConsumeW(s, fail, ck) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. \A q \in Paths : (q \in ConsumeW(s, fail, ck).unlinked /\ ConsumeW(s, fail, ck).sStage = "none")
                           => ConsumeW(s, fail, ck).local[q] = ConsumeW(s, fail, ck).baseline[q]
    BY <2>3, <1>r, <1>f DEF R2
  <2>9. Rescope' BY <1>m, <2>2, <2>3, <1>g, <2>9a, RescopeWrite
  <2>10a. (w[s].pc = "consumed" /\ ConsumeW(s, fail, ck).pc = "scanned") => ConsumeW(s, fail, ck).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>. QED BY <1>1, <1>2

LEMMA Scan_M1 ==
  ASSUME IndM1, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Scan_TypeOK
<1>k. w[s].sStage = "none" BY <1>r DEF R1, Scan
<1>l. \A q \in w[s].unlinked : w[s].local[q] = w[s].baseline[q] BY <1>r, <1>k, <1>f DEF R2
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>h. holder' = holder BY DEF Scan, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Scan
<1>2. \A t \in Writers : w'[t] = IF t = s THEN ScanW(s, dels) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ ScanW(s, dels).st = w[s].st
      /\ ScanW(s, dels).pc = "scanned"
      /\ ScanW(s, dels).deletes = dels
      /\ ScanW(s, dels).inst = w[s].inst
      /\ ScanW(s, dels).uploads = ScanUps(s)
      /\ ScanW(s, dels).gone = {}
      /\ ScanW(s, dels).unlinked = w[s].unlinked
      /\ ScanW(s, dels).sStage = w[s].sStage
      /\ ScanW(s, dels).local = w[s].local
      /\ ScanW(s, dels).baseline = w[s].baseline
      /\ ScanW(s, dels).sDrop = w[s].sDrop
      /\ ScanW(s, dels).sKeep = w[s].sKeep
      /\ ScanW(s, dels).sHeld = w[s].sHeld
      /\ ScanW(s, dels).deposed = w[s].deposed
  BY DEF ScanW
<1>4a. (ScanW(s, dels).pc \in CC <=> w[s].pc \in CC) /\ ScanW(s, dels).deposed = w[s].deposed /\ (w[s].st = "on" => ScanW(s, dels).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. ScanW(s, dels).pc = "cased" => \A q \in ScanW(s, dels).deletes : ScanW(s, dels).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. ScanW(s, dels).deletes \cap ScanW(s, dels).uploads = {} /\ ScanW(s, dels).gone \subseteq ScanW(s, dels).uploads BY <1>1, <1>3 DEF ScanUps, ScanAbsent, ScanDirty
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. ScanW(s, dels).pc \in {"scanned", "claimed", "cased"} => (ScanW(s, dels).uploads \cup ScanW(s, dels).deletes) \cap ScanW(s, dels).unlinked = {} BY <1>1, <1>3, <1>l DEF ScanUps, ScanAbsent, ScanDirty
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. ScanW(s, dels).st = "off" => ScanW(s, dels) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ ScanW(s, dels).sStage = w[s].sStage /\ ScanW(s, dels).unlinked \subseteq w[s].unlinked
       /\ \A q \in ScanW(s, dels).unlinked : ScanW(s, dels).local[q] = w[s].local[q]
       /\ ScanW(s, dels).baseline = w[s].baseline /\ ScanW(s, dels).sDrop = w[s].sDrop
       /\ ScanW(s, dels).sKeep = w[s].sKeep /\ ScanW(s, dels).sHeld = w[s].sHeld
  BY <1>3
<1>9b. /\ (ScanW(s, dels).pc \in {"consumed", "scanned", "claimed", "cased"} => ScanW(s, dels).sStage = "none")
       /\ (ScanW(s, dels).pc = "pulling" => ScanW(s, dels).sStage \in {"none", "saved"})
  BY <1>3, <1>k
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ ScanW(s, dels).pc = "scanned") => ScanW(s, dels).deletes \cap w[s].unlinked = {} BY <1>1, <1>3, <1>l DEF ScanAbsent, ScanDirty
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Skip_M1 ==
  ASSUME IndM1, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Skip_TypeOK
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>h. holder' = holder BY DEF Skip, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Skip
<1>2. \A t \in Writers : w'[t] = IF t = s THEN SkipW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ SkipW(s).st = w[s].st
      /\ SkipW(s).pc = "idle"
      /\ SkipW(s).deletes = w[s].deletes
      /\ SkipW(s).inst = w[s].inst
      /\ SkipW(s).uploads = w[s].uploads
      /\ SkipW(s).gone = w[s].gone
      /\ SkipW(s).unlinked = w[s].unlinked
      /\ SkipW(s).sStage = w[s].sStage
      /\ SkipW(s).local = w[s].local
      /\ SkipW(s).baseline = w[s].baseline
      /\ SkipW(s).sDrop = w[s].sDrop
      /\ SkipW(s).sKeep = w[s].sKeep
      /\ SkipW(s).sHeld = w[s].sHeld
      /\ SkipW(s).deposed = w[s].deposed
  BY DEF SkipW
<1>4a. (SkipW(s).pc \in CC <=> w[s].pc \in CC) /\ SkipW(s).deposed = w[s].deposed /\ (w[s].st = "on" => SkipW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. SkipW(s).pc = "cased" => \A q \in SkipW(s).deletes : SkipW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. SkipW(s).deletes \cap SkipW(s).uploads = {} /\ SkipW(s).gone \subseteq SkipW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. SkipW(s).pc \in {"scanned", "claimed", "cased"} => (SkipW(s).uploads \cup SkipW(s).deletes) \cap SkipW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. SkipW(s).st = "off" => SkipW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ SkipW(s).sStage = w[s].sStage /\ SkipW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in SkipW(s).unlinked : SkipW(s).local[q] = w[s].local[q]
       /\ SkipW(s).baseline = w[s].baseline /\ SkipW(s).sDrop = w[s].sDrop /\ SkipW(s).sKeep = w[s].sKeep /\ SkipW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (SkipW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => SkipW(s).sStage = "none")
       /\ (SkipW(s).pc = "pulling" => SkipW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ SkipW(s).pc = "scanned") => SkipW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Upload_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Upload_TypeOK
<1>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
<1>h. holder' = holder BY DEF Upload
<1>k. p \notin w[s].unlinked BY <1>m, <1>g DEF Ups
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>1 DEF Upload
  <2>h. holder' = holder BY DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN UploadW(s, p) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ UploadW(s, p).st = w[s].st
        /\ UploadW(s, p).pc = w[s].pc
        /\ UploadW(s, p).deletes = w[s].deletes
        /\ UploadW(s, p).inst = w[s].inst
        /\ UploadW(s, p).uploads = w[s].uploads
        /\ UploadW(s, p).gone = w[s].gone
        /\ UploadW(s, p).unlinked = w[s].unlinked
        /\ UploadW(s, p).sStage = w[s].sStage
        /\ UploadW(s, p).local = w[s].local
        /\ UploadW(s, p).baseline = w[s].baseline
        /\ UploadW(s, p).sDrop = w[s].sDrop
        /\ UploadW(s, p).sKeep = w[s].sKeep
        /\ UploadW(s, p).sHeld = w[s].sHeld
        /\ UploadW(s, p).deposed = w[s].deposed
    BY DEF UploadW
  <2>4a. (UploadW(s, p).pc \in CC <=> w[s].pc \in CC) /\ UploadW(s, p).deposed = w[s].deposed /\ (w[s].st = "on" => UploadW(s, p).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. UploadW(s, p).pc = "cased" => \A q \in UploadW(s, p).deletes : UploadW(s, p).inst[q] = Nil BY <1>m, <2>3 DEF Cased
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. UploadW(s, p).deletes \cap UploadW(s, p).uploads = {} /\ UploadW(s, p).gone \subseteq UploadW(s, p).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. UploadW(s, p).pc \in {"scanned", "claimed", "cased"} => (UploadW(s, p).uploads \cup UploadW(s, p).deletes) \cap UploadW(s, p).unlinked = {} BY <1>m, <2>3 DEF Ups
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. UploadW(s, p).st = "off" => UploadW(s, p) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ UploadW(s, p).sStage = w[s].sStage /\ UploadW(s, p).unlinked \subseteq w[s].unlinked
         /\ \A q \in UploadW(s, p).unlinked : UploadW(s, p).local[q] = w[s].local[q]
         /\ UploadW(s, p).baseline = w[s].baseline /\ UploadW(s, p).sDrop = w[s].sDrop /\ UploadW(s, p).sKeep = w[s].sKeep /\ UploadW(s, p).sHeld = w[s].sHeld
    BY <2>3, <1>f
  <2>9b. /\ (UploadW(s, p).pc \in {"consumed", "scanned", "claimed", "cased"} => UploadW(s, p).sStage = "none")
         /\ (UploadW(s, p).pc = "pulling" => UploadW(s, p).sStage \in {"none", "saved"})
    BY <2>3, <2>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ UploadW(s, p).pc = "scanned") => UploadW(s, p).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] BY <1>2 DEF Upload
  <2>h. holder' = holder BY DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN UploadCopyW(s, p, c) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ UploadCopyW(s, p, c).st = w[s].st
        /\ UploadCopyW(s, p, c).pc = w[s].pc
        /\ UploadCopyW(s, p, c).deletes = w[s].deletes
        /\ UploadCopyW(s, p, c).inst = w[s].inst
        /\ UploadCopyW(s, p, c).uploads = w[s].uploads
        /\ UploadCopyW(s, p, c).gone = w[s].gone
        /\ UploadCopyW(s, p, c).unlinked = w[s].unlinked
        /\ UploadCopyW(s, p, c).sStage = w[s].sStage
        /\ UploadCopyW(s, p, c).local = [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]
        /\ UploadCopyW(s, p, c).baseline = w[s].baseline
        /\ UploadCopyW(s, p, c).sDrop = w[s].sDrop
        /\ UploadCopyW(s, p, c).sKeep = w[s].sKeep
        /\ UploadCopyW(s, p, c).sHeld = w[s].sHeld
        /\ UploadCopyW(s, p, c).deposed = w[s].deposed
    BY DEF UploadCopyW
  <2>4a. (UploadCopyW(s, p, c).pc \in CC <=> w[s].pc \in CC) /\ UploadCopyW(s, p, c).deposed = w[s].deposed /\ (w[s].st = "on" => UploadCopyW(s, p, c).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. UploadCopyW(s, p, c).pc = "cased" => \A q \in UploadCopyW(s, p, c).deletes : UploadCopyW(s, p, c).inst[q] = Nil BY <1>m, <2>3 DEF Cased
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. UploadCopyW(s, p, c).deletes \cap UploadCopyW(s, p, c).uploads = {} /\ UploadCopyW(s, p, c).gone \subseteq UploadCopyW(s, p, c).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. UploadCopyW(s, p, c).pc \in {"scanned", "claimed", "cased"} => (UploadCopyW(s, p, c).uploads \cup UploadCopyW(s, p, c).deletes) \cap UploadCopyW(s, p, c).unlinked = {} BY <1>m, <2>3 DEF Ups
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. UploadCopyW(s, p, c).st = "off" => UploadCopyW(s, p, c) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ UploadCopyW(s, p, c).sStage = w[s].sStage /\ UploadCopyW(s, p, c).unlinked \subseteq w[s].unlinked
         /\ \A q \in UploadCopyW(s, p, c).unlinked : UploadCopyW(s, p, c).local[q] = w[s].local[q]
         /\ UploadCopyW(s, p, c).baseline = w[s].baseline /\ UploadCopyW(s, p, c).sDrop = w[s].sDrop
         /\ UploadCopyW(s, p, c).sKeep = w[s].sKeep /\ UploadCopyW(s, p, c).sHeld = w[s].sHeld
    BY <2>3, <1>f, <1>k
  <2>9b. /\ (UploadCopyW(s, p, c).pc \in {"consumed", "scanned", "claimed", "cased"} => UploadCopyW(s, p, c).sStage = "none")
         /\ (UploadCopyW(s, p, c).pc = "pulling" => UploadCopyW(s, p, c).sStage \in {"none", "saved"})
    BY <2>3, <1>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ UploadCopyW(s, p, c).pc = "scanned") => UploadCopyW(s, p, c).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The commit section.                                                      *)

LEMMA PullOnly_M1 ==
  ASSUME IndM1, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, PullOnly_TypeOK
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>h. holder' = holder BY DEF PullOnly, bucket
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF PullOnly
<1>2. \A t \in Writers : w'[t] = IF t = s THEN PullOnlyW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ PullOnlyW(s).st = w[s].st
      /\ PullOnlyW(s).pc = "idle"
      /\ PullOnlyW(s).deletes = w[s].deletes
      /\ PullOnlyW(s).inst = doc
      /\ PullOnlyW(s).uploads = w[s].uploads
      /\ PullOnlyW(s).gone = {}
      /\ PullOnlyW(s).unlinked = w[s].unlinked
      /\ PullOnlyW(s).sStage = w[s].sStage
      /\ PullOnlyW(s).local = w[s].local
      /\ PullOnlyW(s).baseline = w[s].baseline
      /\ PullOnlyW(s).sDrop = w[s].sDrop
      /\ PullOnlyW(s).sKeep = w[s].sKeep
      /\ PullOnlyW(s).sHeld = w[s].sHeld
      /\ PullOnlyW(s).deposed = w[s].deposed
  BY DEF PullOnlyW
<1>4a. (PullOnlyW(s).pc \in CC <=> w[s].pc \in CC) /\ PullOnlyW(s).deposed = w[s].deposed /\ (w[s].st = "on" => PullOnlyW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. PullOnlyW(s).pc = "cased" => \A q \in PullOnlyW(s).deletes : PullOnlyW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. PullOnlyW(s).deletes \cap PullOnlyW(s).uploads = {} /\ PullOnlyW(s).gone \subseteq PullOnlyW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. PullOnlyW(s).pc \in {"scanned", "claimed", "cased"} => (PullOnlyW(s).uploads \cup PullOnlyW(s).deletes) \cap PullOnlyW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. PullOnlyW(s).st = "off" => PullOnlyW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ PullOnlyW(s).sStage = w[s].sStage /\ PullOnlyW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in PullOnlyW(s).unlinked : PullOnlyW(s).local[q] = w[s].local[q]
       /\ PullOnlyW(s).baseline = w[s].baseline /\ PullOnlyW(s).sDrop = w[s].sDrop /\ PullOnlyW(s).sKeep = w[s].sKeep /\ PullOnlyW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (PullOnlyW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => PullOnlyW(s).sStage = "none")
       /\ (PullOnlyW(s).pc = "pulling" => PullOnlyW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ PullOnlyW(s).pc = "scanned") => PullOnlyW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Claim_M1 ==
  ASSUME IndM1, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Claim_TypeOK
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim
<1>h. holder' = s BY DEF Claim
<1>g. On(s) /\ w[s].pc = "scanned" /\ holder = "none" BY DEF Claim
<1>2. \A t \in Writers : w'[t] = IF t = s THEN ClaimW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ ClaimW(s).st = w[s].st
      /\ ClaimW(s).pc = "claimed"
      /\ ClaimW(s).deletes = w[s].deletes
      /\ ClaimW(s).inst = w[s].inst
      /\ ClaimW(s).uploads = w[s].uploads
      /\ ClaimW(s).gone = w[s].gone
      /\ ClaimW(s).unlinked = w[s].unlinked
      /\ ClaimW(s).sStage = w[s].sStage
      /\ ClaimW(s).local = w[s].local
      /\ ClaimW(s).baseline = w[s].baseline
      /\ ClaimW(s).sDrop = w[s].sDrop
      /\ ClaimW(s).sKeep = w[s].sKeep
      /\ ClaimW(s).sHeld = w[s].sHeld
      /\ ClaimW(s).deposed = w[s].deposed
  BY DEF ClaimW
<1>4. Inv_OneHolder' /\ Holder' /\ Dep'
  <2>0. ~w[s].deposed BY <1>m, <1>g DEF Dep, CC
  <2>1. Inv_OneHolder' BY <1>m, <1>0, <1>2, <1>3, <1>h, <1>g, <2>0, NoneWriter DEF Inv_OneHolder
  <2>2. Holder' BY <1>2, <1>3, <1>h, <1>g, NoneWriter DEF Holder, CC, On
  <2>3. Dep' BY <1>m, <1>2, <1>3, <1>h, <1>g, <2>0 DEF Dep, CC
  <2>. QED BY <2>1, <2>2, <2>3
<1>5a. ClaimW(s).pc = "cased" => \A q \in ClaimW(s).deletes : ClaimW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. ClaimW(s).deletes \cap ClaimW(s).uploads = {} /\ ClaimW(s).gone \subseteq ClaimW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. ClaimW(s).pc \in {"scanned", "claimed", "cased"} => (ClaimW(s).uploads \cup ClaimW(s).deletes) \cap ClaimW(s).unlinked = {} BY <1>m, <1>3, <1>g DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. ClaimW(s).st = "off" => ClaimW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ ClaimW(s).sStage = w[s].sStage /\ ClaimW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in ClaimW(s).unlinked : ClaimW(s).local[q] = w[s].local[q]
       /\ ClaimW(s).baseline = w[s].baseline /\ ClaimW(s).sDrop = w[s].sDrop /\ ClaimW(s).sKeep = w[s].sKeep /\ ClaimW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (ClaimW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => ClaimW(s).sStage = "none")
       /\ (ClaimW(s).pc = "pulling" => ClaimW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ ClaimW(s).pc = "scanned") => ClaimW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Verify_M1 ==
  ASSUME IndM1, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Verify_TypeOK
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify
<1>h. holder' = holder BY DEF Verify, bucket
<1>g. On(s) /\ w[s].pc = "claimed" BY DEF Verify
<1>2. \A t \in Writers : w'[t] = IF t = s THEN VerifyW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ VerifyW(s).st = w[s].st
      /\ VerifyW(s).pc = w[s].pc
      /\ VerifyW(s).deletes = w[s].deletes
      /\ VerifyW(s).inst = w[s].inst
      /\ VerifyW(s).uploads = w[s].uploads
      /\ VerifyW(s).gone = IF CommitVerifiesUploads THEN {q \in w[s].uploads \cap w[s].upDone : w[s].snap[q] \notin live} ELSE {}
      /\ VerifyW(s).unlinked = w[s].unlinked
      /\ VerifyW(s).sStage = w[s].sStage
      /\ VerifyW(s).local = w[s].local
      /\ VerifyW(s).baseline = w[s].baseline
      /\ VerifyW(s).sDrop = w[s].sDrop
      /\ VerifyW(s).sKeep = w[s].sKeep
      /\ VerifyW(s).sHeld = w[s].sHeld
      /\ VerifyW(s).deposed = w[s].deposed
  BY DEF VerifyW
<1>4a. (VerifyW(s).pc \in CC <=> w[s].pc \in CC) /\ VerifyW(s).deposed = w[s].deposed /\ (w[s].st = "on" => VerifyW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. VerifyW(s).pc = "cased" => \A q \in VerifyW(s).deletes : VerifyW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. VerifyW(s).deletes \cap VerifyW(s).uploads = {} /\ VerifyW(s).gone \subseteq VerifyW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. VerifyW(s).pc \in {"scanned", "claimed", "cased"} => (VerifyW(s).uploads \cup VerifyW(s).deletes) \cap VerifyW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. VerifyW(s).st = "off" => VerifyW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ VerifyW(s).sStage = w[s].sStage /\ VerifyW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in VerifyW(s).unlinked : VerifyW(s).local[q] = w[s].local[q]
       /\ VerifyW(s).baseline = w[s].baseline /\ VerifyW(s).sDrop = w[s].sDrop /\ VerifyW(s).sKeep = w[s].sKeep /\ VerifyW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (VerifyW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => VerifyW(s).sStage = "none")
       /\ (VerifyW(s).pc = "pulling" => VerifyW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ VerifyW(s).pc = "scanned") => VerifyW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Install_M1 ==
  ASSUME IndM1, NEW s \in Writers, Install(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Install_TypeOK
<1>i. \A q \in w[s].deletes : InstallInst(s)[q] = Nil
  <2>1. DeleteWinsPreserved BY ShippedShape DEF Shipped
  <2>2. \A q \in w[s].deletes : q \notin w[s].gone /\ q \notin InstallMine(s) BY <1>m DEF Mine, InstallMine
  <2>. QED BY <2>1, <2>2, <1>f DEF InstallInst
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] BY DEF Install
<1>h. holder' = holder BY DEF Install
<1>g. On(s) /\ w[s].pc = "claimed" /\ holder = s BY ShippedShape DEF Install, Shipped
<1>2. \A t \in Writers : w'[t] = IF t = s THEN InstallW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ InstallW(s).st = w[s].st
      /\ InstallW(s).pc = "cased"
      /\ InstallW(s).deletes = w[s].deletes
      /\ InstallW(s).inst = InstallInst(s)
      /\ InstallW(s).uploads = w[s].uploads
      /\ InstallW(s).gone = w[s].gone
      /\ InstallW(s).unlinked = w[s].unlinked
      /\ InstallW(s).sStage = w[s].sStage
      /\ InstallW(s).local = w[s].local
      /\ InstallW(s).baseline = w[s].baseline
      /\ InstallW(s).sDrop = w[s].sDrop
      /\ InstallW(s).sKeep = w[s].sKeep
      /\ InstallW(s).sHeld = w[s].sHeld
      /\ InstallW(s).deposed = w[s].deposed
  BY DEF InstallW
<1>4a. (InstallW(s).pc \in CC <=> w[s].pc \in CC) /\ InstallW(s).deposed = w[s].deposed /\ (w[s].st = "on" => InstallW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. InstallW(s).pc = "cased" => \A q \in InstallW(s).deletes : InstallW(s).inst[q] = Nil BY <1>3, <1>i
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. InstallW(s).deletes \cap InstallW(s).uploads = {} /\ InstallW(s).gone \subseteq InstallW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. InstallW(s).pc \in {"scanned", "claimed", "cased"} => (InstallW(s).uploads \cup InstallW(s).deletes) \cap InstallW(s).unlinked = {} BY <1>m, <1>3, <1>g DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. InstallW(s).st = "off" => InstallW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ InstallW(s).sStage = w[s].sStage /\ InstallW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in InstallW(s).unlinked : InstallW(s).local[q] = w[s].local[q]
       /\ InstallW(s).baseline = w[s].baseline /\ InstallW(s).sDrop = w[s].sDrop /\ InstallW(s).sKeep = w[s].sKeep /\ InstallW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (InstallW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => InstallW(s).sStage = "none")
       /\ (InstallW(s).pc = "pulling" => InstallW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ InstallW(s).pc = "scanned") => InstallW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Collect_M1 ==
  ASSUME IndM1, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Collect_TypeOK
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] BY DEF Collect
<1>h. holder' = holder BY DEF Collect
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Collect
<1>2. \A t \in Writers : w'[t] = IF t = s THEN CollectW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ CollectW(s).st = w[s].st
      /\ CollectW(s).pc = w[s].pc
      /\ CollectW(s).deletes = w[s].deletes
      /\ CollectW(s).inst = w[s].inst
      /\ CollectW(s).uploads = w[s].uploads
      /\ CollectW(s).gone = w[s].gone
      /\ CollectW(s).unlinked = w[s].unlinked
      /\ CollectW(s).sStage = w[s].sStage
      /\ CollectW(s).local = w[s].local
      /\ CollectW(s).baseline = w[s].baseline
      /\ CollectW(s).sDrop = w[s].sDrop
      /\ CollectW(s).sKeep = w[s].sKeep
      /\ CollectW(s).sHeld = w[s].sHeld
      /\ CollectW(s).deposed = w[s].deposed
  BY DEF CollectW
<1>4a. (CollectW(s).pc \in CC <=> w[s].pc \in CC) /\ CollectW(s).deposed = w[s].deposed /\ (w[s].st = "on" => CollectW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. CollectW(s).pc = "cased" => \A q \in CollectW(s).deletes : CollectW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. CollectW(s).deletes \cap CollectW(s).uploads = {} /\ CollectW(s).gone \subseteq CollectW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. CollectW(s).pc \in {"scanned", "claimed", "cased"} => (CollectW(s).uploads \cup CollectW(s).deletes) \cap CollectW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. CollectW(s).st = "off" => CollectW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ CollectW(s).sStage = w[s].sStage /\ CollectW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in CollectW(s).unlinked : CollectW(s).local[q] = w[s].local[q]
       /\ CollectW(s).baseline = w[s].baseline /\ CollectW(s).sDrop = w[s].sDrop /\ CollectW(s).sKeep = w[s].sKeep /\ CollectW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (CollectW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => CollectW(s).sStage = "none")
       /\ (CollectW(s).pc = "pulling" => CollectW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ CollectW(s).pc = "scanned") => CollectW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Finish_M1 ==
  ASSUME IndM1, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Finish_TypeOK
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>h. holder' = IF holder = s THEN "none" ELSE holder BY DEF Finish
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Finish
<1>2. \A t \in Writers : w'[t] = IF t = s THEN FinishW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ FinishW(s).st = w[s].st
      /\ FinishW(s).pc = "idle"
      /\ FinishW(s).deletes = {}
      /\ FinishW(s).inst = w[s].inst
      /\ FinishW(s).uploads = {}
      /\ FinishW(s).gone = {}
      /\ FinishW(s).unlinked = w[s].unlinked
      /\ FinishW(s).sStage = w[s].sStage
      /\ FinishW(s).local = w[s].local
      /\ FinishW(s).baseline = [q \in Paths |-> IF q \in w[s].uploads \cap w[s].upDone THEN w[s].snap[q]
                                        ELSE IF q \in w[s].deletes /\ w[s].inst[q] = Nil THEN Nil
                                        ELSE w[s].baseline[q]]
      /\ FinishW(s).sDrop = w[s].sDrop
      /\ FinishW(s).sKeep = w[s].sKeep
      /\ FinishW(s).sHeld = w[s].sHeld
      /\ FinishW(s).deposed = FALSE
  BY DEF FinishW
<1>4. Inv_OneHolder' /\ Holder' /\ Dep'
  <2>0. holder' \in {holder, "none"} /\ (holder' # holder => holder = s) /\ holder' # s BY <1>h, NoneWriter
  <2>1. Inv_OneHolder' BY <1>m, <1>0, <1>2, <1>3, <2>0, NoneWriter DEF Inv_OneHolder, CC
  <2>2. Holder' BY <1>m, <1>0, <1>2, <1>3, <2>0, NoneWriter DEF Holder, CC
  <2>3. Dep' BY <1>m, <1>0, <1>2, <1>3, <2>0, NoneWriter DEF Dep, CC
  <2>. QED BY <2>1, <2>2, <2>3
<1>5a. FinishW(s).pc = "cased" => \A q \in FinishW(s).deletes : FinishW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. FinishW(s).deletes \cap FinishW(s).uploads = {} /\ FinishW(s).gone \subseteq FinishW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. FinishW(s).pc \in {"scanned", "claimed", "cased"} => (FinishW(s).uploads \cup FinishW(s).deletes) \cap FinishW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. FinishW(s).st = "off" => FinishW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>1. w[s].sStage = "none" BY <1>r, <1>g DEF R1
  <2>2. \A q \in w[s].unlinked : q \notin w[s].uploads /\ q \notin w[s].deletes BY <1>m, <1>g DEF Ups
  <2>3. \A q \in Paths : (q \in FinishW(s).unlinked /\ FinishW(s).sStage = "none") => FinishW(s).local[q] = FinishW(s).baseline[q]
    BY <1>3, <1>r, <1>f, <2>1, <2>2 DEF R2
  <2>. QED BY <1>m, <1>2, <1>3, <2>1, <2>3, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ FinishW(s).pc = "scanned") => FinishW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* The restart and the sync.                                                *)

LEMMA Restart_M1 ==
  ASSUME IndM1, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Restart_TypeOK
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>h. holder' = IF holder = s THEN "none" ELSE holder BY DEF Restart
<1>g. On(s) BY DEF Restart
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RestartW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RestartW(s).st = w[s].st
      /\ RestartW(s).pc = "idle"
      /\ RestartW(s).deletes = {}
      /\ RestartW(s).inst = [q \in Paths |-> Nil]
      /\ RestartW(s).uploads = {}
      /\ RestartW(s).gone = {}
      /\ RestartW(s).unlinked = w[s].unlinked
      /\ RestartW(s).sStage = IF w[s].sStage = "none" THEN "none" ELSE "saved"
      /\ RestartW(s).local = w[s].local
      /\ RestartW(s).baseline = w[s].baseline
      /\ RestartW(s).sDrop = w[s].sDrop
      /\ RestartW(s).sKeep = {}
      /\ RestartW(s).sHeld = w[s].sHeld
      /\ RestartW(s).deposed = FALSE
  BY DEF RestartW
<1>4. Inv_OneHolder' /\ Holder' /\ Dep'
  <2>0. holder' \in {holder, "none"} /\ (holder' # holder => holder = s) /\ holder' # s BY <1>h, NoneWriter
  <2>1. Inv_OneHolder' BY <1>m, <1>0, <1>2, <1>3, <2>0, NoneWriter DEF Inv_OneHolder, CC
  <2>2. Holder' BY <1>m, <1>0, <1>2, <1>3, <2>0, NoneWriter DEF Holder, CC
  <2>3. Dep' BY <1>m, <1>0, <1>2, <1>3, <2>0, NoneWriter DEF Dep, CC
  <2>. QED BY <2>1, <2>2, <2>3
<1>5a. RestartW(s).pc = "cased" => \A q \in RestartW(s).deletes : RestartW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RestartW(s).deletes \cap RestartW(s).uploads = {} /\ RestartW(s).gone \subseteq RestartW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RestartW(s).pc \in {"scanned", "claimed", "cased"} => (RestartW(s).uploads \cup RestartW(s).deletes) \cap RestartW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RestartW(s).st = "off" => RestartW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>1. \A q \in Paths : (q \in RestartW(s).unlinked /\ RestartW(s).sStage = "none") => RestartW(s).local[q] = RestartW(s).baseline[q]
    BY <1>3, <1>r DEF R2
  <2>2. \A q \in Paths : (q \in RestartW(s).unlinked /\ RestartW(s).sStage \in {"saved", "mid"}) =>
          \/ RestartW(s).local[q] = RestartW(s).baseline[q]
          \/ /\ q \in RestartW(s).sDrop \ RestartW(s).sKeep /\ RestartW(s).baseline[q] = Nil
             /\ RestartW(s).local[q] # Nil /\ RestartW(s).sHeld[q] = RestartW(s).local[q]
    BY <1>3, <1>r, <1>f DEF R3
  <2>3. RestartW(s).sStage = "mid" => \A q \in RestartW(s).sDrop \ RestartW(s).sKeep : RestartW(s).baseline[q] = Nil BY <1>3
  <2>. QED BY <1>m, <1>2, <1>3, <2>1, <2>2, <2>3, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RestartW(s).pc = "scanned") => RestartW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Sync_M1 ==
  ASSUME IndM1, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Sync_TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>h. holder' = holder BY DEF Sync, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage \in {"none", "saved"} BY DEF Sync
<1>2. \A t \in Writers : w'[t] = IF t = s THEN SyncW(s, fail) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ SyncW(s, fail).st = w[s].st
      /\ SyncW(s, fail).pc = w[s].pc
      /\ SyncW(s, fail).deletes = w[s].deletes
      /\ SyncW(s, fail).inst = w[s].inst
      /\ SyncW(s, fail).uploads = w[s].uploads
      /\ SyncW(s, fail).gone = w[s].gone
      /\ SyncW(s, fail).unlinked = w[s].unlinked
      /\ SyncW(s, fail).sStage = w[s].sStage
      /\ SyncW(s, fail).local = [q \in Paths |-> IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]
      /\ SyncW(s, fail).baseline = SyncBl(s, fail)
      /\ SyncW(s, fail).sDrop = w[s].sDrop
      /\ SyncW(s, fail).sKeep = w[s].sKeep
      /\ SyncW(s, fail).sHeld = w[s].sHeld
      /\ SyncW(s, fail).deposed = w[s].deposed
  BY DEF SyncW
<1>4a. (SyncW(s, fail).pc \in CC <=> w[s].pc \in CC) /\ SyncW(s, fail).deposed = w[s].deposed /\ (w[s].st = "on" => SyncW(s, fail).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. SyncW(s, fail).pc = "cased" => \A q \in SyncW(s, fail).deletes : SyncW(s, fail).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. SyncW(s, fail).deletes \cap SyncW(s, fail).uploads = {} /\ SyncW(s, fail).gone \subseteq SyncW(s, fail).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. SyncW(s, fail).pc \in {"scanned", "claimed", "cased"} => (SyncW(s, fail).uploads \cup SyncW(s, fail).deletes) \cap SyncW(s, fail).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. SyncW(s, fail).st = "off" => SyncW(s, fail) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>0. \A q \in Paths : q \in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] BY DEF SyncOwed, SyncAll, Owed
  <2>1. \A q \in Paths : /\ (w[s].local[q] = w[s].baseline[q] => SyncW(s, fail).local[q] = SyncW(s, fail).baseline[q])
                        /\ (q \notin SyncOwed(s, fail) => SyncW(s, fail).local[q] = w[s].local[q] /\ SyncW(s, fail).baseline[q] = w[s].baseline[q])
    BY <1>3, <2>0 DEF SyncBl
  <2>2. \A q \in Paths : (q \in SyncW(s, fail).unlinked /\ SyncW(s, fail).sStage = "none") => SyncW(s, fail).local[q] = SyncW(s, fail).baseline[q]
    BY <1>3, <1>r, <2>1 DEF R2
  <2>3. \A q \in Paths : (q \in SyncW(s, fail).unlinked /\ SyncW(s, fail).sStage \in {"saved", "mid"}) =>
          \/ SyncW(s, fail).local[q] = SyncW(s, fail).baseline[q]
          \/ /\ q \in SyncW(s, fail).sDrop \ SyncW(s, fail).sKeep /\ SyncW(s, fail).baseline[q] = Nil
             /\ SyncW(s, fail).local[q] # Nil /\ SyncW(s, fail).sHeld[q] = SyncW(s, fail).local[q]
    BY <1>3, <1>r, <2>0, <2>1 DEF R3
  <2>4. SyncW(s, fail).sStage = "mid" => \A q \in SyncW(s, fail).sDrop \ SyncW(s, fail).sKeep : SyncW(s, fail).baseline[q] = Nil BY <1>3, <1>g
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, <2>4, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ SyncW(s, fail).pc = "scanned") => SyncW(s, fail).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* The narrow / widen verb.                                                 *)

LEMMA RescopeBegin_M1 ==
  ASSUME IndM1, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RescopeBegin_TypeOK
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>h. holder' = holder BY DEF RescopeBegin, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF RescopeBegin
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RescopeBeginW(s, T) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RescopeBeginW(s, T).st = w[s].st
      /\ RescopeBeginW(s, T).pc = w[s].pc
      /\ RescopeBeginW(s, T).deletes = w[s].deletes
      /\ RescopeBeginW(s, T).inst = w[s].inst
      /\ RescopeBeginW(s, T).uploads = w[s].uploads
      /\ RescopeBeginW(s, T).gone = w[s].gone
      /\ RescopeBeginW(s, T).unlinked = w[s].unlinked
      /\ RescopeBeginW(s, T).sStage = "saved"
      /\ RescopeBeginW(s, T).local = w[s].local
      /\ RescopeBeginW(s, T).baseline = w[s].baseline
      /\ RescopeBeginW(s, T).sDrop = Leaving(s, T)
      /\ RescopeBeginW(s, T).sKeep = {}
      /\ RescopeBeginW(s, T).sHeld = [q \in Paths |-> Nil]
      /\ RescopeBeginW(s, T).deposed = w[s].deposed
  BY DEF RescopeBeginW
<1>4a. (RescopeBeginW(s, T).pc \in CC <=> w[s].pc \in CC) /\ RescopeBeginW(s, T).deposed = w[s].deposed /\ (w[s].st = "on" => RescopeBeginW(s, T).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RescopeBeginW(s, T).pc = "cased" => \A q \in RescopeBeginW(s, T).deletes : RescopeBeginW(s, T).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RescopeBeginW(s, T).deletes \cap RescopeBeginW(s, T).uploads = {} /\ RescopeBeginW(s, T).gone \subseteq RescopeBeginW(s, T).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RescopeBeginW(s, T).pc \in {"scanned", "claimed", "cased"} => (RescopeBeginW(s, T).uploads \cup RescopeBeginW(s, T).deletes) \cap RescopeBeginW(s, T).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RescopeBeginW(s, T).st = "off" => RescopeBeginW(s, T) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>2. \A q \in Paths : (q \in RescopeBeginW(s, T).unlinked /\ RescopeBeginW(s, T).sStage \in {"saved", "mid"}) =>
          \/ RescopeBeginW(s, T).local[q] = RescopeBeginW(s, T).baseline[q]
          \/ /\ q \in RescopeBeginW(s, T).sDrop \ RescopeBeginW(s, T).sKeep /\ RescopeBeginW(s, T).baseline[q] = Nil
             /\ RescopeBeginW(s, T).local[q] # Nil /\ RescopeBeginW(s, T).sHeld[q] = RescopeBeginW(s, T).local[q]
    BY <1>3, <1>g, <1>r DEF R2
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RescopeBeginW(s, T).pc = "scanned") => RescopeBeginW(s, T).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA RescopeFirst_M1 ==
  ASSUME IndM1, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RescopeFirst_TypeOK
<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>h. holder' = holder BY DEF RescopeFirst, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "saved" BY DEF RescopeFirst
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RescopeFirstW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RescopeFirstW(s).st = w[s].st
      /\ RescopeFirstW(s).pc = w[s].pc
      /\ RescopeFirstW(s).deletes = w[s].deletes
      /\ RescopeFirstW(s).inst = w[s].inst
      /\ RescopeFirstW(s).uploads = w[s].uploads
      /\ RescopeFirstW(s).gone = w[s].gone
      /\ RescopeFirstW(s).unlinked = w[s].unlinked
      /\ RescopeFirstW(s).sStage = "mid"
      /\ RescopeFirstW(s).local = w[s].local
      /\ RescopeFirstW(s).baseline = [q \in Paths |-> IF q \in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[q]]
      /\ RescopeFirstW(s).sDrop = w[s].sDrop
      /\ RescopeFirstW(s).sKeep = KeepSet(s)
      /\ RescopeFirstW(s).sHeld = [q \in Paths |-> IF q \in RescopeFirstDd(s) /\ w[s].baseline[q] # Nil
                                     THEN w[s].baseline[q] ELSE w[s].sHeld[q]]
      /\ RescopeFirstW(s).deposed = w[s].deposed
  BY <1>u DEF RescopeFirstW
<1>4a. (RescopeFirstW(s).pc \in CC <=> w[s].pc \in CC) /\ RescopeFirstW(s).deposed = w[s].deposed /\ (w[s].st = "on" => RescopeFirstW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RescopeFirstW(s).pc = "cased" => \A q \in RescopeFirstW(s).deletes : RescopeFirstW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RescopeFirstW(s).deletes \cap RescopeFirstW(s).uploads = {} /\ RescopeFirstW(s).gone \subseteq RescopeFirstW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RescopeFirstW(s).pc \in {"scanned", "claimed", "cased"} => (RescopeFirstW(s).uploads \cup RescopeFirstW(s).deletes) \cap RescopeFirstW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RescopeFirstW(s).st = "off" => RescopeFirstW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>d. /\ RescopeFirstDd(s) = w[s].sDrop \ KeepSet(s)
        /\ \A q \in w[s].sDrop : w[s].baseline[q] = Nil => q \notin KeepSet(s)
    BY DEF RescopeFirstDd, KeepSet
  <2>e. \A q \in w[s].unlinked :
          \/ w[s].local[q] = w[s].baseline[q]
          \/ /\ q \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[q] = Nil
             /\ w[s].local[q] # Nil /\ w[s].sHeld[q] = w[s].local[q]
    BY <1>r, <1>g, <1>f DEF R3
  <2>2. \A q \in Paths : (q \in RescopeFirstW(s).unlinked /\ RescopeFirstW(s).sStage \in {"saved", "mid"}) =>
          \/ RescopeFirstW(s).local[q] = RescopeFirstW(s).baseline[q]
          \/ /\ q \in RescopeFirstW(s).sDrop \ RescopeFirstW(s).sKeep /\ RescopeFirstW(s).baseline[q] = Nil
             /\ RescopeFirstW(s).local[q] # Nil /\ RescopeFirstW(s).sHeld[q] = RescopeFirstW(s).local[q]
    BY <1>3, <1>f, <2>d, <2>e
  <2>3. RescopeFirstW(s).sStage = "mid" => \A q \in RescopeFirstW(s).sDrop \ RescopeFirstW(s).sKeep : RescopeFirstW(s).baseline[q] = Nil
    BY <1>3, <1>f, <2>d
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RescopeFirstW(s).pc = "scanned") => RescopeFirstW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA RescopeSecond_M1 ==
  ASSUME IndM1, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RescopeSecond_TypeOK
<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>h. holder' = holder BY DEF RescopeSecond, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "mid" BY DEF RescopeSecond
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RescopeSecondW(s, wfail) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RescopeSecondW(s, wfail).st = w[s].st
      /\ RescopeSecondW(s, wfail).pc = w[s].pc
      /\ RescopeSecondW(s, wfail).deletes = w[s].deletes
      /\ RescopeSecondW(s, wfail).inst = w[s].inst
      /\ RescopeSecondW(s, wfail).uploads = w[s].uploads
      /\ RescopeSecondW(s, wfail).gone = w[s].gone
      /\ RescopeSecondW(s, wfail).unlinked = (w[s].unlinked \cup {q \in Paths : RescopeUnlink(s, q) /\ RescopeUnciteFirst}) \ RescopeFetch(s, wfail)
      /\ RescopeSecondW(s, wfail).sStage = "none"
      /\ RescopeSecondW(s, wfail).local = [q \in Paths |-> IF q \in RescopeFetch(s, wfail) /\ (RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal)
                                        THEN doc[q] ELSE RescopeLocal1(s)[q]]
      /\ RescopeSecondW(s, wfail).baseline = [q \in Paths |-> IF q \in RescopeFetch(s, wfail) THEN doc[q] ELSE RescopeBase1(s)[q]]
      /\ RescopeSecondW(s, wfail).sDrop = {}
      /\ RescopeSecondW(s, wfail).sKeep = {}
      /\ RescopeSecondW(s, wfail).sHeld = [q \in Paths |-> Nil]
      /\ RescopeSecondW(s, wfail).deposed = w[s].deposed
  BY DEF RescopeSecondW
<1>4a. (RescopeSecondW(s, wfail).pc \in CC <=> w[s].pc \in CC) /\ RescopeSecondW(s, wfail).deposed = w[s].deposed /\ (w[s].st = "on" => RescopeSecondW(s, wfail).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RescopeSecondW(s, wfail).pc = "cased" => \A q \in RescopeSecondW(s, wfail).deletes : RescopeSecondW(s, wfail).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RescopeSecondW(s, wfail).deletes \cap RescopeSecondW(s, wfail).uploads = {} /\ RescopeSecondW(s, wfail).gone \subseteq RescopeSecondW(s, wfail).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RescopeSecondW(s, wfail).pc \in {"scanned", "claimed", "cased"} => (RescopeSecondW(s, wfail).uploads \cup RescopeSecondW(s, wfail).deletes) \cap RescopeSecondW(s, wfail).unlinked = {} BY <1>3, <1>g
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RescopeSecondW(s, wfail).st = "off" => RescopeSecondW(s, wfail) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>a. /\ RescopeLocal1(s) = [q \in Paths |-> IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]]
        /\ RescopeBase1(s) = w[s].baseline
    BY <1>u DEF RescopeLocal1, RescopeBase1
  <2>b. \A q \in Paths : RescopeUnlink(s, q) => q \in w[s].sDrop \ w[s].sKeep /\ w[s].local[q] # Nil
    BY DEF RescopeUnlink, RescopeDrop
  <2>c. \A q \in Paths : (q \in w[s].sDrop \ w[s].sKeep /\ w[s].local[q] # Nil /\ w[s].sHeld[q] = w[s].local[q]) => RescopeUnlink(s, q)
    BY DEF RescopeUnlink, RescopeDrop
  <2>d. \A q \in w[s].sDrop \ w[s].sKeep : w[s].baseline[q] = Nil BY <1>r, <1>g DEF R4
  <2>e. \A q \in w[s].unlinked :
          \/ w[s].local[q] = w[s].baseline[q]
          \/ /\ q \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[q] = Nil
             /\ w[s].local[q] # Nil /\ w[s].sHeld[q] = w[s].local[q]
    BY <1>r, <1>g, <1>f DEF R3
  <2>1. \A q \in Paths : (q \in RescopeSecondW(s, wfail).unlinked /\ RescopeSecondW(s, wfail).sStage = "none")
          => RescopeSecondW(s, wfail).local[q] = RescopeSecondW(s, wfail).baseline[q]
    <3>. SUFFICES ASSUME NEW q \in Paths, q \in RescopeSecondW(s, wfail).unlinked
                  PROVE  RescopeSecondW(s, wfail).local[q] = RescopeSecondW(s, wfail).baseline[q]
      OBVIOUS
    <3>1. q \notin RescopeFetch(s, wfail) /\ (q \in w[s].unlinked \/ RescopeUnlink(s, q)) BY <1>3, <1>u
    <3>2. /\ RescopeSecondW(s, wfail).local[q] = IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]
          /\ RescopeSecondW(s, wfail).baseline[q] = w[s].baseline[q]
      BY <1>3, <2>a, <3>1
    <3>3. CASE RescopeUnlink(s, q) BY <3>2, <3>3, <2>b, <2>d
    <3>4. CASE ~RescopeUnlink(s, q)
      <4>1. q \in w[s].unlinked BY <3>1, <3>4
      <4>2. w[s].local[q] = w[s].baseline[q] BY <4>1, <2>e, <2>c, <3>4
      <4>. QED BY <3>2, <3>4, <4>2
    <3>. QED BY <3>3, <3>4
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>1, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RescopeSecondW(s, wfail).pc = "scanned") => RescopeSecondW(s, wfail).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* A reader's tick.                                                         *)

LEMMA RPullRead_M1 ==
  ASSUME IndM1, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RPullRead_TypeOK
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>h. holder' = holder BY DEF RPullRead, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage \in {"none", "saved"} BY DEF RPullRead
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RPullReadW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RPullReadW(s).st = w[s].st
      /\ RPullReadW(s).pc = "pulling"
      /\ RPullReadW(s).deletes = w[s].deletes
      /\ RPullReadW(s).inst = w[s].inst
      /\ RPullReadW(s).uploads = w[s].uploads
      /\ RPullReadW(s).gone = w[s].gone
      /\ RPullReadW(s).unlinked = w[s].unlinked
      /\ RPullReadW(s).sStage = w[s].sStage
      /\ RPullReadW(s).local = w[s].local
      /\ RPullReadW(s).baseline = w[s].baseline
      /\ RPullReadW(s).sDrop = w[s].sDrop
      /\ RPullReadW(s).sKeep = w[s].sKeep
      /\ RPullReadW(s).sHeld = w[s].sHeld
      /\ RPullReadW(s).deposed = w[s].deposed
  BY DEF RPullReadW
<1>4a. (RPullReadW(s).pc \in CC <=> w[s].pc \in CC) /\ RPullReadW(s).deposed = w[s].deposed /\ (w[s].st = "on" => RPullReadW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RPullReadW(s).pc = "cased" => \A q \in RPullReadW(s).deletes : RPullReadW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RPullReadW(s).deletes \cap RPullReadW(s).uploads = {} /\ RPullReadW(s).gone \subseteq RPullReadW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RPullReadW(s).pc \in {"scanned", "claimed", "cased"} => (RPullReadW(s).uploads \cup RPullReadW(s).deletes) \cap RPullReadW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RPullReadW(s).st = "off" => RPullReadW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ RPullReadW(s).sStage = w[s].sStage /\ RPullReadW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in RPullReadW(s).unlinked : RPullReadW(s).local[q] = w[s].local[q]
       /\ RPullReadW(s).baseline = w[s].baseline /\ RPullReadW(s).sDrop = w[s].sDrop /\ RPullReadW(s).sKeep = w[s].sKeep /\ RPullReadW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (RPullReadW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => RPullReadW(s).sStage = "none")
       /\ (RPullReadW(s).pc = "pulling" => RPullReadW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ RPullReadW(s).pc = "scanned") => RPullReadW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA RPullSync_M1 ==
  ASSUME IndM1, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RPullSync_TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>h. holder' = holder BY DEF RPullSync, bucket
<1>g. On(s) /\ w[s].pc = "pulling" BY DEF RPullSync
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RPullSyncW(s, fail) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RPullSyncW(s, fail).st = w[s].st
      /\ RPullSyncW(s, fail).pc = "idle"
      /\ RPullSyncW(s, fail).deletes = w[s].deletes
      /\ RPullSyncW(s, fail).inst = w[s].inst
      /\ RPullSyncW(s, fail).uploads = w[s].uploads
      /\ RPullSyncW(s, fail).gone = w[s].gone
      /\ RPullSyncW(s, fail).unlinked = w[s].unlinked
      /\ RPullSyncW(s, fail).sStage = w[s].sStage
      /\ RPullSyncW(s, fail).local = [q \in Paths |-> IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]
      /\ RPullSyncW(s, fail).baseline = SyncBl(s, fail)
      /\ RPullSyncW(s, fail).sDrop = w[s].sDrop
      /\ RPullSyncW(s, fail).sKeep = w[s].sKeep
      /\ RPullSyncW(s, fail).sHeld = w[s].sHeld
      /\ RPullSyncW(s, fail).deposed = w[s].deposed
  BY DEF RPullSyncW
<1>4a. (RPullSyncW(s, fail).pc \in CC <=> w[s].pc \in CC) /\ RPullSyncW(s, fail).deposed = w[s].deposed /\ (w[s].st = "on" => RPullSyncW(s, fail).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RPullSyncW(s, fail).pc = "cased" => \A q \in RPullSyncW(s, fail).deletes : RPullSyncW(s, fail).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RPullSyncW(s, fail).deletes \cap RPullSyncW(s, fail).uploads = {} /\ RPullSyncW(s, fail).gone \subseteq RPullSyncW(s, fail).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RPullSyncW(s, fail).pc \in {"scanned", "claimed", "cased"} => (RPullSyncW(s, fail).uploads \cup RPullSyncW(s, fail).deletes) \cap RPullSyncW(s, fail).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RPullSyncW(s, fail).st = "off" => RPullSyncW(s, fail) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>0. \A q \in Paths : q \in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] BY DEF SyncOwed, SyncAll, Owed
  <2>1. \A q \in Paths : /\ (w[s].local[q] = w[s].baseline[q] => RPullSyncW(s, fail).local[q] = RPullSyncW(s, fail).baseline[q])
                        /\ (q \notin SyncOwed(s, fail) => RPullSyncW(s, fail).local[q] = w[s].local[q] /\ RPullSyncW(s, fail).baseline[q] = w[s].baseline[q])
    BY <1>3, <2>0 DEF SyncBl
  <2>2. \A q \in Paths : (q \in RPullSyncW(s, fail).unlinked /\ RPullSyncW(s, fail).sStage = "none") => RPullSyncW(s, fail).local[q] = RPullSyncW(s, fail).baseline[q]
    BY <1>3, <1>r, <2>1 DEF R2
  <2>3. \A q \in Paths : (q \in RPullSyncW(s, fail).unlinked /\ RPullSyncW(s, fail).sStage \in {"saved", "mid"}) =>
          \/ RPullSyncW(s, fail).local[q] = RPullSyncW(s, fail).baseline[q]
          \/ /\ q \in RPullSyncW(s, fail).sDrop \ RPullSyncW(s, fail).sKeep /\ RPullSyncW(s, fail).baseline[q] = Nil
             /\ RPullSyncW(s, fail).local[q] # Nil /\ RPullSyncW(s, fail).sHeld[q] = RPullSyncW(s, fail).local[q]
    BY <1>3, <1>r, <2>0, <2>1 DEF R3
  <2>4. RPullSyncW(s, fail).sStage = "mid" => \A q \in RPullSyncW(s, fail).sDrop \ RPullSyncW(s, fail).sKeep : RPullSyncW(s, fail).baseline[q] = Nil BY <1>3, <1>g, <1>r DEF R5
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, <2>4, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RPullSyncW(s, fail).pc = "scanned") => RPullSyncW(s, fail).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* The answer (row 15): steps that write only the answer's fields.         *)

LEMMA Touch_M1 ==
  ASSUME IndM1, NEW s \in Writers, Touch(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Touch_TypeOK
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] BY DEF Touch
<1>h. holder' = holder BY DEF Touch, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Touch
<1>2. \A t \in Writers : w'[t] = IF t = s THEN TouchW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ TouchW(s).st = w[s].st
      /\ TouchW(s).pc = w[s].pc
      /\ TouchW(s).deletes = w[s].deletes
      /\ TouchW(s).inst = w[s].inst
      /\ TouchW(s).uploads = w[s].uploads
      /\ TouchW(s).gone = w[s].gone
      /\ TouchW(s).unlinked = w[s].unlinked
      /\ TouchW(s).sStage = w[s].sStage
      /\ TouchW(s).local = w[s].local
      /\ TouchW(s).baseline = w[s].baseline
      /\ TouchW(s).sDrop = w[s].sDrop
      /\ TouchW(s).sKeep = w[s].sKeep
      /\ TouchW(s).sHeld = w[s].sHeld
      /\ TouchW(s).deposed = w[s].deposed
  BY DEF TouchW
<1>4a. (TouchW(s).pc \in CC <=> w[s].pc \in CC) /\ TouchW(s).deposed = w[s].deposed /\ (w[s].st = "on" => TouchW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. TouchW(s).pc = "cased" => \A q \in TouchW(s).deletes : TouchW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. TouchW(s).deletes \cap TouchW(s).uploads = {} /\ TouchW(s).gone \subseteq TouchW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. TouchW(s).pc \in {"scanned", "claimed", "cased"} => (TouchW(s).uploads \cup TouchW(s).deletes) \cap TouchW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. TouchW(s).st = "off" => TouchW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ TouchW(s).sStage = w[s].sStage /\ TouchW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in TouchW(s).unlinked : TouchW(s).local[q] = w[s].local[q]
       /\ TouchW(s).baseline = w[s].baseline /\ TouchW(s).sDrop = w[s].sDrop /\ TouchW(s).sKeep = w[s].sKeep /\ TouchW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (TouchW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => TouchW(s).sStage = "none")
       /\ (TouchW(s).pc = "pulling" => TouchW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ TouchW(s).pc = "scanned") => TouchW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Honor_M1 ==
  ASSUME IndM1, NEW s \in Writers, Honor(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Honor_TypeOK
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] BY DEF Honor
<1>h. holder' = holder BY DEF Honor, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Honor
<1>2. \A t \in Writers : w'[t] = IF t = s THEN HonorW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ HonorW(s).st = w[s].st
      /\ HonorW(s).pc = w[s].pc
      /\ HonorW(s).deletes = w[s].deletes
      /\ HonorW(s).inst = w[s].inst
      /\ HonorW(s).uploads = w[s].uploads
      /\ HonorW(s).gone = w[s].gone
      /\ HonorW(s).unlinked = w[s].unlinked
      /\ HonorW(s).sStage = w[s].sStage
      /\ HonorW(s).local = w[s].local
      /\ HonorW(s).baseline = w[s].baseline
      /\ HonorW(s).sDrop = w[s].sDrop
      /\ HonorW(s).sKeep = w[s].sKeep
      /\ HonorW(s).sHeld = w[s].sHeld
      /\ HonorW(s).deposed = w[s].deposed
  BY DEF HonorW
<1>4a. (HonorW(s).pc \in CC <=> w[s].pc \in CC) /\ HonorW(s).deposed = w[s].deposed /\ (w[s].st = "on" => HonorW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. HonorW(s).pc = "cased" => \A q \in HonorW(s).deletes : HonorW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. HonorW(s).deletes \cap HonorW(s).uploads = {} /\ HonorW(s).gone \subseteq HonorW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. HonorW(s).pc \in {"scanned", "claimed", "cased"} => (HonorW(s).uploads \cup HonorW(s).deletes) \cap HonorW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. HonorW(s).st = "off" => HonorW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ HonorW(s).sStage = w[s].sStage /\ HonorW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in HonorW(s).unlinked : HonorW(s).local[q] = w[s].local[q]
       /\ HonorW(s).baseline = w[s].baseline /\ HonorW(s).sDrop = w[s].sDrop /\ HonorW(s).sKeep = w[s].sKeep /\ HonorW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (HonorW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => HonorW(s).sStage = "none")
       /\ (HonorW(s).pc = "pulling" => HonorW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ HonorW(s).pc = "scanned") => HonorW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA HonorReader_M1 ==
  ASSUME IndM1, NEW s \in Writers, HonorReader(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, HonorReader_TypeOK
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY DEF HonorReader
<1>h. holder' = holder BY DEF HonorReader, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF HonorReader
<1>2. \A t \in Writers : w'[t] = IF t = s THEN HonorReaderW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ HonorReaderW(s).st = w[s].st
      /\ HonorReaderW(s).pc = w[s].pc
      /\ HonorReaderW(s).deletes = w[s].deletes
      /\ HonorReaderW(s).inst = w[s].inst
      /\ HonorReaderW(s).uploads = w[s].uploads
      /\ HonorReaderW(s).gone = w[s].gone
      /\ HonorReaderW(s).unlinked = w[s].unlinked
      /\ HonorReaderW(s).sStage = w[s].sStage
      /\ HonorReaderW(s).local = w[s].local
      /\ HonorReaderW(s).baseline = w[s].baseline
      /\ HonorReaderW(s).sDrop = w[s].sDrop
      /\ HonorReaderW(s).sKeep = w[s].sKeep
      /\ HonorReaderW(s).sHeld = w[s].sHeld
      /\ HonorReaderW(s).deposed = w[s].deposed
  BY DEF HonorReaderW
<1>4a. (HonorReaderW(s).pc \in CC <=> w[s].pc \in CC) /\ HonorReaderW(s).deposed = w[s].deposed /\ (w[s].st = "on" => HonorReaderW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. HonorReaderW(s).pc = "cased" => \A q \in HonorReaderW(s).deletes : HonorReaderW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. HonorReaderW(s).deletes \cap HonorReaderW(s).uploads = {} /\ HonorReaderW(s).gone \subseteq HonorReaderW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. HonorReaderW(s).pc \in {"scanned", "claimed", "cased"} => (HonorReaderW(s).uploads \cup HonorReaderW(s).deletes) \cap HonorReaderW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. HonorReaderW(s).st = "off" => HonorReaderW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ HonorReaderW(s).sStage = w[s].sStage /\ HonorReaderW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in HonorReaderW(s).unlinked : HonorReaderW(s).local[q] = w[s].local[q]
       /\ HonorReaderW(s).baseline = w[s].baseline /\ HonorReaderW(s).sDrop = w[s].sDrop /\ HonorReaderW(s).sKeep = w[s].sKeep /\ HonorReaderW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (HonorReaderW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => HonorReaderW(s).sStage = "none")
       /\ (HonorReaderW(s).pc = "pulling" => HonorReaderW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ HonorReaderW(s).pc = "scanned") => HonorReaderW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA RetireByAck_M1 ==
  ASSUME IndM1, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RetireByAck_TypeOK
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY DEF RetireByAck
<1>h. holder' = holder BY DEF RetireByAck, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RetireByAck
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RetireByAckW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RetireByAckW(s).st = w[s].st
      /\ RetireByAckW(s).pc = w[s].pc
      /\ RetireByAckW(s).deletes = w[s].deletes
      /\ RetireByAckW(s).inst = w[s].inst
      /\ RetireByAckW(s).uploads = w[s].uploads
      /\ RetireByAckW(s).gone = w[s].gone
      /\ RetireByAckW(s).unlinked = w[s].unlinked
      /\ RetireByAckW(s).sStage = w[s].sStage
      /\ RetireByAckW(s).local = w[s].local
      /\ RetireByAckW(s).baseline = w[s].baseline
      /\ RetireByAckW(s).sDrop = w[s].sDrop
      /\ RetireByAckW(s).sKeep = w[s].sKeep
      /\ RetireByAckW(s).sHeld = w[s].sHeld
      /\ RetireByAckW(s).deposed = w[s].deposed
  BY DEF RetireByAckW
<1>4a. (RetireByAckW(s).pc \in CC <=> w[s].pc \in CC) /\ RetireByAckW(s).deposed = w[s].deposed /\ (w[s].st = "on" => RetireByAckW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RetireByAckW(s).pc = "cased" => \A q \in RetireByAckW(s).deletes : RetireByAckW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RetireByAckW(s).deletes \cap RetireByAckW(s).uploads = {} /\ RetireByAckW(s).gone \subseteq RetireByAckW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RetireByAckW(s).pc \in {"scanned", "claimed", "cased"} => (RetireByAckW(s).uploads \cup RetireByAckW(s).deletes) \cap RetireByAckW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RetireByAckW(s).st = "off" => RetireByAckW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ RetireByAckW(s).sStage = w[s].sStage /\ RetireByAckW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in RetireByAckW(s).unlinked : RetireByAckW(s).local[q] = w[s].local[q]
       /\ RetireByAckW(s).baseline = w[s].baseline /\ RetireByAckW(s).sDrop = w[s].sDrop /\ RetireByAckW(s).sKeep = w[s].sKeep /\ RetireByAckW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (RetireByAckW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => RetireByAckW(s).sStage = "none")
       /\ (RetireByAckW(s).pc = "pulling" => RetireByAckW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ RetireByAckW(s).pc = "scanned") => RetireByAckW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* Deposal (row 17).                                                        *)

\* The taker's tree and the deposed holder's: the lease moves to t, the old
\* holder stays in its section, marked.
LEMMA Depose_M1 ==
  ASSUME IndM1, NEW t \in Writers, Depose(t), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>t. IndTypeOK' BY <1>a, Depose_TypeOK
<1>1. /\ On(t) /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t /\ holder' = t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
  BY DEF Depose
<1>2. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>b, <1>1 DEF TypeOK
<1>3. /\ DeposeW(t).pc = "claimed"
      /\ DeposeW(t).deletes = w[t].deletes
      /\ DeposeW(t).inst = w[t].inst
      /\ DeposeW(t).uploads = w[t].uploads
      /\ DeposeW(t).gone = w[t].gone
      /\ DeposeW(t).unlinked = w[t].unlinked
      /\ DeposeW(t).sStage = w[t].sStage
      /\ DeposeW(t).local = w[t].local
      /\ DeposeW(t).baseline = w[t].baseline
      /\ DeposeW(t).sDrop = w[t].sDrop
      /\ DeposeW(t).sKeep = w[t].sKeep
      /\ DeposeW(t).sHeld = w[t].sHeld
      /\ DeposeW(t).deposed = w[t].deposed
      /\ DeposeW(t).st = w[t].st
  BY DEF DeposeW
<1>3a. /\ DeposedW(holder).deposed = TRUE
       /\ DeposedW(holder).deletes = w[holder].deletes
       /\ DeposedW(holder).inst = w[holder].inst
       /\ DeposedW(holder).uploads = w[holder].uploads
       /\ DeposedW(holder).gone = w[holder].gone
       /\ DeposedW(holder).unlinked = w[holder].unlinked
       /\ DeposedW(holder).sStage = w[holder].sStage
       /\ DeposedW(holder).local = w[holder].local
       /\ DeposedW(holder).baseline = w[holder].baseline
       /\ DeposedW(holder).sDrop = w[holder].sDrop
       /\ DeposedW(holder).sKeep = w[holder].sKeep
       /\ DeposedW(holder).sHeld = w[holder].sHeld
       /\ DeposedW(holder).st = w[holder].st
       /\ DeposedW(holder).pc = w[holder].pc
  BY DEF DeposedW
<1>4. ~w[t].deposed /\ w[t].st = "on" BY <1>m, <1>1 DEF Dep, CC, On
<1>5. w[holder].pc \in CC /\ w[holder].st = "on" BY <1>m, <1>1, NoneWriter DEF Holder
<1>6. Inv_OneHolder' /\ Holder' /\ Dep'
  <2>1. Inv_OneHolder' BY <1>m, <1>1, <1>2, <1>3, <1>3a, <1>4 DEF Inv_OneHolder, CC
  <2>2. Holder' BY <1>1, <1>2, <1>3, <1>4 DEF Holder, CC
  <2>3. Dep' BY <1>m, <1>1, <1>2, <1>3, <1>3a, <1>4, <1>5 DEF Dep, CC
  <2>. QED BY <2>1, <2>2, <2>3
<1>7. Cased' /\ Mine' BY <1>m, <1>1, <1>2, <1>3, <1>3a DEF Cased, Mine
<1>8. Ups' BY <1>m, <1>1, <1>2, <1>3, <1>3a DEF Ups
<1>9. Off' BY <1>m, <1>1, <1>2, <1>3, <1>3a, <1>4, <1>5 DEF Off
<1>10. Rescope'
  <2>1. R1' BY <1>r, <1>1, <1>2, <1>3, <1>3a DEF R1
  <2>2. R2' /\ R3' /\ R4' /\ R5' BY <1>r, <1>1, <1>2, <1>3, <1>3a DEF R2, R3, R4, R5
  <2>. QED BY <2>1, <2>2 DEF Rescope
<1>11. NarrowOK BY <1>1, <1>2, <1>3, <1>3a, <1>5 DEF NarrowOK, CC
<1>. QED BY <1>t, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF IndM1, M1

LEMMA Fence_M1 ==
  ASSUME IndM1, NEW s \in Writers, Fence(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Fence_TypeOK
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>h. holder' = holder BY DEF Fence, bucket
  <2>g. On(s) /\ w[s].deposed /\ w[s].pc = "claimed" BY <1>1 DEF Fence
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN FenceBeforeW(s) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ FenceBeforeW(s).st = w[s].st
        /\ FenceBeforeW(s).pc = "idle"
        /\ FenceBeforeW(s).deletes = {}
        /\ FenceBeforeW(s).inst = [q \in Paths |-> Nil]
        /\ FenceBeforeW(s).uploads = {}
        /\ FenceBeforeW(s).gone = {}
        /\ FenceBeforeW(s).unlinked = w[s].unlinked
        /\ FenceBeforeW(s).sStage = w[s].sStage
        /\ FenceBeforeW(s).local = w[s].local
        /\ FenceBeforeW(s).baseline = w[s].baseline
        /\ FenceBeforeW(s).sDrop = w[s].sDrop
        /\ FenceBeforeW(s).sKeep = w[s].sKeep
        /\ FenceBeforeW(s).sHeld = w[s].sHeld
        /\ FenceBeforeW(s).deposed = FALSE
    BY DEF FenceBeforeW
  <2>4. Inv_OneHolder' /\ Holder' /\ Dep'
    <3>0. holder # s BY <1>m, <2>g DEF Dep
    <3>1. Inv_OneHolder' BY <1>m, <1>0, <2>2, <2>3, <2>h, <3>0 DEF Inv_OneHolder, CC
    <3>2. Holder' BY <1>m, <1>0, <2>2, <2>3, <2>h, <3>0 DEF Holder, CC
    <3>3. Dep' BY <1>m, <1>0, <2>2, <2>3, <2>h, <3>0 DEF Dep, CC
    <3>. QED BY <3>1, <3>2, <3>3
  <2>5a. FenceBeforeW(s).pc = "cased" => \A q \in FenceBeforeW(s).deletes : FenceBeforeW(s).inst[q] = Nil BY <2>3
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. FenceBeforeW(s).deletes \cap FenceBeforeW(s).uploads = {} /\ FenceBeforeW(s).gone \subseteq FenceBeforeW(s).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. FenceBeforeW(s).pc \in {"scanned", "claimed", "cased"} => (FenceBeforeW(s).uploads \cup FenceBeforeW(s).deletes) \cap FenceBeforeW(s).unlinked = {} BY <2>3
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. FenceBeforeW(s).st = "off" => FenceBeforeW(s) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ FenceBeforeW(s).sStage = w[s].sStage /\ FenceBeforeW(s).unlinked \subseteq w[s].unlinked
         /\ \A q \in FenceBeforeW(s).unlinked : FenceBeforeW(s).local[q] = w[s].local[q]
         /\ FenceBeforeW(s).baseline = w[s].baseline /\ FenceBeforeW(s).sDrop = w[s].sDrop /\ FenceBeforeW(s).sKeep = w[s].sKeep /\ FenceBeforeW(s).sHeld = w[s].sHeld
    BY <2>3, <1>f
  <2>9b. /\ (FenceBeforeW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => FenceBeforeW(s).sStage = "none")
         /\ (FenceBeforeW(s).pc = "pulling" => FenceBeforeW(s).sStage \in {"none", "saved"})
    BY <2>3, <2>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ FenceBeforeW(s).pc = "scanned") => FenceBeforeW(s).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>h. holder' = holder BY DEF Fence, bucket
  <2>g. On(s) /\ w[s].deposed /\ w[s].pc = "cased" BY <1>2 DEF Fence
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN FenceAfterW(s) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ FenceAfterW(s).st = w[s].st
        /\ FenceAfterW(s).pc = w[s].pc
        /\ FenceAfterW(s).deletes = w[s].deletes
        /\ FenceAfterW(s).inst = w[s].inst
        /\ FenceAfterW(s).uploads = w[s].uploads
        /\ FenceAfterW(s).gone = w[s].gone
        /\ FenceAfterW(s).unlinked = w[s].unlinked
        /\ FenceAfterW(s).sStage = w[s].sStage
        /\ FenceAfterW(s).local = w[s].local
        /\ FenceAfterW(s).baseline = w[s].baseline
        /\ FenceAfterW(s).sDrop = w[s].sDrop
        /\ FenceAfterW(s).sKeep = w[s].sKeep
        /\ FenceAfterW(s).sHeld = w[s].sHeld
        /\ FenceAfterW(s).deposed = w[s].deposed
    BY DEF FenceAfterW
  <2>4a. (FenceAfterW(s).pc \in CC <=> w[s].pc \in CC) /\ FenceAfterW(s).deposed = w[s].deposed /\ (w[s].st = "on" => FenceAfterW(s).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' /\ Dep' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. FenceAfterW(s).pc = "cased" => \A q \in FenceAfterW(s).deletes : FenceAfterW(s).inst[q] = Nil BY <1>m, <2>3 DEF Cased
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. FenceAfterW(s).deletes \cap FenceAfterW(s).uploads = {} /\ FenceAfterW(s).gone \subseteq FenceAfterW(s).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. FenceAfterW(s).pc \in {"scanned", "claimed", "cased"} => (FenceAfterW(s).uploads \cup FenceAfterW(s).deletes) \cap FenceAfterW(s).unlinked = {} BY <1>m, <2>3 DEF Ups
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. FenceAfterW(s).st = "off" => FenceAfterW(s) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ FenceAfterW(s).sStage = w[s].sStage /\ FenceAfterW(s).unlinked \subseteq w[s].unlinked
         /\ \A q \in FenceAfterW(s).unlinked : FenceAfterW(s).local[q] = w[s].local[q]
         /\ FenceAfterW(s).baseline = w[s].baseline /\ FenceAfterW(s).sDrop = w[s].sDrop /\ FenceAfterW(s).sKeep = w[s].sKeep /\ FenceAfterW(s).sHeld = w[s].sHeld
    BY <2>3, <1>f
  <2>9b. /\ (FenceAfterW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => FenceAfterW(s).sStage = "none")
         /\ (FenceAfterW(s).pc = "pulling" => FenceAfterW(s).sStage \in {"none", "saved"})
    BY <2>3, <2>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ FenceAfterW(s).pc = "scanned") => FenceAfterW(s).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The step, and the theorems.                                              *)

LEMMA Next_M1 == IndM1 /\ Next => IndM1' /\ NarrowOK
<1>. SUFFICES ASSUME IndM1, Next PROVE IndM1' /\ NarrowOK OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM1' /\ NarrowOK BY <3>1, GPut_M1
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM1' /\ NarrowOK BY <3>2, GCas_M1
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM1' /\ NarrowOK BY <3>3, GDelete_M1
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM1' /\ NarrowOK BY <3>4, GRename_M1
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M1
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM1' /\ NarrowOK BY <3>1, Checkout_M1
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM1' /\ NarrowOK BY <3>2, Consume_M1
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM1' /\ NarrowOK BY <3>3, Scan_M1
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM1' /\ NarrowOK BY <3>4, Skip_M1
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM1' /\ NarrowOK BY <3>5, PullOnly_M1
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM1' /\ NarrowOK BY <3>6, Claim_M1
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM1' /\ NarrowOK BY <3>7, Verify_M1
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM1' /\ NarrowOK BY <3>8, Install_M1
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM1' /\ NarrowOK BY <3>9, Collect_M1
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM1' /\ NarrowOK BY <3>10, Finish_M1
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM1' /\ NarrowOK BY <3>11, Restart_M1
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM1' /\ NarrowOK BY <3>12, Sync_M1
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM1' /\ NarrowOK BY <3>13, RescopeBegin_M1
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM1' /\ NarrowOK BY <3>14, RescopeFirst_M1
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM1' /\ NarrowOK BY <3>15, RescopeSecond_M1
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM1' /\ NarrowOK BY <3>16, RPullRead_M1
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM1' /\ NarrowOK BY <3>17, RPullSync_M1
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM1' /\ NarrowOK BY <3>18, Edit_M1
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM1' /\ NarrowOK BY <3>19, Delete_M1
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM1' /\ NarrowOK BY <3>20, Upload_M1
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM1' /\ NarrowOK BY <3>21, Sweep_M1
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE IndM1' /\ NarrowOK BY <3>22, Touch_M1
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE IndM1' /\ NarrowOK BY <3>23, Honor_M1
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE IndM1' /\ NarrowOK BY <3>24, HonorReader_M1
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE IndM1' /\ NarrowOK BY <3>25, RetireByAck_M1
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE IndM1' /\ NarrowOK BY <3>26, Depose_M1
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE IndM1' /\ NarrowOK BY <3>27, Fence_M1
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M1
  <2>2. CASE RLoad BY <2>2, RLoad_M1
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM1' /\ NarrowOK BY <2>3, Reap_M1
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M1Invariant == Spec => []IndM1
<1>1. Init => IndM1 BY Init_M1
<1>2. IndM1 /\ [Next]_vars => IndM1'
  <2>1. IndM1 /\ Next => IndM1' BY Next_M1
  <2>2. IndM1 /\ UNCHANGED vars => IndM1'
    <3>1. IndM1 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM1 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM1, vars
    <3>. QED BY <3>1, <3>2 DEF IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

THEOREM OneHolder == Spec => []Inv_OneHolder
<1>1. IndM1 => Inv_OneHolder BY DEF IndM1, M1
<1>. QED BY M1Invariant, <1>1, PTL

THEOREM DeleteSettles == Spec => Prop_DeleteSettles
<1>1. ASSUME IndM1, NEW s \in Writers, Finish(s)
      PROVE  \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]
  BY <1>1 DEF IndM1, M1, Cased, Finish
<1>2. IndM1 => [\A s \in Writers :
                  Finish(s) => \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]]_vars
  BY <1>1
<1>. QED BY M1Invariant, <1>2, PTL DEF Prop_DeleteSettles

THEOREM NarrowNeverDeletes == Spec => Prop_NarrowNeverDeletes
<1>1. IndM1 /\ [Next]_vars => [NarrowOK]_vars
  <2>1. IndM1 /\ Next => NarrowOK BY Next_M1
  <2>. QED BY <2>1
<1>. QED BY M1Invariant, <1>1, PTL DEF Spec, Prop_NarrowNeverDeletes, NarrowOK

==============================================================================
