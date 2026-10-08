---------------------------- MODULE MCLeanP1M5Anc ----------------------------
(***************************************************************************)
(* M5's history conjuncts, over LeanP1Anc.tla (the copy the proof EXTENDS,  *)
(* which carries the ghost `anc`).  Every M5 claim reads Derives or Content *)
(* across a step; these say the step cannot move either between handles     *)
(* already minted: `base`, `anc` and `orig` are written only at the handle  *)
(* the step mints, which was not minted before.                             *)
(***************************************************************************)
EXTENDS LeanP1Anc

\* A handle not yet minted has no history.
HistNew == \A h \in Handles \ minted : base[h] = Nil /\ anc[h] = {} /\ orig[h] = Nil
\* `anc` is what `base` reaches, one level down.
HistAnc == \A h \in Handles : anc[h] = AncOf(base[h])
\* A minted handle's history is minted; an original is no copy.
HistMinted == \A h \in minted :
                /\ base[h] \in Opt(minted) /\ anc[h] \subseteq minted
                /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
\* What a tree's baseline names is minted.
BaselineMinted == \A s \in Writers, p \in Paths : w[s].baseline[p] \in Opt(minted)
Hist == HistNew /\ HistAnc /\ HistMinted /\ BaselineMinted

\* E2 restated over the ghost (Back reads Derives).
M5NoBack == \A s \in Writers, p \in Paths : ~Back(s, p)
\* E4: M5NoBack across paths -- the form a rename needs (it moves doc[q] to
\* p, where a tree may hold a baseline).  A baseline at p never derives from
\* a different-content version published at ANY path q, unless a conflict
\* names the baseline at p.
BackAt(s, p, q) == /\ doc[q] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[q])
                   /\ Content(doc[q]) # Content(w[s].baseline[p])
                   /\ <<p, w[s].baseline[p]>> \notin conflicts
M5NoBackX == \A s \in Writers, p, q \in Paths : ~BackAt(s, p, q)
\* E4: a save in flight has no kin: nothing else minted derives from it, and
\* no tree's baseline is it.
FlightNoKin == \A p \in Paths : gw[p] # Nil =>
                 /\ \A k \in minted \ {gw[p]} : ~Derives(k, gw[p])
                 /\ \A s \in Writers, q \in Paths : w[s].baseline[q] # gw[p]
\* E4b: M5NoBackX fails (a rename forks p1's version to p2, the writer then
\* re-creates p1 over it: its baseline at p1 derives from doc[p2], harmless).
\* A rename moves doc[p] only onto an EMPTY path q, so the form it needs is
\* the cross-path claim for a baseline at a path the document leaves empty.
M5NoBackY == \A s \in Writers, p, q \in Paths : doc[p] = Nil => ~BackAt(s, p, q)
==============================================================================
