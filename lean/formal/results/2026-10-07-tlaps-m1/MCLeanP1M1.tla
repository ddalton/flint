----------------------------- MODULE MCLeanP1M1 -----------------------------
(***************************************************************************)
(* M1's inductive strengthening beyond the plan's I0/I1/I6/I7              *)
(* (docs/plans/lean-tlaps-inductive-proof-plan.md, section 3), stated as   *)
(* state predicates over LeanP1.tla UNCHANGED, to be TLC-checked as        *)
(* INVARIANTs before any becomes a TLAPS lemma.  Read off the actions      *)
(* while writing the step lemmas for Inv_OneHolder, Prop_DeleteSettles and *)
(* Prop_NarrowNeverDeletes (results/2026-10-07-tlaps-m1/NOTES.txt).        *)
(*                                                                         *)
(* M1R3Ctl is the positive control: the rescope conjunct WITHOUT its       *)
(* second disjunct, which must be VIOLATED once a rescope's first half     *)
(* uncites a path the previous rescope unlinked.                           *)
(***************************************************************************)
EXTENDS LeanP1

\* A scan's two sets are disjoint; what the verify withheld is an upload.
M1Mine == \A s \in Writers :
            /\ w[s].deletes \cap w[s].uploads = {}
            /\ w[s].gone \subseteq w[s].uploads

\* From the scan to the finish, nothing the barrier publishes or deletes is
\* a path a rescope unlinked (such a path is clean, I7).
M1Ups == \A s \in Writers :
           w[s].pc \in {"scanned", "claimed", "cased"} =>
             (w[s].uploads \cup w[s].deletes) \cap w[s].unlinked = {}

\* A writer that is off has never run: every step but Checkout needs On(s).
M1Off == \A s \in Writers : w[s].st = "off" => w[s] = WriterInit

\* I7 for a rescope in flight: an unlinked path is clean, or the first half
\* uncited it (its baseline dropped, its bytes those the intent recorded).
M1R3 == \A s \in Writers, p \in Paths :
          (p \in w[s].unlinked /\ w[s].sStage \in {"saved", "mid"}) =>
            \/ w[s].local[p] = w[s].baseline[p]
            \/ /\ p \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[p] = Nil
               /\ w[s].local[p] # Nil /\ w[s].sHeld[p] = w[s].local[p]

\* Between the halves, every path the first half dropped is uncited.
M1R4 == \A s \in Writers :
          w[s].sStage = "mid" => \A p \in w[s].sDrop \ w[s].sKeep : w[s].baseline[p] = Nil

\* A reader's pull never spans the halves.
M1R5 == \A s \in Writers : w[s].pc = "pulling" => w[s].sStage \in {"none", "saved"}

M1All == M1Mine /\ M1Ups /\ M1Off /\ M1R3 /\ M1R4 /\ M1R5

\* THE CONTROL: M1R3 without its second disjunct.
M1R3Ctl == \A s \in Writers, p \in Paths :
             (p \in w[s].unlinked /\ w[s].sStage \in {"saved", "mid"}) =>
               w[s].local[p] = w[s].baseline[p]
==============================================================================
