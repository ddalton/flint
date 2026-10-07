----------------------------- MODULE MCLeanP1M2 -----------------------------
(***************************************************************************)
(* M2's inductive strengthening for Inv_CitationsLive and Inv_OneName      *)
(* (docs/plans/lean-tlaps-inductive-proof-plan.md: I2-I5), as the proof    *)
(* needs it, stated over LeanP1.tla UNCHANGED and TLC-checked as INVARIANTs *)
(* before any becomes a TLAPS lemma.  Read off the actions                 *)
(* (results/2026-10-07-tlaps-m2/NOTES.txt).                                *)
(*                                                                         *)
(* The thread: a handle that was never PUT (not in `upped`) is PRIVATE to  *)
(* the writer that minted it, at its own path (M2Private); an upload's     *)
(* snapshot before its PUT is such a handle or an already-PUT one -- the   *)
(* copy case (M2Pending); from its PUT to the CAS an upload is uncited,    *)
(* unretired, in no save in flight and private (M2Uploaded, I4); verified, *)
(* it is live until the CAS (M2Verified, I5).  A save in flight is live,   *)
(* uncited, unretired, at its own path, in no tree or snapshot (M2Flight,  *)
(* I3).  Freshness (M2Fresh, I2) is what makes a mint new.                 *)
(*                                                                         *)
(* Controls: M2UpsCtl (I4's citation clause with "cased" admitted: the     *)
(* Install cites the upload) and M2PrivCtl (privacy without the "never     *)
(* PUT" condition: two writers hold the document's handle).                *)
(***************************************************************************)
EXTENDS LeanP1

\* In no other tree, in no other snapshot.
Priv(s, h) == \A t \in Writers \ {s}, q \in Paths : w[t].local[q] # h /\ w[t].snap[q] # h

\* I2: freshness.
M2Fresh ==
  /\ live \subseteq upped /\ upped \subseteq minted
  /\ (retiring \cup aged) \subseteq upped
  /\ nextGen <= MaxMint + 1
  /\ \A h \in minted : Gen(h) < nextGen \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies)
\* ...and a snapshot holds minted handles (doc, gw, local: the proof's Minted).
M2SnapMinted == \A s \in Writers, p \in Paths : w[s].snap[p] \in Opt(minted)

\* I3: a save in flight.
M2Flight == \A p \in Paths : gw[p] # Nil =>
              /\ gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p] \notin retiring \cup aged
              /\ gw[p][1] = p
              /\ \A s \in Writers, q \in Paths : w[s].local[q] # gw[p] /\ w[s].snap[q] # gw[p]

\* A tree entry never PUT is its writer's own, at its own path.
M2Private == \A s \in Writers, p \in Paths :
               (w[s].local[p] # Nil /\ w[s].local[p] \notin upped)
                 => w[s].local[p][1] = p /\ Priv(s, w[s].local[p])

\* An upload before its PUT: a never-PUT handle (private, at its path) or an
\* already-PUT one (it will be copied).
M2Pending == \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
               \A p \in w[s].uploads \ w[s].upDone :
                 /\ w[s].snap[p] # Nil
                 /\ (w[s].snap[p] \notin upped => w[s].snap[p][1] = p /\ Priv(s, w[s].snap[p]))

\* I4: an upload from its PUT to the CAS.
M2Uploaded == \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
                \A p \in w[s].upDone :
                  /\ w[s].snap[p] # Nil /\ w[s].snap[p][1] = p /\ w[s].snap[p] \in upped
                  /\ ~Cited(w[s].snap[p]) /\ w[s].snap[p] \notin retiring \cup aged
                  /\ \A q \in Paths : gw[q] # w[s].snap[p]
                  /\ Priv(s, w[s].snap[p])

\* I5: a verified upload is live until the CAS.
M2Verified == \A s \in Writers : (w[s].pc = "claimed" /\ w[s].verified) =>
                \A p \in (w[s].uploads \cap w[s].upDone) \ w[s].gone : w[s].snap[p] \in live
\* A scanned writer has not verified yet (Claim keeps `verified`).
M2Unverified == \A s \in Writers : w[s].pc = "scanned" => ~w[s].verified

M2All == M2Fresh /\ M2SnapMinted /\ M2Flight /\ M2Private /\ M2Pending /\ M2Uploaded
         /\ M2Verified /\ M2Unverified /\ Inv_CitationsLive /\ Inv_OneName

\* THE CONTROLS.
M2UpsCtl == \A s \in Writers : w[s].pc \in {"scanned", "claimed", "cased"} =>
              \A p \in w[s].upDone : ~Cited(w[s].snap[p])
M2PrivCtl == \A s \in Writers, p \in Paths : w[s].local[p] # Nil => Priv(s, w[s].local[p])
==============================================================================
