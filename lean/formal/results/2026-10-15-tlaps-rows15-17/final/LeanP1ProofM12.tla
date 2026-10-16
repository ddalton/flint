------------------------------ MODULE LeanP1ProofM12 ------------------------------
(* M12 (row 15): AK1's side invariant -- what M11's Reap and Install steps
   assume, proved on its own.  Generated: results/2026-10-15-tlaps-rows15-17/
   gen-m12.py (its docstring has the argument).
   NOT YET TLC-CHECKED with two writers: GoneUnconf and the nine helpers.   *)
EXTENDS LeanP1ProofM11

A2Bar == {"scanned", "claimed", "cased"}
A2Lost == UNION {w[t].lost : t \in Writers}
\* A lost handle is not live: M11's A1LostDead.
\* A withheld snap is dead and was PUT.
A2GoneDead == \A s \in Writers, p \in Paths : p \in w[s].gone => w[s].snap[p] \notin live
A2GoneUpped == \A s \in Writers, p \in Paths : p \in w[s].gone => w[s].snap[p] \in upped
\* A finished upload's snap was PUT.
A2UpDoneUpped == \A s \in Writers, p \in Paths : p \in w[s].upDone => w[s].snap[p] \in upped
\* Lost handles were PUT (so minted).  (A2Lost \subseteq upped, per writer.)
A2LostUpped == \A t \in Writers : w[t].lost \subseteq upped /\ w[t].lost \subseteq minted
\* From the claim, every upload is done or withheld.
A2UpsDone == \A s \in Writers : w[s].pc \in {"claimed", "cased"} => w[s].uploads \subseteq w[s].upDone \cup w[s].gone
\* A barrier's delete has a baseline (it was dirty at the scan).
A2DelBase == \A s \in Writers, p \in Paths : w[s].pc \in A2Bar /\ p \in w[s].deletes => w[s].baseline[p] # Nil
\* Before the install no conflict names a withheld snap.
A2GoneUnconf == \A s \in Writers, p \in Paths :
                  w[s].pc = "claimed" /\ p \in w[s].gone => \A c \in conflicts : c[2] # w[s].snap[p]
\* Helpers.
A2GoneBar == \A s \in Writers : w[s].pc \notin {"claimed", "cased"} => w[s].gone = {}
A2GoneSub == \A s \in Writers : w[s].pc = "claimed" => w[s].gone \subseteq w[s].upDone
A2ConfUpped == \A c \in conflicts : c[2] \in upped \cup {Nil}
A2TombUpped == \A q \in Paths : tomb[q] \in upped \cup {Nil}
A2BaseUpped == \A t \in Writers, q \in Paths : w[t].baseline[q] \in upped \cup {Nil}
A2Up(s, p) == w[s].pc \in {"scanned", "claimed"} /\ p \in w[s].upDone
A2UpUnconf == \A s \in Writers, p \in Paths : A2Up(s, p) => \A c \in conflicts : c[2] # w[s].snap[p]
A2UpNotTomb == \A s \in Writers, p \in Paths : A2Up(s, p) => \A q \in Paths : tomb[q] # w[s].snap[p]
A2UpNotBase == \A s \in Writers, p \in Paths : A2Up(s, p) =>
                 \A t \in Writers, q \in Paths : w[t].baseline[q] # w[s].snap[p]
A2Side == /\ A1LostDead /\ A2GoneDead /\ A2GoneUpped /\ A2UpDoneUpped /\ A2LostUpped
          /\ A2UpsDone /\ A2DelBase /\ A2GoneUnconf
          /\ A2GoneBar /\ A2GoneSub /\ A2ConfUpped /\ A2TombUpped /\ A2BaseUpped
          /\ A2UpUnconf /\ A2UpNotTomb /\ A2UpNotBase

\* The fields A2Side reads in a tree.
A2SameNP(R, W) ==
  /\ R.lost = W.lost /\ R.gone = W.gone /\ R.snap = W.snap /\ R.upDone = W.upDone
  /\ R.uploads = W.uploads /\ R.deletes = W.deletes /\ R.baseline = W.baseline
A2Same(R, W) == A2SameNP(R, W) /\ R.pc = W.pc
\* A tombstone the step writes is kept, cleared, or a cited handle.
A2TombOK == \A q \in Paths : \/ tomb'[q] = tomb[q] \/ tomb'[q] = Nil
                             \/ tomb'[q] # Nil /\ \E r \in Paths : tomb'[q] = doc[r]
\* A baseline the step writes is kept, cleared, a cited handle, or this
\* tree's own PUT snap.
A2BaseOK(R, W) == \A q \in Paths : \/ R.baseline[q] = W.baseline[q] \/ R.baseline[q] = Nil
                                   \/ R.baseline[q] # Nil /\ \E r \in Paths : R.baseline[q] = doc[r]
                                   \/ q \in W.upDone /\ R.baseline[q] = W.snap[q]

------------------------------------------------------------------------------
(* What IndC gives (M2: Fresh, Uploaded, CitationsLive; M1: Mine).          *)

LEMMA A2Ind ==
  ASSUME IndC
  PROVE  /\ TypeOK
         /\ live \subseteq upped /\ upped \subseteq minted
         /\ \A q \in Paths : doc[q] # Nil => doc[q] \in live
         /\ \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
              \A p \in w[s].upDone : /\ w[s].snap[p] # Nil /\ w[s].snap[p] \in upped
                                     /\ ~Cited(w[s].snap[p]) /\ Priv(s, w[s].snap[p])
         /\ \A s \in Writers : w[s].gone \subseteq w[s].uploads
         /\ mv = Nil
         /\ \A s \in Writers, p \in Paths : p \in w[s].uploads => w[s].snap[p] \in minted
<1>1. TypeOK /\ mv = Nil BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>2. live \subseteq upped /\ upped \subseteq minted BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
<1>3. \A q \in Paths : doc[q] # Nil => doc[q] \in live BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Inv_CitationsLive
<1>4. \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
         \A p \in w[s].upDone : /\ w[s].snap[p] # Nil /\ w[s].snap[p] \in upped
                                /\ ~Cited(w[s].snap[p]) /\ Priv(s, w[s].snap[p])
  BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Uploaded, Up
<1>5. \A s \in Writers : w[s].gone \subseteq w[s].uploads BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Mine
<1>6. \A s \in Writers, p \in Paths : p \in w[s].uploads => w[s].snap[p] \in minted
  <2>. SUFFICES ASSUME NEW s \in Writers, NEW p \in Paths, p \in w[s].uploads PROVE w[s].snap[p] \in minted OBVIOUS
  <2>. QED BY TreeTy DEF IndC
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6

\* GoneUnconf from UpUnconf and GoneSub.
LEMMA A2GU == A2UpUnconf /\ A2GoneSub => A2GoneUnconf
BY DEF A2UpUnconf, A2GoneSub, A2GoneUnconf, A2Up
LEMMA A2GUP == A2UpUnconf' /\ A2GoneSub' => A2GoneUnconf'
BY DEF A2UpUnconf, A2GoneSub, A2GoneUnconf, A2Up

------------------------------------------------------------------------------
(* A step that keeps every field A2Side reads in every tree.                *)

