----------------------------- MODULE MCLeanP1M3 -----------------------------
(***************************************************************************)
(* M3's inductive strengthening for Inv_ShortcutSound and Inv_ReaderSound  *)
(* (docs/plans/lean-tlaps-inductive-proof-plan.md: I8-I10), as the proof   *)
(* needs it, stated over LeanP1.tla UNCHANGED and TLC-checked as INVARIANTs *)
(* before any becomes a TLAPS lemma.  Read off the actions                 *)
(* (results/2026-10-07-tlaps-m3/NOTES.txt).                                *)
(*                                                                         *)
(* The thread: the cheap path's RECORD (M3Record, I8) says that while the  *)
(* pointer is the one a writer last derived against, every held path where *)
(* the document and the baseline differ is in `skipped`.  Only the consume,*)
(* the sync, the reader's sync, the checkout and the finish write it.  The *)
(* finish needs three facts carried from the CAS: the install's own paths  *)
(* hold the snapshot (M3CasedMine); with `adv`, every other held path where *)
(* the INSTALLED document differs from the baseline is skipped             *)
(* (M3CasedAdv: Install's `adv` guard is what puts it there); and a cased  *)
(* writer whose record is still current did not move the document          *)
(* (M3CasedSame).  The bounds (I9, I10) make `derived = seq` mean "nothing *)
(* moved since" and turn a reader's memo into the record.                  *)
(*                                                                         *)
(* Controls: M3AdvCtl (M3CasedAdv without `adv`: a foreign change between  *)
(* the consume and the CAS) and M3RecordCtl (I8 without the rescope guard: *)
(* the uncite drops a baseline the record still names).                    *)
(***************************************************************************)
EXTENDS LeanP1

\* seq starts at 1, so a record of 0 ("nothing derived") never matches it.
M3Seq == seq >= 1

\* I10 and I9's bounds, for every tree (a non-reader's memo stays 0).
M3Bounds == \A s \in Writers :
              /\ w[s].derived <= seq /\ w[s].memo <= seq /\ w[s].rnow <= seq
              /\ (w[s].derived # 0 => w[s].memo <= w[s].derived)

\* I8: the cheap path's record.
M3Record == \A s \in Writers : (w[s].derived = seq /\ w[s].sStage = "none") =>
              \A p \in Paths : (Held(s, p) /\ doc[p] # w[s].baseline[p]) => p \in w[s].skipped

\* Between the CAS and the finish.
M3CasedMine == \A s \in Writers : w[s].pc = "cased" =>
                 \A p \in w[s].uploads \cap w[s].upDone : w[s].inst[p] = w[s].snap[p]
M3CasedAdv == \A s \in Writers : (w[s].pc = "cased" /\ w[s].adv) =>
                \A p \in Paths \ ((w[s].uploads \cap w[s].upDone) \cup w[s].deletes) :
                  (Held(s, p) /\ w[s].inst[p] # w[s].baseline[p]) => p \in w[s].skipped
M3CasedSame == \A s \in Writers : (w[s].pc = "cased" /\ w[s].derived = seq) =>
                 w[s].adv /\ w[s].inst = doc

M3All == M3Seq /\ M3Bounds /\ M3Record /\ M3CasedMine /\ M3CasedAdv /\ M3CasedSame
         /\ Inv_ShortcutSound /\ Inv_ReaderSound

\* THE CONTROLS.
M3AdvCtl == \A s \in Writers : w[s].pc = "cased" =>
              \A p \in Paths \ ((w[s].uploads \cap w[s].upDone) \cup w[s].deletes) :
                (Held(s, p) /\ w[s].inst[p] # w[s].baseline[p]) => p \in w[s].skipped
M3RecordCtl == \A s \in Writers : w[s].derived = seq =>
                 \A p \in Paths : (Held(s, p) /\ doc[p] # w[s].baseline[p]) => p \in w[s].skipped
==============================================================================
