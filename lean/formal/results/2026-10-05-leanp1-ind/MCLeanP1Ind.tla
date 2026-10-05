---------------------------- MODULE MCLeanP1Ind ----------------------------
(***************************************************************************)
(* The candidate conjuncts of the inductive invariant                      *)
(* (docs/plans/lean-tlaps-inductive-proof-plan.md §3, I0-I11), stated as   *)
(* state predicates over LeanP1.tla UNCHANGED (md5 94da7541), to be        *)
(* TLC-checked as INVARIANTs before any of them becomes a TLAPS lemma.     *)
(* A conjunct that fails here is a finding about the plan, not the model.  *)
(*                                                                         *)
(* Ind4Ctl is the positive control: the I4 shape without its pc guard,    *)
(* which must be VIOLATED right after an Install cites the upload.         *)
(***************************************************************************)
EXTENDS LeanP1

CitedSet == {doc[p] : p \in {q \in Paths : doc[q] # Nil}}
HandlesIn(f) == {f[p] : p \in {q \in Paths : f[q] # Nil}}

\* I0: the two-CAS rename is the mutation's; shipped, nothing is ever mid-rename.
Ind0 == mv = Nil

\* I1: the holder is in its commit section (with Inv_OneHolder, the converse).
Ind1 == holder # "none" => w[holder].pc \in {"claimed", "cased"} /\ On(holder)

\* I2: FRESHNESS.
Ind2a == live \subseteq upped /\ upped \subseteq minted
Ind2b == (retiring \cup aged) \subseteq upped
Ind2c == nextGen <= MaxMint + 1
Ind2d == \A h \in minted :
           Gen(h) < nextGen \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies)
Ind2e ==
  /\ HandlesIn(doc) \subseteq minted
  /\ HandlesIn(tomb) \subseteq minted
  /\ HandlesIn(gw) \subseteq minted
  /\ HandlesIn(rdoc) \subseteq minted
  /\ ({c[2] : c \in conflicts} \ {Nil}) \subseteq minted
  /\ {a[2] : a \in acked} \subseteq minted
  /\ {d[2] : d \in udel} \subseteq minted
  /\ \A s \in Writers :
       (HandlesIn(w[s].local) \cup HandlesIn(w[s].baseline) \cup HandlesIn(w[s].snap)
        \cup HandlesIn(w[s].inst) \cup HandlesIn(w[s].sHeld)) \subseteq minted
Ind2 == Ind2a /\ Ind2b /\ Ind2c /\ Ind2d /\ Ind2e

\* I3: A SAVE IN FLIGHT is live, uncited, unretired, minted at its path, and
\* in no tree.
Ind3 == \A p \in Paths : gw[p] # Nil =>
          /\ gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p] \notin retiring \cup aged
          /\ gw[p][1] = p
          /\ \A s \in Writers, q \in Paths : w[s].local[q] # gw[p] /\ w[s].baseline[q] # gw[p]

\* I4: AN UPLOAD BEFORE ITS CAS.
Ind4 == \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
          \A p \in w[s].upDone :
            /\ w[s].snap[p] # Nil /\ w[s].snap[p][1] = p
            /\ w[s].snap[p] \in upped
            /\ ~Cited(w[s].snap[p])
            /\ w[s].snap[p] \notin retiring \cup aged
            /\ \A t \in Writers \ {s}, q \in Paths :
                 w[t].local[q] # w[s].snap[p] /\ w[t].baseline[q] # w[s].snap[p]

\* THE CONTROL: I4's citation clause with the pc guard widened to "cased".
\* Install cites the upload, so this must fail at the first Install.
Ind4Ctl == \A s \in Writers : w[s].pc \in {"scanned", "claimed", "cased"} =>
             \A p \in w[s].upDone : ~Cited(w[s].snap[p])

\* I5: VERIFIED uploads are live until the CAS.
Ind5 == \A s \in Writers : (w[s].pc = "claimed" /\ w[s].verified) =>
          \A p \in (w[s].uploads \cap w[s].upDone) \ w[s].gone : w[s].snap[p] \in live

\* I6: CASED: every delete landed (M3).
Ind6 == \A s \in Writers : w[s].pc = "cased" => \A p \in w[s].deletes : w[s].inst[p] = Nil

\* I7: RESCOPE.
Ind7a == \A s \in Writers :
           w[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w[s].sStage = "none"
Ind7b == \A s \in Writers, p \in Paths :
           (p \in w[s].unlinked /\ w[s].sStage = "none") => w[s].local[p] = w[s].baseline[p]
Ind7 == Ind7a /\ Ind7b

\* I8: THE CHEAP PATH'S RECORD (Inv_ShortcutSound's strengthening).
Ind8 == \A s \in Writers : (w[s].derived = seq /\ w[s].sStage = "none") =>
          \A p \in Paths : (Held(s, p) /\ doc[p] # w[s].baseline[p]) => p \in w[s].skipped

\* I9: A READER'S MEMO.
Ind9 == \A s \in Readers :
          /\ w[s].memo <= seq /\ w[s].rnow <= seq
          /\ (w[s].pc = "idle" /\ w[s].derived # 0) => w[s].memo <= w[s].derived

\* I10: the records never run ahead of the pointer.
Ind10 == \A s \in Writers : w[s].derived <= seq /\ w[s].synced <= seq

\* I11: THE RETIRE AGE.
Ind11a == (retiring \cup aged) \cap CitedSet = {}
Ind11b == ~rlag => \A p \in Paths : rdoc[p] # Nil =>
                      rdoc[p] \in live /\ (Cited(rdoc[p]) \/ rdoc[p] \in retiring)
Ind11 == Ind11a /\ Ind11b
==============================================================================