LEMMA A2Keep ==
  ASSUME IndC, A2Side,
         \A u \in Writers : A2Same(w'[u], w[u]),
         \A h \in live' : h \in live \/ h \notin upped,
         upped \subseteq upped', minted \subseteq minted',
         conflicts' = conflicts, A2TombOK
  PROVE  A2Side'
<1>i. /\ live \subseteq upped /\ upped \subseteq minted
      /\ \A q \in Paths : doc[q] # Nil => doc[q] \in live
      /\ \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
           \A p \in w[s].upDone : /\ w[s].snap[p] # Nil /\ w[s].snap[p] \in upped
                                  /\ ~Cited(w[s].snap[p]) /\ Priv(s, w[s].snap[p])
  BY A2Ind
<1>w. \A u \in Writers : A2Same(w'[u], w[u]) OBVIOUS
<1>1. A1LostDead'
  <2>. SUFFICES ASSUME NEW t \in Writers, NEW h \in w'[t].lost, h \in live' PROVE FALSE BY DEF A1LostDead
  <2>1. h \in w[t].lost BY <1>w DEF A2Same, A2SameNP
  <2>2. h \in upped BY <2>1 DEF A2Side, A2LostUpped
  <2>3. h \in live BY <2>2
  <2>. QED BY <2>1, <2>3 DEF A2Side, A1LostDead
<1>2. A2GoneDead'
  <2>. SUFFICES ASSUME NEW s \in Writers, NEW p \in Paths, p \in w'[s].gone, w'[s].snap[p] \in live' PROVE FALSE
    BY DEF A2GoneDead
  <2>1. p \in w[s].gone /\ w'[s].snap[p] = w[s].snap[p] BY <1>w DEF A2Same, A2SameNP
  <2>2. w[s].snap[p] \in upped BY <2>1 DEF A2Side, A2GoneUpped
  <2>. QED BY <2>1, <2>2 DEF A2Side, A2GoneDead
<1>3. A2GoneUpped' /\ A2UpDoneUpped' /\ A2LostUpped'
  BY <1>w DEF A2Side, A2GoneUpped, A2UpDoneUpped, A2LostUpped, A2Same, A2SameNP
<1>4. A2UpsDone' /\ A2DelBase' /\ A2GoneBar' /\ A2GoneSub'
  BY <1>w DEF A2Side, A2UpsDone, A2DelBase, A2GoneBar, A2GoneSub, A2Same, A2SameNP
<1>5. A2ConfUpped' BY DEF A2Side, A2ConfUpped
<1>6. A2TombUpped'
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE tomb'[q] \in upped' \cup {Nil} BY DEF A2TombUpped
  <2>. QED BY <1>i DEF A2TombOK, A2Side, A2TombUpped
<1>7. A2BaseUpped' BY <1>w DEF A2Side, A2BaseUpped, A2Same, A2SameNP
<1>8. A2UpUnconf' BY <1>w DEF A2Side, A2UpUnconf, A2Up, A2Same, A2SameNP
<1>9. A2UpNotTomb'
  <2>. SUFFICES ASSUME NEW s \in Writers, NEW p \in Paths, A2Up(s, p)', NEW q \in Paths
                PROVE  tomb'[q] # w'[s].snap[p]
    BY DEF A2UpNotTomb
  <2>1. A2Up(s, p) /\ w'[s].snap[p] = w[s].snap[p] BY <1>w DEF A2Up, A2Same, A2SameNP
  <2>2. w[s].snap[p] # Nil /\ ~Cited(w[s].snap[p]) BY <2>1, <1>i DEF A2Up
  <2>. QED BY <2>1, <2>2 DEF A2TombOK, A2Side, A2UpNotTomb, Cited
<1>10. A2UpNotBase' BY <1>w DEF A2Side, A2UpNotBase, A2Up, A2Same, A2SameNP
<1>11. A2GoneUnconf' BY <1>8, <1>4, A2GUP
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11 DEF A2Side

\* One tree written, A2Same.
LEMMA A2Keep1 ==
  ASSUME IndC, A2Side, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R], A2Same(R, w[s]),
         \A h \in live' : h \in live \/ h \notin upped,
         upped \subseteq upped', minted \subseteq minted',
         conflicts' = conflicts, A2TombOK
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>1. \A u \in Writers : A2Same(w'[u], w[u]) BY <1>0 DEF TypeOK, A2Same, A2SameNP
<1>. QED BY <1>1, A2Keep

\* No tree written.
LEMMA A2Quiet ==
  ASSUME IndC, A2Side, w' = w,
         \A h \in live' : h \in live \/ h \notin upped,
         upped \subseteq upped', minted \subseteq minted',
         conflicts' = conflicts, A2TombOK
  PROVE  A2Side'
<1>1. \A u \in Writers : A2Same(w'[u], w[u]) BY DEF A2Same, A2SameNP
<1>. QED BY <1>1, A2Keep

------------------------------------------------------------------------------
(* A step that leaves (or stays out of) the barrier: one tree written, its   *)
(* pc outside scanned..cased, nothing withheld, lost kept; the upDone set   *)
(* cleared or kept with its snaps; every baseline it writes A2BaseOK.       *)

LEMMA A2Out ==
  ASSUME IndC, A2Side, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         R.pc \notin A2Bar, R.gone = {}, R.lost = w[s].lost,
         R.upDone = {} \/ (R.upDone = w[s].upDone /\ R.snap = w[s].snap),
         A2BaseOK(R, w[s]),
         live' = live, upped \subseteq upped', minted \subseteq minted',
         conflicts' = conflicts, tomb' = tomb
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>i. /\ live \subseteq upped /\ upped \subseteq minted
      /\ \A q \in Paths : doc[q] # Nil => doc[q] \in live
      /\ \A u \in Writers : w[u].pc \in {"scanned", "claimed"} =>
           \A p \in w[u].upDone : /\ w[u].snap[p] # Nil /\ w[u].snap[p] \in upped
                                  /\ ~Cited(w[u].snap[p]) /\ Priv(u, w[u].snap[p])
  BY A2Ind
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>o. \A u \in Writers \ {s} : w'[u] = w[u] BY <1>w
<1>s. w'[s] = R BY <1>w
<1>l. \A u \in Writers : w'[u].lost = w[u].lost BY <1>w
<1>1. A1LostDead' BY <1>l DEF A2Side, A1LostDead
<1>2. A2GoneDead' /\ A2GoneUpped' BY <1>o, <1>s DEF A2Side, A2GoneDead, A2GoneUpped
<1>3. A2UpDoneUpped'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths, p \in w'[u].upDone PROVE w'[u].snap[p] \in upped'
    BY DEF A2UpDoneUpped
  <2>1. CASE u = s BY <2>1, <1>s DEF A2Side, A2UpDoneUpped
  <2>2. CASE u # s BY <2>2, <1>o DEF A2Side, A2UpDoneUpped
  <2>. QED BY <2>1, <2>2
<1>4. A2LostUpped' BY <1>l DEF A2Side, A2LostUpped
<1>5. A2UpsDone' /\ A2DelBase' /\ A2GoneBar' /\ A2GoneSub'
  BY <1>o, <1>s DEF A2Side, A2UpsDone, A2DelBase, A2GoneBar, A2GoneSub, A2Bar
<1>6. A2ConfUpped' /\ A2TombUpped' BY DEF A2Side, A2ConfUpped, A2TombUpped
<1>7. A2BaseUpped'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths PROVE w'[u].baseline[q] \in upped' \cup {Nil}
    BY DEF A2BaseUpped
  <2>1. CASE u # s BY <2>1, <1>o DEF A2Side, A2BaseUpped
  <2>2. CASE u = s
    <3>1. CASE R.baseline[q] = w[s].baseline[q] BY <3>1, <2>2, <1>s DEF A2Side, A2BaseUpped
    <3>2. CASE R.baseline[q] = Nil BY <3>2, <2>2, <1>s
    <3>3. CASE R.baseline[q] # Nil /\ \E r \in Paths : R.baseline[q] = doc[r] BY <3>3, <2>2, <1>s, <1>i
    <3>4. CASE q \in w[s].upDone /\ R.baseline[q] = w[s].snap[q] BY <3>4, <2>2, <1>s DEF A2Side, A2UpDoneUpped
    <3>. QED BY <3>1, <3>2, <3>3, <3>4 DEF A2BaseOK
  <2>. QED BY <2>1, <2>2
\* A tree in the barrier before the CAS is not s (R's pc is outside it).
<1>u. \A u \in Writers, p \in Paths : A2Up(u, p)' => u # s /\ A2Up(u, p) /\ w'[u] = w[u]
  BY <1>o, <1>s DEF A2Up, A2Bar
<1>8. A2UpUnconf' /\ A2UpNotTomb' BY <1>u DEF A2Side, A2UpUnconf, A2UpNotTomb
<1>9. A2UpNotBase'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths, A2Up(u, p)', NEW t \in Writers, NEW q \in Paths
                PROVE  w'[t].baseline[q] # w'[u].snap[p]
    BY DEF A2UpNotBase
  <2>0. u # s /\ A2Up(u, p) /\ w'[u] = w[u] BY <1>u
  <2>h. w[u].snap[p] # Nil /\ ~Cited(w[u].snap[p]) /\ Priv(u, w[u].snap[p]) BY <2>0, <1>i DEF A2Up
  <2>1. CASE t # s BY <2>1, <2>0, <1>o DEF A2Side, A2UpNotBase
  <2>2. CASE t = s
    <3>1. CASE R.baseline[q] = w[s].baseline[q] BY <3>1, <2>2, <2>0, <1>s DEF A2Side, A2UpNotBase
    <3>2. CASE R.baseline[q] = Nil BY <3>2, <2>2, <2>0, <2>h, <1>s
    <3>3. CASE R.baseline[q] # Nil /\ \E r \in Paths : R.baseline[q] = doc[r] BY <3>3, <2>2, <2>0, <2>h, <1>s DEF Cited
    <3>4. CASE q \in w[s].upDone /\ R.baseline[q] = w[s].snap[q] BY <3>4, <2>2, <2>0, <2>h, <1>s DEF Priv
    <3>. QED BY <3>1, <3>2, <3>3, <3>4 DEF A2BaseOK
  <2>. QED BY <2>1, <2>2
<1>10. A2GoneUnconf' BY <1>8, <1>5, A2GUP
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF A2Side

------------------------------------------------------------------------------
(* A tree moves from scanned to claimed with every upload done (Claim, and   *)
(* Depose's taker); every other field A2Side reads is kept everywhere.      *)

LEMMA A2ToClaimed ==
  ASSUME IndC, A2Side, NEW t \in Writers, w[t].pc = "scanned", w[t].uploads \subseteq w[t].upDone,
         \A u \in Writers : A2SameNP(w'[u], w[u]),
         w'[t].pc = "claimed", \A u \in Writers \ {t} : w'[u].pc = w[u].pc,
         live' = live, upped \subseteq upped', minted \subseteq minted',
         conflicts' = conflicts, tomb' = tomb
  PROVE  A2Side'
<1>w. \A u \in Writers : A2SameNP(w'[u], w[u]) OBVIOUS
<1>g. w[t].gone = {} BY DEF A2Side, A2GoneBar
<1>p. \A u \in Writers : (w'[u].pc \in {"scanned", "claimed"} <=> w[u].pc \in {"scanned", "claimed"})
                         /\ (w'[u].pc \in A2Bar <=> w[u].pc \in A2Bar)
  BY DEF A2Bar
<1>1. A1LostDead' /\ A2GoneDead' /\ A2GoneUpped' /\ A2UpDoneUpped' /\ A2LostUpped'
  BY <1>w DEF A2Side, A1LostDead, A2GoneDead, A2GoneUpped, A2UpDoneUpped, A2LostUpped, A2SameNP
<1>2. A2UpsDone'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc \in {"claimed", "cased"}
                PROVE  w'[u].uploads \subseteq w'[u].upDone \cup w'[u].gone
    BY DEF A2UpsDone
  <2>1. CASE u = t BY <2>1, <1>w DEF A2SameNP
  <2>2. CASE u # t BY <2>2, <1>w DEF A2Side, A2UpsDone, A2SameNP
  <2>. QED BY <2>1, <2>2
<1>3. A2DelBase' BY <1>w, <1>p DEF A2Side, A2DelBase, A2SameNP
<1>4. A2GoneBar' /\ A2GoneSub'
  <2>1. A2GoneBar' BY <1>w DEF A2Side, A2GoneBar, A2SameNP
  <2>2. A2GoneSub'
    <3>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "claimed" PROVE w'[u].gone \subseteq w'[u].upDone
      BY DEF A2GoneSub
    <3>1. CASE u = t BY <3>1, <1>g, <1>w DEF A2SameNP
    <3>2. CASE u # t BY <3>2, <1>w DEF A2Side, A2GoneSub, A2SameNP
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2
<1>5. A2ConfUpped' /\ A2TombUpped' /\ A2BaseUpped'
  BY <1>w DEF A2Side, A2ConfUpped, A2TombUpped, A2BaseUpped, A2SameNP
<1>6. A2UpUnconf' /\ A2UpNotTomb' /\ A2UpNotBase'
  BY <1>w, <1>p DEF A2Side, A2UpUnconf, A2UpNotTomb, A2UpNotBase, A2Up, A2SameNP
<1>7. A2GoneUnconf' BY <1>6, <1>4, A2GUP
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7 DEF A2Side

------------------------------------------------------------------------------
(* The gateway: no tree; a GPut's handle is fresh (not minted, so not upped). *)

LEMMA GPut_A2 ==
  ASSUME IndC, IndC', A2Side, NEW p \in Paths, GPut(p), Frame
  PROVE  A2Side'
<1>. DEFINE h == <<p, nextGen>>
<1>1. /\ live' = live \cup {h} /\ minted' = minted \cup {h} /\ upped' = upped \cup {h}
      /\ nextGen <= MaxMint /\ UNCHANGED <<w, conflicts, tomb>>
  BY DEF GPut
<1>2. nextGen \in Nat /\ nextGen > Seed BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>3. h \notin minted
  <2>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>. QED BY <2>1, <1>1, <1>2, MaxMintNat DEF Gen
<1>4. upped \subseteq minted BY A2Ind
<1>5. \A x \in live' : x \in live \/ x \notin upped BY <1>1, <1>3, <1>4
<1>6. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>5, <1>6, A2Quiet

LEMMA GCas_A2 ==
  ASSUME IndC, IndC', A2Side, NEW p \in Paths, GCas(p), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>1. UNCHANGED <<live, minted, conflicts, w, upped>> BY DEF GCas, aux
<1>2. tomb' = tomb \/ tomb' = [tomb EXCEPT ![p] = Nil] BY DEF GCas
<1>3. A2TombOK BY <1>0, <1>2 DEF A2TombOK, TypeOK
<1>. QED BY <1>1, <1>3, A2Quiet

LEMMA GDelete_A2 ==
  ASSUME IndC, IndC', A2Side, NEW p \in Paths, GDelete(p), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>1. UNCHANGED <<live, minted, conflicts, w, upped>> BY DEF GDelete, aux
<1>2. tomb' = [tomb EXCEPT ![p] = doc[p]] /\ doc[p] # Nil BY DEF GDelete
<1>3. A2TombOK BY <1>0, <1>2 DEF A2TombOK, TypeOK
<1>. QED BY <1>1, <1>3, A2Quiet

LEMMA GRename_A2 ==
  ASSUME IndC, IndC', A2Side, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>r. RenameAtomic BY ShippedShape DEF Shipped
<1>1. UNCHANGED <<live, minted, conflicts, w, upped>> BY DEF GRename, aux
<1>2. tomb' = [tomb EXCEPT ![q] = Nil, ![p] = doc[p]] /\ doc[p] # Nil BY <1>r DEF GRename
<1>3. A2TombOK BY <1>0, <1>2 DEF A2TombOK, TypeOK
<1>. QED BY <1>1, <1>3, A2Quiet

\* The rename's second CAS needs mv # Nil; with RenameAtomic mv stays Nil.
LEMMA GRenameFinish_A2 ==
  ASSUME IndC, IndC', A2Side, GRenameFinish, Frame
  PROVE  A2Side'
<1>1. mv = Nil BY A2Ind
<1>. QED BY <1>1 DEF GRenameFinish

LEMMA Sweep_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  A2Side'
<1>1. /\ w' = w /\ live' = live \ {h} /\ UNCHANGED <<minted, conflicts, tomb, upped>> BY DEF Sweep, aux
<1>2. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, A2Quiet

\* Age, RLoad, Reap: no tree, no conflict, no tombstone; live kept or shrunk.
LEMMA Quiet_A2 ==
  ASSUME IndC, A2Side, w' = w, live' \subseteq live, UNCHANGED <<minted, conflicts, tomb, upped>>
  PROVE  A2Side'
<1>1. A2TombOK BY DEF A2TombOK
<1>. QED BY <1>1, A2Quiet

LEMMA Age_A2 == ASSUME IndC, A2Side, Age PROVE A2Side'
<1>1. w' = w /\ live' = live /\ UNCHANGED <<minted, conflicts, tomb, upped>> BY DEF Age, aux
<1>. QED BY <1>1, Quiet_A2

LEMMA RLoad_A2 == ASSUME IndC, A2Side, RLoad PROVE A2Side'
<1>1. w' = w /\ live' = live /\ UNCHANGED <<minted, conflicts, tomb, upped>> BY DEF RLoad, aux
<1>. QED BY <1>1, Quiet_A2

LEMMA Reap_A2 == ASSUME IndC, A2Side, NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE A2Side'
<1>1. w' = w /\ live' = live \ {h} /\ UNCHANGED <<minted, conflicts, tomb, upped>> BY DEF Reap, aux
<1>. QED BY <1>1, Quiet_A2

------------------------------------------------------------------------------
(* Writer steps that keep every field A2Side reads.                         *)

LEMMA Touch_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Touch(s), Frame
  PROVE  A2Side'
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY DEF Touch, bucket, aux
<1>2. A2Same(TouchW(s), w[s])
  <2>1. TouchW(s).lost = w[s].lost BY DEF TouchW
  <2>2. TouchW(s).gone = w[s].gone BY DEF TouchW
  <2>3. TouchW(s).snap = w[s].snap BY DEF TouchW
  <2>4. TouchW(s).upDone = w[s].upDone BY DEF TouchW
  <2>5. TouchW(s).uploads = w[s].uploads BY DEF TouchW
  <2>6. TouchW(s).deletes = w[s].deletes BY DEF TouchW
  <2>7. TouchW(s).baseline = w[s].baseline BY DEF TouchW
  <2>8. TouchW(s).pc = w[s].pc BY DEF TouchW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

LEMMA Honor_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Honor(s), Frame
  PROVE  A2Side'
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY DEF Honor, bucket, aux
<1>2. A2Same(HonorW(s), w[s])
  <2>1. HonorW(s).lost = w[s].lost BY DEF HonorW
  <2>2. HonorW(s).gone = w[s].gone BY DEF HonorW
  <2>3. HonorW(s).snap = w[s].snap BY DEF HonorW
  <2>4. HonorW(s).upDone = w[s].upDone BY DEF HonorW
  <2>5. HonorW(s).uploads = w[s].uploads BY DEF HonorW
  <2>6. HonorW(s).deletes = w[s].deletes BY DEF HonorW
  <2>7. HonorW(s).baseline = w[s].baseline BY DEF HonorW
  <2>8. HonorW(s).pc = w[s].pc BY DEF HonorW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

LEMMA HonorReader_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, HonorReader(s), Frame
  PROVE  A2Side'
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY DEF HonorReader, bucket, aux
<1>2. A2Same(HonorReaderW(s), w[s])
  <2>1. HonorReaderW(s).lost = w[s].lost BY DEF HonorReaderW
  <2>2. HonorReaderW(s).gone = w[s].gone BY DEF HonorReaderW
  <2>3. HonorReaderW(s).snap = w[s].snap BY DEF HonorReaderW
  <2>4. HonorReaderW(s).upDone = w[s].upDone BY DEF HonorReaderW
  <2>5. HonorReaderW(s).uploads = w[s].uploads BY DEF HonorReaderW
  <2>6. HonorReaderW(s).deletes = w[s].deletes BY DEF HonorReaderW
  <2>7. HonorReaderW(s).baseline = w[s].baseline BY DEF HonorReaderW
  <2>8. HonorReaderW(s).pc = w[s].pc BY DEF HonorReaderW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

LEMMA RetireByAck_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  A2Side'
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY DEF RetireByAck, bucket, aux
<1>2. A2Same(RetireByAckW(s), w[s])
  <2>1. RetireByAckW(s).lost = w[s].lost BY DEF RetireByAckW
  <2>2. RetireByAckW(s).gone = w[s].gone BY DEF RetireByAckW
  <2>3. RetireByAckW(s).snap = w[s].snap BY DEF RetireByAckW
  <2>4. RetireByAckW(s).upDone = w[s].upDone BY DEF RetireByAckW
  <2>5. RetireByAckW(s).uploads = w[s].uploads BY DEF RetireByAckW
  <2>6. RetireByAckW(s).deletes = w[s].deletes BY DEF RetireByAckW
  <2>7. RetireByAckW(s).baseline = w[s].baseline BY DEF RetireByAckW
  <2>8. RetireByAckW(s).pc = w[s].pc BY DEF RetireByAckW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

LEMMA RescopeBegin_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  A2Side'
<1>0. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>1. w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY <1>0 DEF RescopeBegin, bucket
<1>2. A2Same(RescopeBeginW(s, T), w[s])
  <2>1. RescopeBeginW(s, T).lost = w[s].lost BY DEF RescopeBeginW
  <2>2. RescopeBeginW(s, T).gone = w[s].gone BY DEF RescopeBeginW
  <2>3. RescopeBeginW(s, T).snap = w[s].snap BY DEF RescopeBeginW
  <2>4. RescopeBeginW(s, T).upDone = w[s].upDone BY DEF RescopeBeginW
  <2>5. RescopeBeginW(s, T).uploads = w[s].uploads BY DEF RescopeBeginW
  <2>6. RescopeBeginW(s, T).deletes = w[s].deletes BY DEF RescopeBeginW
  <2>7. RescopeBeginW(s, T).baseline = w[s].baseline BY DEF RescopeBeginW
  <2>8. RescopeBeginW(s, T).pc = w[s].pc BY DEF RescopeBeginW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

LEMMA Collect_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Collect(s), Frame
  PROVE  A2Side'
<1>r. RetireAge BY ShippedShape DEF Shipped
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY <1>r DEF Collect, aux
<1>2. A2Same(CollectW(s), w[s])
  <2>1. CollectW(s).lost = w[s].lost BY DEF CollectW
  <2>2. CollectW(s).gone = w[s].gone BY DEF CollectW
  <2>3. CollectW(s).snap = w[s].snap BY DEF CollectW
  <2>4. CollectW(s).upDone = w[s].upDone BY DEF CollectW
  <2>5. CollectW(s).uploads = w[s].uploads BY DEF CollectW
  <2>6. CollectW(s).deletes = w[s].deletes BY DEF CollectW
  <2>7. CollectW(s).baseline = w[s].baseline BY DEF CollectW
  <2>8. CollectW(s).pc = w[s].pc BY DEF CollectW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

LEMMA Delete_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  A2Side'
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY DEF Delete, bucket, aux
<1>2. A2Same(DeleteW(s, p), w[s])
  <2>1. DeleteW(s, p).lost = w[s].lost BY DEF DeleteW
  <2>2. DeleteW(s, p).gone = w[s].gone BY DEF DeleteW
  <2>3. DeleteW(s, p).snap = w[s].snap BY DEF DeleteW
  <2>4. DeleteW(s, p).upDone = w[s].upDone BY DEF DeleteW
  <2>5. DeleteW(s, p).uploads = w[s].uploads BY DEF DeleteW
  <2>6. DeleteW(s, p).deletes = w[s].deletes BY DEF DeleteW
  <2>7. DeleteW(s, p).baseline = w[s].baseline BY DEF DeleteW
  <2>8. DeleteW(s, p).pc = w[s].pc BY DEF DeleteW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

LEMMA Edit_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  A2Side'
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] /\ UNCHANGED <<live, conflicts, tomb, upped>> /\ minted \subseteq minted'
  BY DEF Edit, aux
<1>2. A2Same(EditW(s, p), w[s])
  <2>1. EditW(s, p).lost = w[s].lost BY DEF EditW
  <2>2. EditW(s, p).gone = w[s].gone BY DEF EditW
  <2>3. EditW(s, p).snap = w[s].snap BY DEF EditW
  <2>4. EditW(s, p).upDone = w[s].upDone BY DEF EditW
  <2>5. EditW(s, p).uploads = w[s].uploads BY DEF EditW
  <2>6. EditW(s, p).deletes = w[s].deletes BY DEF EditW
  <2>7. EditW(s, p).baseline = w[s].baseline BY DEF EditW
  <2>8. EditW(s, p).pc = w[s].pc BY DEF EditW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>3. A2TombOK BY <1>1 DEF A2TombOK
<1>. QED BY <1>1, <1>2, <1>3, A2Keep1

------------------------------------------------------------------------------
(* Writer steps outside the barrier, and the barrier's ends (A2Out).        *)

LEMMA Checkout_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Checkout(s), Frame
  PROVE  A2Side'
<1>i. w[s] = WriterInit BY DEF Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
<1>T. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>w. w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY <1>T
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Checkout, bucket, aux
<1>a. CheckoutW(s, T).pc \notin A2Bar BY <1>i DEF CheckoutW, WriterInit, A2Bar
<1>b. CheckoutW(s, T).gone = {} BY <1>i DEF CheckoutW, WriterInit
<1>c. CheckoutW(s, T).lost = w[s].lost BY DEF CheckoutW
<1>d. CheckoutW(s, T).upDone = {} \/ (CheckoutW(s, T).upDone = w[s].upDone /\ CheckoutW(s, T).snap = w[s].snap) BY DEF CheckoutW
<1>e. A2BaseOK(CheckoutW(s, T), w[s]) BY DEF A2BaseOK, CheckoutW, CheckoutHeld
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA Consume_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Consume(s), Frame
  PROVE  A2Side'
<1>g. w[s].pc = "idle" BY DEF Consume
<1>z. w[s].gone = {} BY <1>g DEF A2Side, A2GoneBar
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Consume
<1>1. PICK ck \in CarryClocks(s) :
        \/ CheapPath(s) /\ w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
        \/ ~CheapPath(s) /\ \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>2. CASE CheapPath(s) /\ w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
  <2>a. ConsumeCheapW(s, ck).pc \notin A2Bar BY DEF ConsumeCheapW, A2Bar
  <2>b. ConsumeCheapW(s, ck).gone = {} BY <1>z DEF ConsumeCheapW
  <2>c. ConsumeCheapW(s, ck).lost = w[s].lost BY DEF ConsumeCheapW
  <2>d. ConsumeCheapW(s, ck).upDone = w[s].upDone /\ ConsumeCheapW(s, ck).snap = w[s].snap BY DEF ConsumeCheapW
  <2>e. A2BaseOK(ConsumeCheapW(s, ck), w[s]) BY DEF A2BaseOK, ConsumeCheapW
  <2>. QED BY <1>2, <1>u, <2>a, <2>b, <2>c, <2>d, <2>e, A2Out
<1>3. CASE ~CheapPath(s) /\ \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>3
  <2>a. ConsumeW(s, fail, ck).pc \notin A2Bar BY DEF ConsumeW, A2Bar
  <2>b. ConsumeW(s, fail, ck).gone = {} BY <1>z DEF ConsumeW
  <2>c. ConsumeW(s, fail, ck).lost = w[s].lost BY DEF ConsumeW
  <2>d. ConsumeW(s, fail, ck).upDone = w[s].upDone /\ ConsumeW(s, fail, ck).snap = w[s].snap BY DEF ConsumeW
  <2>e. A2BaseOK(ConsumeW(s, fail, ck), w[s]) BY DEF A2BaseOK, ConsumeW
  <2>. QED BY <2>0, <1>u, <2>a, <2>b, <2>c, <2>d, <2>e, A2Out
<1>. QED BY <1>1, <1>2, <1>3

LEMMA Skip_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Skip(s), Frame
  PROVE  A2Side'
<1>g. w[s].pc = "consumed" BY DEF Skip
<1>z. w[s].gone = {} BY <1>g DEF A2Side, A2GoneBar
<1>w. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Skip, bucket, aux
<1>a. SkipW(s).pc \notin A2Bar BY <1>g DEF SkipW, A2Bar
<1>b. SkipW(s).gone = {} BY <1>z DEF SkipW
<1>c. SkipW(s).lost = w[s].lost BY DEF SkipW
<1>d. SkipW(s).upDone = {} \/ (SkipW(s).upDone = w[s].upDone /\ SkipW(s).snap = w[s].snap) BY DEF SkipW
<1>e. A2BaseOK(SkipW(s), w[s]) BY DEF A2BaseOK, SkipW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA RPullRead_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, RPullRead(s), Frame
  PROVE  A2Side'
<1>g. w[s].pc = "idle" BY DEF RPullRead
<1>z. w[s].gone = {} BY <1>g DEF A2Side, A2GoneBar
<1>w. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF RPullRead, bucket, aux
<1>a. RPullReadW(s).pc \notin A2Bar BY <1>g DEF RPullReadW, A2Bar
<1>b. RPullReadW(s).gone = {} BY <1>z DEF RPullReadW
<1>c. RPullReadW(s).lost = w[s].lost BY DEF RPullReadW
<1>d. RPullReadW(s).upDone = {} \/ (RPullReadW(s).upDone = w[s].upDone /\ RPullReadW(s).snap = w[s].snap) BY DEF RPullReadW
<1>e. A2BaseOK(RPullReadW(s), w[s]) BY DEF A2BaseOK, RPullReadW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA Sync_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Sync(s), Frame
  PROVE  A2Side'
<1>f. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>g. w[s].pc = "idle" BY DEF Sync
<1>z. w[s].gone = {} BY <1>g DEF A2Side, A2GoneBar
<1>w. w' = [w EXCEPT ![s] = SyncW(s, fail)] BY <1>f
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Sync, bucket
<1>a. SyncW(s, fail).pc \notin A2Bar BY <1>g DEF SyncW, A2Bar
<1>b. SyncW(s, fail).gone = {} BY <1>z DEF SyncW
<1>c. SyncW(s, fail).lost = w[s].lost BY DEF SyncW
<1>d. SyncW(s, fail).upDone = {} \/ (SyncW(s, fail).upDone = w[s].upDone /\ SyncW(s, fail).snap = w[s].snap) BY DEF SyncW
<1>e. A2BaseOK(SyncW(s, fail), w[s]) BY DEF A2BaseOK, SyncBl, SyncW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA RPullSync_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, RPullSync(s), Frame
  PROVE  A2Side'
<1>f. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>g. w[s].pc = "pulling" BY DEF RPullSync
<1>z. w[s].gone = {} BY <1>g DEF A2Side, A2GoneBar
<1>w. w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY <1>f
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF RPullSync, bucket
<1>a. RPullSyncW(s, fail).pc \notin A2Bar BY <1>g DEF RPullSyncW, A2Bar
<1>b. RPullSyncW(s, fail).gone = {} BY <1>z DEF RPullSyncW
<1>c. RPullSyncW(s, fail).lost = w[s].lost BY DEF RPullSyncW
<1>d. RPullSyncW(s, fail).upDone = {} \/ (RPullSyncW(s, fail).upDone = w[s].upDone /\ RPullSyncW(s, fail).snap = w[s].snap) BY DEF RPullSyncW
<1>e. A2BaseOK(RPullSyncW(s, fail), w[s]) BY DEF A2BaseOK, SyncBl, RPullSyncW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA RescopeFirst_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  A2Side'
<1>g. w[s].pc = "idle" BY DEF RescopeFirst
<1>z. w[s].gone = {} BY <1>g DEF A2Side, A2GoneBar
<1>w. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF RescopeFirst, bucket, aux
<1>a. RescopeFirstW(s).pc \notin A2Bar BY <1>g DEF RescopeFirstW, A2Bar
<1>b. RescopeFirstW(s).gone = {} BY <1>z DEF RescopeFirstW
<1>c. RescopeFirstW(s).lost = w[s].lost BY DEF RescopeFirstW
<1>d. RescopeFirstW(s).upDone = {} \/ (RescopeFirstW(s).upDone = w[s].upDone /\ RescopeFirstW(s).snap = w[s].snap) BY DEF RescopeFirstW
<1>e. A2BaseOK(RescopeFirstW(s), w[s]) BY DEF A2BaseOK, RescopeFirstW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA RescopeSecond_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  A2Side'
<1>f. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>g. w[s].pc = "idle" BY DEF RescopeSecond
<1>z. w[s].gone = {} BY <1>g DEF A2Side, A2GoneBar
<1>w. w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] BY <1>f
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF RescopeSecond, bucket
<1>a. RescopeSecondW(s, wfail).pc \notin A2Bar BY <1>g DEF RescopeSecondW, A2Bar
<1>b. RescopeSecondW(s, wfail).gone = {} BY <1>z DEF RescopeSecondW
<1>c. RescopeSecondW(s, wfail).lost = w[s].lost BY DEF RescopeSecondW
<1>d. RescopeSecondW(s, wfail).upDone = {} \/ (RescopeSecondW(s, wfail).upDone = w[s].upDone /\ RescopeSecondW(s, wfail).snap = w[s].snap) BY DEF RescopeSecondW
<1>e. A2BaseOK(RescopeSecondW(s, wfail), w[s]) BY DEF A2BaseOK, RescopeBase1, RescopeSecondW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA Finish_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Finish(s), Frame
  PROVE  A2Side'
<1>w. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Finish, aux
<1>a. FinishW(s).pc \notin A2Bar BY DEF FinishW, A2Bar
<1>b. FinishW(s).gone = {} BY DEF FinishW
<1>c. FinishW(s).lost = w[s].lost BY DEF FinishW
<1>d. FinishW(s).upDone = {} \/ (FinishW(s).upDone = w[s].upDone /\ FinishW(s).snap = w[s].snap) BY DEF FinishW
<1>e. A2BaseOK(FinishW(s), w[s]) BY DEF A2BaseOK, FinishW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA Restart_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Restart(s), Frame
  PROVE  A2Side'
<1>w. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Restart
<1>a. RestartW(s).pc \notin A2Bar BY DEF RestartW, A2Bar
<1>b. RestartW(s).gone = {} BY DEF RestartW
<1>c. RestartW(s).lost = w[s].lost BY DEF RestartW
<1>d. RestartW(s).upDone = {} \/ (RestartW(s).upDone = w[s].upDone /\ RestartW(s).snap = w[s].snap) BY DEF RestartW
<1>e. A2BaseOK(RestartW(s), w[s]) BY DEF A2BaseOK, RestartW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

LEMMA PullOnly_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, PullOnly(s), Frame
  PROVE  A2Side'
<1>w. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF PullOnly, bucket, aux
<1>a. PullOnlyW(s).pc \notin A2Bar BY DEF PullOnlyW, A2Bar
<1>b. PullOnlyW(s).gone = {} BY DEF PullOnlyW
<1>c. PullOnlyW(s).lost = w[s].lost BY DEF PullOnlyW
<1>d. PullOnlyW(s).upDone = {} \/ (PullOnlyW(s).upDone = w[s].upDone /\ PullOnlyW(s).snap = w[s].snap) BY DEF PullOnlyW
<1>e. A2BaseOK(PullOnlyW(s), w[s]) BY DEF A2BaseOK, PullOnlyW
<1>. QED BY <1>w, <1>u, <1>a, <1>b, <1>c, <1>d, <1>e, A2Out

------------------------------------------------------------------------------
(* The scan: a fresh barrier, nothing PUT or withheld; a delete is an       *)
(* absent dirty path, so its baseline is a handle.                         *)

LEMMA Scan_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Scan(s), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>d. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) :
        w' = [w EXCEPT ![s] = ScanW(s, dels)]
  BY DEF Scan
<1>e. dels \subseteq ScanAbsent(s) BY <1>d
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Scan, bucket, aux
<1>w. \A u \in Writers : w'[u] = IF u = s THEN ScanW(s, dels) ELSE w[u] BY <1>0, <1>d DEF TypeOK
<1>o. \A u \in Writers \ {s} : w'[u] = w[u] BY <1>w
<1>f. /\ ScanW(s, dels).pc = "scanned" /\ ScanW(s, dels).deletes = dels
      /\ ScanW(s, dels).upDone = {} /\ ScanW(s, dels).gone = {}
      /\ ScanW(s, dels).lost = w[s].lost /\ ScanW(s, dels).baseline = w[s].baseline
  BY DEF ScanW
<1>s. w'[s] = ScanW(s, dels) BY <1>w
<1>1. A1LostDead' /\ A2GoneDead' /\ A2GoneUpped' /\ A2UpDoneUpped' /\ A2LostUpped'
  BY <1>o, <1>s, <1>f, <1>u DEF A2Side, A1LostDead, A2GoneDead, A2GoneUpped, A2UpDoneUpped, A2LostUpped
<1>2. A2UpsDone' /\ A2GoneBar' /\ A2GoneSub' BY <1>o, <1>s, <1>f DEF A2Side, A2UpsDone, A2GoneBar, A2GoneSub
<1>3. A2DelBase'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths, w'[u].pc \in A2Bar, p \in w'[u].deletes
                PROVE  w'[u].baseline[p] # Nil
    BY DEF A2DelBase
  <2>1. CASE u = s BY <2>1, <1>s, <1>f, <1>e DEF ScanAbsent, ScanDirty
  <2>2. CASE u # s BY <2>2, <1>o DEF A2Side, A2DelBase
  <2>. QED BY <2>1, <2>2
<1>4. A2ConfUpped' /\ A2TombUpped' BY <1>u DEF A2Side, A2ConfUpped, A2TombUpped
<1>5. A2BaseUpped' BY <1>o, <1>s, <1>f, <1>u DEF A2Side, A2BaseUpped
<1>6. A2UpUnconf' /\ A2UpNotTomb' /\ A2UpNotBase'
  <2>1. \A u \in Writers, p \in Paths : A2Up(u, p)' => u # s /\ A2Up(u, p) /\ w'[u] = w[u]
    BY <1>o, <1>s, <1>f DEF A2Up
  <2>2. \A t \in Writers, q \in Paths : w'[t].baseline[q] = w[t].baseline[q] BY <1>o, <1>s, <1>f
  <2>. QED BY <2>1, <2>2, <1>u DEF A2Side, A2UpUnconf, A2UpNotTomb, A2UpNotBase
<1>7. A2GoneUnconf' BY <1>6, <1>2, A2GUP
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7 DEF A2Side

------------------------------------------------------------------------------
(* The claim and the deposal: scanned to claimed, every upload done.       *)

LEMMA Claim_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Claim(s), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>1. /\ w' = [w EXCEPT ![s] = ClaimW(s)] /\ w[s].pc = "scanned" /\ w[s].uploads \subseteq w[s].upDone
      /\ UNCHANGED <<live, minted, conflicts, tomb, upped>>
  BY DEF Claim, aux
<1>2. A2SameNP(ClaimW(s), w[s])
  <2>1. ClaimW(s).lost = w[s].lost BY DEF ClaimW
  <2>2. ClaimW(s).gone = w[s].gone BY DEF ClaimW
  <2>3. ClaimW(s).snap = w[s].snap BY DEF ClaimW
  <2>4. ClaimW(s).upDone = w[s].upDone BY DEF ClaimW
  <2>5. ClaimW(s).uploads = w[s].uploads BY DEF ClaimW
  <2>6. ClaimW(s).deletes = w[s].deletes BY DEF ClaimW
  <2>7. ClaimW(s).baseline = w[s].baseline BY DEF ClaimW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7 DEF A2SameNP
<1>3. ClaimW(s).pc = "claimed" BY DEF ClaimW
<1>w. \A u \in Writers : w'[u] = IF u = s THEN ClaimW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>4. \A u \in Writers : A2SameNP(w'[u], w[u]) BY <1>w, <1>2 DEF A2SameNP
<1>5. w'[s].pc = "claimed" /\ \A u \in Writers \ {s} : w'[u].pc = w[u].pc BY <1>w, <1>3
<1>. QED BY <1>1, <1>4, <1>5, A2ToClaimed

LEMMA Depose_A2 ==
  ASSUME IndC, IndC', A2Side, NEW t \in Writers, Depose(t), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>1. /\ w[t].pc = "scanned" /\ w[t].uploads \subseteq w[t].upDone /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<live, minted, conflicts, tomb, upped>>
  BY DEF Depose, aux
<1>w. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>2. A2SameNP(DeposeW(t), w[t])
  <2>1. DeposeW(t).lost = w[t].lost BY DEF DeposeW
  <2>2. DeposeW(t).gone = w[t].gone BY DEF DeposeW
  <2>3. DeposeW(t).snap = w[t].snap BY DEF DeposeW
  <2>4. DeposeW(t).upDone = w[t].upDone BY DEF DeposeW
  <2>5. DeposeW(t).uploads = w[t].uploads BY DEF DeposeW
  <2>6. DeposeW(t).deletes = w[t].deletes BY DEF DeposeW
  <2>7. DeposeW(t).baseline = w[t].baseline BY DEF DeposeW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7 DEF A2SameNP
<1>3. A2Same(DeposedW(holder), w[holder])
  <2>1. DeposedW(holder).lost = w[holder].lost BY DEF DeposedW
  <2>2. DeposedW(holder).gone = w[holder].gone BY DEF DeposedW
  <2>3. DeposedW(holder).snap = w[holder].snap BY DEF DeposedW
  <2>4. DeposedW(holder).upDone = w[holder].upDone BY DEF DeposedW
  <2>5. DeposedW(holder).uploads = w[holder].uploads BY DEF DeposedW
  <2>6. DeposedW(holder).deletes = w[holder].deletes BY DEF DeposedW
  <2>7. DeposedW(holder).baseline = w[holder].baseline BY DEF DeposedW
  <2>8. DeposedW(holder).pc = w[holder].pc BY DEF DeposedW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8 DEF A2Same, A2SameNP
<1>4. DeposeW(t).pc = "claimed" BY DEF DeposeW
<1>5. \A u \in Writers : A2SameNP(w'[u], w[u]) BY <1>w, <1>2, <1>3 DEF A2Same, A2SameNP
<1>6. w'[t].pc = "claimed" /\ \A u \in Writers \ {t} : w'[u].pc = w[u].pc BY <1>w, <1>3, <1>4 DEF A2Same
<1>. QED BY <1>1, <1>5, <1>6, A2ToClaimed

------------------------------------------------------------------------------
(* The verify: what it withholds is PUT and dead, and inside upDone.       *)

LEMMA Verify_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Verify(s), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>v. CommitVerifiesUploads BY ShippedShape DEF Shipped
<1>1. /\ w' = [w EXCEPT ![s] = VerifyW(s)] /\ w[s].pc = "claimed"
      /\ UNCHANGED <<live, minted, conflicts, tomb, upped>>
  BY DEF Verify, bucket, aux
<1>2. /\ VerifyW(s).lost = w[s].lost /\ VerifyW(s).snap = w[s].snap /\ VerifyW(s).upDone = w[s].upDone
      /\ VerifyW(s).uploads = w[s].uploads /\ VerifyW(s).deletes = w[s].deletes
      /\ VerifyW(s).baseline = w[s].baseline /\ VerifyW(s).pc = w[s].pc
  <2>1. VerifyW(s).lost = w[s].lost BY DEF VerifyW
  <2>2. VerifyW(s).snap = w[s].snap BY DEF VerifyW
  <2>3. VerifyW(s).upDone = w[s].upDone BY DEF VerifyW
  <2>4. VerifyW(s).uploads = w[s].uploads BY DEF VerifyW
  <2>5. VerifyW(s).deletes = w[s].deletes BY DEF VerifyW
  <2>6. VerifyW(s).baseline = w[s].baseline BY DEF VerifyW
  <2>7. VerifyW(s).pc = w[s].pc BY DEF VerifyW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7
<1>3. VerifyW(s).gone = {p \in w[s].uploads \cap w[s].upDone : w[s].snap[p] \notin live} BY <1>v DEF VerifyW
<1>w. \A u \in Writers : w'[u] = IF u = s THEN VerifyW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>o. \A u \in Writers \ {s} : w'[u] = w[u] BY <1>w
<1>s. w'[s] = VerifyW(s) BY <1>w
<1>g. w[s].uploads \subseteq w[s].upDone
  <2>1. w[s].uploads \subseteq w[s].upDone \cup w[s].gone BY <1>1 DEF A2Side, A2UpsDone
  <2>2. w[s].gone \subseteq w[s].upDone BY <1>1 DEF A2Side, A2GoneSub
  <2>. QED BY <2>1, <2>2
<1>4. A1LostDead' /\ A2LostUpped' /\ A2UpDoneUpped'
  BY <1>o, <1>s, <1>2, <1>1 DEF A2Side, A1LostDead, A2LostUpped, A2UpDoneUpped
<1>5. A2GoneDead' /\ A2GoneUpped'
  <2>1. \A p \in Paths : p \in w'[s].gone => w'[s].snap[p] \notin live' /\ w'[s].snap[p] \in upped'
    BY <1>s, <1>2, <1>3, <1>1 DEF A2Side, A2UpDoneUpped
  <2>. QED BY <2>1, <1>o, <1>1 DEF A2Side, A2GoneDead, A2GoneUpped
<1>6. A2UpsDone' /\ A2GoneSub' /\ A2GoneBar' /\ A2DelBase'
  <2>1. w'[s].uploads \subseteq w'[s].upDone \cup w'[s].gone /\ w'[s].gone \subseteq w'[s].upDone
    BY <1>s, <1>2, <1>3, <1>g
  <2>. QED BY <2>1, <1>o, <1>s, <1>2, <1>1 DEF A2Side, A2UpsDone, A2GoneSub, A2GoneBar, A2DelBase
<1>7. A2ConfUpped' /\ A2TombUpped' /\ A2BaseUpped' BY <1>o, <1>s, <1>2, <1>1 DEF A2Side, A2ConfUpped, A2TombUpped, A2BaseUpped
<1>8. A2UpUnconf' /\ A2UpNotTomb' /\ A2UpNotBase'
  <2>1. \A u \in Writers : A2SameNP(w'[u], w[u]) \/ (u = s /\ w'[u].snap = w[u].snap /\ w'[u].upDone = w[u].upDone
                                                     /\ w'[u].baseline = w[u].baseline)
    BY <1>o, <1>s, <1>2 DEF A2SameNP
  <2>2. \A u \in Writers : w'[u].pc = w[u].pc /\ w'[u].snap = w[u].snap /\ w'[u].upDone = w[u].upDone
                           /\ w'[u].baseline = w[u].baseline
    BY <1>o, <1>s, <1>2
  <2>. QED BY <2>2, <1>1 DEF A2Side, A2UpUnconf, A2UpNotTomb, A2UpNotBase, A2Up
<1>9. A2GoneUnconf' BY <1>8, <1>6, A2GUP
<1>. QED BY <1>4, <1>5, <1>6, <1>7, <1>8, <1>9 DEF A2Side

------------------------------------------------------------------------------
(* The install: the withheld snaps become conflicts and lost (dead, PUT);    *)
(* every other conflict and tombstone it writes names a cited handle, a    *)
(* tombstone or this tree's baseline -- none is another barrier's PUT snap *)
(* (UpNotTomb, UpNotBase, Uploaded's Priv and ~Cited).                     *)

LEMMA Install_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Install(s), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>i. /\ live \subseteq upped /\ upped \subseteq minted
      /\ \A q \in Paths : doc[q] # Nil => doc[q] \in live
      /\ \A u \in Writers : w[u].pc \in {"scanned", "claimed"} =>
           \A p \in w[u].upDone : /\ w[u].snap[p] # Nil /\ w[u].snap[p] \in upped
                                  /\ ~Cited(w[u].snap[p]) /\ Priv(u, w[u].snap[p])
  BY A2Ind
<1>1. /\ w[s].pc = "claimed"
      /\ w' = [w EXCEPT ![s] = InstallW(s)]
      /\ UNCHANGED <<live, minted, upped>>
      /\ conflicts' = conflicts \cup {<<p, w[s].snap[p]>> : p \in w[s].gone}
                                \cup {<<p, doc[p]>> : p \in InstallContested(s)}
                                \cup {<<p, DeletedAt(s, p)>> : p \in InstallDelOverridden(s)}
                                \cup {<<p, doc[p]>> : p \in InstallDelOver(s)}
      /\ tomb' = [p \in Paths |-> IF InstallInst(s)[p] # Nil THEN Nil
                                  ELSE IF doc[p] # Nil THEN doc[p]
                                  ELSE tomb[p]]
  BY DEF Install, aux
\* What a new conflict names.
<1>gp. w[s].gone \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
  <2>1. w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths BY TreeTy DEF IndC
  <2>. QED BY <2>1, A2Ind
<1>n. \A c \in conflicts' \ conflicts :
        \/ \E p \in w[s].gone : c[2] = w[s].snap[p]
        \/ c[2] # Nil /\ \E r \in Paths : c[2] = doc[r]
        \/ \E p \in Paths : c[2] = DeletedAt(s, p)
  <2>. SUFFICES ASSUME NEW c \in conflicts' \ conflicts
                PROVE  \/ \E p \in w[s].gone : c[2] = w[s].snap[p]
                       \/ c[2] # Nil /\ \E r \in Paths : c[2] = doc[r]
                       \/ \E p \in Paths : c[2] = DeletedAt(s, p) OBVIOUS
  <2>ps. InstallContested(s) \cup InstallDelOver(s) \cup InstallDelOverridden(s) \subseteq Paths
    BY <1>gp DEF InstallContested, InstallDelOver, InstallDelOverridden, InstallMine
  <2>1. CASE c \in {<<p, w[s].snap[p]>> : p \in w[s].gone} BY <2>1
  <2>2. CASE c \in {<<p, doc[p]>> : p \in InstallContested(s)} \cup {<<p, doc[p]>> : p \in InstallDelOver(s)}
    <3>1. PICK p \in InstallContested(s) \cup InstallDelOver(s) : c = <<p, doc[p]>> BY <2>2
    <3>2. p \in Paths /\ doc[p] # Nil BY <3>1, <2>ps DEF InstallContested, InstallDelOver
    <3>. QED BY <3>1, <3>2
  <2>3. CASE c \in {<<p, DeletedAt(s, p)>> : p \in InstallDelOverridden(s)}
    <3>1. PICK p \in InstallDelOverridden(s) : c = <<p, DeletedAt(s, p)>> BY <2>3
    <3>. QED BY <3>1, <2>ps
  <2>. QED BY <1>1, <2>1, <2>2, <2>3
<1>t. \A q \in Paths : \/ tomb'[q] = tomb[q] \/ tomb'[q] = Nil
                       \/ tomb'[q] # Nil /\ \E r \in Paths : tomb'[q] = doc[r]
  BY <1>1
<1>2. /\ InstallW(s).gone = w[s].gone /\ InstallW(s).snap = w[s].snap /\ InstallW(s).uploads = w[s].uploads
      /\ InstallW(s).deletes = w[s].deletes /\ InstallW(s).baseline = w[s].baseline
  <2>1. InstallW(s).gone = w[s].gone BY DEF InstallW
  <2>2. InstallW(s).snap = w[s].snap BY DEF InstallW
  <2>3. InstallW(s).uploads = w[s].uploads BY DEF InstallW
  <2>4. InstallW(s).deletes = w[s].deletes BY DEF InstallW
  <2>5. InstallW(s).baseline = w[s].baseline BY DEF InstallW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5
<1>3. /\ InstallW(s).pc = "cased" /\ InstallW(s).upDone = w[s].upDone \ w[s].gone
      /\ InstallW(s).lost = IF MaxTouches > 0 THEN w[s].lost \cup {w[s].snap[p] : p \in w[s].gone} ELSE w[s].lost
  BY DEF InstallW
<1>w. \A u \in Writers : w'[u] = IF u = s THEN InstallW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>o. \A u \in Writers \ {s} : w'[u] = w[u] BY <1>w
<1>s. w'[s] = InstallW(s) BY <1>w
<1>gs. \A p \in w[s].gone : w[s].snap[p] \notin live /\ w[s].snap[p] \in upped BY <1>gp DEF A2Side, A2GoneDead, A2GoneUpped
<1>4. A1LostDead'
  <2>. SUFFICES ASSUME NEW t \in Writers, NEW h \in w'[t].lost, h \in live' PROVE FALSE BY DEF A1LostDead
  <2>1. CASE t = s BY <2>1, <1>s, <1>3, <1>gs, <1>1 DEF A2Side, A1LostDead
  <2>2. CASE t # s BY <2>2, <1>o, <1>1 DEF A2Side, A1LostDead
  <2>. QED BY <2>1, <2>2
<1>5. A2LostUpped' BY <1>o, <1>s, <1>3, <1>gs, <1>i, <1>1 DEF A2Side, A2LostUpped
<1>6. A2GoneDead' /\ A2GoneUpped' /\ A2UpDoneUpped'
  BY <1>o, <1>s, <1>2, <1>3, <1>1 DEF A2Side, A2GoneDead, A2GoneUpped, A2UpDoneUpped
<1>7. A2UpsDone' /\ A2DelBase' /\ A2GoneBar' /\ A2GoneSub'
  <2>1. w'[s].uploads \subseteq w'[s].upDone \cup w'[s].gone BY <1>s, <1>2, <1>3, <1>1 DEF A2Side, A2UpsDone
  <2>2. \A p \in w'[s].deletes : w'[s].baseline[p] # Nil BY <1>s, <1>2, <1>1, <1>gp DEF A2Side, A2DelBase, A2Bar
  <2>. QED BY <2>1, <2>2, <1>o, <1>s, <1>3 DEF A2Side, A2UpsDone, A2DelBase, A2GoneBar, A2GoneSub, A2Bar
<1>8. A2ConfUpped'
  <2>. SUFFICES ASSUME NEW c \in conflicts' PROVE c[2] \in upped' \cup {Nil} BY DEF A2ConfUpped
  <2>1. CASE c \in conflicts BY <2>1, <1>1 DEF A2Side, A2ConfUpped
  <2>2. CASE c \notin conflicts
    <3>1. CASE \E p \in w[s].gone : c[2] = w[s].snap[p] BY <3>1, <1>gs, <1>1
    <3>2. CASE c[2] # Nil /\ \E r \in Paths : c[2] = doc[r] BY <3>2, <1>i, <1>1
    <3>3. CASE \E p \in Paths : c[2] = DeletedAt(s, p) BY <3>3, <1>1 DEF DeletedAt, A2Side, A2TombUpped, A2BaseUpped
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <1>n
  <2>. QED BY <2>1, <2>2
<1>9. A2TombUpped'
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE tomb'[q] \in upped' \cup {Nil} BY DEF A2TombUpped
  <2>. QED BY <1>t, <1>i, <1>1 DEF A2Side, A2TombUpped
<1>10. A2BaseUpped' BY <1>o, <1>s, <1>2, <1>1 DEF A2Side, A2BaseUpped
\* A tree in the barrier before the CAS is not s (s is now cased).
<1>u. \A u \in Writers, p \in Paths : A2Up(u, p)' => u # s /\ A2Up(u, p) /\ w'[u] = w[u]
  BY <1>o, <1>s, <1>3 DEF A2Up
<1>11. A2UpUnconf'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths, A2Up(u, p)', NEW c \in conflicts'
                PROVE  c[2] # w'[u].snap[p]
    BY DEF A2UpUnconf
  <2>0. u # s /\ A2Up(u, p) /\ w'[u] = w[u] BY <1>u
  <2>h. w[u].snap[p] # Nil /\ ~Cited(w[u].snap[p]) /\ Priv(u, w[u].snap[p]) BY <2>0, <1>i DEF A2Up
  <2>1. CASE c \in conflicts BY <2>1, <2>0 DEF A2Side, A2UpUnconf
  <2>2. CASE c \notin conflicts
    <3>1. CASE \E q \in w[s].gone : c[2] = w[s].snap[q] BY <3>1, <2>0, <2>h, <1>gp DEF Priv
    <3>2. CASE c[2] # Nil /\ \E r \in Paths : c[2] = doc[r] BY <3>2, <2>0, <2>h DEF Cited
    <3>3. CASE \E q \in Paths : c[2] = DeletedAt(s, q) BY <3>3, <2>0 DEF DeletedAt, A2Side, A2UpNotTomb, A2UpNotBase
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <1>n
  <2>. QED BY <2>1, <2>2
<1>12. A2UpNotTomb'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths, A2Up(u, p)', NEW q \in Paths
                PROVE  tomb'[q] # w'[u].snap[p]
    BY DEF A2UpNotTomb
  <2>0. u # s /\ A2Up(u, p) /\ w'[u] = w[u] BY <1>u
  <2>h. w[u].snap[p] # Nil /\ ~Cited(w[u].snap[p]) BY <2>0, <1>i DEF A2Up
  <2>. QED BY <2>0, <2>h, <1>t DEF A2Side, A2UpNotTomb, Cited
<1>13. A2UpNotBase' BY <1>u, <1>o, <1>s, <1>2 DEF A2Side, A2UpNotBase
<1>14. A2GoneUnconf' BY <1>11, <1>7, A2GUP
<1>. QED BY <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13, <1>14 DEF A2Side

------------------------------------------------------------------------------
(* The upload: a first PUT names a handle not upped; a copy a fresh one.    *)
(* Either is named by no conflict, tombstone or baseline (all upped).      *)

\* The shared argument: tree s at scanned gets p into upDone with snap x, a
\* handle not upped before; x joins live and upped.
LEMMA A2Put ==
  ASSUME IndC, A2Side, NEW s \in Writers, NEW p \in Paths, NEW x, NEW R,
         w[s].pc = "scanned", x \notin upped, x # Nil,
         w' = [w EXCEPT ![s] = R],
         R.lost = w[s].lost, R.gone = w[s].gone, R.uploads = w[s].uploads, R.deletes = w[s].deletes,
         R.baseline = w[s].baseline, R.pc = w[s].pc,
         R.upDone = w[s].upDone \cup {p}, R.snap = [w[s].snap EXCEPT ![p] = x],
         live' = live \cup {x}, upped' = upped \cup {x}, minted \subseteq minted',
         conflicts' = conflicts, tomb' = tomb
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>ws. w[s] \in Writer /\ w[s].snap \in [Paths -> Opt(Handles)] BY <1>0, WriterFields DEF TypeOK
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>o. \A u \in Writers \ {s} : w'[u] = w[u] BY <1>w
<1>s. w'[s] = R BY <1>w
<1>sn. \A q \in Paths : R.snap[q] = IF q = p THEN x ELSE w[s].snap[q] BY <1>ws
<1>g. w[s].gone = {} BY DEF A2Side, A2GoneBar
<1>lu. \A t \in Writers : w[t].lost \subseteq upped BY DEF A2Side, A2LostUpped
<1>gu. \A t \in Writers, q \in Paths : q \in w[t].gone => w[t].snap[q] \in upped BY DEF A2Side, A2GoneUpped
<1>1. A1LostDead'
  <2>. SUFFICES ASSUME NEW t \in Writers, NEW h \in w'[t].lost, h \in live' PROVE FALSE BY DEF A1LostDead
  <2>1. h \in w[t].lost BY <1>o, <1>s
  <2>2. h # x BY <2>1, <1>lu
  <2>. QED BY <2>1, <2>2 DEF A2Side, A1LostDead
<1>2. A2GoneDead' /\ A2GoneUpped'
  <2>1. \A t \in Writers, q \in Paths : q \in w'[t].gone => t # s /\ w'[t] = w[t] BY <1>o, <1>s, <1>g
  <2>. QED BY <2>1, <1>gu DEF A2Side, A2GoneDead, A2GoneUpped
<1>3. A2UpDoneUpped'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths, q \in w'[u].upDone PROVE w'[u].snap[q] \in upped'
    BY DEF A2UpDoneUpped
  <2>1. CASE u = s /\ q = p BY <2>1, <1>s, <1>sn
  <2>2. CASE u = s /\ q # p BY <2>2, <1>s, <1>sn DEF A2Side, A2UpDoneUpped
  <2>3. CASE u # s BY <2>3, <1>o DEF A2Side, A2UpDoneUpped
  <2>. QED BY <2>1, <2>2, <2>3
<1>4. A2LostUpped' BY <1>o, <1>s DEF A2Side, A2LostUpped
<1>5. A2UpsDone' /\ A2DelBase' /\ A2GoneBar' /\ A2GoneSub'
  BY <1>o, <1>s, <1>g DEF A2Side, A2UpsDone, A2DelBase, A2GoneBar, A2GoneSub
<1>6. A2ConfUpped' /\ A2TombUpped' /\ A2BaseUpped' BY <1>o, <1>s DEF A2Side, A2ConfUpped, A2TombUpped, A2BaseUpped
\* x is named by no conflict, tombstone or baseline: those are upped (or Nil).
<1>x. /\ \A c \in conflicts : c[2] # x
      /\ \A q \in Paths : tomb[q] # x
      /\ \A t \in Writers, q \in Paths : w[t].baseline[q] # x
  BY DEF A2Side, A2ConfUpped, A2TombUpped, A2BaseUpped
<1>7. A2UpUnconf' /\ A2UpNotTomb' /\ A2UpNotBase'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths, A2Up(u, q)'
                PROVE  /\ \A c \in conflicts' : c[2] # w'[u].snap[q]
                       /\ \A r \in Paths : tomb'[r] # w'[u].snap[q]
                       /\ \A t \in Writers, r \in Paths : w'[t].baseline[r] # w'[u].snap[q]
    BY DEF A2UpUnconf, A2UpNotTomb, A2UpNotBase
  <2>b. \A t \in Writers, r \in Paths : w'[t].baseline[r] = w[t].baseline[r] BY <1>o, <1>s
  <2>1. CASE u = s /\ q = p BY <2>1, <2>b, <1>s, <1>sn, <1>x
  <2>2. CASE u = s /\ q # p
    <3>1. A2Up(s, q) /\ w'[s].snap[q] = w[s].snap[q] BY <2>2, <1>s, <1>sn DEF A2Up
    <3>. QED BY <2>2, <3>1, <2>b DEF A2Side, A2UpUnconf, A2UpNotTomb, A2UpNotBase
  <2>3. CASE u # s
    <3>1. A2Up(u, q) /\ w'[u] = w[u] BY <2>3, <1>o DEF A2Up
    <3>. QED BY <3>1, <2>b DEF A2Side, A2UpUnconf, A2UpNotTomb, A2UpNotBase
  <2>. QED BY <2>1, <2>2, <2>3
<1>8. A2GoneUnconf' BY <1>7, <1>5, A2GUP
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8 DEF A2Side

LEMMA Upload_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  A2Side'
<1>0. TypeOK BY A2Ind
<1>k. w[s].pc = "scanned" /\ p \in w[s].uploads \ w[s].upDone BY DEF Upload
<1>y. w[s].snap[p] \in minted /\ upped \subseteq minted BY <1>k, A2Ind
<1>ws. w[s] \in Writer /\ w[s].snap \in [Paths -> Opt(Handles)] BY <1>0, WriterFields DEF TypeOK
<1>e. UNCHANGED <<conflicts, tomb>> BY DEF Upload
<1>1. CASE w[s].snap[p] \notin upped
  <2>. DEFINE h == w[s].snap[p]
  <2>1. /\ live' = live \cup {h} /\ upped' = upped \cup {h} /\ minted' = minted
        /\ w' = [w EXCEPT ![s] = UploadW(s, p)]
    BY <1>1 DEF Upload
  <2>2. /\ UploadW(s, p).lost = w[s].lost /\ UploadW(s, p).gone = w[s].gone
        /\ UploadW(s, p).snap = w[s].snap /\ UploadW(s, p).uploads = w[s].uploads
        /\ UploadW(s, p).deletes = w[s].deletes /\ UploadW(s, p).baseline = w[s].baseline
        /\ UploadW(s, p).pc = w[s].pc
  <3>1. UploadW(s, p).lost = w[s].lost BY DEF UploadW
  <3>2. UploadW(s, p).gone = w[s].gone BY DEF UploadW
  <3>3. UploadW(s, p).snap = w[s].snap BY DEF UploadW
  <3>4. UploadW(s, p).uploads = w[s].uploads BY DEF UploadW
  <3>5. UploadW(s, p).deletes = w[s].deletes BY DEF UploadW
  <3>6. UploadW(s, p).baseline = w[s].baseline BY DEF UploadW
  <3>7. UploadW(s, p).pc = w[s].pc BY DEF UploadW
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7
  <2>3. UploadW(s, p).upDone = w[s].upDone \cup {p} BY DEF UploadW
  <2>4. w[s].snap = [w[s].snap EXCEPT ![p] = h] BY <1>ws
  <2>5. h # Nil
    <3>1. minted \subseteq Handles BY MintedHandles DEF IndC
    <3>2. Nil \notin Handles BY NilHandle
    <3>. QED BY <3>1, <3>2, <1>y
  <2>. QED BY <1>k, <1>1, <2>1, <2>2, <2>3, <2>4, <2>5, <1>e, A2Put
<1>2. CASE w[s].snap[p] \in upped
  <2>g. copies \in Nat /\ copies < MaxCopies BY <1>2 DEF Upload, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
  <2>c. PICK c \in Handles : c = <<p, MaxMint + copies + 1>>
    BY <2>g, MaxMintNat, MaxCopiesNat DEF Handles, Gens, Seed
  <2>1. /\ live' = live \cup {c} /\ upped' = upped \cup {c} /\ minted' = minted \cup {c}
        /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
    BY <1>2, <2>c DEF Upload
  <2>4. c \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
      BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>2. nextGen <= MaxMint + 1 BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>3. nextGen \in Nat BY <1>0 DEF TypeOK
    <3>. QED BY <3>1, <3>2, <3>3, <2>c, <2>g, MaxMintNat DEF Gen
  <2>5. c \notin upped /\ c # Nil BY <2>4, <1>y, NilHandle
  <2>2. /\ UploadCopyW(s, p, c).lost = w[s].lost /\ UploadCopyW(s, p, c).gone = w[s].gone
        /\ UploadCopyW(s, p, c).uploads = w[s].uploads /\ UploadCopyW(s, p, c).deletes = w[s].deletes
        /\ UploadCopyW(s, p, c).baseline = w[s].baseline /\ UploadCopyW(s, p, c).pc = w[s].pc
  <3>1. UploadCopyW(s, p, c).lost = w[s].lost BY DEF UploadCopyW
  <3>2. UploadCopyW(s, p, c).gone = w[s].gone BY DEF UploadCopyW
  <3>3. UploadCopyW(s, p, c).uploads = w[s].uploads BY DEF UploadCopyW
  <3>4. UploadCopyW(s, p, c).deletes = w[s].deletes BY DEF UploadCopyW
  <3>5. UploadCopyW(s, p, c).baseline = w[s].baseline BY DEF UploadCopyW
  <3>6. UploadCopyW(s, p, c).pc = w[s].pc BY DEF UploadCopyW
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6
  <2>3. UploadCopyW(s, p, c).upDone = w[s].upDone \cup {p} /\ UploadCopyW(s, p, c).snap = [w[s].snap EXCEPT ![p] = c]
    BY DEF UploadCopyW
  <2>. QED BY <1>k, <2>1, <2>2, <2>3, <2>5, <1>e, A2Put
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The fence: before the CAS the barrier ends (A2Out); after it every field *)
(* is kept (A2Keep1).                                                      *)

LEMMA Fence_A2 ==
  ASSUME IndC, IndC', A2Side, NEW s \in Writers, Fence(s), Frame
  PROVE  A2Side'
<1>u. live' = live /\ upped \subseteq upped' /\ minted \subseteq minted' /\ conflicts' = conflicts /\ tomb' = tomb
  BY DEF Fence, bucket, aux
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>a. FenceBeforeW(s).pc \notin A2Bar BY DEF FenceBeforeW, A2Bar
  <2>b. FenceBeforeW(s).gone = {} BY DEF FenceBeforeW
  <2>c. FenceBeforeW(s).lost = w[s].lost BY DEF FenceBeforeW
  <2>d. FenceBeforeW(s).upDone = {} BY DEF FenceBeforeW
  <2>e. A2BaseOK(FenceBeforeW(s), w[s]) BY DEF A2BaseOK, FenceBeforeW
  <2>. QED BY <2>1, <1>u, <2>a, <2>b, <2>c, <2>d, <2>e, A2Out
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>2. A2Same(FenceAfterW(s), w[s])
  <3>1. FenceAfterW(s).lost = w[s].lost BY DEF FenceAfterW
  <3>2. FenceAfterW(s).gone = w[s].gone BY DEF FenceAfterW
  <3>3. FenceAfterW(s).snap = w[s].snap BY DEF FenceAfterW
  <3>4. FenceAfterW(s).upDone = w[s].upDone BY DEF FenceAfterW
  <3>5. FenceAfterW(s).uploads = w[s].uploads BY DEF FenceAfterW
  <3>6. FenceAfterW(s).deletes = w[s].deletes BY DEF FenceAfterW
  <3>7. FenceAfterW(s).baseline = w[s].baseline BY DEF FenceAfterW
  <3>8. FenceAfterW(s).pc = w[s].pc BY DEF FenceAfterW
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8 DEF A2Same, A2SameNP
  <2>3. A2TombOK BY <1>u DEF A2TombOK
  <2>. QED BY <2>1, <2>2, <2>3, <1>u, A2Keep1
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* Init, the step, the theorem.                                            *)

LEMMA Init_A2 == Init => A2Side
<1>. SUFFICES ASSUME Init PROVE A2Side OBVIOUS
<1>1. \A u \in Writers : w[u] = WriterInit BY DEF Init
\* One fact per field (M5D's Init_D: expanding WriterInit whole timed out).
<1>2. \A u \in Writers : /\ w[u].lost = {} /\ w[u].gone = {} /\ w[u].upDone = {} /\ w[u].pc = "idle"
                         /\ w[u].baseline = [p \in Paths |-> Nil]
  BY <1>1 DEF WriterInit
<1>3. conflicts = {} /\ tomb = [p \in Paths |-> Nil] BY DEF Init
<1>4. A1LostDead /\ A2GoneDead /\ A2GoneUpped /\ A2UpDoneUpped /\ A2LostUpped
  BY <1>2 DEF A1LostDead, A2GoneDead, A2GoneUpped, A2UpDoneUpped, A2LostUpped
<1>5. A2UpsDone /\ A2DelBase /\ A2GoneUnconf /\ A2GoneBar /\ A2GoneSub
  BY <1>2 DEF A2UpsDone, A2DelBase, A2GoneUnconf, A2GoneBar, A2GoneSub, A2Bar
<1>6. A2ConfUpped /\ A2TombUpped /\ A2BaseUpped BY <1>2, <1>3 DEF A2ConfUpped, A2TombUpped, A2BaseUpped
<1>7. A2UpUnconf /\ A2UpNotTomb /\ A2UpNotBase BY <1>2 DEF A2UpUnconf, A2UpNotTomb, A2UpNotBase, A2Up
<1>. QED BY <1>4, <1>5, <1>6, <1>7 DEF A2Side

LEMMA Next_A2 == IndC /\ IndC' /\ A2Side /\ Next => A2Side'
<1>. SUFFICES ASSUME IndC, IndC', A2Side, Next PROVE A2Side' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE A2Side' BY <3>1, GPut_A2
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE A2Side' BY <3>2, GCas_A2
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE A2Side' BY <3>3, GDelete_A2
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE A2Side' BY <3>4, GRename_A2
    <3>5. ASSUME GRenameFinish PROVE A2Side' BY <3>5, GRenameFinish_A2
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE A2Side' BY <3>1, Checkout_A2
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE A2Side' BY <3>2, Consume_A2
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE A2Side' BY <3>3, Scan_A2
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE A2Side' BY <3>4, Skip_A2
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE A2Side' BY <3>5, PullOnly_A2
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE A2Side' BY <3>6, Claim_A2
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE A2Side' BY <3>7, Verify_A2
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE A2Side' BY <3>8, Install_A2
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE A2Side' BY <3>9, Collect_A2
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE A2Side' BY <3>10, Finish_A2
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE A2Side' BY <3>11, Restart_A2
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE A2Side' BY <3>12, Sync_A2
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE A2Side' BY <3>13, RescopeBegin_A2
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE A2Side' BY <3>14, RescopeFirst_A2
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE A2Side' BY <3>15, RescopeSecond_A2
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE A2Side' BY <3>16, RPullRead_A2
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE A2Side' BY <3>17, RPullSync_A2
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE A2Side' BY <3>18, Edit_A2
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE A2Side' BY <3>19, Delete_A2
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE A2Side' BY <3>20, Upload_A2
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE A2Side' BY <3>21, Sweep_A2
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE A2Side' BY <3>22, Touch_A2
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE A2Side' BY <3>23, Honor_A2
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE A2Side' BY <3>24, HonorReader_A2
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE A2Side' BY <3>25, RetireByAck_A2
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE A2Side' BY <3>26, Depose_A2
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE A2Side' BY <3>27, Fence_A2
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>1. CASE Age BY <2>1, Age_A2
  <2>2. CASE RLoad BY <2>2, RLoad_A2
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE A2Side' BY <2>3, Reap_A2
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA A2Step == IndC /\ IndC' /\ A2Side /\ [Next]_vars => A2Side'
<1>. SUFFICES ASSUME IndC, IndC', A2Side, [Next]_vars PROVE A2Side' OBVIOUS
<1>1. CASE Next BY <1>1, Next_A2
<1>2. CASE UNCHANGED vars
  <2>1. UNCHANGED <<w, live, minted, conflicts, tomb, upped>> BY <1>2 DEF vars, bucket, aux
  <2>2. A2TombOK BY <2>1 DEF A2TombOK
  <2>. QED BY <2>1, <2>2, A2Quiet
<1>. QED BY <1>1, <1>2

THEOREM A2Inv == Spec => []A2Side
BY Init_A2, A2Step, CInvariant, PTL DEF Spec

------------------------------------------------------------------------------
(* M11's side facts: A2Side gives five of A1Side's eight; M13 two more (the  *)
(* narrow's), and the theorem AK1 is there.                                 *)
LEMMA A2ToA1 == A2Side => A1LostDead /\ A1UpsDone /\ A1DelBase /\ A1GoneUnconf /\ A1TombUpped
BY DEF A2Side, A2UpsDone, A2DelBase, A2GoneUnconf, A2TombUpped, A2Bar,
       A1UpsDone, A1DelBase, A1GoneUnconf, A1TombUpped

==============================================================================
