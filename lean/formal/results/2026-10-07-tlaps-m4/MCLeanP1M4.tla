----------------------------- MODULE MCLeanP1M4 -----------------------------
(***************************************************************************)
(* M4's inductive strengthening for Inv_ReaderFetches                      *)
(* (docs/plans/lean-tlaps-inductive-proof-plan.md: I11 and the             *)
(* never-re-cited lemma), as the proof needs it, stated over LeanP1.tla     *)
(* UNCHANGED and TLC-checked as INVARIANTs before any becomes a TLAPS       *)
(* lemma (results/2026-10-07-tlaps-m4/NOTES.txt).                          *)
(*                                                                         *)
(* The thread: a cited handle is never retiring or aged (M4NeverReCited:   *)
(* a step cites a fresh handle or moves a cited one, and what stops being  *)
(* cited is logged retiring).  So a reader that loaded the document less   *)
(* than G ago holds handles each still cited or retiring, never aged, and  *)
(* live (M4ReaderLive): the sweep spares the retiring, the reaper takes    *)
(* only the aged, and `aged` grows only when G elapses (`rlag`).           *)
(*                                                                         *)
(* Controls: M4CitedCtl (the reader's handles still CITED, without the     *)
(* retiring case) and M4AgedCtl (never aged, without the `rlag` guard).    *)
(***************************************************************************)
EXTENDS LeanP1

M4NeverReCited == \A p \in Paths : doc[p] # Nil => doc[p] \notin retiring \cup aged
M4ReaderLive == ~rlag => \A p \in Paths : rdoc[p] # Nil =>
                  /\ rdoc[p] \in live /\ rdoc[p] \notin aged
                  /\ (Cited(rdoc[p]) \/ rdoc[p] \in retiring)
M4All == M4NeverReCited /\ M4ReaderLive /\ Inv_ReaderFetches

\* THE CONTROLS.
M4CitedCtl == ~rlag => \A p \in Paths : rdoc[p] # Nil => Cited(rdoc[p])
M4AgedCtl == \A p \in Paths : rdoc[p] # Nil => rdoc[p] \notin aged
==============================================================================
