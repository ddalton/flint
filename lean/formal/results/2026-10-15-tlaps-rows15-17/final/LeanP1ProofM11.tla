------------------------------ MODULE LeanP1ProofM11 ------------------------------
(* M11 (row 15): AK1 part 2, BoundOK an invariant.  Generated:
   results/2026-10-15-tlaps-rows15-17/gen-m11.py (its docstring has the plan;
   AK1-PLAN.txt there too).                                                 *)
EXTENDS LeanP1ProofM10, LeanP1ProofM5D

\* A declared path the agent has not moved on from (MCLeanP1M10's A1Decl).
A1Decl(s, p) == w[s].pend.id # 0 /\ p \in w[s].dset /\ p \notin w[s].moved
\* A narrow's leftover: in "mid" a dropped path not kept, after a restart
\* mid-narrow ("saved") one uncited with its held entry recorded; either way the
\* file still holds the very handle the uncite recorded.  RescopeSecond unlinks
\* it, Touch never declares it, and a declared file there is in carrier form (it
\* was clean at RescopeBegin).  (The rescope fork, 2026-10-16; the restart arm,
\* FINDINGS Table 3's restart row.)  By handle, so it reads the tree record
\* only; LeanP1Anc's RescopeLeftover compares bytes, and A1HeldSame (M12) says
\* the two agree.
A1LeftSet(W) == {p \in W.sDrop : /\ W.local[p] # Nil /\ W.sHeld[p] # Nil /\ W.local[p] = W.sHeld[p]
                                 /\ \/ W.sStage = "mid" /\ p \notin W.sKeep
                                    \/ W.sStage = "saved" /\ W.baseline[p] = Nil}
\* (D) a dirty declared path holds the declared bytes, but for a leftover.
A1DTree == \A s \in Writers, p \in Paths :
           A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]) =>
             Content(w[s].local[p]) = Content(w[s].decl[p])
\* (C) a clean declared FILE is in carrier form.  (No fetch arm: Touch declares
\* only bytes the document does not hold, and the document holds a declared
\* file's bytes only once the carrier published them, A1DocCar.  TLC: m10c
\* CPubCar/DocCar, 2026-10-16.)
A1CPub3 == \A s \in Writers, p \in Paths :
          A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p] =>
            w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* (L) a declared FILE the narrow left is in carrier form (RescopeSecond unlinks
\* it: clean after).  It was clean at RescopeBegin (CPub3), and nothing between
\* the halves touches the carrier.  TLC: m10c CPubLeft2, CPubLeftH.
A1CPubL == \A s \in Writers, p \in Paths :
          A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s]) =>
            w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* (P) the document holds a declared file's bytes only in carrier form.
A1DocCar == \A s \in Writers, p \in Paths :
            A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]) =>
              w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* (F) folds: a touch makes the honour wait for the next barrier end.
A1Folds == \A s \in Writers : w[s].pend.fold <= w[s].nt /\ w[s].bound.fold <= w[s].nt /\ w[s].cFold <= w[s].nt
\* (Z) in a barrier the record is the one it consumed (touch, retire: idle only).
A1Frozen == \A s \in Writers : w[s].pc \in {"consumed", "scanned", "claimed", "cased"} =>
            w[s].cId = w[s].pend.id /\ w[s].cFold = w[s].pend.fold
\* (M) after a carrying scan, a dirty declared path is published by it, and a
\* declared upload still holds its scanned bytes, a declared delete is still
\* absent (an edit or a delete under a record moves the path; an upload's
\* copy moves snap and local together).  Finish needs the second half.
A1MidBar == \A s \in Writers, p \in Paths :
            w[s].pc \in {"scanned", "claimed", "cased"} /\ Carries(s) /\ A1Decl(s, p) =>
              /\ w[s].local[p] # w[s].baseline[p] =>
                   \/ p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p]
                   \/ p \in w[s].deletes /\ w[s].local[p] = Nil
              /\ p \in w[s].uploads => w[s].snap[p] = w[s].local[p]
              /\ p \in w[s].deletes => w[s].local[p] = Nil
\* (I) after a carrying install, a declared file is in carrier form, or withheld
\* (the barrier end names it: wd = gone; the install took it out of upDone, so
\* step 7 leaves its baseline).  TLC: m10c Cased5.
A1Cased == \A s \in Writers, p \in Paths :
           w[s].pc = "cased" /\ Carries(s) /\ A1Decl(s, p) /\ w[s].decl[p] # Nil =>
             \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
             \/ p \in w[s].gone /\ p \notin w[s].upDone
\* Typing the steps need: what is declared, the bound's document, what was
\* installed, what the carrier published, and what a conflict preserves, are minted.
A1ConflictsMinted == \A c \in conflicts : c[2] \in Opt(minted)
A1DeclMinted == \A s \in Writers, p \in Paths : w[s].decl[p] \in Opt(minted)
A1BoundMinted == \A s \in Writers : w[s].bound.doc \in [Paths -> Opt(minted)]
A1InstMinted == \A s \in Writers, p \in Paths : w[s].inst[p] \in Opt(minted)
A1CarPubMinted == \A s \in Writers, p \in Paths : w[s].car.pub[p] \in Opt(minted)
\* The invariant.
A1IAK1 == /\ A1Folds /\ A1Frozen /\ A1MidBar /\ A1Cased /\ A1DTree /\ A1CPub3 /\ A1CPubL /\ A1DocCar
          /\ A1DeclMinted /\ A1BoundMinted /\ A1InstMinted /\ A1CarPubMinted /\ A1ConflictsMinted
          /\ BoundOK
\* The side facts M11's steps assume beyond A1IAK1 (module M12 proves them).
A1LostDead == \A t \in Writers : \A h \in w[t].lost : h \notin live
\* Install's barrier facts (MCLeanP1M10 UpsDone, DelBase, GoneUnconf; M12 proves
\* them with A1LostDead): from the claim every upload is done or withheld; a
\* barrier's delete has a baseline (dirty at the scan); before the install no
\* conflict names a withheld upload.
A1UpsDone == \A s \in Writers : w[s].pc \in {"claimed", "cased"} => w[s].uploads \subseteq w[s].upDone \cup w[s].gone
A1DelBase == \A s \in Writers, p \in Paths :
               w[s].pc \in {"scanned", "claimed", "cased"} /\ p \in w[s].deletes => w[s].baseline[p] # Nil
A1GoneUnconf == \A s \in Writers, p \in Paths :
                  w[s].pc = "claimed" /\ p \in w[s].gone => \A c \in conflicts : c[2] # w[s].snap[p]
\* M11's side invariant: what the steps assume beyond A1IAK1 (M12).
\* A tombstone names an uploaded handle (MCLeanP1M10 STombUpped): the install's
\* delete-override conflicts cite it.
A1TombUpped == \A q \in Paths : tomb[q] \in upped \cup {Nil}
\* DocCar at one path (what a gateway step that lands a handle there must show).
A1DocCarAt(q) == \A u \in Writers :
                   (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[u].decl[q])) =>
                     w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)
\* A declared FILE not yet in carrier form keeps its bytes private to its tree: no
\* document path, no save in flight and no other tree's upload holds them (TLC
\* candidate MCLeanP1M10 DeclPriv; worlds ZG, D2).  GCas (the save), GRename (the
\* document elsewhere) and Install (a peer's upload) each land a handle it covers.
A1Priv(u, q) == A1Decl(u, q) /\ w[u].decl[q] # Nil /\ ~(w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car))
A1DeclPriv == \A u \in Writers, q \in Paths : A1Priv(u, q) =>
                /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q])
                /\ \A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q])
                /\ \A s \in Writers \ {u}, k \in Paths : k \in w[s].uploads => Content(w[s].snap[k]) # Content(w[u].decl[q])
\* The narrow's record (M12).  A path the narrow kept is still cited between the
\* halves (KeepSet: cited and dirty; nothing at "mid" moves a baseline), so the
\* restart arm, which reads no sKeep, takes no kept path in.  A leftover candidate
\* holding the held bytes holds the held handle (the unlink compares bytes,
\* A1LeftSet handles: a file there is the uncite's, or the agent's own write).
A1KeepCited == \A s \in Writers, p \in Paths : w[s].sStage = "mid" /\ p \in w[s].sKeep => w[s].baseline[p] # Nil
A1LeftCand(W, p) == /\ p \in W.sDrop /\ W.local[p] # Nil /\ W.sHeld[p] # Nil
                    /\ \/ W.sStage = "mid" /\ p \notin W.sKeep
                       \/ W.sStage = "saved" /\ W.baseline[p] = Nil
A1HeldSame == \A s \in Writers, p \in Paths :
                A1LeftCand(w[s], p) /\ Content(w[s].local[p]) = Content(w[s].sHeld[p]) => w[s].local[p] = w[s].sHeld[p]
A1Side == A1LostDead /\ A1UpsDone /\ A1DelBase /\ A1GoneUnconf /\ A1TombUpped /\ A1DeclPriv
          /\ A1KeepCited /\ A1HeldSame

------------------------------------------------------------------------------
(* Broken is kept by a step that keeps the declaration, only adds conflicts   *)
(* and live handles, and adds no lost handle.                              *)


LEMMA A1ContentStep ==
  ASSUME IndC, HStep, NEW h \in Opt(minted)
  PROVE  Content(h)' = Content(h)
<1>1. CASE h = Nil BY <1>1 DEF Content
<1>2. CASE h \in minted BY <1>2, ContentKeep
<1>. QED BY <1>1, <1>2 DEF Opt

\* The document's content, where a step keeps the document's handle (A1DocCar
\* reads Content(doc[q]); doc is minted, Minted).
LEMMA A1DocContent ==
  ASSUME IndC, HStep
  PROVE  \A q \in Paths : doc'[q] = doc[q] => Content(doc[q])' = Content(doc[q])
<1>. SUFFICES ASSUME NEW q \in Paths PROVE doc'[q] = doc[q] => Content(doc[q])' = Content(doc[q]) OBVIOUS
<1>1. PICK h \in Opt(minted) : h = doc[q] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>2. Content(h)' = Content(h) BY <1>1, A1ContentStep
<1>. QED BY <1>1, <1>2

LEMMA A1BrokenKeep ==
  ASSUME IndC, HStep, A1ConflictsMinted, conflicts \subseteq conflicts',
         \A c \in conflicts : c[2] \in live => c[2] \in live' \/ c[2] \notin UNION {w[t].lost : t \in Writers},
         UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers},
         NEW s \in Writers, NEW p \in Paths, NEW d,
         w'[s].dset = w[s].dset, w[s].moved \subseteq w'[s].moved,
         w'[s].decl = w[s].decl, w'[s].dMint = w[s].dMint,
         d[p] \in Opt(minted), w[s].decl[p] \in Opt(minted),
         Broken(s, p, d)'
  PROVE  Broken(s, p, d)
\* The declared handle as a constant (A1ContentStep takes a constant handle).
<1>0. PICK hd \in Opt(minted) : hd = w[s].decl[p] OBVIOUS
<1>z. w'[s].decl[p] = hd BY <1>0
<1>1. Content(d[p])' = Content(d[p]) BY A1ContentStep
<1>2. Content(hd)' = Content(hd) BY A1ContentStep
<1>4. LaterVersion(s, p, d) => LaterVersion(s, p, d)'
  <2>. SUFFICES ASSUME LaterVersion(s, p, d) PROVE LaterVersion(s, p, d)' OBVIOUS
  <2>1. d[p] # Nil BY DEF LaterVersion
  <2>2. CASE hd = Nil BY <2>1, <2>2, <1>0, <1>z DEF LaterVersion
  <2>3. CASE Derives(d[p], hd) /\ hd # Nil
    <3>1. hd \in minted BY <2>3 DEF Opt
    <3>0. IndM5 BY DEF IndC
    <3>k. d[p] \in Opt(Handles) BY <3>0, MintedHandles DEF Opt
    <3>2. Derives(d[p], hd)' BY <2>3, <3>0, <3>1, <3>k, DerivesStep
    <3>. QED BY <2>1, <3>2, <1>z DEF LaterVersion
  <2>4. CASE Gen(Content(d[p])) >= w[s].dMint[p]
    BY <2>1, <2>4, <1>1 DEF LaterVersion
  <2>. QED BY <2>2, <2>3, <2>4, <1>0 DEF LaterVersion
<1>5. SurfacedDecl(s, p) => SurfacedDecl(s, p)'
  <2>. SUFFICES ASSUME SurfacedDecl(s, p) PROVE SurfacedDecl(s, p)' OBVIOUS
  <2>1. PICK c \in conflicts : /\ c[1] = p /\ c[2] # Nil /\ Content(c[2]) = Content(hd)
                               /\ (c[2] \in live \/ c[2] \notin UNION {w[t].lost : t \in Writers})
    BY <1>0 DEF SurfacedDecl
  <2>2. Content(c[2])' = Content(c[2]) BY A1ContentStep DEF A1ConflictsMinted
  <2>. QED BY <2>1, <2>2, <1>2, <1>z DEF SurfacedDecl
<1>. QED BY <1>0, <1>z, <1>1, <1>2, <1>4, <1>5 DEF Broken

\* The same, when lost grows but by no handle a conflict names (Install: its
\* withheld uploads, A1GoneUnconf).
LEMMA A1BrokenKeepL ==
  ASSUME IndC, HStep, A1ConflictsMinted, conflicts \subseteq conflicts',
         \A c \in conflicts : c[2] \in live => c[2] \in live' \/ c[2] \notin UNION {w[t].lost : t \in Writers},
         \A c \in conflicts : c[2] \in UNION {w'[t].lost : t \in Writers} => c[2] \in UNION {w[t].lost : t \in Writers},
         NEW s \in Writers, NEW p \in Paths, NEW d,
         w'[s].dset = w[s].dset, w[s].moved \subseteq w'[s].moved,
         w'[s].decl = w[s].decl, w'[s].dMint = w[s].dMint,
         d[p] \in Opt(minted), w[s].decl[p] \in Opt(minted),
         Broken(s, p, d)'
  PROVE  Broken(s, p, d)
\* The declared handle as a constant (A1ContentStep takes a constant handle).
<1>0. PICK hd \in Opt(minted) : hd = w[s].decl[p] OBVIOUS
<1>z. w'[s].decl[p] = hd BY <1>0
<1>1. Content(d[p])' = Content(d[p]) BY A1ContentStep
<1>2. Content(hd)' = Content(hd) BY A1ContentStep
<1>4. LaterVersion(s, p, d) => LaterVersion(s, p, d)'
  <2>. SUFFICES ASSUME LaterVersion(s, p, d) PROVE LaterVersion(s, p, d)' OBVIOUS
  <2>1. d[p] # Nil BY DEF LaterVersion
  <2>2. CASE hd = Nil BY <2>1, <2>2, <1>0, <1>z DEF LaterVersion
  <2>3. CASE Derives(d[p], hd) /\ hd # Nil
    <3>1. hd \in minted BY <2>3 DEF Opt
    <3>0. IndM5 BY DEF IndC
    <3>k. d[p] \in Opt(Handles) BY <3>0, MintedHandles DEF Opt
    <3>2. Derives(d[p], hd)' BY <2>3, <3>0, <3>1, <3>k, DerivesStep
    <3>. QED BY <2>1, <3>2, <1>z DEF LaterVersion
  <2>4. CASE Gen(Content(d[p])) >= w[s].dMint[p]
    BY <2>1, <2>4, <1>1 DEF LaterVersion
  <2>. QED BY <2>2, <2>3, <2>4, <1>0 DEF LaterVersion
<1>5. SurfacedDecl(s, p) => SurfacedDecl(s, p)'
  <2>. SUFFICES ASSUME SurfacedDecl(s, p) PROVE SurfacedDecl(s, p)' OBVIOUS
  <2>1. PICK c \in conflicts : /\ c[1] = p /\ c[2] # Nil /\ Content(c[2]) = Content(hd)
                               /\ (c[2] \in live \/ c[2] \notin UNION {w[t].lost : t \in Writers})
    BY <1>0 DEF SurfacedDecl
  <2>2. Content(c[2])' = Content(c[2]) BY A1ContentStep DEF A1ConflictsMinted
  <2>. QED BY <2>1, <2>2, <1>2, <1>z DEF SurfacedDecl
<1>. QED BY <1>0, <1>z, <1>1, <1>2, <1>4, <1>5 DEF Broken
------------------------------------------------------------------------------
(* A step that writes no tree, keeps the conflicts and only adds live       *)
(* handles keeps every conjunct, where each document path is kept, cleared,  *)
(* or shown DocCar-safe by the caller (A1DocCarAt): GPut, GDelete, GCas,     *)
(* GRename, the sweep, the reap.  GRenameFinish: vacuous (mv = Nil).        *)

LEMMA A1NoTree ==
  ASSUME IndC, A1IAK1, HStep, w' = w, conflicts' = conflicts,
         \A q \in Paths : doc'[q] = doc[q] \/ doc'[q] = Nil \/ A1DocCarAt(q)',
         \A c \in conflicts : c[2] \in live => c[2] \in live' \/ c[2] \notin UNION {w[t].lost : t \in Writers}
  PROVE  A1IAK1'
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>5. IndM5 BY DEF IndC
<1>ty. \A u \in Writers, q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].inst[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>5, TreeTy DEF A1IAK1, A1DeclMinted, A1InstMinted, A1CarPubMinted
<1>c. \A u \in Writers, q \in Paths :
        /\ Content(w[u].local[q])' = Content(w[u].local[q])
        /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
        /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
        /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
        /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths
                PROVE  /\ Content(w[u].local[q])' = Content(w[u].local[q])
                       /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                       /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
                       /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
                       /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
  <2>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
  <2>4. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>ty
  <2>5. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
  <2>6. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
        /\ Content(h4)' = Content(h4) /\ Content(h5)' = Content(h5)
    BY A1ContentStep
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6
<1>1. A1Folds' /\ A1Frozen' /\ A1MidBar' BY DEF A1IAK1, A1Folds, A1Frozen, A1MidBar, A1Decl, Carries
<1>2. A1DTree' BY <1>c DEF A1IAK1, A1DTree, A1Decl
<1>3. A1CPub3' BY <1>c DEF A1IAK1, A1CPub3, A1Decl, CarForm
<1>3l. A1CPubL' BY <1>c DEF A1IAK1, A1CPubL, A1Decl, CarForm
<1>4. A1Cased' BY <1>c DEF A1IAK1, A1Cased, A1Decl, CarForm, Carries
\* DocCar: no tree and no document handle changes; the document's content is kept.
<1>dc. A1DocCar'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths,
                       (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil
                        /\ Content(doc[q]) = Content(w[u].decl[q]))'
                PROVE  (w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car))'
    BY DEF A1DocCar
  \* a cleared path is not cited (the antecedent is false); a landed one is the
  \* caller's (A1DocCarAt); else the document's handle there is kept
  <2>1. CASE doc'[q] = doc[q]
    <3>1. Content(doc[q])' = Content(doc[q]) BY <2>1, A1DocContent
    <3>2. A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[u].decl[q])
      BY <2>1, <3>1, <1>c DEF A1Decl
    <3>3. w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car) BY <3>2 DEF A1IAK1, A1DocCar
    <3>4. PICK c0 : c0 = w[u].car OBVIOUS
    <3>5. CarForm(u, q, c0)' <=> CarForm(u, q, c0) BY <3>4, <1>c DEF CarForm
    <3>. QED BY <3>3, <3>4, <3>5
  <2>2. CASE A1DocCarAt(q)' BY <2>2 DEF A1DocCarAt
  <2>. QED BY <2>1, <2>2
<1>6. A1DeclMinted' /\ A1BoundMinted' /\ A1InstMinted' /\ A1CarPubMinted' /\ A1ConflictsMinted'
  BY <1>m DEF A1IAK1, A1DeclMinted, A1BoundMinted, A1InstMinted, A1CarPubMinted, A1ConflictsMinted, Opt
<1>7. BoundOK'
  <2>. SUFFICES ASSUME NEW s \in Writers, Honorable(s)', NEW p \in Paths,
                       Broken(s, p, w[s].bound.doc)'
                PROVE  p \in HDropped(s)'
    BY DEF BoundOK
  <2>0. PICK d \in [Paths -> Opt(minted)] : d = w[s].bound.doc BY DEF A1IAK1, A1BoundMinted
  <2>1. Broken(s, p, d)' BY <2>0
  <2>2. Broken(s, p, d)
    BY <2>0, <2>1, A1BrokenKeep DEF A1IAK1, A1DeclMinted
  <2>. QED BY <2>0, <2>2 DEF A1IAK1, BoundOK, Honorable, HDropped
<1>. QED BY <1>1, <1>2, <1>3, <1>3l, <1>4, <1>dc, <1>6, <1>7 DEF A1IAK1
------------------------------------------------------------------------------
(* The gateway steps.                                                       *)

\* The save's CAS lands gw[p] at p (or 412s): a declared file not in carrier form is
\* private (A1DeclPriv: no save in flight carries its bytes), so DocCar holds there.
LEMMA GCas_A1 ==
  ASSUME IndC, IndC', A1IAK1, A1Side, NEW p \in Paths, GCas(p), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = w /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF GCas, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>h. PICK h \in minted : h = gw[p]
  BY DEF GCas, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted, Opt
<1>d. \A q \in Paths : doc'[q] = doc[q] \/ (q = p /\ doc'[q] = h) BY <1>0, <1>h DEF GCas, TypeOK
<1>at. A1DocCarAt(p)'
  <2>. SUFFICES ASSUME NEW u \in Writers,
                       (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p]))'
                PROVE  (w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car))'
    BY DEF A1DocCarAt
  <2>c. PICK hd \in Opt(minted) : hd = w[u].decl[p] BY DEF A1IAK1, A1DeclMinted
  <2>k. Content(hd)' = Content(hd) /\ Content(h)' = Content(h) BY <2>c, <1>h, <1>hs, A1ContentStep DEF Opt
  <2>0. PICK c0 : c0 = w[u].car OBVIOUS
  <2>cf. CarForm(u, p, c0)' <=> CarForm(u, p, c0) BY <2>0, <2>c, <2>k, <1>1, <1>hs, A1ContentStep DEF CarForm, A1IAK1, A1CarPubMinted
  <2>1. CASE doc'[p] = doc[p]
    <3>1. Content(doc[p])' = Content(doc[p]) BY <2>1, <1>hs, A1DocContent
    <3>2. A1Decl(u, p) /\ w[u].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p])
      BY <2>1, <3>1, <2>c, <2>k, <1>1 DEF A1Decl
    <3>3. w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car) BY <3>2 DEF A1IAK1, A1DocCar
    <3>. QED BY <3>3, <2>0, <2>cf, <1>1
  <2>2. CASE doc'[p] = h
    \* in carrier form already, or private: the landed save does not carry the bytes
    <3>0. PICK c0 : c0 = w[u].car OBVIOUS
    <3>cf. CarForm(u, p, c0)' <=> CarForm(u, p, c0) BY <3>0, <2>c, <2>k, <1>1, <1>hs, A1ContentStep DEF CarForm, A1IAK1, A1CarPubMinted
    <3>1. CASE w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car) BY <3>1, <3>0, <3>cf, <1>1
    <3>2. CASE ~(w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car))
      <4>1. A1Decl(u, p) /\ hd # Nil BY <2>c, <1>1 DEF A1Decl
      <4>a. A1DeclPriv BY DEF A1Side
      <4>b. A1Priv(u, p) BY <4>1, <3>2, <2>c DEF A1Priv
      <4>c. gw[p] = Nil \/ Content(gw[p]) # Content(w[u].decl[p]) BY <4>a, <4>b DEF A1DeclPriv
      <4>2. Content(h) # Content(hd) BY <4>c, <1>h, <2>c DEF GCas
      <4>. QED BY <2>2, <2>c, <2>k, <4>2, <1>1
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2, <1>d
<1>. QED BY <1>1, <1>hs, <1>d, <1>at, A1NoTree

LEMMA GDelete_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW p \in Paths, GDelete(p), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = w /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF GDelete, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the delete clears p; every other path is kept
<1>d. \A q \in Paths : doc'[q] = doc[q] \/ doc'[q] = Nil BY <1>0 DEF GDelete, TypeOK
<1>. QED BY <1>1, <1>hs, <1>d, A1NoTree

\* The rename moves doc[p] to q (one CAS: mv stays Nil, IndC'): p is cleared; at q
\* the document now holds what it held at p: bytes the document held are not private
\* (A1DeclPriv, k = p), so a declared file with them is in carrier form.
LEMMA GRename_A1 ==
  ASSUME IndC, IndC', A1IAK1, A1Side, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = w /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF GRename, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>r. RenameAtomic BY ShippedShape DEF Shipped
<1>a. doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil] BY <1>r DEF GRename
<1>d. \A x \in Paths : doc'[x] = doc[x] \/ doc'[x] = Nil \/ (x = q /\ doc'[x] = doc[p]) BY <1>0, <1>a DEF TypeOK
<1>at. A1DocCarAt(q)'
  <2>. SUFFICES ASSUME NEW u \in Writers,
                       (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[u].decl[q]))'
                PROVE  (w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car))'
    BY DEF A1DocCarAt
  <2>c. PICK hd \in Opt(minted) : hd = w[u].decl[q] BY DEF A1IAK1, A1DeclMinted
  \* doc[p] as a constant (Content(doc[p])' would read doc'[p] = Nil)
  <2>m. PICK hp \in Opt(minted) : hp = doc[p] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
  <2>k. Content(hd)' = Content(hd) /\ Content(hp)' = Content(hp) BY <2>c, <2>m, <1>hs, A1ContentStep
  <2>1. doc'[q] = hp BY <1>a, <2>m, <1>0 DEF GRename, TypeOK
  <2>2. A1Decl(u, q) /\ w[u].decl[q] # Nil /\ hp # Nil /\ Content(hp) = Content(w[u].decl[q])
    BY <2>1, <2>c, <2>k, <1>1 DEF A1Decl
  \* the document held these bytes at p: not private, so in carrier form
  <2>a. A1DeclPriv BY DEF A1Side
  <2>3. w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)
    <3>. SUFFICES ASSUME ~(w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)) PROVE FALSE OBVIOUS
    <3>1. A1Priv(u, q) BY <2>2 DEF A1Priv
    <3>2. doc[p] = Nil \/ Content(doc[p]) # Content(w[u].decl[q]) BY <3>1, <2>a DEF A1DeclPriv
    <3>. QED BY <3>2, <2>2, <2>m
  <2>. QED BY <2>3, <2>c, <2>k, <1>1, <1>hs, A1ContentStep DEF A1IAK1, CarForm, A1CarPubMinted
<1>. QED BY <1>1, <1>hs, <1>d, <1>at, A1NoTree

\* Vacuous: the rename is one CAS (RenameAtomic, shipped), so no second half is
\* ever pending (IndTypeOK: mv = Nil) and GRenameFinish needs mv # Nil.
LEMMA GRenameFinish_A1 ==
  ASSUME IndC, IndC', A1IAK1, GRenameFinish, Frame
  PROVE  A1IAK1'
<1>1. mv = Nil BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

LEMMA GPut_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW p \in Paths, GPut(p), Frame
  PROVE  A1IAK1'
<1>m. IndM5' BY DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>. DEFINE h == <<p, nextGen>>
<1>1. /\ minted' = minted \cup {h} /\ base' = [base EXCEPT ![h] = doc[p]] /\ orig' = orig
      /\ nextGen <= MaxMint /\ nextGen' = nextGen + 1
      /\ w' = w /\ UNCHANGED <<doc, conflicts, udel, acked>> /\ live' = live \cup {h}
  BY DEF GPut, aux
<1>2. nextGen \in Nat /\ nextGen > Seed BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>3. h \in Handles BY <1>1, <1>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
<1>4. h \notin minted
  <2>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>. QED BY <2>1, <1>1, <1>2, MaxMintNat DEF Gen
<1>ty. base \in [Handles -> Opt(Handles)] /\ orig \in [Handles -> Opt(Handles)]
  BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>f. MintFacts(h) BY <1>1, <1>3, <1>4, <1>ty, MintedHandles DEF MintFacts, IndC
<1>5. doc[p] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>6. orig[h] = Nil BY <1>3, <1>4 DEF IndC, IndM5, M5, Hist, HistNew
<1>7. base'[h] = doc[p] /\ orig'[h] = Nil BY <1>1, <1>3, <1>6, <1>ty
<1>8. \A y \in Handles : y # h => base'[y] = base[y] /\ orig'[y] = orig[y] BY <1>1, <1>ty
<1>hs. HStep BY <1>m, <1>u, <1>f, <1>5, <1>7, <1>8, HStepMint DEF IndC, Opt
<1>. QED BY <1>1, <1>hs, A1NoTree

------------------------------------------------------------------------------
(* A writer step that keeps every field AK1's conjuncts read but pc, and     *)
(* moves pc only within or out of the barrier (never into "cased", never   *)
(* into the barrier from outside) keeps every conjunct.                    *)

\* The fields the conjuncts read.
A1Same(R, W) ==
  /\ R.pend = W.pend /\ R.dset = W.dset /\ R.decl = W.decl /\ R.moved = W.moved /\ R.dMint = W.dMint
  /\ R.local = W.local /\ R.baseline = W.baseline /\ R.car = W.car /\ R.bound = W.bound
  /\ R.cId = W.cId /\ R.cFold = W.cFold /\ R.nt = W.nt /\ R.inst = W.inst
  /\ R.uploads = W.uploads /\ R.deletes = W.deletes /\ R.snap = W.snap /\ R.lost = W.lost
  /\ A1LeftSet(R) = A1LeftSet(W)
A1PcOK(R, W) ==
  /\ R.pc \in {"consumed", "scanned", "claimed", "cased"} => W.pc \in {"consumed", "scanned", "claimed", "cased"}
  /\ R.pc \in {"scanned", "claimed", "cased"} => W.pc \in {"scanned", "claimed", "cased"}
  \* (Cased's withheld arm reads gone: a step staying at "cased" keeps it)
  /\ R.pc = "cased" => W.pc = "cased" /\ R.gone = W.gone /\ R.upDone = W.upDone

LEMMA A1Keep ==
  ASSUME IndC, A1IAK1, HStep, NEW s \in Writers, NEW R,
         w' = [w EXCEPT ![s] = R], A1Same(R, w[s]), A1PcOK(R, w[s]),
         conflicts' = conflicts, doc' = doc, live \subseteq live'
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>f. \A u \in Writers : A1Same(w'[u], w[u]) /\ A1PcOK(w'[u], w[u]) BY <1>w DEF A1Same, A1PcOK
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>5. IndM5 BY DEF IndC
<1>ty. \A u \in Writers, q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].inst[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>5, TreeTy DEF A1IAK1, A1DeclMinted, A1InstMinted, A1CarPubMinted
<1>c. \A u \in Writers, q \in Paths :
        /\ Content(w[u].local[q])' = Content(w[u].local[q])
        /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
        /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
        /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
        /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths
                PROVE  /\ Content(w[u].local[q])' = Content(w[u].local[q])
                       /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                       /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
                       /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
                       /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
  <2>0. A1Same(w'[u], w[u]) BY <1>f
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
  <2>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
  <2>4. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>ty
  <2>5. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
  <2>6. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
        /\ Content(h4)' = Content(h4) /\ Content(h5)' = Content(h5)
    BY A1ContentStep
  <2>7. /\ w'[u].local[q] = h1 /\ w'[u].baseline[q] = h2 /\ w'[u].decl[q] = h3
        /\ w'[u].inst[q] = h4 /\ w'[u].car.pub[q] = h5
    BY <2>0, <2>1, <2>2, <2>3, <2>4, <2>5 DEF A1Same
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7
<1>1. A1Folds' BY <1>f DEF A1IAK1, A1Folds, A1Same
<1>2. A1Frozen' BY <1>f DEF A1IAK1, A1Frozen, A1Same, A1PcOK
<1>3. A1MidBar' BY <1>f DEF A1IAK1, A1MidBar, A1Same, A1PcOK, A1Decl, Carries
<1>4. A1DTree' BY <1>f, <1>c DEF A1IAK1, A1DTree, A1Same, A1Decl
<1>6. A1CPub3' BY <1>f, <1>c DEF A1IAK1, A1CPub3, A1Same, A1Decl, CarForm
<1>6l. A1CPubL' BY <1>f, <1>c DEF A1IAK1, A1CPubL, A1Same, A1Decl, CarForm
<1>7. A1Cased' BY <1>f, <1>c DEF A1IAK1, A1Cased, A1Same, A1PcOK, A1Decl, CarForm, Carries
\* DocCar reads doc (unchanged) and its content (kept: doc is minted).
<1>dc. A1DocCar'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths,
                       (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil
                        /\ Content(doc[q]) = Content(w[u].decl[q]))'
                PROVE  (w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car))'
    BY DEF A1DocCar
  <2>0. A1Same(w'[u], w[u]) BY <1>f
  <2>1. PICK h \in Opt(minted) : h = doc[q] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
  <2>2. Content(doc[q])' = Content(doc[q]) BY <2>1, A1ContentStep
  <2>3. /\ A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil
        /\ Content(doc[q]) = Content(w[u].decl[q])
    BY <2>0, <2>2, <1>c DEF A1Same, A1Decl
  <2>4. w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car) BY <2>3 DEF A1IAK1, A1DocCar
  <2>. QED BY <2>0, <2>4, <1>c DEF A1Same, CarForm
<1>8. A1DeclMinted' /\ A1BoundMinted' /\ A1InstMinted' /\ A1CarPubMinted' /\ A1ConflictsMinted'
  BY <1>f, <1>m DEF A1IAK1, A1DeclMinted, A1BoundMinted, A1InstMinted, A1CarPubMinted, A1ConflictsMinted, A1Same, Opt
<1>9. BoundOK'
  <2>. SUFFICES ASSUME NEW u \in Writers, Honorable(u)', NEW p \in Paths,
                       Broken(u, p, w[u].bound.doc)'
                PROVE  p \in HDropped(u)'
    BY DEF BoundOK
  <2>s. A1Same(w'[u], w[u]) BY <1>f
  <2>l. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>f DEF A1Same
  <2>0. PICK d \in [Paths -> Opt(minted)] : d = w[u].bound.doc BY DEF A1IAK1, A1BoundMinted
  <2>1. Broken(u, p, d)' BY <2>0, <2>s DEF A1Same
  <2>2. Broken(u, p, d)
    BY <2>0, <2>1, <2>s, <2>l, A1BrokenKeep DEF A1IAK1, A1DeclMinted, A1Same
  <2>3. Honorable(u) BY <2>s DEF Honorable, A1Same
  <2>. QED BY <2>0, <2>2, <2>3, <2>s DEF A1IAK1, BoundOK, HDropped, A1Same
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>6, <1>6l, <1>7, <1>dc, <1>8, <1>9 DEF A1IAK1
------------------------------------------------------------------------------
(* The writer steps that keep AK1's fields (A1Keep).                       *)

LEMMA Claim_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Claim(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Claim, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>g. w[s].pc = "scanned" BY DEF Claim
<1>2. A1Same(ClaimW(s), w[s])
  <2>1. ClaimW(s).pend = w[s].pend BY DEF ClaimW
  <2>2. ClaimW(s).dset = w[s].dset BY DEF ClaimW
  <2>3. ClaimW(s).decl = w[s].decl BY DEF ClaimW
  <2>4. ClaimW(s).moved = w[s].moved BY DEF ClaimW
  <2>5. ClaimW(s).dMint = w[s].dMint BY DEF ClaimW
  <2>6. ClaimW(s).local = w[s].local BY DEF ClaimW
  <2>7. ClaimW(s).baseline = w[s].baseline BY DEF ClaimW
  <2>8. ClaimW(s).car = w[s].car BY DEF ClaimW
  <2>9. ClaimW(s).bound = w[s].bound BY DEF ClaimW
  <2>10. ClaimW(s).cId = w[s].cId BY DEF ClaimW
  <2>11. ClaimW(s).cFold = w[s].cFold BY DEF ClaimW
  <2>12. ClaimW(s).nt = w[s].nt BY DEF ClaimW
  <2>13. ClaimW(s).inst = w[s].inst BY DEF ClaimW
  <2>14. ClaimW(s).uploads = w[s].uploads BY DEF ClaimW
  <2>15. ClaimW(s).deletes = w[s].deletes BY DEF ClaimW
  <2>16. ClaimW(s).snap = w[s].snap BY DEF ClaimW
  <2>17. ClaimW(s).lost = w[s].lost BY DEF ClaimW
  <2>ls. A1LeftSet(ClaimW(s)) = A1LeftSet(w[s]) BY DEF ClaimW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(ClaimW(s), w[s])
  <2>1. ClaimW(s).pc = "claimed" BY DEF ClaimW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA Verify_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Verify(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Verify, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>g. w[s].pc = "claimed" BY DEF Verify
<1>2. A1Same(VerifyW(s), w[s])
  <2>1. VerifyW(s).pend = w[s].pend BY DEF VerifyW
  <2>2. VerifyW(s).dset = w[s].dset BY DEF VerifyW
  <2>3. VerifyW(s).decl = w[s].decl BY DEF VerifyW
  <2>4. VerifyW(s).moved = w[s].moved BY DEF VerifyW
  <2>5. VerifyW(s).dMint = w[s].dMint BY DEF VerifyW
  <2>6. VerifyW(s).local = w[s].local BY DEF VerifyW
  <2>7. VerifyW(s).baseline = w[s].baseline BY DEF VerifyW
  <2>8. VerifyW(s).car = w[s].car BY DEF VerifyW
  <2>9. VerifyW(s).bound = w[s].bound BY DEF VerifyW
  <2>10. VerifyW(s).cId = w[s].cId BY DEF VerifyW
  <2>11. VerifyW(s).cFold = w[s].cFold BY DEF VerifyW
  <2>12. VerifyW(s).nt = w[s].nt BY DEF VerifyW
  <2>13. VerifyW(s).inst = w[s].inst BY DEF VerifyW
  <2>14. VerifyW(s).uploads = w[s].uploads BY DEF VerifyW
  <2>15. VerifyW(s).deletes = w[s].deletes BY DEF VerifyW
  <2>16. VerifyW(s).snap = w[s].snap BY DEF VerifyW
  <2>17. VerifyW(s).lost = w[s].lost BY DEF VerifyW
  <2>ls. A1LeftSet(VerifyW(s)) = A1LeftSet(w[s]) BY DEF VerifyW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(VerifyW(s), w[s])
  <2>1. VerifyW(s).pc = w[s].pc BY DEF VerifyW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA RPullRead_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, RPullRead(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF RPullRead, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>g. w[s].pc = "idle" BY DEF RPullRead
<1>2. A1Same(RPullReadW(s), w[s])
  <2>1. RPullReadW(s).pend = w[s].pend BY DEF RPullReadW
  <2>2. RPullReadW(s).dset = w[s].dset BY DEF RPullReadW
  <2>3. RPullReadW(s).decl = w[s].decl BY DEF RPullReadW
  <2>4. RPullReadW(s).moved = w[s].moved BY DEF RPullReadW
  <2>5. RPullReadW(s).dMint = w[s].dMint BY DEF RPullReadW
  <2>6. RPullReadW(s).local = w[s].local BY DEF RPullReadW
  <2>7. RPullReadW(s).baseline = w[s].baseline BY DEF RPullReadW
  <2>8. RPullReadW(s).car = w[s].car BY DEF RPullReadW
  <2>9. RPullReadW(s).bound = w[s].bound BY DEF RPullReadW
  <2>10. RPullReadW(s).cId = w[s].cId BY DEF RPullReadW
  <2>11. RPullReadW(s).cFold = w[s].cFold BY DEF RPullReadW
  <2>12. RPullReadW(s).nt = w[s].nt BY DEF RPullReadW
  <2>13. RPullReadW(s).inst = w[s].inst BY DEF RPullReadW
  <2>14. RPullReadW(s).uploads = w[s].uploads BY DEF RPullReadW
  <2>15. RPullReadW(s).deletes = w[s].deletes BY DEF RPullReadW
  <2>16. RPullReadW(s).snap = w[s].snap BY DEF RPullReadW
  <2>17. RPullReadW(s).lost = w[s].lost BY DEF RPullReadW
  <2>ls. A1LeftSet(RPullReadW(s)) = A1LeftSet(w[s]) BY DEF RPullReadW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(RPullReadW(s), w[s])
  <2>1. RPullReadW(s).pc = "pulling" BY DEF RPullReadW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA Honor_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Honor(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Honor, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>g. w[s].pc = "idle" BY DEF Honor
<1>2. A1Same(HonorW(s), w[s])
  <2>1. HonorW(s).pend = w[s].pend BY DEF HonorW
  <2>2. HonorW(s).dset = w[s].dset BY DEF HonorW
  <2>3. HonorW(s).decl = w[s].decl BY DEF HonorW
  <2>4. HonorW(s).moved = w[s].moved BY DEF HonorW
  <2>5. HonorW(s).dMint = w[s].dMint BY DEF HonorW
  <2>6. HonorW(s).local = w[s].local BY DEF HonorW
  <2>7. HonorW(s).baseline = w[s].baseline BY DEF HonorW
  <2>8. HonorW(s).car = w[s].car BY DEF HonorW
  <2>9. HonorW(s).bound = w[s].bound BY DEF HonorW
  <2>10. HonorW(s).cId = w[s].cId BY DEF HonorW
  <2>11. HonorW(s).cFold = w[s].cFold BY DEF HonorW
  <2>12. HonorW(s).nt = w[s].nt BY DEF HonorW
  <2>13. HonorW(s).inst = w[s].inst BY DEF HonorW
  <2>14. HonorW(s).uploads = w[s].uploads BY DEF HonorW
  <2>15. HonorW(s).deletes = w[s].deletes BY DEF HonorW
  <2>16. HonorW(s).snap = w[s].snap BY DEF HonorW
  <2>17. HonorW(s).lost = w[s].lost BY DEF HonorW
  <2>ls. A1LeftSet(HonorW(s)) = A1LeftSet(w[s]) BY DEF HonorW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(HonorW(s), w[s])
  <2>1. HonorW(s).pc = w[s].pc BY DEF HonorW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA HonorReader_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, HonorReader(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF HonorReader, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>g. w[s].pc = "idle" BY DEF HonorReader
<1>2. A1Same(HonorReaderW(s), w[s])
  <2>1. HonorReaderW(s).pend = w[s].pend BY DEF HonorReaderW
  <2>2. HonorReaderW(s).dset = w[s].dset BY DEF HonorReaderW
  <2>3. HonorReaderW(s).decl = w[s].decl BY DEF HonorReaderW
  <2>4. HonorReaderW(s).moved = w[s].moved BY DEF HonorReaderW
  <2>5. HonorReaderW(s).dMint = w[s].dMint BY DEF HonorReaderW
  <2>6. HonorReaderW(s).local = w[s].local BY DEF HonorReaderW
  <2>7. HonorReaderW(s).baseline = w[s].baseline BY DEF HonorReaderW
  <2>8. HonorReaderW(s).car = w[s].car BY DEF HonorReaderW
  <2>9. HonorReaderW(s).bound = w[s].bound BY DEF HonorReaderW
  <2>10. HonorReaderW(s).cId = w[s].cId BY DEF HonorReaderW
  <2>11. HonorReaderW(s).cFold = w[s].cFold BY DEF HonorReaderW
  <2>12. HonorReaderW(s).nt = w[s].nt BY DEF HonorReaderW
  <2>13. HonorReaderW(s).inst = w[s].inst BY DEF HonorReaderW
  <2>14. HonorReaderW(s).uploads = w[s].uploads BY DEF HonorReaderW
  <2>15. HonorReaderW(s).deletes = w[s].deletes BY DEF HonorReaderW
  <2>16. HonorReaderW(s).snap = w[s].snap BY DEF HonorReaderW
  <2>17. HonorReaderW(s).lost = w[s].lost BY DEF HonorReaderW
  <2>ls. A1LeftSet(HonorReaderW(s)) = A1LeftSet(w[s]) BY DEF HonorReaderW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(HonorReaderW(s), w[s])
  <2>1. HonorReaderW(s).pc = w[s].pc BY DEF HonorReaderW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

------------------------------------------------------------------------------
(* A step that ends a barrier without a bound write (Fence before the        *)
(* install, Restart): pc idle, the barrier's fields cleared, every field   *)
(* BoundOK, DTree and CPub3 read kept.                                     *)

A1Kept(R, W) ==
  /\ R.pend = W.pend /\ R.dset = W.dset /\ R.decl = W.decl /\ R.moved = W.moved /\ R.dMint = W.dMint
  /\ R.local = W.local /\ R.baseline = W.baseline /\ R.car = W.car /\ R.bound = W.bound
  /\ R.nt = W.nt /\ R.lost = W.lost /\ A1LeftSet(R) = A1LeftSet(W)
A1Cleared(R) == R.pc = "idle" /\ R.inst = [q \in Paths |-> Nil] /\ R.cId = 0 /\ R.cFold = 0

LEMMA A1Reset ==
  ASSUME IndC, A1IAK1, HStep, NEW s \in Writers, NEW R,
         w' = [w EXCEPT ![s] = R], A1Kept(R, w[s]), A1Cleared(R),
         conflicts' = conflicts, doc' = doc, live \subseteq live'
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>k. \A u \in Writers : A1Kept(w'[u], w[u]) BY <1>w DEF A1Kept
<1>o. \A u \in Writers \ {s} : w'[u] = w[u] BY <1>w
<1>n. w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>5. IndM5 BY DEF IndC
<1>ty. \A u \in Writers, q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>5, TreeTy DEF A1IAK1, A1DeclMinted, A1CarPubMinted
<1>c. \A u \in Writers, q \in Paths :
        /\ Content(w[u].local[q])' = Content(w[u].local[q])
        /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
        /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
        /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths
                PROVE  /\ Content(w[u].local[q])' = Content(w[u].local[q])
                       /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                       /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
                       /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
  <2>0. A1Kept(w'[u], w[u]) BY <1>k
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
  <2>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
  <2>5. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
  <2>6. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
        /\ Content(h5)' = Content(h5)
    BY A1ContentStep
  <2>7. /\ w'[u].local[q] = h1 /\ w'[u].baseline[q] = h2 /\ w'[u].decl[q] = h3 /\ w'[u].car.pub[q] = h5
    BY <2>0, <2>1, <2>2, <2>3, <2>5 DEF A1Kept
  <2>. QED BY <2>1, <2>2, <2>3, <2>5, <2>6, <2>7
\* The barrier conjuncts: s is idle (vacuous), every other tree is unchanged.
<1>i. \A u \in Writers, q \in Paths : w[u].inst[q] \in Opt(minted) BY DEF A1IAK1, A1InstMinted
<1>ic. \A u \in Writers \ {s}, q \in Paths : Content(w[u].inst[q])' = Content(w[u].inst[q])
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s}, NEW q \in Paths
                PROVE  Content(w[u].inst[q])' = Content(w[u].inst[q]) OBVIOUS
  <2>1. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>i
  <2>2. Content(h4)' = Content(h4) BY A1ContentStep
  <2>. QED BY <2>1, <2>2, <1>o
<1>1. A1Folds'
  <2>. SUFFICES ASSUME NEW u \in Writers
                PROVE  w'[u].pend.fold <= w'[u].nt /\ w'[u].bound.fold <= w'[u].nt /\ w'[u].cFold <= w'[u].nt
    BY DEF A1Folds
  <2>1. CASE u = s BY <2>1, <1>w, <1>n DEF A1IAK1, A1Folds, A1Kept, A1Cleared
  <2>2. CASE u # s BY <2>2, <1>o DEF A1IAK1, A1Folds
  <2>. QED BY <2>1, <2>2
<1>2. A1Frozen' /\ A1MidBar'
  BY <1>w, <1>o DEF A1IAK1, A1Frozen, A1MidBar, A1Cleared, A1Decl, Carries
<1>3. A1Cased' BY <1>w, <1>o, <1>c, <1>ic DEF A1IAK1, A1Cased, A1Cleared, A1Decl, CarForm, Carries
<1>4. A1DTree' BY <1>k, <1>c DEF A1IAK1, A1DTree, A1Kept, A1Decl
<1>6. A1CPub3' BY <1>k, <1>c DEF A1IAK1, A1CPub3, A1Kept, A1Decl, CarForm
<1>6l. A1CPubL' BY <1>k, <1>c DEF A1IAK1, A1CPubL, A1Kept, A1Decl, CarForm
\* DocCar reads doc (unchanged) and its content (kept: doc is minted).
<1>dc. A1DocCar'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths,
                       (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil
                        /\ Content(doc[q]) = Content(w[u].decl[q]))'
                PROVE  (w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car))'
    BY DEF A1DocCar
  <2>0. A1Kept(w'[u], w[u]) BY <1>k
  <2>1. PICK h \in Opt(minted) : h = doc[q] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
  <2>2. Content(doc[q])' = Content(doc[q]) BY <2>1, A1ContentStep
  <2>3. /\ A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil
        /\ Content(doc[q]) = Content(w[u].decl[q])
    BY <2>0, <2>2, <1>c DEF A1Kept, A1Decl
  <2>4. w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car) BY <2>3 DEF A1IAK1, A1DocCar
  <2>. QED BY <2>0, <2>4, <1>c DEF A1Kept, CarForm
<1>7. A1DeclMinted' /\ A1BoundMinted' /\ A1CarPubMinted' /\ A1ConflictsMinted'
  BY <1>k, <1>m DEF A1IAK1, A1DeclMinted, A1BoundMinted, A1CarPubMinted, A1ConflictsMinted, A1Kept, Opt
<1>8. A1InstMinted' BY <1>w, <1>m DEF A1IAK1, A1InstMinted, A1Cleared, Opt
<1>9. BoundOK'
  <2>. SUFFICES ASSUME NEW u \in Writers, Honorable(u)', NEW p \in Paths,
                       Broken(u, p, w[u].bound.doc)'
                PROVE  p \in HDropped(u)'
    BY DEF BoundOK
  <2>s. A1Kept(w'[u], w[u]) BY <1>k
  <2>l. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>k DEF A1Kept
  <2>0. PICK d \in [Paths -> Opt(minted)] : d = w[u].bound.doc BY DEF A1IAK1, A1BoundMinted
  <2>1. Broken(u, p, d)' BY <2>0, <2>s DEF A1Kept
  <2>2. Broken(u, p, d)
    BY <2>0, <2>1, <2>s, <2>l, A1BrokenKeep DEF A1IAK1, A1DeclMinted, A1Kept
  <2>3. Honorable(u) BY <2>s DEF Honorable, A1Kept
  <2>. QED BY <2>0, <2>2, <2>3, <2>s DEF A1IAK1, BoundOK, HDropped, A1Kept
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>6, <1>6l, <1>7, <1>dc, <1>8, <1>9 DEF A1IAK1

LEMMA Restart_A1 ==
  ASSUME IndC, IndC', A1IAK1, A1KeepCited, NEW s \in Writers, Restart(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Restart
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>2. A1Kept(RestartW(s), w[s])
  <2>1. RestartW(s).pend = w[s].pend BY DEF RestartW
  <2>2. RestartW(s).dset = w[s].dset BY DEF RestartW
  <2>3. RestartW(s).decl = w[s].decl BY DEF RestartW
  <2>4. RestartW(s).moved = w[s].moved BY DEF RestartW
  <2>5. RestartW(s).dMint = w[s].dMint BY DEF RestartW
  <2>6. RestartW(s).local = w[s].local BY DEF RestartW
  <2>7. RestartW(s).baseline = w[s].baseline BY DEF RestartW
  <2>8. RestartW(s).car = w[s].car BY DEF RestartW
  <2>9. RestartW(s).bound = w[s].bound BY DEF RestartW
  <2>10. RestartW(s).nt = w[s].nt BY DEF RestartW
  <2>11. RestartW(s).lost = w[s].lost BY DEF RestartW
  \* mid -> saved, sKeep cleared: the same leftover.  A mid one is uncited (R4);
  \* a saved one was not kept (a kept path is cited, A1KeepCited).
  <2>ls. A1LeftSet(RestartW(s)) = A1LeftSet(w[s])
    <3>0. w[s].sDrop \subseteq Paths /\ w[s].sStage \in {"none", "saved", "mid"} BY <1>0, WriterFields DEF TypeOK
    <3>1. /\ RestartW(s).sDrop = w[s].sDrop /\ RestartW(s).sHeld = w[s].sHeld /\ RestartW(s).local = w[s].local
          /\ RestartW(s).baseline = w[s].baseline /\ RestartW(s).sKeep = {}
          /\ RestartW(s).sStage = IF w[s].sStage = "none" THEN "none" ELSE "saved" BY DEF RestartW
    <3>2. w[s].sStage = "mid" => \A q \in w[s].sDrop \ w[s].sKeep : w[s].baseline[q] = Nil
      BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R4
    <3>3. \A q \in Paths : w[s].sStage = "mid" /\ q \in w[s].sKeep => w[s].baseline[q] # Nil BY DEF A1KeepCited
    <3>. QED BY <3>0, <3>1, <3>2, <3>3 DEF A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>ls DEF A1Kept
<1>3. A1Cleared(RestartW(s))
  <2>1. RestartW(s).pc = "idle" BY DEF RestartW
  <2>2. RestartW(s).inst = [q \in Paths |-> Nil] BY DEF RestartW
  <2>3. RestartW(s).cId = 0 /\ RestartW(s).cFold = 0 BY DEF RestartW
  <2> QED BY <2>1, <2>2, <2>3 DEF A1Cleared
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Reset

\* Fence: before the install a reset; after it the fields are kept (A1Keep).
LEMMA Fence_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Fence(s), Frame
  PROVE  A1IAK1'
<1>u. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Fence, bucket, aux
<1>hs. HStep BY <1>u, HStepQ DEF Frame
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>2. A1Kept(FenceBeforeW(s), w[s])
    <3>1. FenceBeforeW(s).pend = w[s].pend BY DEF FenceBeforeW
    <3>2. FenceBeforeW(s).dset = w[s].dset BY DEF FenceBeforeW
    <3>3. FenceBeforeW(s).decl = w[s].decl BY DEF FenceBeforeW
    <3>4. FenceBeforeW(s).moved = w[s].moved BY DEF FenceBeforeW
    <3>5. FenceBeforeW(s).dMint = w[s].dMint BY DEF FenceBeforeW
    <3>6. FenceBeforeW(s).local = w[s].local BY DEF FenceBeforeW
    <3>7. FenceBeforeW(s).baseline = w[s].baseline BY DEF FenceBeforeW
    <3>8. FenceBeforeW(s).car = w[s].car BY DEF FenceBeforeW
    <3>9. FenceBeforeW(s).bound = w[s].bound BY DEF FenceBeforeW
    <3>10. FenceBeforeW(s).nt = w[s].nt BY DEF FenceBeforeW
    <3>11. FenceBeforeW(s).lost = w[s].lost BY DEF FenceBeforeW
    <3>ls. A1LeftSet(FenceBeforeW(s)) = A1LeftSet(w[s]) BY DEF FenceBeforeW, A1LeftSet
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10, <3>11, <3>ls DEF A1Kept
  <2>3. A1Cleared(FenceBeforeW(s))
    <3>1. FenceBeforeW(s).pc = "idle" BY DEF FenceBeforeW
    <3>2. FenceBeforeW(s).inst = [q \in Paths |-> Nil] BY DEF FenceBeforeW
    <3>3. FenceBeforeW(s).cId = 0 /\ FenceBeforeW(s).cFold = 0 BY DEF FenceBeforeW
    <3> QED BY <3>1, <3>2, <3>3 DEF A1Cleared
  <2>. QED BY <2>1, <2>2, <2>3, <1>u, <1>hs, A1Reset
<1>2. CASE w[s].pc # "claimed"
  <2>0. w[s].pc = "cased" BY <1>2 DEF Fence
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>2. A1Same(FenceAfterW(s), w[s])
    <3>1. FenceAfterW(s).pend = w[s].pend BY DEF FenceAfterW
    <3>2. FenceAfterW(s).dset = w[s].dset BY DEF FenceAfterW
    <3>3. FenceAfterW(s).decl = w[s].decl BY DEF FenceAfterW
    <3>4. FenceAfterW(s).moved = w[s].moved BY DEF FenceAfterW
    <3>5. FenceAfterW(s).dMint = w[s].dMint BY DEF FenceAfterW
    <3>6. FenceAfterW(s).local = w[s].local BY DEF FenceAfterW
    <3>7. FenceAfterW(s).baseline = w[s].baseline BY DEF FenceAfterW
    <3>8. FenceAfterW(s).car = w[s].car BY DEF FenceAfterW
    <3>9. FenceAfterW(s).bound = w[s].bound BY DEF FenceAfterW
    <3>10. FenceAfterW(s).cId = w[s].cId BY DEF FenceAfterW
    <3>11. FenceAfterW(s).cFold = w[s].cFold BY DEF FenceAfterW
    <3>12. FenceAfterW(s).nt = w[s].nt BY DEF FenceAfterW
    <3>13. FenceAfterW(s).inst = w[s].inst BY DEF FenceAfterW
    <3>14. FenceAfterW(s).uploads = w[s].uploads BY DEF FenceAfterW
    <3>15. FenceAfterW(s).deletes = w[s].deletes BY DEF FenceAfterW
    <3>16. FenceAfterW(s).snap = w[s].snap BY DEF FenceAfterW
    <3>17. FenceAfterW(s).lost = w[s].lost BY DEF FenceAfterW
    <3>ls. A1LeftSet(FenceAfterW(s)) = A1LeftSet(w[s]) BY DEF FenceAfterW, A1LeftSet
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10, <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>ls DEF A1Same
  <2>3. A1PcOK(FenceAfterW(s), w[s])
    <3>1. FenceAfterW(s).pc = w[s].pc /\ FenceAfterW(s).gone = w[s].gone /\ FenceAfterW(s).upDone = w[s].upDone BY DEF FenceAfterW
    <3> QED BY <2>0, <3>1 DEF A1PcOK
  <2>. QED BY <2>1, <2>2, <2>3, <1>u, <1>hs, A1Keep
<1>. QED BY <1>1, <1>2

LEMMA RescopeBegin_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  A1IAK1'
<1>0. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>u. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF RescopeBegin, bucket
<1>hs. HStep BY <1>u, HStepQ DEF Frame
<1>g. w[s].pc = "idle" BY DEF RescopeBegin
<1>2. A1Same(RescopeBeginW(s, T), w[s])
  <2>1. RescopeBeginW(s, T).pend = w[s].pend BY DEF RescopeBeginW
  <2>2. RescopeBeginW(s, T).dset = w[s].dset BY DEF RescopeBeginW
  <2>3. RescopeBeginW(s, T).decl = w[s].decl BY DEF RescopeBeginW
  <2>4. RescopeBeginW(s, T).moved = w[s].moved BY DEF RescopeBeginW
  <2>5. RescopeBeginW(s, T).dMint = w[s].dMint BY DEF RescopeBeginW
  <2>6. RescopeBeginW(s, T).local = w[s].local BY DEF RescopeBeginW
  <2>7. RescopeBeginW(s, T).baseline = w[s].baseline BY DEF RescopeBeginW
  <2>8. RescopeBeginW(s, T).car = w[s].car BY DEF RescopeBeginW
  <2>9. RescopeBeginW(s, T).bound = w[s].bound BY DEF RescopeBeginW
  <2>10. RescopeBeginW(s, T).cId = w[s].cId BY DEF RescopeBeginW
  <2>11. RescopeBeginW(s, T).cFold = w[s].cFold BY DEF RescopeBeginW
  <2>12. RescopeBeginW(s, T).nt = w[s].nt BY DEF RescopeBeginW
  <2>13. RescopeBeginW(s, T).inst = w[s].inst BY DEF RescopeBeginW
  <2>14. RescopeBeginW(s, T).uploads = w[s].uploads BY DEF RescopeBeginW
  <2>15. RescopeBeginW(s, T).deletes = w[s].deletes BY DEF RescopeBeginW
  <2>16. RescopeBeginW(s, T).snap = w[s].snap BY DEF RescopeBeginW
  <2>17. RescopeBeginW(s, T).lost = w[s].lost BY DEF RescopeBeginW
  <2>ls. A1LeftSet(RescopeBeginW(s, T)) = A1LeftSet(w[s])
    <3>a. w[s].sStage = "none" BY DEF RescopeBegin
    <3>b. A1LeftSet(w[s]) = {} BY <3>a DEF A1LeftSet
    <3>c. RescopeBeginW(s, T).sHeld = [p \in Paths |-> Nil] BY DEF RescopeBeginW
    <3>e. RescopeBeginW(s, T).sDrop \subseteq Paths BY DEF RescopeBeginW, Leaving
    <3>d. A1LeftSet(RescopeBeginW(s, T)) = {} BY <3>c, <3>e DEF A1LeftSet
    <3>. QED BY <3>b, <3>d
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(RescopeBeginW(s, T), w[s])
  <2>1. RescopeBeginW(s, T).pc = w[s].pc BY DEF RescopeBeginW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>0, <1>u, <1>hs, <1>2, <1>3, A1Keep

------------------------------------------------------------------------------
(* A1Keep with the per-tree fact as the hypothesis (a step writing two trees). *)

LEMMA A1KeepF ==
  ASSUME IndC, A1IAK1, HStep,
         \A u \in Writers : A1Same(w'[u], w[u]) /\ A1PcOK(w'[u], w[u]),
         conflicts' = conflicts, doc' = doc, live \subseteq live'
  PROVE  A1IAK1'
<1>f. \A u \in Writers : A1Same(w'[u], w[u]) /\ A1PcOK(w'[u], w[u]) OBVIOUS
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>5. IndM5 BY DEF IndC
<1>ty. \A u \in Writers, q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].inst[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>5, TreeTy DEF A1IAK1, A1DeclMinted, A1InstMinted, A1CarPubMinted
<1>c. \A u \in Writers, q \in Paths :
        /\ Content(w[u].local[q])' = Content(w[u].local[q])
        /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
        /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
        /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
        /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths
                PROVE  /\ Content(w[u].local[q])' = Content(w[u].local[q])
                       /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                       /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
                       /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
                       /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
  <2>0. A1Same(w'[u], w[u]) BY <1>f
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
  <2>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
  <2>4. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>ty
  <2>5. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
  <2>6. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
        /\ Content(h4)' = Content(h4) /\ Content(h5)' = Content(h5)
    BY A1ContentStep
  <2>7. /\ w'[u].local[q] = h1 /\ w'[u].baseline[q] = h2 /\ w'[u].decl[q] = h3
        /\ w'[u].inst[q] = h4 /\ w'[u].car.pub[q] = h5
    BY <2>0, <2>1, <2>2, <2>3, <2>4, <2>5 DEF A1Same
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7
<1>1. A1Folds' BY <1>f DEF A1IAK1, A1Folds, A1Same
<1>2. A1Frozen' BY <1>f DEF A1IAK1, A1Frozen, A1Same, A1PcOK
<1>3. A1MidBar' BY <1>f DEF A1IAK1, A1MidBar, A1Same, A1PcOK, A1Decl, Carries
<1>4. A1DTree' BY <1>f, <1>c DEF A1IAK1, A1DTree, A1Same, A1Decl
<1>6. A1CPub3' BY <1>f, <1>c DEF A1IAK1, A1CPub3, A1Same, A1Decl, CarForm
<1>6l. A1CPubL' BY <1>f, <1>c DEF A1IAK1, A1CPubL, A1Same, A1Decl, CarForm
<1>7. A1Cased' BY <1>f, <1>c DEF A1IAK1, A1Cased, A1Same, A1PcOK, A1Decl, CarForm, Carries
\* DocCar reads doc (unchanged) and its content (kept: doc is minted).
<1>dc. A1DocCar'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths,
                       (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil
                        /\ Content(doc[q]) = Content(w[u].decl[q]))'
                PROVE  (w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car))'
    BY DEF A1DocCar
  <2>0. A1Same(w'[u], w[u]) BY <1>f
  <2>1. PICK h \in Opt(minted) : h = doc[q] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
  <2>2. Content(doc[q])' = Content(doc[q]) BY <2>1, A1ContentStep
  <2>3. /\ A1Decl(u, q) /\ w[u].decl[q] # Nil /\ doc[q] # Nil
        /\ Content(doc[q]) = Content(w[u].decl[q])
    BY <2>0, <2>2, <1>c DEF A1Same, A1Decl
  <2>4. w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car) BY <2>3 DEF A1IAK1, A1DocCar
  <2>. QED BY <2>0, <2>4, <1>c DEF A1Same, CarForm
<1>8. A1DeclMinted' /\ A1BoundMinted' /\ A1InstMinted' /\ A1CarPubMinted' /\ A1ConflictsMinted'
  BY <1>f, <1>m DEF A1IAK1, A1DeclMinted, A1BoundMinted, A1InstMinted, A1CarPubMinted, A1ConflictsMinted, A1Same, Opt
<1>9. BoundOK'
  <2>. SUFFICES ASSUME NEW u \in Writers, Honorable(u)', NEW p \in Paths,
                       Broken(u, p, w[u].bound.doc)'
                PROVE  p \in HDropped(u)'
    BY DEF BoundOK
  <2>s. A1Same(w'[u], w[u]) BY <1>f
  <2>l. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>f DEF A1Same
  <2>0. PICK d \in [Paths -> Opt(minted)] : d = w[u].bound.doc BY DEF A1IAK1, A1BoundMinted
  <2>1. Broken(u, p, d)' BY <2>0, <2>s DEF A1Same
  <2>2. Broken(u, p, d)
    BY <2>0, <2>1, <2>s, <2>l, A1BrokenKeep DEF A1IAK1, A1DeclMinted, A1Same
  <2>3. Honorable(u) BY <2>s DEF Honorable, A1Same
  <2>. QED BY <2>0, <2>2, <2>3, <2>s DEF A1IAK1, BoundOK, HDropped, A1Same
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>6, <1>6l, <1>7, <1>dc, <1>8, <1>9 DEF A1IAK1
LEMMA Depose_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW t \in Writers, Depose(t), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY DEF Depose, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>2. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>3. A1Same(DeposeW(t), w[t])
  <2>1. DeposeW(t).pend = w[t].pend BY DEF DeposeW
  <2>2. DeposeW(t).dset = w[t].dset BY DEF DeposeW
  <2>3. DeposeW(t).decl = w[t].decl BY DEF DeposeW
  <2>4. DeposeW(t).moved = w[t].moved BY DEF DeposeW
  <2>5. DeposeW(t).dMint = w[t].dMint BY DEF DeposeW
  <2>6. DeposeW(t).local = w[t].local BY DEF DeposeW
  <2>7. DeposeW(t).baseline = w[t].baseline BY DEF DeposeW
  <2>8. DeposeW(t).car = w[t].car BY DEF DeposeW
  <2>9. DeposeW(t).bound = w[t].bound BY DEF DeposeW
  <2>10. DeposeW(t).cId = w[t].cId BY DEF DeposeW
  <2>11. DeposeW(t).cFold = w[t].cFold BY DEF DeposeW
  <2>12. DeposeW(t).nt = w[t].nt BY DEF DeposeW
  <2>13. DeposeW(t).inst = w[t].inst BY DEF DeposeW
  <2>14. DeposeW(t).uploads = w[t].uploads BY DEF DeposeW
  <2>15. DeposeW(t).deletes = w[t].deletes BY DEF DeposeW
  <2>16. DeposeW(t).snap = w[t].snap BY DEF DeposeW
  <2>17. DeposeW(t).lost = w[t].lost BY DEF DeposeW
  <2>ls. A1LeftSet(DeposeW(t)) = A1LeftSet(w[t]) BY DEF DeposeW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>4. A1Same(DeposedW(holder), w[holder])
  <2>1. DeposedW(holder).pend = w[holder].pend BY DEF DeposedW
  <2>2. DeposedW(holder).dset = w[holder].dset BY DEF DeposedW
  <2>3. DeposedW(holder).decl = w[holder].decl BY DEF DeposedW
  <2>4. DeposedW(holder).moved = w[holder].moved BY DEF DeposedW
  <2>5. DeposedW(holder).dMint = w[holder].dMint BY DEF DeposedW
  <2>6. DeposedW(holder).local = w[holder].local BY DEF DeposedW
  <2>7. DeposedW(holder).baseline = w[holder].baseline BY DEF DeposedW
  <2>8. DeposedW(holder).car = w[holder].car BY DEF DeposedW
  <2>9. DeposedW(holder).bound = w[holder].bound BY DEF DeposedW
  <2>10. DeposedW(holder).cId = w[holder].cId BY DEF DeposedW
  <2>11. DeposedW(holder).cFold = w[holder].cFold BY DEF DeposedW
  <2>12. DeposedW(holder).nt = w[holder].nt BY DEF DeposedW
  <2>13. DeposedW(holder).inst = w[holder].inst BY DEF DeposedW
  <2>14. DeposedW(holder).uploads = w[holder].uploads BY DEF DeposedW
  <2>15. DeposedW(holder).deletes = w[holder].deletes BY DEF DeposedW
  <2>16. DeposedW(holder).snap = w[holder].snap BY DEF DeposedW
  <2>17. DeposedW(holder).lost = w[holder].lost BY DEF DeposedW
  <2>ls. A1LeftSet(DeposedW(holder)) = A1LeftSet(w[holder]) BY DEF DeposedW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>5. DeposeW(t).pc = "claimed" /\ DeposedW(holder).pc = w[holder].pc /\ DeposedW(holder).gone = w[holder].gone
      /\ DeposedW(holder).upDone = w[holder].upDone
  BY DEF DeposeW, DeposedW
<1>6. \A u \in Writers : A1Same(w'[u], w[u]) /\ A1PcOK(w'[u], w[u])
  BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF A1Same, A1PcOK
<1>. QED BY <1>1, <1>hs, <1>6, A1KeepF

\* Upload's first arm keeps every field AK1 reads (upDone only) and adds a live
\* handle; the copy arm (a fresh copy handle in the tree) is UploadCopy_A1.
LEMMA UploadFirst_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame,
         w[s].snap[p] \notin upped
  PROVE  A1IAK1'
<1>1. /\ w' = [w EXCEPT ![s] = UploadW(s, p)] /\ live \subseteq live'
      /\ UNCHANGED <<minted, base, conflicts, orig, doc>>
  BY DEF Upload
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>g. w[s].pc = "scanned" BY DEF Upload
<1>2. A1Same(UploadW(s, p), w[s])
  <2>1. UploadW(s, p).pend = w[s].pend BY DEF UploadW
  <2>2. UploadW(s, p).dset = w[s].dset BY DEF UploadW
  <2>3. UploadW(s, p).decl = w[s].decl BY DEF UploadW
  <2>4. UploadW(s, p).moved = w[s].moved BY DEF UploadW
  <2>5. UploadW(s, p).dMint = w[s].dMint BY DEF UploadW
  <2>6. UploadW(s, p).local = w[s].local BY DEF UploadW
  <2>7. UploadW(s, p).baseline = w[s].baseline BY DEF UploadW
  <2>8. UploadW(s, p).car = w[s].car BY DEF UploadW
  <2>9. UploadW(s, p).bound = w[s].bound BY DEF UploadW
  <2>10. UploadW(s, p).cId = w[s].cId BY DEF UploadW
  <2>11. UploadW(s, p).cFold = w[s].cFold BY DEF UploadW
  <2>12. UploadW(s, p).nt = w[s].nt BY DEF UploadW
  <2>13. UploadW(s, p).inst = w[s].inst BY DEF UploadW
  <2>14. UploadW(s, p).uploads = w[s].uploads BY DEF UploadW
  <2>15. UploadW(s, p).deletes = w[s].deletes BY DEF UploadW
  <2>16. UploadW(s, p).snap = w[s].snap BY DEF UploadW
  <2>17. UploadW(s, p).lost = w[s].lost BY DEF UploadW
  <2>ls. A1LeftSet(UploadW(s, p)) = A1LeftSet(w[s]) BY DEF UploadW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(UploadW(s, p), w[s])
  <2>1. UploadW(s, p).pc = w[s].pc BY DEF UploadW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

\* The sweep spares every handle a conflict preserves: SurfacedDecl's live
\* arm survives it (Reap has no such guard: see AK1-PLAN, class L).
LEMMA Sweep_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  A1IAK1'
<1>1. /\ w' = w /\ live' = live \ {h} /\ ~Preserved(h)
      /\ UNCHANGED <<minted, base, conflicts, orig, doc>>
  BY DEF Sweep, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>2. \A c \in conflicts : c[2] \in live => c[2] \in live' BY <1>1 DEF Preserved
<1>. QED BY <1>1, <1>hs, <1>2, A1NoTree

\* Age and RLoad: no tree, no conflict, no live handle changes, anc kept.
LEMMA A1Quiet ==
  ASSUME IndC, IndC', A1IAK1, UNCHANGED <<anc, minted, base, orig, conflicts, w, live, doc>>
  PROVE  A1IAK1'
<1>u. AncUpdate BY AncUpdateSame DEF IndC
<1>hs. HStep BY <1>u, HStepQ
<1>. QED BY <1>hs, A1NoTree

------------------------------------------------------------------------------
(* The invariant writer by writer: a step writing tree s argues only about s; *)
(* every other tree is unchanged (A1Frame).                                *)

\* A tree's per-path facts outside the barrier: DTree, CPub3 and the typing.
A1TreePath(u, p) ==
  /\ (A1Decl(u, p) /\ w[u].local[p] # w[u].baseline[p] /\ p \notin A1LeftSet(w[u])) =>
       Content(w[u].local[p]) = Content(w[u].decl[p])
  /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ w[u].local[p] = w[u].baseline[p]) =>
       w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
  /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ p \in A1LeftSet(w[u])) =>
       w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
  /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p])) =>
       w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
  /\ w[u].decl[p] \in Opt(minted) /\ w[u].inst[p] \in Opt(minted) /\ w[u].car.pub[p] \in Opt(minted)

A1Per(u) ==
  /\ w[u].pend.fold <= w[u].nt /\ w[u].bound.fold <= w[u].nt /\ w[u].cFold <= w[u].nt
  /\ w[u].pc \in {"consumed", "scanned", "claimed", "cased"} => w[u].cId = w[u].pend.id /\ w[u].cFold = w[u].pend.fold
  /\ \A p \in Paths :
       /\ (w[u].pc \in {"scanned", "claimed", "cased"} /\ w[u].cId # 0 /\ A1Decl(u, p) /\ w[u].local[p] # w[u].baseline[p]) =>
            \/ p \in w[u].uploads /\ w[u].snap[p] = w[u].local[p]
            \/ p \in w[u].deletes /\ w[u].local[p] = Nil
       /\ (w[u].pc \in {"scanned", "claimed", "cased"} /\ w[u].cId # 0 /\ A1Decl(u, p)) =>
            /\ p \in w[u].uploads => w[u].snap[p] = w[u].local[p]
            /\ p \in w[u].deletes => w[u].local[p] = Nil
       /\ (w[u].pc = "cased" /\ w[u].cId # 0 /\ A1Decl(u, p) /\ w[u].decl[p] # Nil) =>
            \/ w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
            \/ p \in w[u].gone /\ p \notin w[u].upDone
       /\ (A1Decl(u, p) /\ w[u].local[p] # w[u].baseline[p] /\ p \notin A1LeftSet(w[u])) =>
            Content(w[u].local[p]) = Content(w[u].decl[p])
       /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ w[u].local[p] = w[u].baseline[p]) =>
            w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
       /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ p \in A1LeftSet(w[u])) =>
            w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
       /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p])) =>
            w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
       /\ w[u].decl[p] \in Opt(minted) /\ w[u].inst[p] \in Opt(minted) /\ w[u].car.pub[p] \in Opt(minted)
  /\ w[u].bound.doc \in [Paths -> Opt(minted)]
  /\ Honorable(u) => \A p \in Paths : Broken(u, p, w[u].bound.doc) => p \in HDropped(u)

LEMMA A1PerAll == A1IAK1 <=> (\A u \in Writers : A1Per(u)) /\ A1ConflictsMinted
BY DEF A1IAK1, A1Per, A1Folds, A1Frozen, A1MidBar, A1Cased, A1DTree, A1CPub3, A1CPubL, A1DocCar, A1DeclMinted,
       A1BoundMinted, A1InstMinted, A1CarPubMinted, BoundOK, Carries

\* An unchanged tree keeps its part, when Content and Derives are kept (HStep),
\* conflicts only grow, a preserved handle stays live and no lost handle is added.
LEMMA A1Frame ==
  ASSUME IndC, A1IAK1, HStep, NEW u \in Writers, w'[u] = w[u],
         A1ConflictsMinted, conflicts \subseteq conflicts', doc' = doc,
         \A c \in conflicts : c[2] \in live => c[2] \in live' \/ c[2] \notin UNION {w[t].lost : t \in Writers},
         UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers}
  PROVE  A1Per(u)'
<1>0. A1Per(u) BY A1PerAll
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>5. IndM5 BY DEF IndC
<1>ty. \A q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].inst[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>0, <1>5, TreeTy DEF A1Per
<1>c. \A q \in Paths :
        /\ Content(w[u].local[q])' = Content(w[u].local[q])
        /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
        /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
        /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
        /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
  <2>. SUFFICES ASSUME NEW q \in Paths
                PROVE  /\ Content(w[u].local[q])' = Content(w[u].local[q])
                       /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                       /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
                       /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
                       /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
  <2>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
  <2>4. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>ty
  <2>5. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
  <2>6. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
        /\ Content(h4)' = Content(h4) /\ Content(h5)' = Content(h5)
    BY A1ContentStep
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6
<1>dk. \A q \in Paths : Content(doc[q])' = Content(doc[q]) BY A1DocContent
<1>b. Honorable(u)' => \A p \in Paths : Broken(u, p, w[u].bound.doc)' => p \in HDropped(u)'
  <2>. SUFFICES ASSUME Honorable(u)', NEW p \in Paths, Broken(u, p, w[u].bound.doc)'
                PROVE  p \in HDropped(u)' OBVIOUS
  <2>0. PICK d \in [Paths -> Opt(minted)] : d = w[u].bound.doc BY <1>0 DEF A1Per
  <2>1. Broken(u, p, d)' BY <2>0
  <2>2. Broken(u, p, d) BY <2>0, <2>1, <1>0, A1BrokenKeep DEF A1Per
  <2>3. Honorable(u) BY DEF Honorable
  <2>. QED BY <1>0, <2>0, <2>2, <2>3 DEF A1Per, HDropped
<1>. QED BY <1>0, <1>c, <1>m, <1>b, <1>dk DEF A1Per, A1Decl, CarForm, Honorable, HDropped, Opt

\* The same, when the document's handles change (each kept, cleared, or a minted
\* handle not carrying this tree's declared bytes unless it is in carrier form) and
\* lost grows by handles no conflict names (Install, for every other tree).
A1DD(u, q) == \/ doc'[q] = doc[q] \/ doc'[q] = Nil
              \/ \E h \in minted : /\ doc'[q] = h
                                   /\ \/ ~A1Decl(u, q) \/ w[u].decl[q] = Nil
                                      \/ w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)
                                      \/ Content(h) # Content(w[u].decl[q])
LEMMA A1FrameD ==
  ASSUME IndC, A1IAK1, HStep, NEW u \in Writers, w'[u] = w[u],
         A1ConflictsMinted, conflicts \subseteq conflicts', \A q \in Paths : A1DD(u, q),
         \A c \in conflicts : c[2] \in live => c[2] \in live' \/ c[2] \notin UNION {w[t].lost : t \in Writers},
         \A c \in conflicts : c[2] \in UNION {w'[t].lost : t \in Writers} => c[2] \in UNION {w[t].lost : t \in Writers}
  PROVE  A1Per(u)'
<1>0. A1Per(u) BY A1PerAll
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>5. IndM5 BY DEF IndC
<1>ty. \A q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].inst[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>0, <1>5, TreeTy DEF A1Per
<1>c. \A q \in Paths :
        /\ Content(w[u].local[q])' = Content(w[u].local[q])
        /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
        /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
        /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
        /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
  <2>. SUFFICES ASSUME NEW q \in Paths
                PROVE  /\ Content(w[u].local[q])' = Content(w[u].local[q])
                       /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                       /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
                       /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
                       /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
  <2>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
  <2>4. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>ty
  <2>5. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
  <2>6. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
        /\ Content(h4)' = Content(h4) /\ Content(h5)' = Content(h5)
    BY A1ContentStep
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6
<1>b. Honorable(u)' => \A p \in Paths : Broken(u, p, w[u].bound.doc)' => p \in HDropped(u)'
  <2>. SUFFICES ASSUME Honorable(u)', NEW p \in Paths, Broken(u, p, w[u].bound.doc)'
                PROVE  p \in HDropped(u)' OBVIOUS
  <2>0. PICK d \in [Paths -> Opt(minted)] : d = w[u].bound.doc BY <1>0 DEF A1Per
  <2>1. Broken(u, p, d)' BY <2>0
  <2>2. Broken(u, p, d) BY <2>0, <2>1, <1>0, A1BrokenKeepL DEF A1Per
  <2>3. Honorable(u) BY DEF Honorable
  <2>. QED BY <1>0, <2>0, <2>2, <2>3 DEF A1Per, HDropped
\* DocCar: per path, by A1DD's arms
<1>dc. \A p \in Paths : (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p]))' =>
                         (w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car))'
  <2>. SUFFICES ASSUME NEW p \in Paths,
                       (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p]))'
                PROVE  (w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car))' OBVIOUS
  <2>0. PICK c0 : c0 = w[u].car OBVIOUS
  <2>cf. CarForm(u, p, c0)' <=> CarForm(u, p, c0) BY <2>0, <1>c DEF CarForm
  <2>d. A1Decl(u, p) /\ w[u].decl[p] # Nil BY DEF A1Decl
  <2>1. CASE doc'[p] = doc[p]
    <3>1. Content(doc[p])' = Content(doc[p]) BY <2>1, A1DocContent
    <3>2. (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p])) =>
            w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
      BY <1>0 DEF A1Per
    <3>3. doc[p] # Nil /\ Content(doc[p]) = Content(w[u].decl[p]) BY <2>1, <3>1, <1>c
    <3>. QED BY <3>2, <3>3, <2>d, <2>0, <2>cf
  <2>2. CASE doc'[p] = Nil BY <2>2
  <2>3. CASE \E h \in minted : /\ doc'[p] = h
                               /\ \/ ~A1Decl(u, p) \/ w[u].decl[p] = Nil
                                  \/ w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
                                  \/ Content(h) # Content(w[u].decl[p])
    <3>1. PICK h \in minted : /\ doc'[p] = h
                              /\ \/ ~A1Decl(u, p) \/ w[u].decl[p] = Nil
                                 \/ w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
                                 \/ Content(h) # Content(w[u].decl[p]) BY <2>3
    <3>2. Content(h)' = Content(h) BY <3>1, A1ContentStep DEF Opt
    <3>. QED BY <3>1, <3>2, <2>d, <2>0, <2>cf, <1>c
  <2>. QED BY <2>1, <2>2, <2>3 DEF A1DD
<1>. QED BY <1>0, <1>c, <1>m, <1>b, <1>dc DEF A1Per, A1Decl, CarForm, Honorable, HDropped, Opt

LEMMA A1PerAllP == (\A u \in Writers : A1Per(u)') /\ A1ConflictsMinted' => A1IAK1'
BY DEF A1IAK1, A1Per, A1Folds, A1Frozen, A1MidBar, A1Cased, A1DTree, A1CPub3, A1CPubL, A1DocCar, A1DeclMinted,
       A1BoundMinted, A1InstMinted, A1CarPubMinted, BoundOK, Carries

\* A part's folds and frozen record, without expanding all of A1Per (a goal
\* that does times out).
LEMMA A1FoldsOf ==
  ASSUME NEW u \in Writers, A1Per(u)
  PROVE  w[u].pend.fold <= w[u].nt /\ w[u].bound.fold <= w[u].nt /\ w[u].cFold <= w[u].nt
BY DEF A1Per
LEMMA A1FrozenOf ==
  ASSUME NEW u \in Writers, A1Per(u)
  PROVE  w[u].pc \in {"consumed", "scanned", "claimed", "cased"} => w[u].cId = w[u].pend.id /\ w[u].cFold = w[u].pend.fold
BY DEF A1Per

\* Every step keeps Content of every tree handle the conjuncts read, for each
\* field the step keeps.  (Content(w[u].local[q])' primes w too: it is the new
\* content of the NEW handle, so it equals the old one only where the field
\* is kept -- an unconditional form is false.)
LEMMA A1ContentAll ==
  ASSUME IndC, A1IAK1, HStep
  PROVE  \A u \in Writers, q \in Paths :
           /\ w'[u].local[q] = w[u].local[q] => Content(w[u].local[q])' = Content(w[u].local[q])
           /\ w'[u].baseline[q] = w[u].baseline[q] => Content(w[u].baseline[q])' = Content(w[u].baseline[q])
           /\ w'[u].decl[q] = w[u].decl[q] => Content(w[u].decl[q])' = Content(w[u].decl[q])
           /\ w'[u].inst[q] = w[u].inst[q] => Content(w[u].inst[q])' = Content(w[u].inst[q])
           /\ w'[u].car.pub[q] = w[u].car.pub[q] => Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
<1>5. IndM5 BY DEF IndC
<1>ty. \A u \in Writers, q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].inst[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>5, TreeTy DEF A1IAK1, A1DeclMinted, A1InstMinted, A1CarPubMinted
<1>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths
              PROVE  /\ w'[u].local[q] = w[u].local[q] => Content(w[u].local[q])' = Content(w[u].local[q])
                     /\ w'[u].baseline[q] = w[u].baseline[q] => Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                     /\ w'[u].decl[q] = w[u].decl[q] => Content(w[u].decl[q])' = Content(w[u].decl[q])
                     /\ w'[u].inst[q] = w[u].inst[q] => Content(w[u].inst[q])' = Content(w[u].inst[q])
                     /\ w'[u].car.pub[q] = w[u].car.pub[q] => Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
<1>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
<1>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
<1>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
<1>4. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>ty
<1>6. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
<1>7. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
      /\ Content(h4)' = Content(h4) /\ Content(h5)' = Content(h5)
  BY A1ContentStep
<1>a. w'[u].local[q] = w[u].local[q] => Content(w[u].local[q])' = Content(w[u].local[q]) BY <1>1, <1>7
<1>b. w'[u].baseline[q] = w[u].baseline[q] => Content(w[u].baseline[q])' = Content(w[u].baseline[q]) BY <1>2, <1>7
<1>c. w'[u].decl[q] = w[u].decl[q] => Content(w[u].decl[q])' = Content(w[u].decl[q]) BY <1>3, <1>7
<1>d. w'[u].inst[q] = w[u].inst[q] => Content(w[u].inst[q])' = Content(w[u].inst[q]) BY <1>4, <1>7
<1>e. w'[u].car.pub[q] = w[u].car.pub[q] => Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) BY <1>6, <1>7
<1>. QED BY <1>a, <1>b, <1>c, <1>d, <1>e

------------------------------------------------------------------------------
(* The touch: a new declaration, at the tree's bytes, of its dirty paths;   *)
(* the honour then waits for the next barrier end (the fold moved).        *)

\* TouchDirty's membership, out of Touch_A1's large context
LEMMA TouchDirtyMem ==
  ASSUME NEW s, NEW p \in TouchDirty(s)
  PROVE  /\ w[s].local[p] # w[s].baseline[p] /\ Content(w[s].local[p]) # Content(doc[p])
         /\ ~RescopeLeftover(s, p)
BY DEF TouchDirty

LEMMA Touch_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Touch(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY DEF Touch, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>w. \A u \in Writers : w'[u] = IF u = s THEN TouchW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>g. w[s].pc = "idle" BY DEF Touch
<1>sh. RescopeUnciteFirst /\ RescopeKeepsDirty /\ WidenKeepsLocal /\ UnlinkChecksBytes
  BY ShippedShape DEF Shipped
<1>per. A1Per(s) BY A1PerAll
<1>n. w[s].nt \in Nat /\ TouchN(s) = w[s].nt + 1 BY <1>0, WriterFields DEF TypeOK, TouchN
<1>fx. /\ TouchW(s).pc = w[s].pc /\ TouchW(s).local = w[s].local /\ TouchW(s).baseline = w[s].baseline
       /\ TouchW(s).car = w[s].car /\ TouchW(s).bound = w[s].bound /\ TouchW(s).lost = w[s].lost
       /\ TouchW(s).cFold = w[s].cFold /\ TouchW(s).inst = w[s].inst
  <2>1. TouchW(s).pc = w[s].pc BY DEF TouchW
  <2>2. TouchW(s).local = w[s].local BY DEF TouchW
  <2>3. TouchW(s).baseline = w[s].baseline BY DEF TouchW
  <2>4. TouchW(s).car = w[s].car BY DEF TouchW
  <2>5. TouchW(s).bound = w[s].bound BY DEF TouchW
  <2>6. TouchW(s).lost = w[s].lost BY DEF TouchW
  <2>7. TouchW(s).cFold = w[s].cFold BY DEF TouchW
  <2>8. TouchW(s).inst = w[s].inst BY DEF TouchW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8
<1>fn. /\ TouchW(s).nt = TouchN(s)
       /\ TouchW(s).pend.fold = TouchN(s)
       /\ TouchW(s).pend.id = IF TouchFresh(s) THEN TouchN(s) ELSE w[s].pend.id
  <2>1. TouchW(s).nt = TouchN(s) BY DEF TouchW
  \* one goal per field (the whole pend record times out)
  <2>2. TouchW(s).pend.fold = TouchN(s) BY DEF TouchW
  <2>3. TouchW(s).pend.id = IF TouchFresh(s) THEN TouchN(s) ELSE w[s].pend.id BY DEF TouchW
  <2>. QED BY <2>1, <2>2, <2>3
<1>fd. /\ TouchW(s).dset = (IF TouchFresh(s) THEN {} ELSE w[s].dset) \cup TouchDirty(s)
       /\ TouchW(s).decl = [p \in Paths |-> IF p \in TouchDirty(s) THEN w[s].local[p]
                                           ELSE IF TouchFresh(s) THEN Nil ELSE w[s].decl[p]]
       /\ TouchW(s).moved = (IF TouchFresh(s) THEN {} ELSE w[s].moved) \ TouchDirty(s)
  <2>1. TouchW(s).dset = (IF TouchFresh(s) THEN {} ELSE w[s].dset) \cup TouchDirty(s) BY DEF TouchW
  <2>2. TouchW(s).decl = [p \in Paths |-> IF p \in TouchDirty(s) THEN w[s].local[p]
                                         ELSE IF TouchFresh(s) THEN Nil ELSE w[s].decl[p]] BY DEF TouchW
  <2>3. TouchW(s).moved = (IF TouchFresh(s) THEN {} ELSE w[s].moved) \ TouchDirty(s) BY DEF TouchW
  <2>. QED BY <2>1, <2>2, <2>3
\* what a declared path is, one fact (TouchDirty read once)
<1>td. \A p \in TouchDirty(s) :
         /\ w[s].local[p] # w[s].baseline[p] /\ Content(w[s].local[p]) # Content(doc[p])
         /\ ~RescopeLeftover(s, p)
  <2>. SUFFICES ASSUME NEW p \in TouchDirty(s)
                PROVE  /\ w[s].local[p] # w[s].baseline[p] /\ Content(w[s].local[p]) # Content(doc[p])
                       /\ ~RescopeLeftover(s, p) OBVIOUS
  <2>. QED BY TouchDirtyMem
<1>dy. TouchDirty(s) \subseteq {p \in Paths : w[s].local[p] # w[s].baseline[p]}
  <2>1. TouchDirty(s) \subseteq Paths BY DEF TouchDirty
  <2>. QED BY <2>1, <1>td
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lm. \A q \in Paths : w[s].local[q] \in Opt(minted) BY TreeTy DEF IndC
<1>tl. A1LeftSet(TouchW(s)) = A1LeftSet(w[s]) BY DEF TouchW, A1LeftSet
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
\* Every other tree keeps its part.
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>1. w'[u] = w[u] BY <1>w
  <2>2. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
  <2>. QED BY <1>1, <1>hs, <2>1, <2>2, A1Frame DEF A1IAK1
\* s's part.
<1>s. A1Per(s)'
  <2>w. w'[s] = TouchW(s) BY <1>w
  \* the old folds alone, then arithmetic (expanding A1Per here times out)
  <2>0. w[s].bound.fold <= w[s].nt /\ w[s].cFold <= w[s].nt BY <1>per DEF A1Per
  <2>t. w[s].bound.fold \in Nat /\ w[s].cFold \in Nat BY <1>0, WriterFields DEF TypeOK, BoundT
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <2>0, <2>t, <1>n, <1>fx, <1>fn
  <2>2. w'[s].pc = "idle" BY <2>w, <1>g, <1>fx
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>1. CASE p \in TouchDirty(s)
      \* declared now, at the tree's bytes; dirty, so not clean
      <4>1. w'[s].decl[p] = w[s].local[p] /\ w'[s].local[p] = w[s].local[p] /\ w'[s].local[p] # w'[s].baseline[p]
        BY <3>1, <2>w, <1>fx, <1>fd, <1>dy
      <4>2. w'[s].inst[p] = w[s].inst[p] /\ w'[s].car = w[s].car BY <2>w, <1>fx
      \* DocCar: Touch declares only bytes the document does not hold
      <4>3. Content(w[s].local[p]) # Content(doc[p]) BY <3>1, <1>td
      \* not a leftover: Touch spares the narrow's (A1LeftSet's handle is the held one)
      <4>l1. ~RescopeLeftover(s, p) BY <3>1, <1>td
      <4>l2. p \notin A1LeftSet(w[s])
            <5>. SUFFICES ASSUME p \in A1LeftSet(w[s]) PROVE FALSE OBVIOUS
            <5>a. /\ p \in w[s].sDrop /\ w[s].local[p] # Nil /\ w[s].sHeld[p] # Nil /\ w[s].local[p] = w[s].sHeld[p]
                  /\ \/ w[s].sStage = "mid" /\ p \notin w[s].sKeep
                     \/ w[s].sStage = "saved" /\ w[s].baseline[p] = Nil
              BY DEF A1LeftSet
            <5>b. Content(w[s].local[p]) = Content(w[s].sHeld[p]) BY <5>a
            <5>c. RescopeLeftover(s, p) BY <5>a, <5>b, <1>sh, SMTT(300) DEF RescopeLeftover
            <5>. QED BY <5>c, <4>l1
      <4>l. p \notin A1LeftSet(w'[s]) BY <4>l2, <1>tl, <2>w
      <4>. QED BY <4>1, <4>2, <4>3, <4>l, <1>per, <1>lm, <1>m, <1>c, <1>dk, <1>1 DEF A1TreePath, A1Per, A1Decl, Opt
    <3>2. CASE p \notin TouchDirty(s)
      <4>1. A1Decl(s, p)' => ~TouchFresh(s) /\ A1Decl(s, p)
        BY <3>2, <2>w, <1>fn, <1>fd DEF A1Decl, TouchFresh
      <4>2. ~TouchFresh(s) => w'[s].decl[p] = w[s].decl[p] /\ w'[s].pend.id = w[s].pend.id
        BY <3>2, <2>w, <1>fn, <1>fd
      <4>3. TouchFresh(s) => w'[s].decl[p] = Nil BY <3>2, <2>w, <1>fd
      <4>4. w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
            /\ w'[s].car = w[s].car /\ w'[s].inst[p] = w[s].inst[p]
        BY <2>w, <1>fx
      \* one goal per conjunct (the whole A1TreePath goal times out)
      <4>5. A1Decl(s, p)' => /\ A1Decl(s, p) /\ w'[s].decl[p] = w[s].decl[p] /\ w'[s].pend.id = w[s].pend.id
        BY <4>1, <4>2
      <4>o. /\ (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s])) =>
                 Content(w[s].local[p]) = Content(w[s].decl[p])
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p]) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s])) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p])) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            /\ w[s].decl[p] \in Opt(minted) /\ w[s].inst[p] \in Opt(minted) /\ w[s].car.pub[p] \in Opt(minted)
        BY <1>per DEF A1Per
      \* the carrier as a constant (CarForm's prime would double-prime w'[s].car)
      <4>c0. PICK c0 : c0 = w[s].car OBVIOUS
      <4>cf. A1Decl(s, p)' => (CarForm(s, p, c0)' <=> CarForm(s, p, c0))
        BY <4>5, <4>4, <4>c0, <1>c, <2>w, <1>fx DEF CarForm
      <4>a. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <4>5, <4>4, <4>o, <1>c, <1>tl, <2>w
      <4>b. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <4>5, <4>4, <4>o, <4>c0, <4>cf
      <4>d. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <4>5, <4>4, <4>o, <4>c0, <4>cf, <1>dk, <1>1, <1>c
      <4>l. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <4>5, <4>4, <4>o, <4>c0, <4>cf, <1>tl, <2>w
      <4>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
        BY <4>2, <4>3, <4>4, <4>o, <1>m DEF Opt
      <4>. QED BY <4>a, <4>b, <4>l, <4>d, <4>t DEF A1TreePath
    <3>. QED BY <3>1, <3>2
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
  <2>5. ~Honorable(s)'
    <3>1. w'[s].bound.fold = w[s].bound.fold /\ w[s].bound.fold <= w[s].nt BY <2>w, <1>fx, <1>per DEF A1Per
    <3>2. w'[s].pend.fold = w[s].nt + 1 BY <2>w, <1>fn, <1>n
    <3>. QED BY <3>1, <3>2, <1>n DEF Honorable
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

------------------------------------------------------------------------------
(* The retire: the record is gone (id 0), so nothing is declared and the     *)
(* honour cannot fire; every other field is kept.                          *)

LEMMA RetireByAck_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY DEF RetireByAck, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>w. \A u \in Writers : w'[u] = IF u = s THEN RetireByAckW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>g. w[s].pc = "idle" BY DEF RetireByAck
<1>per. A1Per(s) BY A1PerAll
<1>n. w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>fx. /\ RetireByAckW(s).pc = w[s].pc /\ RetireByAckW(s).nt = w[s].nt /\ RetireByAckW(s).bound = w[s].bound
       /\ RetireByAckW(s).cFold = w[s].cFold /\ RetireByAckW(s).lost = w[s].lost
       /\ RetireByAckW(s).decl = w[s].decl /\ RetireByAckW(s).inst = w[s].inst /\ RetireByAckW(s).car = w[s].car
       /\ RetireByAckW(s).pend = NoPend
  <2>1. RetireByAckW(s).pc = w[s].pc BY DEF RetireByAckW
  <2>2. RetireByAckW(s).nt = w[s].nt BY DEF RetireByAckW
  <2>3. RetireByAckW(s).bound = w[s].bound BY DEF RetireByAckW
  <2>4. RetireByAckW(s).cFold = w[s].cFold BY DEF RetireByAckW
  <2>5. RetireByAckW(s).lost = w[s].lost BY DEF RetireByAckW
  <2>6. RetireByAckW(s).decl = w[s].decl BY DEF RetireByAckW
  <2>7. RetireByAckW(s).inst = w[s].inst BY DEF RetireByAckW
  <2>8. RetireByAckW(s).car = w[s].car BY DEF RetireByAckW
  <2>9. RetireByAckW(s).pend = NoPend BY DEF RetireByAckW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>1. w'[u] = w[u] BY <1>w
  <2>2. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
  <2>. QED BY <1>1, <1>hs, <2>1, <2>2, A1Frame DEF A1IAK1
<1>s. A1Per(s)'
  <2>w. w'[s] = RetireByAckW(s) BY <1>w
  <2>0. w'[s].pend.id = 0 /\ w'[s].pend.fold = 0 BY <2>w, <1>fx DEF NoPend
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <2>0, <1>per, A1FoldsOf, <1>n, <1>fx
  <2>2. w'[s].pc = "idle" BY <2>w, <1>g, <1>fx
  <2>3. \A p \in Paths : ~A1Decl(s, p)' BY <2>0 DEF A1Decl
  <2>4. \A p \in Paths : w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
    BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
  <2>5. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
  <2>6. ~Honorable(s)' BY <2>0 DEF Honorable
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF A1Per
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

------------------------------------------------------------------------------
(* Two helpers for a step that writes tree u but keeps what a part reads.   *)

A1BoundFields(R, W) ==
  /\ R.pend = W.pend /\ R.dset = W.dset /\ R.decl = W.decl /\ R.moved = W.moved /\ R.dMint = W.dMint
  /\ R.bound = W.bound /\ R.car = W.car
A1TreeFields(R, W) ==
  /\ R.pend = W.pend /\ R.dset = W.dset /\ R.decl = W.decl /\ R.moved = W.moved
  /\ R.local = W.local /\ R.baseline = W.baseline /\ R.car = W.car /\ R.inst = W.inst
  /\ A1LeftSet(R) = A1LeftSet(W)

LEMMA A1BoundKeep ==
  ASSUME IndC, A1IAK1, HStep, NEW u \in Writers, A1BoundFields(w'[u], w[u]),
         A1ConflictsMinted, conflicts \subseteq conflicts', doc' = doc,
         \A c \in conflicts : c[2] \in live => c[2] \in live' \/ c[2] \notin UNION {w[t].lost : t \in Writers},
         UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers}
  PROVE  (Honorable(u) => \A p \in Paths : Broken(u, p, w[u].bound.doc) => p \in HDropped(u))'
<1>0. A1Per(u) BY A1PerAll
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>. SUFFICES ASSUME Honorable(u)', NEW p \in Paths, Broken(u, p, w[u].bound.doc)'
              PROVE  p \in HDropped(u)' OBVIOUS
<1>1. PICK d \in [Paths -> Opt(minted)] : d = w[u].bound.doc BY <1>0 DEF A1Per
<1>2. Broken(u, p, d)' BY <1>1 DEF A1BoundFields
<1>3. Broken(u, p, d) BY <1>1, <1>2, <1>0, A1BrokenKeep DEF A1Per, A1BoundFields
<1>4. Honorable(u) BY DEF Honorable, A1BoundFields
<1>. QED BY <1>0, <1>1, <1>3, <1>4 DEF A1Per, HDropped, A1BoundFields

LEMMA A1TreeKeep ==
  ASSUME IndC, A1IAK1, HStep, NEW u \in Writers, A1TreeFields(w'[u], w[u]), doc' = doc
  PROVE  \A p \in Paths : A1TreePath(u, p)'
<1>0. A1Per(u) BY A1PerAll
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>c. \A q \in Paths :
        /\ (w'[u].local[q] = w[u].local[q] => Content(w[u].local[q])' = Content(w[u].local[q]))
        /\ (w'[u].baseline[q] = w[u].baseline[q] => Content(w[u].baseline[q])' = Content(w[u].baseline[q]))
        /\ (w'[u].decl[q] = w[u].decl[q] => Content(w[u].decl[q])' = Content(w[u].decl[q]))
        /\ (w'[u].car.pub[q] = w[u].car.pub[q] => Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]))
  BY A1ContentAll
<1>. QED BY <1>0, <1>c, <1>m, A1DocContent DEF A1Per, A1TreePath, A1TreeFields, A1Decl, CarForm, Opt

------------------------------------------------------------------------------
(* The scan: a carrying one publishes every dirty declared path (MidBar).   *)

LEMMA Scan_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Scan(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>d. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) :
        w' = [w EXCEPT ![s] = ScanW(s, dels)]
  BY DEF Scan
<1>1. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Scan, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
<1>w. \A u \in Writers : w'[u] = IF u = s THEN ScanW(s, dels) ELSE w[u] BY <1>0, <1>d DEF TypeOK
<1>g. w[s].pc = "consumed" BY DEF Scan
<1>per. A1Per(s) BY A1PerAll
<1>f. /\ ScanW(s, dels).pc = "scanned"
      /\ ScanW(s, dels).uploads = ScanUps(s) /\ ScanW(s, dels).deletes = dels
      /\ ScanW(s, dels).snap = w[s].local
      /\ ScanW(s, dels).pend = w[s].pend
      /\ ScanW(s, dels).dset = w[s].dset
      /\ ScanW(s, dels).decl = w[s].decl
      /\ ScanW(s, dels).moved = w[s].moved
      /\ ScanW(s, dels).dMint = w[s].dMint
      /\ ScanW(s, dels).local = w[s].local
      /\ ScanW(s, dels).baseline = w[s].baseline
      /\ ScanW(s, dels).car = w[s].car
      /\ ScanW(s, dels).bound = w[s].bound
      /\ ScanW(s, dels).cId = w[s].cId
      /\ ScanW(s, dels).cFold = w[s].cFold
      /\ ScanW(s, dels).nt = w[s].nt
      /\ ScanW(s, dels).inst = w[s].inst
      /\ ScanW(s, dels).lost = w[s].lost
  <2>1. ScanW(s, dels).pc = "scanned" BY DEF ScanW
  <2>2. ScanW(s, dels).uploads = ScanUps(s) /\ ScanW(s, dels).deletes = dels BY DEF ScanW
  <2>3. ScanW(s, dels).snap = w[s].local BY DEF ScanW
  <2>4. ScanW(s, dels).pend = w[s].pend BY DEF ScanW
  <2>5. ScanW(s, dels).dset = w[s].dset BY DEF ScanW
  <2>6. ScanW(s, dels).decl = w[s].decl BY DEF ScanW
  <2>7. ScanW(s, dels).moved = w[s].moved BY DEF ScanW
  <2>8. ScanW(s, dels).dMint = w[s].dMint BY DEF ScanW
  <2>9. ScanW(s, dels).local = w[s].local BY DEF ScanW
  <2>10. ScanW(s, dels).baseline = w[s].baseline BY DEF ScanW
  <2>11. ScanW(s, dels).car = w[s].car BY DEF ScanW
  <2>12. ScanW(s, dels).bound = w[s].bound BY DEF ScanW
  <2>13. ScanW(s, dels).cId = w[s].cId BY DEF ScanW
  <2>14. ScanW(s, dels).cFold = w[s].cFold BY DEF ScanW
  <2>15. ScanW(s, dels).nt = w[s].nt BY DEF ScanW
  <2>16. ScanW(s, dels).inst = w[s].inst BY DEF ScanW
  <2>17. ScanW(s, dels).lost = w[s].lost BY DEF ScanW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>f
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
<1>s. A1Per(s)'
  <2>w. w'[s] = ScanW(s, dels) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>f, <1>per, A1FoldsOf
  <2>2. w'[s].cId = w'[s].pend.id /\ w'[s].cFold = w'[s].pend.fold BY <2>w, <1>f, <1>g, <1>per, A1FrozenOf
  <2>3. \A p \in Paths :
          (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] =>
             \/ p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p]
             \/ p \in w[s].deletes /\ w[s].local[p] = Nil)'
    <3>. SUFFICES ASSUME NEW p \in Paths, w'[s].cId # 0, A1Decl(s, p)', w'[s].local[p] # w'[s].baseline[p]
                  PROVE  \/ p \in w'[s].uploads /\ w'[s].snap[p] = w'[s].local[p]
                         \/ p \in w'[s].deletes /\ w'[s].local[p] = Nil
      BY <2>w, <1>f
    <3>1. Carries(s) /\ dels = ScanAbsent(s) BY <2>w, <1>f, <1>d DEF Carries
    <3>2. p \in ScanDirty(s) BY <2>w, <1>f DEF ScanDirty
    <3>3. CASE w[s].local[p] # Nil BY <3>2, <3>3, <2>w, <1>f DEF ScanUps
    <3>4. CASE w[s].local[p] = Nil BY <3>1, <3>2, <3>4, <2>w, <1>f DEF ScanAbsent
    <3>. QED BY <3>3, <3>4
  <2>3b. \A p \in Paths :
           (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p) =>
              /\ p \in w[s].uploads => w[s].snap[p] = w[s].local[p]
              /\ p \in w[s].deletes => w[s].local[p] = Nil)'
    <3>1. dels \subseteq ScanAbsent(s) BY <1>d
    <3>. QED BY <3>1, <2>w, <1>f DEF ScanAbsent
  <2>4. \A p \in Paths : A1TreePath(s, p)'
    <3>0. A1LeftSet(ScanW(s, dels)) = A1LeftSet(w[s]) BY DEF ScanW, A1LeftSet
    <3>1. A1TreeFields(w'[s], w[s]) BY <3>0, <2>w, <1>f DEF A1TreeFields
    <3>. QED BY <3>1, <1>1, <1>hs, A1TreeKeep
  <2>5. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>f, <1>per, <1>hs DEF A1Per, Opt, HStep, HistEv
  <2>6. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>f DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, A1BoundKeep DEF A1IAK1
  <2>. QED BY <2>1, <2>2, <2>3, <2>3b, <2>4, <2>5, <2>6, <2>w, <1>f, <1>dk DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>hs DEF A1IAK1, A1ConflictsMinted, Opt, HStep, HistEv
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

------------------------------------------------------------------------------
(* The finish: the barrier's end binds the install (bound.doc = inst, wd =  *)
(* gone) and the carrier's uncited set against it.  A declared file is in   *)
(* carrier form (UncitedCatches), or the install carries it, or it was     *)
(* withheld (wd); a declared delete is never broken.  The baseline moves   *)
(* only to the tree's bytes (MidBar's snap = local), so DTree and CPub3    *)
(* hold by the old DTree.                                                  *)

LEMMA Finish_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Finish(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY DEF Finish, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
<1>w. \A u \in Writers : w'[u] = IF u = s THEN FinishW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>g. w[s].pc = "cased" BY DEF Finish
<1>per. A1Per(s) BY A1PerAll
\* no leftover in a barrier (R1: the rescope is not in flight)
<1>st. w[s].sStage = "none" BY <1>g DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>nl2. A1LeftSet(w[s]) = {} /\ A1LeftSet(w'[s]) = {}
  <2>1. FinishW(s).sStage = w[s].sStage BY DEF FinishW
  <2>. QED BY <1>st, <2>1, <1>w DEF A1LeftSet
<1>W. w[s] \in Writer BY <1>0 DEF TypeOK
<1>n. w[s].nt \in Nat /\ w[s].inst \in [Paths -> Opt(Handles)] BY <1>W, WriterFields
<1>z. w[s].cId = w[s].pend.id /\ w[s].cFold = w[s].pend.fold BY <1>g, <1>per DEF A1Per
<1>fx. /\ FinishW(s).pc = "idle" /\ FinishW(s).local = w[s].local
       /\ FinishW(s).pend = w[s].pend /\ FinishW(s).dset = w[s].dset /\ FinishW(s).decl = w[s].decl
       /\ FinishW(s).moved = w[s].moved /\ FinishW(s).dMint = w[s].dMint
       /\ FinishW(s).nt = w[s].nt /\ FinishW(s).lost = w[s].lost /\ FinishW(s).inst = w[s].inst
       /\ FinishW(s).cId = 0 /\ FinishW(s).cFold = 0
  <2>1. FinishW(s).pc = "idle" /\ FinishW(s).local = w[s].local BY DEF FinishW
  <2>2. FinishW(s).pend = w[s].pend /\ FinishW(s).dset = w[s].dset /\ FinishW(s).decl = w[s].decl BY DEF FinishW
  <2>3. FinishW(s).moved = w[s].moved /\ FinishW(s).dMint = w[s].dMint BY DEF FinishW
  <2>4. FinishW(s).nt = w[s].nt /\ FinishW(s).lost = w[s].lost /\ FinishW(s).inst = w[s].inst BY DEF FinishW
  <2>5. FinishW(s).cId = 0 /\ FinishW(s).cFold = 0 BY DEF FinishW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5
<1>fb. FinishW(s).baseline = [p \in Paths |->
                               IF p \in w[s].uploads \cap w[s].upDone THEN w[s].snap[p]
                               ELSE IF p \in w[s].deletes /\ w[s].inst[p] = Nil THEN Nil
                               ELSE w[s].baseline[p]]
  BY DEF FinishW
<1>fc. FinishW(s).car = IF w[s].car.id # 0 THEN CarUncited(w[s].car, w[s].inst) ELSE w[s].car
  BY DEF FinishW
<1>fd. /\ w[s].cId # 0 => /\ FinishW(s).bound.id = w[s].cId /\ FinishW(s).bound.fold = w[s].cFold
                        /\ FinishW(s).bound.doc = w[s].inst /\ FinishW(s).bound.wd = w[s].gone
       /\ w[s].cId = 0 => FinishW(s).bound = w[s].bound
  <2>1. w[s].cId # 0 => FinishW(s).bound.id = w[s].cId /\ FinishW(s).bound.fold = w[s].cFold BY DEF FinishW
  <2>2. w[s].cId # 0 => FinishW(s).bound.doc = w[s].inst BY DEF FinishW
  <2>3. w[s].cId # 0 => FinishW(s).bound.wd = w[s].gone BY DEF FinishW
  <2>4. w[s].cId = 0 => FinishW(s).bound = w[s].bound BY DEF FinishW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
\* The carrier keeps every field CarForm reads; its id is kept.
<1>cr. /\ FinishW(s).car.id = w[s].car.id /\ FinishW(s).car.paths = w[s].car.paths
       /\ FinishW(s).car.deletes = w[s].car.deletes /\ FinishW(s).car.dropped = w[s].car.dropped
       /\ FinishW(s).car.pub = w[s].car.pub
  BY <1>fc DEF CarUncited
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
\* A declared path's baseline moves only to the tree's bytes.
<1>bm. \A p \in Paths : A1Decl(s, p) /\ FinishW(s).baseline[p] # w[s].baseline[p] =>
                        FinishW(s).baseline[p] = w[s].local[p]
  <2>. SUFFICES ASSUME NEW p \in Paths, A1Decl(s, p), FinishW(s).baseline[p] # w[s].baseline[p]
                PROVE  FinishW(s).baseline[p] = w[s].local[p] OBVIOUS
  <2>0. w[s].cId # 0 BY <1>z DEF A1Decl
  <2>1. CASE p \in w[s].uploads \cap w[s].upDone
    <3>1. w[s].snap[p] = w[s].local[p] BY <2>0, <2>1, <1>g, <1>per DEF A1Per
    <3>. QED BY <2>1, <3>1, <1>fb
  <2>2. CASE p \notin w[s].uploads \cap w[s].upDone /\ p \in w[s].deletes /\ w[s].inst[p] = Nil
    <3>1. w[s].local[p] = Nil BY <2>0, <2>2, <1>g, <1>per DEF A1Per
    <3>. QED BY <2>2, <3>1, <1>fb
  <2>. QED BY <2>1, <2>2, <1>fb
<1>s. A1Per(s)'
  <2>w. w'[s] = FinishW(s) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    <3>1. w'[s].bound.fold = w[s].cFold \/ w'[s].bound.fold = w[s].bound.fold BY <2>w, <1>fd
    <3>. QED BY <3>1, <2>w, <1>fx, <1>n, <1>per, A1FoldsOf
  <2>2. w'[s].pc = "idle" BY <2>w, <1>fx
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>fx DEF A1Decl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>fx, <1>cr, <1>per, <1>m DEF A1Per, Opt
    <3>1. CASE FinishW(s).baseline[p] = w[s].baseline[p]
      \* the carriers as constants (CarForm's prime would double-prime w'[s].car)
      <4>0. PICK c0, c1 : c0 = w[s].car /\ c1 = FinishW(s).car OBVIOUS
      <4>1. CarForm(s, p, c1)' <=> CarForm(s, p, c0)'
        BY <4>0, <1>cr DEF CarForm
      <4>2. CarForm(s, p, c0)' <=> CarForm(s, p, c0)
        BY <2>w, <1>fx, <1>cr, <1>c, <4>0 DEF CarForm
      <4>. QED BY <3>1, <3>d, <3>t, <4>0, <4>1, <4>2, <2>w, <1>fx, <1>cr, <1>c, <1>per, <1>dk, <1>nl2 DEF A1TreePath, A1Per
    <3>2. CASE FinishW(s).baseline[p] # w[s].baseline[p]
      \* declared: baseline' = local, so not dirty; clean: the old tree was dirty
      \* (else the baseline did not move), DTree gives the declared bytes.
      <4>. SUFFICES ASSUME A1Decl(s, p) PROVE A1TreePath(s, p)'
        BY <3>d, <3>t, <2>w, <1>fx, <1>dk, <1>nl2 DEF A1TreePath
      <4>1. w'[s].baseline[p] = w[s].local[p] /\ w'[s].local[p] = w[s].local[p] BY <3>2, <1>bm, <2>w, <1>fx
      <4>2. w[s].local[p] # w[s].baseline[p] BY <3>2, <1>bm
      <4>3. Content(w[s].local[p]) = Content(w[s].decl[p]) BY <4>2, <1>per, <1>nl2 DEF A1Per
      \* a declared FILE whose baseline moved was published, not withheld (the
      \* install took a withheld one out of upDone): carrier form (Cased), kept
      <4>4. w[s].decl[p] # Nil => w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
        <5>. SUFFICES ASSUME w[s].decl[p] # Nil PROVE w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car) OBVIOUS
        <5>1. w[s].local[p] # Nil BY <4>3 DEF Content
        <5>2. p \in w[s].uploads \cap w[s].upDone BY <3>2, <5>1, <1>fb, <1>bm
        <5>3. w[s].cId # 0 BY <1>z DEF A1Decl
        <5>4. \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
              \/ p \in w[s].gone /\ p \notin w[s].upDone
          BY <5>3, <1>g, <1>per DEF A1Per
        <5>. QED BY <5>2, <5>4
      <4>0. PICK c0, c1 : c0 = w[s].car /\ c1 = FinishW(s).car OBVIOUS
      <4>5. CarForm(s, p, c1)' <=> CarForm(s, p, c0)' BY <4>0, <1>cr DEF CarForm
      <4>6. CarForm(s, p, c0)' <=> CarForm(s, p, c0) BY <2>w, <1>fx, <1>cr, <1>c, <4>0 DEF CarForm
      <4>b. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <4>4, <4>0, <4>5, <4>6, <3>d, <2>w, <1>fx, <1>cr
      <4>dc. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
               (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <4>4, <4>0, <4>5, <4>6, <3>d, <2>w, <1>fx, <1>cr
      <4>. QED BY <4>1, <4>3, <4>b, <4>dc, <3>d, <3>t, <2>w, <1>fx, <1>c, <1>dk, <1>nl2 DEF A1TreePath
    <3>. QED BY <3>1, <3>2
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')]
    <3>1. CASE w[s].cId # 0
      <4>1. w'[s].bound.doc = w[s].inst BY <3>1, <2>w, <1>fd
      <4>. QED BY <4>1, <1>n, <1>per, <1>m DEF A1Per, Opt
    <3>2. CASE w[s].cId = 0 BY <3>2, <2>w, <1>fd, <1>per, <1>m DEF A1Per, Opt
    <3>. QED BY <3>1, <3>2
  <2>5. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>. SUFFICES ASSUME Honorable(s)', NEW p \in Paths, Broken(s, p, w[s].bound.doc)'
                  PROVE  p \in HDropped(s)' OBVIOUS
    <3>0. w[s].pend.id # 0 BY <2>w, <1>fx DEF Honorable
    <3>1. w[s].cId # 0 BY <3>0, <1>z
    <3>2. w'[s].bound.doc = w[s].inst /\ w'[s].bound.wd = w[s].gone BY <3>1, <2>w, <1>fd
    <3>3. PICK d \in [Paths -> Opt(minted)] : d = w[s].inst BY <1>n, <1>per DEF A1Per
    <3>4. Broken(s, p, d)' BY <3>2, <3>3
    <3>5. Broken(s, p, d)
      BY <3>3, <3>4, <1>1, <1>hs, <1>lost, <2>w, <1>fx, <1>per, A1BrokenKeep DEF A1IAK1, A1Per
    <3>6. A1Decl(s, p) BY <3>0, <3>5 DEF A1Decl, Broken
    <3>7. w[s].decl[p] # Nil
      \* a declared delete is never broken: present is a later version, absent carries it
      <4>. SUFFICES ASSUME w[s].decl[p] = Nil PROVE FALSE OBVIOUS
      <4>1. d[p] = Nil BY <3>5 DEF Broken, LaterVersion
      <4>. QED BY <4>1, <3>5 DEF Broken, Content
    <3>8. HDropped(s)' = w[s].gone \cup (IF w[s].car.id = w[s].pend.id THEN w[s].car.dropped \cup Uncited(w[s].car, w[s].inst) ELSE {})
      <4>1. w[s].car.id = w[s].pend.id => w'[s].car = CarUncited(w[s].car, w[s].inst) BY <3>0, <2>w, <1>fc
      <4>. QED BY <4>1, <3>2, <2>w, <1>fx, <1>cr DEF HDropped, CarUncited
    <3>9. \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
          \/ Content(w[s].inst[p]) = Content(w[s].decl[p])
          \/ p \in w[s].gone
      BY <3>1, <3>6, <3>7, <1>g, <1>per DEF A1Per
    <3>10. CASE w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
      <4>1. p \in w[s].car.dropped \cup Uncited(w[s].car, d) BY <3>10, <3>5, UncitedCatches
      <4>. QED BY <4>1, <3>3, <3>8, <3>10
    <3>11. CASE Content(w[s].inst[p]) = Content(w[s].decl[p]) BY <3>11, <3>3, <3>5 DEF Broken
    <3>12. CASE p \in w[s].gone BY <3>12, <3>8
    <3>. QED BY <3>9, <3>10, <3>11, <3>12
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <1>dk, <1>nl2 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

------------------------------------------------------------------------------
(* The collect: under RetireAge the retired handles wait for the age (live   *)
(* kept); only retire and collected change.                                *)

LEMMA Collect_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Collect(s), Frame
  PROVE  A1IAK1'
<1>r. RetireAge BY ShippedShape DEF Shipped
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY <1>r DEF Collect, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>g. w[s].pc = "cased" BY DEF Collect
<1>2. A1Same(CollectW(s), w[s])
  <2>1. CollectW(s).pend = w[s].pend BY DEF CollectW
  <2>2. CollectW(s).dset = w[s].dset BY DEF CollectW
  <2>3. CollectW(s).decl = w[s].decl BY DEF CollectW
  <2>4. CollectW(s).moved = w[s].moved BY DEF CollectW
  <2>5. CollectW(s).dMint = w[s].dMint BY DEF CollectW
  <2>6. CollectW(s).local = w[s].local BY DEF CollectW
  <2>7. CollectW(s).baseline = w[s].baseline BY DEF CollectW
  <2>8. CollectW(s).car = w[s].car BY DEF CollectW
  <2>9. CollectW(s).bound = w[s].bound BY DEF CollectW
  <2>10. CollectW(s).cId = w[s].cId BY DEF CollectW
  <2>11. CollectW(s).cFold = w[s].cFold BY DEF CollectW
  <2>12. CollectW(s).nt = w[s].nt BY DEF CollectW
  <2>13. CollectW(s).inst = w[s].inst BY DEF CollectW
  <2>14. CollectW(s).uploads = w[s].uploads BY DEF CollectW
  <2>15. CollectW(s).deletes = w[s].deletes BY DEF CollectW
  <2>16. CollectW(s).snap = w[s].snap BY DEF CollectW
  <2>17. CollectW(s).lost = w[s].lost BY DEF CollectW
  <2>ls. A1LeftSet(CollectW(s)) = A1LeftSet(w[s]) BY DEF CollectW, A1LeftSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>ls DEF A1Same
<1>3. A1PcOK(CollectW(s), w[s])
  <2>1. CollectW(s).pc = w[s].pc /\ CollectW(s).gone = w[s].gone /\ CollectW(s).upDone = w[s].upDone BY DEF CollectW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

------------------------------------------------------------------------------
(* An agent write (Edit, Delete): one path's bytes change, and under a       *)
(* record the path is moved on from (moved); every other field is kept.    *)
(* A moved path is not declared, so nothing at it is read; Broken shrinks. *)

A1Moved(R, W, p) ==
  /\ R.pc = W.pc /\ R.pend = W.pend /\ R.dset = W.dset /\ R.decl = W.decl /\ R.dMint = W.dMint
  /\ R.baseline = W.baseline /\ R.car = W.car /\ R.bound = W.bound /\ R.cId = W.cId /\ R.cFold = W.cFold
  /\ R.nt = W.nt /\ R.inst = W.inst /\ R.uploads = W.uploads /\ R.deletes = W.deletes /\ R.snap = W.snap
  /\ R.lost = W.lost /\ R.gone = W.gone /\ R.upDone = W.upDone
  /\ \A q \in Paths \ {p} : (q \in A1LeftSet(R)) <=> (q \in A1LeftSet(W))
  /\ \A q \in Paths \ {p} : R.local[q] = W.local[q]
  /\ R.moved = IF W.pend.id # 0 THEN W.moved \cup {p} ELSE W.moved

LEMMA A1Move ==
  ASSUME IndC, A1IAK1, HStep, NEW s \in Writers, NEW p \in Paths, NEW R,
         w' = [w EXCEPT ![s] = R], A1Moved(R, w[s], p),
         conflicts' = conflicts, doc' = doc, live' = live
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>per. A1Per(s) BY A1PerAll
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY A1DocContent
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY A1ContentAll
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w DEF A1Moved
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>lost, A1Frame DEF A1IAK1
\* A path declared after the write is not p (under a record p is moved), and
\* every field at it is kept.
<1>d. \A q \in Paths : A1Decl(s, q)' => q # p /\ A1Decl(s, q) /\ w'[s].local[q] = w[s].local[q]
  BY <1>w DEF A1Decl, A1Moved
<1>s. A1Per(s)'
  <2>w. w'[s] = R BY <1>w
  <2>f. A1Moved(w'[s], w[s], p) BY <2>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>f, <1>per, A1FoldsOf DEF A1Moved
  <2>2. w'[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w'[s].cId = w'[s].pend.id /\ w'[s].cFold = w'[s].pend.fold
    BY <2>f, <1>per, A1FrozenOf DEF A1Moved
  <2>3. \A q \in Paths :
          /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] =>
               \/ q \in w[s].uploads /\ w[s].snap[q] = w[s].local[q]
               \/ q \in w[s].deletes /\ w[s].local[q] = Nil)'
          /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) =>
               /\ q \in w[s].uploads => w[s].snap[q] = w[s].local[q]
               /\ q \in w[s].deletes => w[s].local[q] = Nil)'
          /\ (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].decl[q] # Nil =>
               \/ w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
               \/ q \in w[s].gone /\ q \notin w[s].upDone)'
          /\ A1TreePath(s, q)'
    <3>. SUFFICES ASSUME NEW q \in Paths
                  PROVE  /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] =>
                              \/ q \in w[s].uploads /\ w[s].snap[q] = w[s].local[q]
                              \/ q \in w[s].deletes /\ w[s].local[q] = Nil)'
                         /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) =>
                              /\ q \in w[s].uploads => w[s].snap[q] = w[s].local[q]
                              /\ q \in w[s].deletes => w[s].local[q] = Nil)'
                         /\ (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].decl[q] # Nil =>
                              \/ w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
                              \/ q \in w[s].gone /\ q \notin w[s].upDone)'
                         /\ A1TreePath(s, q)'
      OBVIOUS
    <3>t. w'[s].decl[q] \in Opt(minted') /\ w'[s].inst[q] \in Opt(minted') /\ w'[s].car.pub[q] \in Opt(minted')
      BY <2>f, <1>per, <1>m DEF A1Per, A1Moved, Opt
    <3>1. CASE A1Decl(s, q)'
      <4>1. q # p /\ A1Decl(s, q) /\ w'[s].local[q] = w[s].local[q] BY <3>1, <1>d
      \* the carrier as a constant (CarForm's prime would double-prime w'[s].car)
      <4>0. PICK c0 : c0 = w[s].car OBVIOUS
      <4>2. CarForm(s, q, c0)' <=> CarForm(s, q, c0) BY <2>f, <1>c, <4>0 DEF CarForm, A1Moved
      <4>h. A1TreePath(s, q) BY <1>per DEF A1Per, A1TreePath
      <4>ls. q \in A1LeftSet(w'[s]) <=> q \in A1LeftSet(w[s]) BY <4>1, <2>f DEF A1Moved
      <4>a. /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] =>
                 \/ q \in w[s].uploads /\ w[s].snap[q] = w[s].local[q]
                 \/ q \in w[s].deletes /\ w[s].local[q] = Nil)'
            /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) =>
                 /\ q \in w[s].uploads => w[s].snap[q] = w[s].local[q]
                 /\ q \in w[s].deletes => w[s].local[q] = Nil)'
        BY <4>1, <2>f, <1>per DEF A1Per, A1Moved
      <4>b. (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].decl[q] # Nil =>
               \/ w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
               \/ q \in w[s].gone /\ q \notin w[s].upDone)'
        BY <4>0, <4>1, <4>2, <2>f, <1>per DEF A1Per, A1Moved
      <4>e1. (A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] /\ q \notin A1LeftSet(w[s]) =>
                Content(w[s].local[q]) = Content(w[s].decl[q]))'
        BY <4>h, <4>1, <4>ls, <2>f, <1>c DEF A1TreePath, A1Moved
      <4>e2. (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ w[s].local[q] = w[s].baseline[q] =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car))'
        BY <4>h, <4>0, <4>1, <4>2, <2>f DEF A1TreePath, A1Moved
      <4>e3. (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ q \in A1LeftSet(w[s]) =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car))'
        BY <4>h, <4>0, <4>1, <4>2, <4>ls, <2>f DEF A1TreePath, A1Moved
      <4>e4. (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[s].decl[q]) =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car))'
        BY <4>h, <4>0, <4>1, <4>2, <2>f, <1>c, <1>dk DEF A1TreePath, A1Moved
      <4>e. A1TreePath(s, q)' BY <4>e1, <4>e2, <4>e3, <4>e4, <3>t DEF A1TreePath
      <4>. QED BY <4>a, <4>b, <4>e
    <3>2. CASE ~A1Decl(s, q)' BY <3>2, <3>t, <1>dk DEF A1TreePath
    <3>. QED BY <3>1, <3>2
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>f, <1>per, <1>m DEF A1Per, A1Moved, Opt
  <2>5. (Honorable(s) => \A q \in Paths : Broken(s, q, w[s].bound.doc) => q \in HDropped(s))'
    <3>. SUFFICES ASSUME Honorable(s)', NEW q \in Paths, Broken(s, q, w[s].bound.doc)'
                  PROVE  q \in HDropped(s)' OBVIOUS
    <3>1. PICK d \in [Paths -> Opt(minted)] : d = w[s].bound.doc BY <1>per DEF A1Per
    <3>2. Broken(s, q, d)' BY <3>1, <2>f DEF A1Moved
    <3>3. w[s].moved \subseteq w'[s].moved BY <2>f DEF A1Moved
    <3>4. Broken(s, q, d) BY <3>1, <3>2, <3>3, <2>f, <1>lost, <1>per, A1BrokenKeep DEF A1IAK1, A1Per, A1Moved
    <3>5. Honorable(s) BY <2>f DEF Honorable, A1Moved
    <3>. QED BY <3>1, <3>4, <3>5, <2>f, <1>per DEF A1Per, HDropped, A1Moved
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <1>dk DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

LEMMA Delete_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY DEF Delete, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY <1>0, WriterFields DEF TypeOK
<1>2. A1Moved(DeleteW(s, p), w[s], p)
  <2>1. DeleteW(s, p).pc = w[s].pc /\ DeleteW(s, p).pend = w[s].pend /\ DeleteW(s, p).dset = w[s].dset BY DEF DeleteW
  <2>2. DeleteW(s, p).decl = w[s].decl /\ DeleteW(s, p).dMint = w[s].dMint /\ DeleteW(s, p).baseline = w[s].baseline BY DEF DeleteW
  <2>3. DeleteW(s, p).car = w[s].car /\ DeleteW(s, p).bound = w[s].bound /\ DeleteW(s, p).cId = w[s].cId BY DEF DeleteW
  <2>4. DeleteW(s, p).cFold = w[s].cFold /\ DeleteW(s, p).nt = w[s].nt /\ DeleteW(s, p).inst = w[s].inst BY DEF DeleteW
  <2>5. DeleteW(s, p).uploads = w[s].uploads /\ DeleteW(s, p).deletes = w[s].deletes /\ DeleteW(s, p).snap = w[s].snap BY DEF DeleteW
  <2>6. DeleteW(s, p).lost = w[s].lost /\ DeleteW(s, p).gone = w[s].gone /\ DeleteW(s, p).upDone = w[s].upDone BY DEF DeleteW
  <2>7. \A q \in Paths \ {p} : DeleteW(s, p).local[q] = w[s].local[q] BY <1>lt DEF DeleteW
  <2>8. DeleteW(s, p).moved = IF w[s].pend.id # 0 THEN w[s].moved \cup {p} ELSE w[s].moved BY DEF DeleteW
  \* p leaves the leftover (its file is no longer the held handle); the rest keep
  <2>ls. \A q \in Paths \ {p} : (q \in A1LeftSet(DeleteW(s, p))) <=> (q \in A1LeftSet(w[s])) BY <2>7 DEF DeleteW, A1LeftSet
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>ls DEF A1Moved
<1>. QED BY <1>1, <1>hs, <1>2, A1Move

LEMMA Edit_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  A1IAK1'
<1>m. IndM5' BY DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles /\ w[s] \in Writer
  BY TreeTy DEF IndC
<1>g. nextGen \in Nat /\ nextGen > Seed BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>e. PICK e \in Opt(minted) :
        e = (IF w[s].local[p] # Nil THEN w[s].local[p] ELSE IF p \in w[s].uploads THEN w[s].snap[p] ELSE w[s].baseline[p])
  BY <1>y
<1>h. PICK hh \in Handles : hh = <<p, nextGen>>
  BY <1>g, MaxMintNat, MaxCopiesNat DEF Edit, Handles, Gens, Seed
<1>1. /\ minted' = minted \cup {hh} /\ base' = [base EXCEPT ![hh] = e] /\ orig' = orig
      /\ nextGen' = nextGen + 1 /\ w' = [w EXCEPT ![s] = EditW(s, p)]
      /\ UNCHANGED <<live, doc, conflicts, udel, acked>>
  BY <1>e, <1>h DEF Edit, aux
<1>4. hh \notin minted
  <2>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>2. nextGen <= MaxMint BY DEF Edit
  <2>. QED BY <2>1, <2>2, <1>g, <1>h, MaxMintNat DEF Gen
<1>ty. base \in [Handles -> Opt(Handles)] /\ orig \in [Handles -> Opt(Handles)]
  BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>f. MintFacts(hh) BY <1>1, <1>4, <1>ty, <1>y DEF MintFacts
<1>6. orig[hh] = Nil BY <1>4 DEF IndC, IndM5, M5, Hist, HistNew
<1>7. base'[hh] = e /\ orig'[hh] = Nil BY <1>1, <1>6, <1>ty
<1>8. \A z \in Handles : z # hh => base'[z] = base[z] /\ orig'[z] = orig[z] BY <1>1, <1>ty
<1>hs. HStep BY <1>m, <1>u, <1>f, <1>7, <1>8, HStepMint DEF IndC, Opt
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
<1>2. A1Moved(EditW(s, p), w[s], p)
  <2>1. EditW(s, p).pc = w[s].pc /\ EditW(s, p).pend = w[s].pend /\ EditW(s, p).dset = w[s].dset BY DEF EditW
  <2>2. EditW(s, p).decl = w[s].decl /\ EditW(s, p).dMint = w[s].dMint /\ EditW(s, p).baseline = w[s].baseline BY DEF EditW
  <2>3. EditW(s, p).car = w[s].car /\ EditW(s, p).bound = w[s].bound /\ EditW(s, p).cId = w[s].cId BY DEF EditW
  <2>4. EditW(s, p).cFold = w[s].cFold /\ EditW(s, p).nt = w[s].nt /\ EditW(s, p).inst = w[s].inst BY DEF EditW
  <2>5. EditW(s, p).uploads = w[s].uploads /\ EditW(s, p).deletes = w[s].deletes /\ EditW(s, p).snap = w[s].snap BY DEF EditW
  <2>6. EditW(s, p).lost = w[s].lost /\ EditW(s, p).gone = w[s].gone /\ EditW(s, p).upDone = w[s].upDone BY DEF EditW
  <2>7. \A q \in Paths \ {p} : EditW(s, p).local[q] = w[s].local[q] BY <1>lt DEF EditW
  <2>8. EditW(s, p).moved = IF w[s].pend.id # 0 THEN w[s].moved \cup {p} ELSE w[s].moved BY DEF EditW
  \* p leaves the leftover (its file is no longer the held handle); the rest keep
  <2>ls. \A q \in Paths \ {p} : (q \in A1LeftSet(EditW(s, p))) <=> (q \in A1LeftSet(w[s])) BY <2>7 DEF EditW, A1LeftSet
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>ls DEF A1Moved
<1>. QED BY <1>1, <1>hs, <1>2, A1Move

------------------------------------------------------------------------------
(* Upload's copy arm: the bytes were PUT before (upped), so they land at a  *)
(* fresh copy handle c (orig[c] = their content) that the tree and the snap *)
(* name from here on.  A declared upload moves both (MidBar: snap = local). *)

LEMMA UploadCopy_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame,
         w[s].snap[p] \in upped
  PROVE  A1IAK1'
<1>m5. IndM5' BY DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles /\ w[s] \in Writer
  BY TreeTy DEF IndC
<1>k. w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
<1>h0. PICK h0 \in minted : h0 = w[s].snap[p] BY <1>k, <1>y
<1>g. copies \in Nat /\ copies < MaxCopies BY <1>h0 DEF Upload, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
<1>c. PICK c \in Handles : c = <<p, MaxMint + copies + 1>>
  BY <1>g, MaxMintNat, MaxCopiesNat DEF Handles, Gens, Seed
<1>1. /\ minted' = minted \cup {c} /\ base' = [base EXCEPT ![c] = base[h0]]
      /\ orig' = [orig EXCEPT ![c] = Content(h0)] /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
      /\ live' = live \cup {c} /\ UNCHANGED <<conflicts, doc>>
  BY <1>h0, <1>c DEF Upload
<1>4. c \notin minted
  <2>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
    BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>2. nextGen <= MaxMint + 1 BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>3. nextGen \in Nat BY <1>0 DEF TypeOK
  <2>. QED BY <2>1, <2>2, <2>3, <1>c, <1>g, MaxMintNat DEF Gen
<1>ty. base \in [Handles -> Opt(Handles)] /\ orig \in [Handles -> Opt(Handles)]
  BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>f. MintFacts(c) BY <1>1, <1>4, <1>ty, <1>y DEF MintFacts
<1>hm. base[h0] \in Opt(minted) /\ Content(h0) \in minted /\ orig[Content(h0)] = Nil
  BY ContentMinted DEF IndC, IndM5, M5, Hist, HistMinted
<1>7. base'[c] = base[h0] /\ orig'[c] = Content(h0) BY <1>1, <1>ty
<1>8. \A z \in Handles : z # c => base'[z] = base[z] /\ orig'[z] = orig[z] BY <1>1, <1>ty
<1>hs. HStep BY <1>m5, <1>u, <1>f, <1>7, <1>8, <1>hm, HStepMint DEF IndC, Opt
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
\* The copy names the bytes it copied.
<1>cc. Content(c)' = Content(h0)
  <2>1. Content(h0) # Nil BY <1>hm, NilHandle, <1>y
  <2>2. c # Nil BY <1>c, NilHandle
  <2>. QED BY <1>7, <2>1, <2>2 DEF Content
<1>c0. Content(h0)' = Content(h0) BY <1>h0, <1>hs, A1ContentStep DEF Opt
<1>lt. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
<1>w. \A u \in Writers : w'[u] = IF u = s THEN UploadCopyW(s, p, c) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>fx. /\ UploadCopyW(s, p, c).pc = w[s].pc /\ UploadCopyW(s, p, c).pend = w[s].pend
       /\ UploadCopyW(s, p, c).dset = w[s].dset /\ UploadCopyW(s, p, c).decl = w[s].decl
       /\ UploadCopyW(s, p, c).moved = w[s].moved /\ UploadCopyW(s, p, c).dMint = w[s].dMint
       /\ UploadCopyW(s, p, c).baseline = w[s].baseline /\ UploadCopyW(s, p, c).car = w[s].car
       /\ UploadCopyW(s, p, c).bound = w[s].bound /\ UploadCopyW(s, p, c).cId = w[s].cId
       /\ UploadCopyW(s, p, c).cFold = w[s].cFold /\ UploadCopyW(s, p, c).nt = w[s].nt
       /\ UploadCopyW(s, p, c).inst = w[s].inst /\ UploadCopyW(s, p, c).uploads = w[s].uploads
       /\ UploadCopyW(s, p, c).deletes = w[s].deletes /\ UploadCopyW(s, p, c).lost = w[s].lost
       /\ UploadCopyW(s, p, c).gone = w[s].gone
  <2>1. UploadCopyW(s, p, c).pc = w[s].pc /\ UploadCopyW(s, p, c).pend = w[s].pend BY DEF UploadCopyW
  <2>2. UploadCopyW(s, p, c).dset = w[s].dset /\ UploadCopyW(s, p, c).decl = w[s].decl BY DEF UploadCopyW
  <2>3. UploadCopyW(s, p, c).moved = w[s].moved /\ UploadCopyW(s, p, c).dMint = w[s].dMint BY DEF UploadCopyW
  <2>4. UploadCopyW(s, p, c).baseline = w[s].baseline /\ UploadCopyW(s, p, c).car = w[s].car BY DEF UploadCopyW
  <2>5. UploadCopyW(s, p, c).bound = w[s].bound /\ UploadCopyW(s, p, c).cId = w[s].cId BY DEF UploadCopyW
  <2>6. UploadCopyW(s, p, c).cFold = w[s].cFold /\ UploadCopyW(s, p, c).nt = w[s].nt BY DEF UploadCopyW
  <2>7. UploadCopyW(s, p, c).inst = w[s].inst /\ UploadCopyW(s, p, c).uploads = w[s].uploads BY DEF UploadCopyW
  <2>8. UploadCopyW(s, p, c).deletes = w[s].deletes /\ UploadCopyW(s, p, c).lost = w[s].lost BY DEF UploadCopyW
  <2>9. UploadCopyW(s, p, c).gone = w[s].gone BY DEF UploadCopyW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9
<1>l. \A q \in Paths : /\ UploadCopyW(s, p, c).local[q] = IF q = p /\ w[s].local[p] = h0 THEN c ELSE w[s].local[q]
                       /\ UploadCopyW(s, p, c).snap[q] = IF q = p THEN c ELSE w[s].snap[q]
  BY <1>lt, <1>h0 DEF UploadCopyW
<1>per. A1Per(s) BY A1PerAll
\* no leftover in a barrier (R1: the rescope is not in flight)
<1>st. w[s].sStage = "none" BY <1>k DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>nl2. A1LeftSet(w[s]) = {} /\ A1LeftSet(w'[s]) = {}
  <2>1. UploadCopyW(s, p, c).sStage = w[s].sStage BY DEF UploadCopyW
  <2>. QED BY <1>st, <2>1, <1>w DEF A1LeftSet
<1>z. w[s].cId = w[s].pend.id BY <1>k, <1>per DEF A1Per
<1>cn. \A q \in Paths :
         /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
         /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
         /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
         /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
         /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>mm. minted \subseteq minted' BY <1>1
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
<1>lv. live \subseteq live' BY <1>1
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, <1>lv, A1Frame DEF A1IAK1
\* At a declared p the tree held the snap (MidBar), so both move to c; the
\* tree was dirty (UDR), so DTree gave the snap's bytes the declared ones.
<1>p. A1Decl(s, p) =>
        /\ w[s].local[p] = h0 /\ w[s].local[p] # w[s].baseline[p]
        /\ Content(h0) = Content(w[s].decl[p])
        /\ c # w[s].baseline[p]
  <2>. SUFFICES ASSUME A1Decl(s, p)
                PROVE  /\ w[s].local[p] = h0 /\ w[s].local[p] # w[s].baseline[p]
                       /\ Content(h0) = Content(w[s].decl[p])
                       /\ c # w[s].baseline[p] OBVIOUS
  <2>0. w[s].cId # 0 BY <1>z DEF A1Decl
  <2>1. w[s].local[p] = h0 BY <2>0, <1>k, <1>h0, <1>per DEF A1Per
  <2>2. w[s].local[p] # w[s].baseline[p]
    <3>1. CASE w[s].baseline[p] # Nil BY <3>1, <1>k DEF DInv, UD, UDR, Bar
    <3>2. CASE w[s].baseline[p] = Nil BY <3>2, <2>1, <1>y, NilHandle
    <3>. QED BY <3>1, <3>2
  \* no leftover during a barrier (R1: sStage none)
  <2>2a. p \notin A1LeftSet(w[s]) BY <1>k DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1, A1LeftSet
  <2>3. Content(h0) = Content(w[s].decl[p]) BY <2>1, <2>2, <2>2a, <1>per DEF A1Per
  <2>4. c # w[s].baseline[p] BY <1>4, <1>y, <1>c, NilHandle DEF Opt
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
<1>s. A1Per(s)'
  <2>w. w'[s] = UploadCopyW(s, p, c) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>fx, <1>per, A1FoldsOf
  <2>2. w'[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w'[s].cId = w'[s].pend.id /\ w'[s].cFold = w'[s].pend.fold
    BY <2>w, <1>fx, <1>per, A1FrozenOf
  <2>3. \A q \in Paths :
          /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] =>
               \/ q \in w[s].uploads /\ w[s].snap[q] = w[s].local[q]
               \/ q \in w[s].deletes /\ w[s].local[q] = Nil)'
          /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) =>
               /\ q \in w[s].uploads => w[s].snap[q] = w[s].local[q]
               /\ q \in w[s].deletes => w[s].local[q] = Nil)'
          /\ (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].decl[q] # Nil =>
               \/ w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
               \/ q \in w[s].gone /\ q \notin w[s].upDone)'
          /\ A1TreePath(s, q)'
    <3>. SUFFICES ASSUME NEW q \in Paths
                  PROVE  /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] =>
                              \/ q \in w[s].uploads /\ w[s].snap[q] = w[s].local[q]
                              \/ q \in w[s].deletes /\ w[s].local[q] = Nil)'
                         /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) =>
                              /\ q \in w[s].uploads => w[s].snap[q] = w[s].local[q]
                              /\ q \in w[s].deletes => w[s].local[q] = Nil)'
                         /\ (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].decl[q] # Nil =>
                              \/ w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
                              \/ q \in w[s].gone /\ q \notin w[s].upDone)'
                         /\ A1TreePath(s, q)'
      OBVIOUS
    <3>d. A1Decl(s, q)' <=> A1Decl(s, q) BY <2>w, <1>fx DEF A1Decl
    <3>t. w'[s].decl[q] \in Opt(minted') /\ w'[s].inst[q] \in Opt(minted') /\ w'[s].car.pub[q] \in Opt(minted')
      BY <2>w, <1>fx, <1>per, <1>mm DEF A1Per, Opt
    <3>c0. PICK c0 : c0 = w[s].car OBVIOUS
    <3>cf. CarForm(s, q, c0)' <=> CarForm(s, q, c0) BY <2>w, <1>fx, <1>cn, <3>c0 DEF CarForm
    <3>pc. w'[s].pc # "cased" BY <2>w, <1>fx, <1>k
    <3>1. CASE q = p /\ A1Decl(s, p)
      <4>1. w'[s].local[q] = c /\ w'[s].snap[q] = c BY <3>1, <1>p, <1>l, <2>w
      <4>2. Content(c)' = Content(w[s].decl[q])' BY <3>1, <1>cc, <1>p, <1>cn, <2>w, <1>fx
      <4>3. w'[s].local[q] # w'[s].baseline[q] BY <4>1, <3>1, <1>p, <2>w, <1>fx
      <4>5. q \notin w[s].deletes
        <5>1. w[s].cId # 0 BY <3>1, <1>z DEF A1Decl
        <5>2. h0 # Nil BY <1>h0, <1>y, NilHandle
        <5>. QED BY <3>1, <5>1, <5>2, <1>p, <1>k, <1>per DEF A1Per
      <4>a. /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] =>
                 \/ q \in w[s].uploads /\ w[s].snap[q] = w[s].local[q]
                 \/ q \in w[s].deletes /\ w[s].local[q] = Nil)'
            /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, q) =>
                 /\ q \in w[s].uploads => w[s].snap[q] = w[s].local[q]
                 /\ q \in w[s].deletes => w[s].local[q] = Nil)'
        BY <3>1, <4>1, <4>5, <1>k, <2>w, <1>fx
      <4>b. (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].decl[q] # Nil =>
               \/ w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
               \/ q \in w[s].gone /\ q \notin w[s].upDone)'
        BY <3>pc
      <4>e1. (A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] /\ q \notin A1LeftSet(w[s]) =>
                Content(w[s].local[q]) = Content(w[s].decl[q]))'
        BY <4>1, <4>2, <2>w, <1>fx
      <4>e2. (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ w[s].local[q] = w[s].baseline[q] =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car))'
        BY <4>3
      <4>e3. (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ q \in A1LeftSet(w[s]) =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car))'
        BY <1>nl2
      <4>e4. (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[s].decl[q]) =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car))'
        <5>1. A1TreePath(s, q) BY <1>per DEF A1Per, A1TreePath
        <5>2. A1Decl(s, q) /\ w[s].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[s].decl[q]) =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, q, c0) BY <5>1, <3>c0 DEF A1TreePath
        <5>. QED BY <5>2, <3>d, <3>cf, <3>c0, <1>dk, <1>cn, <2>w, <1>fx
      <4>e. A1TreePath(s, q)' BY <4>e1, <4>e2, <4>e3, <4>e4, <3>t DEF A1TreePath
      <4>. QED BY <4>a, <4>b, <4>e
    <3>2. CASE ~(q = p /\ A1Decl(s, p))
      <4>0. A1Decl(s, q)' => w'[s].local[q] = w[s].local[q] /\ w'[s].snap[q] = w[s].snap[q]
        \* at q # p nothing at q moved; at an undeclared p nothing is read
        BY <3>2, <3>d, <1>l, <2>w
      <4>. QED BY <3>d, <3>t, <3>c0, <3>cf, <3>pc, <4>0, <2>w, <1>fx, <1>cn, <1>per, <1>dk, <1>nl2 DEF A1Per, A1TreePath
    <3>. QED BY <3>1, <3>2
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>fx, <1>per, <1>mm DEF A1Per, Opt
  <2>5. (Honorable(s) => \A q \in Paths : Broken(s, q, w[s].bound.doc) => q \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>fx DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, <1>lv, A1BoundKeep DEF A1IAK1
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <1>dk, <1>nl2 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1 DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

------------------------------------------------------------------------------
(* The reap (Q1): it takes an aged handle with no guard for a conflict that  *)
(* preserves it.  SurfacedDecl survives: a lost handle is never live        *)
(* (A1LostDead: Verify's gone are the uploads already swept, and live grows *)
(* only by fresh handles), so a live one a conflict names is not lost.     *)
(* A1LostDead is its own invariant (its module follows M11).               *)

\* (A1LostDead and the other side facts: defined with A1IAK1, above.)

\* (The reap is ReapQ_A1 below: Next gives it UNCHANGED anc, not Frame.)

------------------------------------------------------------------------------
(* The checkout: an off writer is WriterInit (M1's Off), no record, so      *)
(* nothing is declared and no honour is due; the install is the document.  *)

LEMMA Checkout_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Checkout(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>i. w[s] = WriterInit BY DEF Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
<1>T. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>1. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Checkout, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>w. \A u \in Writers : w'[u] = IF u = s THEN CheckoutW(s, T) ELSE w[u] BY <1>0, <1>T DEF TypeOK
<1>fx. /\ CheckoutW(s, T).pc = "idle" /\ CheckoutW(s, T).pend = NoPend /\ CheckoutW(s, T).nt = 0
       /\ CheckoutW(s, T).bound = WriterInit.bound /\ CheckoutW(s, T).cFold = 0
       /\ CheckoutW(s, T).decl = [p \in Paths |-> Nil] /\ CheckoutW(s, T).car = NoCar
       /\ CheckoutW(s, T).inst = doc /\ CheckoutW(s, T).lost = {}
  <2>1. CheckoutW(s, T).pc = "idle" /\ CheckoutW(s, T).pend = NoPend /\ CheckoutW(s, T).nt = 0 BY <1>i DEF CheckoutW, WriterInit
  <2>2. CheckoutW(s, T).bound = WriterInit.bound /\ CheckoutW(s, T).cFold = 0 BY <1>i DEF CheckoutW, WriterInit
  <2>3. CheckoutW(s, T).decl = [p \in Paths |-> Nil] /\ CheckoutW(s, T).car = NoCar BY <1>i DEF CheckoutW, WriterInit
  <2>4. CheckoutW(s, T).inst = doc /\ CheckoutW(s, T).lost = {} BY <1>i DEF CheckoutW, WriterInit
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
<1>m. minted \subseteq minted' BY <1>1
<1>dm. \A p \in Paths : doc[p] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
<1>s. A1Per(s)'
  <2>w. w'[s] = CheckoutW(s, T) BY <1>w
  <2>0. w'[s].pend.id = 0 /\ w'[s].pend.fold = 0 BY <2>w, <1>fx DEF NoPend
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <2>0, <1>fx DEF WriterInit
  <2>2. w'[s].pc = "idle" BY <2>w, <1>fx
  <2>3. \A p \in Paths : ~A1Decl(s, p)' BY <2>0 DEF A1Decl
  <2>4. \A p \in Paths : w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
    BY <2>w, <1>fx, <1>dm, <1>m DEF NoCar, Opt
  <2>5. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>fx DEF WriterInit, Opt
  <2>6. ~Honorable(s)' BY <2>0 DEF Honorable
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF A1Per
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP


------------------------------------------------------------------------------
(* The steps still to write (TO WRITE: each is OMITTED until its proof is    *)
(* written; Next_A1 below already dispatches to them).                     *)

\* The consume: pc idle -> consumed, the record frozen (cId := pend.id).  A
\* taken path (owed: clean; converged: the tree's bytes ARE the document's)
\* becomes clean at doc[p]; a converged declared file is in carrier form by
\* DTree + DocCar (the reason DocCar exists).  Nothing else of s changes.
LEMMA Consume_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1LostDead, NEW s \in Writers, Consume(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Consume
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
<1>g. w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF Consume
<1>per. A1Per(s) BY A1PerAll
<1>W. w[s] \in Writer BY <1>0 DEF TypeOK
<1>n. w[s].nt \in Nat BY <1>W, WriterFields
<1>dm. \A q \in Paths : doc[q] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
\* Both records: the frozen record, every field AK1 reads but the tree kept;
\* each path either kept or taken (owed or converged) at doc[p].
<1>R. PICK R : /\ w' = [w EXCEPT ![s] = R]
               /\ R.pc = "consumed" /\ R.cId = w[s].pend.id /\ R.cFold = w[s].pend.fold
               /\ R.pend = w[s].pend /\ R.dset = w[s].dset /\ R.decl = w[s].decl /\ R.moved = w[s].moved /\ R.dMint = w[s].dMint /\ R.car = w[s].car /\ R.bound = w[s].bound /\ R.nt = w[s].nt /\ R.lost = w[s].lost /\ R.inst = w[s].inst
               /\ A1LeftSet(R) = A1LeftSet(w[s])
               /\ \A q \in Paths : \/ R.local[q] = w[s].local[q] /\ R.baseline[q] = w[s].baseline[q]
                                    \/ /\ R.local[q] = doc[q] /\ R.baseline[q] = doc[q]
                                       /\ (Owed(s, q) \/ Converges(s, q))
  <2>1. CASE CheapPath(s)
    <3>0. PICK ck : w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <2>1 DEF Consume
  <3>a. ConsumeCheapW(s, ck).pc = "consumed" /\ ConsumeCheapW(s, ck).cId = w[s].pend.id /\ ConsumeCheapW(s, ck).cFold = w[s].pend.fold BY DEF ConsumeCheapW
  <3>k0. ConsumeCheapW(s, ck).pend = w[s].pend /\ ConsumeCheapW(s, ck).dset = w[s].dset /\ ConsumeCheapW(s, ck).decl = w[s].decl BY DEF ConsumeCheapW
  <3>k1. ConsumeCheapW(s, ck).moved = w[s].moved /\ ConsumeCheapW(s, ck).dMint = w[s].dMint /\ ConsumeCheapW(s, ck).car = w[s].car BY DEF ConsumeCheapW
  <3>k2. ConsumeCheapW(s, ck).bound = w[s].bound /\ ConsumeCheapW(s, ck).nt = w[s].nt /\ ConsumeCheapW(s, ck).lost = w[s].lost BY DEF ConsumeCheapW
  <3>k3. ConsumeCheapW(s, ck).inst = w[s].inst BY DEF ConsumeCheapW
  <3>l. A1LeftSet(ConsumeCheapW(s, ck)) = A1LeftSet(w[s]) BY <1>g DEF ConsumeCheapW, A1LeftSet
    <3>t. \A q \in Paths : ConsumeCheapW(s, ck).local[q] = w[s].local[q] /\ ConsumeCheapW(s, ck).baseline[q] = w[s].baseline[q]
      BY DEF ConsumeCheapW
    <3>. QED BY <3>0, <3>a, <3>k0, <3>k1, <3>k2, <3>k3, <3>l, <3>t
  <2>2. CASE ~CheapPath(s)
    <3>e. \E ck \in CarryClocks(s) : \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
      BY <2>2 DEF Consume
    <3>0. PICK ck \in CarryClocks(s), fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <3>e
  <3>a. ConsumeW(s, fail, ck).pc = "consumed" /\ ConsumeW(s, fail, ck).cId = w[s].pend.id /\ ConsumeW(s, fail, ck).cFold = w[s].pend.fold BY DEF ConsumeW
  <3>k0. ConsumeW(s, fail, ck).pend = w[s].pend /\ ConsumeW(s, fail, ck).dset = w[s].dset /\ ConsumeW(s, fail, ck).decl = w[s].decl BY DEF ConsumeW
  <3>k1. ConsumeW(s, fail, ck).moved = w[s].moved /\ ConsumeW(s, fail, ck).dMint = w[s].dMint /\ ConsumeW(s, fail, ck).car = w[s].car BY DEF ConsumeW
  <3>k2. ConsumeW(s, fail, ck).bound = w[s].bound /\ ConsumeW(s, fail, ck).nt = w[s].nt /\ ConsumeW(s, fail, ck).lost = w[s].lost BY DEF ConsumeW
  <3>k3. ConsumeW(s, fail, ck).inst = w[s].inst BY DEF ConsumeW
  <3>l. A1LeftSet(ConsumeW(s, fail, ck)) = A1LeftSet(w[s]) BY <1>g DEF ConsumeW, A1LeftSet
    <3>t. \A q \in Paths : \/ ConsumeW(s, fail, ck).local[q] = w[s].local[q] /\ ConsumeW(s, fail, ck).baseline[q] = w[s].baseline[q]
                           \/ /\ ConsumeW(s, fail, ck).local[q] = doc[q] /\ ConsumeW(s, fail, ck).baseline[q] = doc[q]
                              /\ (Owed(s, q) \/ Converges(s, q))
      BY DEF ConsumeW, ConsumeTaken, ConsumeOwed, ConsumeConv
    <3>. QED BY <3>0, <3>a, <3>k0, <3>k1, <3>k2, <3>k3, <3>l, <3>t
  <2>. QED BY <2>1, <2>2
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0, <1>R DEF TypeOK
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>R
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
\* s's part; the old per-path facts, read once.
<1>old. \A q \in Paths :
          /\ (A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] /\ q \notin A1LeftSet(w[s])) =>
               Content(w[s].local[q]) = Content(w[s].decl[q])
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ w[s].local[q] = w[s].baseline[q]) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[s].decl[q])) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ w[s].decl[q] \in Opt(minted) /\ w[s].inst[q] \in Opt(minted) /\ w[s].car.pub[q] \in Opt(minted)
  BY <1>per DEF A1Per
<1>nl. A1LeftSet(w[s]) = {} BY <1>g DEF A1LeftSet
<1>s. A1Per(s)'
  <2>w. w'[s] = R BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>R, <1>per, A1FoldsOf
  <2>2. w'[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w'[s].cId = w'[s].pend.id /\ w'[s].cFold = w'[s].pend.fold
    BY <2>w, <1>R
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>R DEF A1Decl
    <3>ls. p \notin A1LeftSet(w'[s]) BY <2>w, <1>R, <1>nl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>R, <1>old, <1>m DEF Opt
    <3>cf. CarForm(s, p, w[s].car)' <=> CarForm(s, p, w[s].car)
      <4>0. PICK c0 : c0 = w[s].car OBVIOUS
      <4>1. CarForm(s, p, c0)' <=> CarForm(s, p, c0) BY <2>w, <1>R, <1>c, <4>0 DEF CarForm
      <4>. QED BY <4>0, <4>1, <2>w, <1>R
    <3>car. w'[s].car = w[s].car /\ w'[s].pend = w[s].pend /\ w'[s].decl = w[s].decl BY <2>w, <1>R
    \* DocCar: doc, the declaration and the carrier are kept
    <3>dc. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
             (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
      BY <3>d, <3>cf, <3>car, <1>old, <1>dk, <1>c
    <3>1. CASE w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
      <4>1. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>1, <3>d, <3>car, <1>old, <1>nl, <1>c
      <4>2. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>1, <3>d, <3>cf, <3>car, <1>old
      <4>. QED BY <4>1, <4>2, <3>dc, <3>t, <3>ls DEF A1TreePath
    <3>2. CASE /\ w'[s].local[p] = doc[p] /\ w'[s].baseline[p] = doc[p] /\ (Owed(s, p) \/ Converges(s, p))
      \* clean now: DTree is vacuous
      <4>1. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>2
      \* CPub3: owed was clean (old CPub3); converged holds the declared bytes in doc (DocCar)
      <4>2. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        <5>. SUFFICES ASSUME A1Decl(s, p), w[s].decl[p] # Nil
                      PROVE  w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
          BY <3>d, <3>cf, <3>car
        <5>1. CASE Owed(s, p) BY <5>1, <1>old DEF Owed
        <5>2. CASE Converges(s, p) /\ ~Owed(s, p)
          <6>1. w[s].local[p] # w[s].baseline[p] BY <5>2 DEF Converges
          <6>2. Content(w[s].local[p]) = Content(w[s].decl[p]) BY <6>1, <1>old, <1>nl
          <6>3. CASE w[s].local[p] # Nil /\ doc[p] # Nil /\ Content(w[s].local[p]) = Content(doc[p])
            BY <6>2, <6>3, <1>old
          <6>4. CASE w[s].local[p] = Nil /\ doc[p] = Nil
            \* the declared bytes would be Nil: a declared delete, not a file
            BY <6>2, <6>4 DEF Content
          <6>. QED BY <5>2, <6>3, <6>4 DEF Converges
        <5>. QED BY <5>1, <5>2, <3>2
      <4>. QED BY <4>1, <4>2, <3>dc, <3>t, <3>ls DEF A1TreePath
    <3>. QED BY <3>1, <3>2, <2>w, <1>R
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>R, <1>per, <1>m DEF A1Per, Opt
  <2>5. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>R DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, A1BoundKeep DEF A1IAK1
  \* the barrier's per-path conjuncts (scanned, claimed, cased) are vacuous at "consumed"
  <2>6. w'[s].pc = "consumed" BY <2>w, <1>R
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP
\* Sync: takes what is owed (clean paths) to the document; every field AK1
\* reads but the tree is kept; pc kept (outside the barrier).
LEMMA Sync_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1LostDead, NEW s \in Writers, Sync(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF Sync, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
<1>g. w[s].pc = "idle" /\ w[s].sStage # "mid" BY DEF Sync
<1>per. A1Per(s) BY A1PerAll
<1>F. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1> DEFINE R == SyncW(s, fail)
<1>R. /\ R.pc = w[s].pc
               /\ R.pend = w[s].pend /\ R.dset = w[s].dset /\ R.decl = w[s].decl /\ R.moved = w[s].moved /\ R.dMint = w[s].dMint /\ R.car = w[s].car /\ R.bound = w[s].bound /\ R.nt = w[s].nt /\ R.lost = w[s].lost /\ R.inst = w[s].inst /\ R.cId = w[s].cId /\ R.cFold = w[s].cFold
               /\ A1LeftSet(R) = A1LeftSet(w[s]) /\ R.sStage = w[s].sStage
               /\ \A q \in Paths : \/ R.local[q] = w[s].local[q] /\ R.baseline[q] = w[s].baseline[q]
                                   \/ R.local[q] = doc[q] /\ R.baseline[q] = doc[q] /\ Owed(s, q)
  <2>p. SyncW(s, fail).pc = w[s].pc BY DEF SyncW
  <2>k0. SyncW(s, fail).pend = w[s].pend /\ SyncW(s, fail).dset = w[s].dset /\ SyncW(s, fail).decl = w[s].decl BY DEF SyncW
  <2>k1. SyncW(s, fail).moved = w[s].moved /\ SyncW(s, fail).dMint = w[s].dMint /\ SyncW(s, fail).car = w[s].car BY DEF SyncW
  <2>k2. SyncW(s, fail).bound = w[s].bound /\ SyncW(s, fail).nt = w[s].nt /\ SyncW(s, fail).lost = w[s].lost BY DEF SyncW
  <2>k3. SyncW(s, fail).inst = w[s].inst /\ SyncW(s, fail).cId = w[s].cId /\ SyncW(s, fail).cFold = w[s].cFold BY DEF SyncW
  <2>t1. SyncW(s, fail).local = [p \in Paths |-> IF p \in SyncOwed(s, fail) THEN doc[p] ELSE w[s].local[p]] BY DEF SyncW
  <2>t2. SyncW(s, fail).baseline = SyncBl(s, fail) BY DEF SyncW
  <2>t. \A q \in Paths : \/ SyncW(s, fail).local[q] = w[s].local[q] /\ SyncW(s, fail).baseline[q] = w[s].baseline[q]
                         \/ SyncW(s, fail).local[q] = doc[q] /\ SyncW(s, fail).baseline[q] = doc[q] /\ Owed(s, q)
    <3>. SUFFICES ASSUME NEW q \in Paths
                  PROVE  \/ SyncW(s, fail).local[q] = w[s].local[q] /\ SyncW(s, fail).baseline[q] = w[s].baseline[q]
                         \/ SyncW(s, fail).local[q] = doc[q] /\ SyncW(s, fail).baseline[q] = doc[q] /\ Owed(s, q) OBVIOUS
    <3>1. CASE q \in SyncOwed(s, fail) BY <3>1, <2>t1, <2>t2 DEF SyncBl, SyncOwed, SyncAll
    <3>2. CASE q \notin SyncOwed(s, fail) BY <3>2, <2>t1, <2>t2 DEF SyncBl
    <3>. QED BY <3>1, <3>2
  \* the leftover is kept: a taken path was owed, so clean (a leftover is dirty:
  \* uncited with a file) and clean after (local' = baseline')
  <2>l. A1LeftSet(SyncW(s, fail)) = A1LeftSet(w[s])
    <3>0. w[s].sDrop \subseteq Paths BY <1>0, WriterFields DEF TypeOK
    <3>1. /\ SyncW(s, fail).sDrop = w[s].sDrop /\ SyncW(s, fail).sHeld = w[s].sHeld
          /\ SyncW(s, fail).sStage = w[s].sStage /\ SyncW(s, fail).sKeep = w[s].sKeep BY DEF SyncW
    <3>2. \A q \in Paths : q \in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] \/ w[s].local[q] = Nil
      BY DEF SyncOwed, SyncAll, Owed
    <3>3. \A q \in Paths : (q \in A1LeftSet(SyncW(s, fail))) <=> (q \in A1LeftSet(w[s]))
      <4>. SUFFICES ASSUME NEW q \in Paths
                    PROVE  (q \in A1LeftSet(SyncW(s, fail))) <=> (q \in A1LeftSet(w[s])) OBVIOUS
      <4>1. CASE q \in SyncOwed(s, fail)
        <5>1. SyncW(s, fail).local[q] = doc[q] /\ SyncW(s, fail).baseline[q] = doc[q] BY <4>1, <2>t1, <2>t2 DEF SyncBl
        <5>2. q \notin A1LeftSet(SyncW(s, fail)) BY <5>1, <3>1, <1>g DEF A1LeftSet
        <5>3. q \notin A1LeftSet(w[s]) BY <4>1, <3>2, <1>g DEF A1LeftSet
        <5>. QED BY <5>2, <5>3
      <4>2. CASE q \notin SyncOwed(s, fail)
        <5>1. SyncW(s, fail).local[q] = w[s].local[q] /\ SyncW(s, fail).baseline[q] = w[s].baseline[q] BY <4>2, <2>t1, <2>t2 DEF SyncBl
        <5>. QED BY <5>1, <3>1 DEF A1LeftSet
      <4>. QED BY <4>1, <4>2
    <3>4. A1LeftSet(SyncW(s, fail)) \subseteq Paths /\ A1LeftSet(w[s]) \subseteq Paths BY <3>0, <3>1 DEF A1LeftSet
    <3>5. \A x : (x \in A1LeftSet(SyncW(s, fail))) <=> (x \in A1LeftSet(w[s])) BY <3>3, <3>4
    <3>. QED BY <3>5, Zenon
  <2>st. SyncW(s, fail).sStage = w[s].sStage BY DEF SyncW
  <2>. QED BY <2>p, <2>k0, <2>k1, <2>k2, <2>k3, <2>l, <2>t, <2>st, Zenon DEF R
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0, <1>F DEF TypeOK
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>R
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
\* s's part; the old per-path facts, read once.
<1>old. \A q \in Paths :
          /\ (A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] /\ q \notin A1LeftSet(w[s])) =>
               Content(w[s].local[q]) = Content(w[s].decl[q])
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ w[s].local[q] = w[s].baseline[q]) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ q \in A1LeftSet(w[s])) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[s].decl[q])) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ w[s].decl[q] \in Opt(minted) /\ w[s].inst[q] \in Opt(minted) /\ w[s].car.pub[q] \in Opt(minted)
  BY <1>per DEF A1Per
<1>s. A1Per(s)'
  <2>w. w'[s] = R BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>R, <1>per, A1FoldsOf
  <2>2. w'[s].pc \notin {"consumed", "scanned", "claimed", "cased"} BY <2>w, <1>R, <1>g
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>R DEF A1Decl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>R, <1>old, <1>m DEF Opt
    <3>cf. CarForm(s, p, w[s].car)' <=> CarForm(s, p, w[s].car)
      <4>0. PICK c0 : c0 = w[s].car OBVIOUS
      <4>1. CarForm(s, p, c0)' <=> CarForm(s, p, c0) BY <2>w, <1>R, <1>c, <4>0 DEF CarForm
      <4>. QED BY <4>0, <4>1, <2>w, <1>R
    <3>car. w'[s].car = w[s].car /\ w'[s].pend = w[s].pend /\ w'[s].decl = w[s].decl BY <2>w, <1>R
    \* DocCar: doc, the declaration and the carrier are kept
    <3>dc. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
             (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
      BY <3>d, <3>cf, <3>car, <1>old, <1>dk, <1>c
    <3>1. CASE w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
      <4>1. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>1, <3>d, <3>car, <1>old, <1>c, <2>w, <1>R
      <4>l. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>d, <3>cf, <3>car, <1>old, <2>w, <1>R
      <4>2. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>1, <3>d, <3>cf, <3>car, <1>old
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>2. CASE w'[s].local[p] = doc[p] /\ w'[s].baseline[p] = doc[p] /\ Owed(s, p)
      \* clean now (DTree vacuous); it was clean (owed), so the old CPub3 holds
      <4>1. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>2
      <4>2. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>2, <3>d, <3>cf, <3>car, <1>old DEF Owed
      <4>l. p \notin A1LeftSet(w'[s]) BY <3>2, <2>w, <1>R, <1>g DEF A1LeftSet
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>. QED BY <3>1, <3>2, <2>w, <1>R
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>R, <1>per, <1>m DEF A1Per, Opt
  <2>5. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>R DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, A1BoundKeep DEF A1IAK1
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP
\* TO WRITE

------------------------------------------------------------------------------
(* The rescope's halves.  Both run at pc idle (the barrier conjuncts are     *)
(* vacuous for s) and keep every field but the tree (local, baseline): the *)
(* record, the declaration, the carrier, the bound, lost and gone.  The    *)
(* paths they do not touch keep A1TreePath; the touched ones (OMITTED      *)
(* below) are where DTree / CPub3 can break -- see each step's note.       *)

LEMMA RescopeFirst_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1LostDead, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>sh. RescopeUnciteFirst /\ RescopeKeepsDirty /\ WidenKeepsLocal /\ UnlinkChecksBytes
  BY ShippedShape DEF Shipped
<1>e. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>1. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF RescopeFirst, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>w. \A u \in Writers : w'[u] = IF u = s THEN RescopeFirstW(s) ELSE w[u] BY <1>0, <1>e DEF TypeOK
<1>g. w[s].pc = "idle" BY DEF RescopeFirst
<1>per. A1Per(s) BY A1PerAll
<1>gs. w[s].sStage = "saved" BY DEF RescopeFirst
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
\* the leftover after ("mid"): the uncited paths.  One cited before was clean
\* (not kept), so it holds the handle sHeld now records; one uncited before (a
\* replay after a restart) keeps its record, so it is a leftover after iff
\* before.  A kept path is neither.
<1>ls. \A p \in Paths :
         /\ p \notin RescopeFirstDd(s) => p \notin A1LeftSet(w'[s]) /\ p \notin A1LeftSet(w[s])
         /\ p \in RescopeFirstDd(s) /\ w[s].baseline[p] # Nil =>
              w[s].local[p] = w[s].baseline[p] /\ p \in A1LeftSet(w'[s]) /\ p \notin A1LeftSet(w[s])
         /\ p \in RescopeFirstDd(s) /\ w[s].baseline[p] = Nil =>
              (p \in A1LeftSet(w'[s]) <=> p \in A1LeftSet(w[s]))
  <2>. SUFFICES ASSUME NEW p \in Paths
                PROVE  /\ p \notin RescopeFirstDd(s) => p \notin A1LeftSet(w'[s]) /\ p \notin A1LeftSet(w[s])
                       /\ p \in RescopeFirstDd(s) /\ w[s].baseline[p] # Nil =>
                            w[s].local[p] = w[s].baseline[p] /\ p \in A1LeftSet(w'[s]) /\ p \notin A1LeftSet(w[s])
                       /\ p \in RescopeFirstDd(s) /\ w[s].baseline[p] = Nil =>
                            (p \in A1LeftSet(w'[s]) <=> p \in A1LeftSet(w[s])) OBVIOUS
  <2>1. RescopeFirstW(s).sStage = "mid" /\ RescopeFirstW(s).sDrop = w[s].sDrop /\ RescopeFirstW(s).sKeep = KeepSet(s)
    BY DEF RescopeFirstW
  <2>2. RescopeFirstW(s).local = w[s].local BY <1>sh DEF RescopeFirstW
  <2>3. RescopeFirstW(s).sHeld[p] = IF p \in RescopeFirstDd(s) /\ w[s].baseline[p] # Nil THEN w[s].baseline[p] ELSE w[s].sHeld[p]
    BY DEF RescopeFirstW
  <2>4. RescopeFirstW(s).baseline[p] = IF p \in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[p] BY <1>sh DEF RescopeFirstW
  <2>5. KeepSet(s) = {q \in w[s].sDrop : w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q]} BY <1>sh DEF KeepSet
  <2>6. w'[s] = RescopeFirstW(s) BY <1>w
  <2>7. w[s].baseline[p] # Nil => w[s].baseline[p] # Nil OBVIOUS
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <1>gs DEF A1LeftSet, RescopeFirstDd
\* Every field AK1's conjuncts read but the tree (local, baseline) is kept.
<1>fx. /\ RescopeFirstW(s).pc = w[s].pc
       /\ RescopeFirstW(s).pend = w[s].pend
       /\ RescopeFirstW(s).dset = w[s].dset
       /\ RescopeFirstW(s).decl = w[s].decl
       /\ RescopeFirstW(s).moved = w[s].moved
       /\ RescopeFirstW(s).dMint = w[s].dMint
       /\ RescopeFirstW(s).car = w[s].car
       /\ RescopeFirstW(s).bound = w[s].bound
       /\ RescopeFirstW(s).lost = w[s].lost
       /\ RescopeFirstW(s).gone = w[s].gone
       /\ RescopeFirstW(s).nt = w[s].nt
       /\ RescopeFirstW(s).inst = w[s].inst
       /\ RescopeFirstW(s).cId = w[s].cId
       /\ RescopeFirstW(s).cFold = w[s].cFold
       /\ RescopeFirstW(s).uploads = w[s].uploads
       /\ RescopeFirstW(s).deletes = w[s].deletes
       /\ RescopeFirstW(s).snap = w[s].snap
  <2>1. RescopeFirstW(s).pc = w[s].pc BY DEF RescopeFirstW
  <2>2. RescopeFirstW(s).pend = w[s].pend BY DEF RescopeFirstW
  <2>3. RescopeFirstW(s).dset = w[s].dset BY DEF RescopeFirstW
  <2>4. RescopeFirstW(s).decl = w[s].decl BY DEF RescopeFirstW
  <2>5. RescopeFirstW(s).moved = w[s].moved BY DEF RescopeFirstW
  <2>6. RescopeFirstW(s).dMint = w[s].dMint BY DEF RescopeFirstW
  <2>7. RescopeFirstW(s).car = w[s].car BY DEF RescopeFirstW
  <2>8. RescopeFirstW(s).bound = w[s].bound BY DEF RescopeFirstW
  <2>9. RescopeFirstW(s).lost = w[s].lost BY DEF RescopeFirstW
  <2>10. RescopeFirstW(s).gone = w[s].gone BY DEF RescopeFirstW
  <2>11. RescopeFirstW(s).nt = w[s].nt BY DEF RescopeFirstW
  <2>12. RescopeFirstW(s).inst = w[s].inst BY DEF RescopeFirstW
  <2>13. RescopeFirstW(s).cId = w[s].cId BY DEF RescopeFirstW
  <2>14. RescopeFirstW(s).cFold = w[s].cFold BY DEF RescopeFirstW
  <2>15. RescopeFirstW(s).uploads = w[s].uploads BY DEF RescopeFirstW
  <2>16. RescopeFirstW(s).deletes = w[s].deletes BY DEF RescopeFirstW
  <2>17. RescopeFirstW(s).snap = w[s].snap BY DEF RescopeFirstW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17
<1>m. minted \subseteq minted' BY <1>1
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>s. A1Per(s)'
  <2>w. w'[s] = RescopeFirstW(s) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>fx, <1>per, A1FoldsOf
  <2>2. w'[s].pc = "idle" BY <2>w, <1>fx, <1>g
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>fx DEF A1Decl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
    \* the carrier as a constant (CarForm's prime would double-prime w'[s].car)
    <3>c0. PICK c0 : c0 = w[s].car OBVIOUS
    <3>cf. CarForm(s, p, c0)' <=> CarForm(s, p, c0) BY <2>w, <1>fx, <1>c, <3>c0 DEF CarForm
    <3>1. CASE p \notin RescopeFirstDd(s)
      \* the tree is kept at p: every part of A1TreePath at p is kept
      <4>1. w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p] BY <3>1, <2>w, <1>sh DEF RescopeFirstW
      <4>2. p \notin A1LeftSet(w'[s]) /\ p \notin A1LeftSet(w[s]) BY <3>1, <1>ls
      <4>. QED BY <3>d, <3>t, <3>c0, <3>cf, <4>1, <4>2, <2>w, <1>fx, <1>c, <1>per, <1>dk DEF A1Per, A1TreePath
    <3>2. CASE p \in RescopeFirstDd(s)
      \* The uncite: baseline'[p] = Nil, local kept, and p is the narrow's leftover
      \* now, so DTree' spares it.  Not in KeepSet: p was uncited already or clean,
      \* so clean' (local' = Nil = baseline') means it was clean: the old CPub3.
      <4>1. w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = Nil BY <3>2, <2>w, <1>sh DEF RescopeFirstW
      <4>2. w[s].baseline[p] = Nil \/ w[s].local[p] = w[s].baseline[p] BY <3>2, <1>sh DEF RescopeFirstDd, KeepSet
      <4>car. w'[s].car = w[s].car /\ w'[s].pend = w[s].pend /\ w'[s].decl = w[s].decl BY <2>w, <1>fx
      <4>o. /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p]) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s])) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            /\ (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s])) =>
                 Content(w[s].local[p]) = Content(w[s].decl[p])
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p])) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
        BY <1>per DEF A1Per
      \* DTree': a leftover after is spared; else p was uncited before and no
      \* leftover, and dirty (a file, no baseline): the old DTree
      <4>a. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        <5>. SUFFICES ASSUME A1Decl(s, p), w'[s].local[p] # w'[s].baseline[p], p \notin A1LeftSet(w'[s])
                      PROVE  Content(w[s].local[p]) = Content(w[s].decl[p])
          BY <3>d, <4>car, <1>c, <4>1
        <5>1. w[s].baseline[p] = Nil BY <3>2, <1>ls
        <5>2. p \notin A1LeftSet(w[s]) BY <3>2, <5>1, <1>ls
        <5>3. w[s].local[p] # w[s].baseline[p] BY <4>1, <5>1
        <5>. QED BY <5>2, <5>3, <4>o
      \* CPubL': a leftover after was clean before (CPub3) or a leftover before
      <4>l. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        <5>. SUFFICES ASSUME A1Decl(s, p), w[s].decl[p] # Nil, p \in A1LeftSet(w'[s])
                      PROVE  w[s].car.id = w[s].pend.id /\ CarForm(s, p, c0)
          BY <3>d, <3>cf, <3>c0, <4>car
        <5>1. CASE w[s].baseline[p] # Nil BY <5>1, <3>2, <1>ls, <4>o, <3>c0
        <5>2. CASE w[s].baseline[p] = Nil BY <5>2, <3>2, <1>ls, <4>o, <3>c0
        <5>. QED BY <5>1, <5>2
      <4>b. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        <5>. SUFFICES ASSUME A1Decl(s, p), w[s].decl[p] # Nil, w'[s].local[p] = w'[s].baseline[p]
                      PROVE  w[s].car.id = w[s].pend.id /\ CarForm(s, p, c0)
          BY <3>d, <3>cf, <3>c0, <4>car
        <5>1. w[s].local[p] = Nil BY <4>1
        <5>2. w[s].local[p] = w[s].baseline[p] BY <5>1, <4>2
        <5>. QED BY <5>2, <4>o, <3>c0
      <4>c. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>d, <3>cf, <3>c0, <4>car, <4>o, <1>dk, <1>c
      <4>. QED BY <4>a, <4>l, <4>b, <4>c, <3>t DEF A1TreePath
    <3>. QED BY <3>1, <3>2
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
  <2>5. (Honorable(s) => \A q \in Paths : Broken(s, q, w[s].bound.doc) => q \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>fx DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, A1BoundKeep DEF A1IAK1
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

LEMMA RescopeSecond_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1LostDead, A1HeldSame, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>sh. RescopeUnciteFirst /\ RescopeKeepsDirty /\ WidenKeepsLocal /\ UnlinkChecksBytes
  BY ShippedShape DEF Shipped
<1>e. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>1. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF RescopeSecond, bucket
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>w. \A u \in Writers : w'[u] = IF u = s THEN RescopeSecondW(s, wfail) ELSE w[u] BY <1>0, <1>e DEF TypeOK
<1>g. w[s].pc = "idle" BY DEF RescopeSecond
<1>per. A1Per(s) BY A1PerAll
<1>gm. w[s].sStage = "mid" BY DEF RescopeSecond
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
\* the narrow is over: no leftover after; what it drops was uncited (R4)
<1>nl. A1LeftSet(w'[s]) = {}
  <2>1. RescopeSecondW(s, wfail).sStage = "none" BY DEF RescopeSecondW
  <2>. QED BY <2>1, <1>w DEF A1LeftSet
<1>r4. \A q \in RescopeDrop(s) : w[s].baseline[q] = Nil
  BY <1>gm DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R4, RescopeDrop
\* Every field AK1's conjuncts read but the tree (local, baseline) is kept.
<1>fx. /\ RescopeSecondW(s, wfail).pc = w[s].pc
       /\ RescopeSecondW(s, wfail).pend = w[s].pend
       /\ RescopeSecondW(s, wfail).dset = w[s].dset
       /\ RescopeSecondW(s, wfail).decl = w[s].decl
       /\ RescopeSecondW(s, wfail).moved = w[s].moved
       /\ RescopeSecondW(s, wfail).dMint = w[s].dMint
       /\ RescopeSecondW(s, wfail).car = w[s].car
       /\ RescopeSecondW(s, wfail).bound = w[s].bound
       /\ RescopeSecondW(s, wfail).lost = w[s].lost
       /\ RescopeSecondW(s, wfail).gone = w[s].gone
       /\ RescopeSecondW(s, wfail).nt = w[s].nt
       /\ RescopeSecondW(s, wfail).inst = w[s].inst
       /\ RescopeSecondW(s, wfail).cId = w[s].cId
       /\ RescopeSecondW(s, wfail).cFold = w[s].cFold
       /\ RescopeSecondW(s, wfail).uploads = w[s].uploads
       /\ RescopeSecondW(s, wfail).deletes = w[s].deletes
       /\ RescopeSecondW(s, wfail).snap = w[s].snap
  <2>1. RescopeSecondW(s, wfail).pc = w[s].pc BY DEF RescopeSecondW
  <2>2. RescopeSecondW(s, wfail).pend = w[s].pend BY DEF RescopeSecondW
  <2>3. RescopeSecondW(s, wfail).dset = w[s].dset BY DEF RescopeSecondW
  <2>4. RescopeSecondW(s, wfail).decl = w[s].decl BY DEF RescopeSecondW
  <2>5. RescopeSecondW(s, wfail).moved = w[s].moved BY DEF RescopeSecondW
  <2>6. RescopeSecondW(s, wfail).dMint = w[s].dMint BY DEF RescopeSecondW
  <2>7. RescopeSecondW(s, wfail).car = w[s].car BY DEF RescopeSecondW
  <2>8. RescopeSecondW(s, wfail).bound = w[s].bound BY DEF RescopeSecondW
  <2>9. RescopeSecondW(s, wfail).lost = w[s].lost BY DEF RescopeSecondW
  <2>10. RescopeSecondW(s, wfail).gone = w[s].gone BY DEF RescopeSecondW
  <2>11. RescopeSecondW(s, wfail).nt = w[s].nt BY DEF RescopeSecondW
  <2>12. RescopeSecondW(s, wfail).inst = w[s].inst BY DEF RescopeSecondW
  <2>13. RescopeSecondW(s, wfail).cId = w[s].cId BY DEF RescopeSecondW
  <2>14. RescopeSecondW(s, wfail).cFold = w[s].cFold BY DEF RescopeSecondW
  <2>15. RescopeSecondW(s, wfail).uploads = w[s].uploads BY DEF RescopeSecondW
  <2>16. RescopeSecondW(s, wfail).deletes = w[s].deletes BY DEF RescopeSecondW
  <2>17. RescopeSecondW(s, wfail).snap = w[s].snap BY DEF RescopeSecondW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17
<1>m. minted \subseteq minted' BY <1>1
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>s. A1Per(s)'
  <2>w. w'[s] = RescopeSecondW(s, wfail) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>fx, <1>per, A1FoldsOf
  <2>2. w'[s].pc = "idle" BY <2>w, <1>fx, <1>g
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>fx DEF A1Decl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
    \* the carrier as a constant (CarForm's prime would double-prime w'[s].car)
    <3>c0. PICK c0 : c0 = w[s].car OBVIOUS
    <3>cf. CarForm(s, p, c0)' <=> CarForm(s, p, c0) BY <2>w, <1>fx, <1>c, <3>c0 DEF CarForm
    <3>1. CASE ~RescopeUnlink(s, p) /\ p \notin RescopeFetch(s, wfail)
      \* the tree is kept at p: every part of A1TreePath at p is kept
      <4>1. w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p] BY <3>1, <2>w, <1>sh DEF RescopeSecondW, RescopeLocal1, RescopeBase1
      \* no leftover: a leftover is unlinked (its file is the held handle)
      <4>2. p \notin A1LeftSet(w[s]) BY <3>1, <1>sh, <1>gm DEF RescopeUnlink, RescopeDrop, A1LeftSet
      <4>. QED BY <3>d, <3>t, <3>c0, <3>cf, <4>1, <4>2, <2>w, <1>fx, <1>c, <1>per, <1>nl, <1>dk DEF A1Per, A1TreePath
    <3>2. CASE RescopeUnlink(s, p) \/ p \in RescopeFetch(s, wfail)
      \* Touched: an unlink (local' = Nil; baseline Nil, R4) or a widen fetch
      \* (baseline' = doc[p]; local' = doc[p] where the tree had nothing, else
      \* kept: an adopt, its bytes ARE the document's).
      <4>L1. RescopeLocal1(s)[p] = IF RescopeUnlink(s, p) THEN Nil ELSE w[s].local[p] BY <1>sh DEF RescopeLocal1
      <4>L. w'[s].local[p] = IF p \in RescopeFetch(s, wfail) /\ RescopeLocal1(s)[p] = Nil THEN doc[p] ELSE RescopeLocal1(s)[p]
        BY <2>w, <1>sh DEF RescopeSecondW
      <4>B. w'[s].baseline[p] = IF p \in RescopeFetch(s, wfail) THEN doc[p] ELSE w[s].baseline[p]
        BY <2>w, <1>sh DEF RescopeSecondW, RescopeBase1
      \* a fetched path had no baseline; an adopted one holds the document's bytes
      <4>F. p \in RescopeFetch(s, wfail) =>
              /\ w[s].baseline[p] = Nil /\ doc[p] # Nil
              /\ RescopeLocal1(s)[p] # Nil => Content(RescopeLocal1(s)[p]) = Content(doc[p])
        BY <1>sh DEF RescopeFetch, RescopeFetch0, RescopeAdd, RescopeKept, RescopeBase1
      <4>car. w'[s].car = w[s].car /\ w'[s].pend = w[s].pend /\ w'[s].decl = w[s].decl BY <2>w, <1>fx
      <4>o. /\ (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s])) =>
                 Content(w[s].local[p]) = Content(w[s].decl[p])
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p]) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s])) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            /\ (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p])) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
        BY <1>per DEF A1Per
      \* an unlinked path was the leftover: its file holds the held bytes, so the
      \* held handle (A1HeldSame)
      <4>U. RescopeUnlink(s, p) => p \in A1LeftSet(w[s])
        <5>. SUFFICES ASSUME RescopeUnlink(s, p) PROVE p \in A1LeftSet(w[s]) OBVIOUS
        <5>1. A1LeftCand(w[s], p) BY <1>sh, <1>gm DEF RescopeUnlink, RescopeDrop, A1LeftCand
        <5>2. w[s].local[p] = w[s].sHeld[p] BY <5>1, <1>sh DEF RescopeUnlink, A1HeldSame
        <5>. QED BY <5>1, <5>2 DEF A1LeftCand, A1LeftSet
      \* not unlinked: not the leftover (A1LeftSet's file would unlink)
      <4>N. ~RescopeUnlink(s, p) => p \notin A1LeftSet(w[s]) BY <1>sh, <1>gm DEF RescopeUnlink, RescopeDrop, A1LeftSet
      \* DTree': clean after but for an adopt, whose bytes were the declared ones
      <4>a. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        <5>. SUFFICES ASSUME A1Decl(s, p), w'[s].local[p] # w'[s].baseline[p]
                      PROVE  (Content(w[s].local[p]) = Content(w[s].decl[p]))'
          BY <3>d
        <5>1. ~RescopeUnlink(s, p) /\ p \in RescopeFetch(s, wfail) /\ w[s].local[p] # Nil
          <6>1. CASE RescopeUnlink(s, p) /\ p \notin RescopeFetch(s, wfail)
            <7>1. w'[s].local[p] = Nil /\ w'[s].baseline[p] = Nil BY <6>1, <4>L, <4>L1, <4>B, <1>r4 DEF RescopeUnlink
            <7>. QED BY <7>1
          <6>2. CASE p \in RescopeFetch(s, wfail) /\ RescopeLocal1(s)[p] = Nil BY <6>2, <4>L, <4>B
          <6>. QED BY <6>1, <6>2, <3>2, <4>L1
        <5>2. w'[s].local[p] = w[s].local[p] BY <5>1, <4>L, <4>L1
        <5>3. w[s].local[p] # w[s].baseline[p] BY <5>1, <4>F
        <5>4. Content(w[s].local[p]) = Content(w[s].decl[p]) BY <5>1, <5>3, <4>N, <4>o
        <5>. QED BY <5>2, <5>4, <4>car, <1>c
      \* CPub3': an unlink is the leftover (CPubL); an adopt clean after holds the
      \* document's bytes, the declared ones (DocCar); a fetch over nothing was clean
      <4>b. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        <5>. SUFFICES ASSUME A1Decl(s, p), w[s].decl[p] # Nil, w'[s].local[p] = w'[s].baseline[p]
                      PROVE  w[s].car.id = w[s].pend.id /\ CarForm(s, p, c0)
          BY <3>d, <3>cf, <3>c0, <4>car
        <5>1. CASE RescopeUnlink(s, p) BY <5>1, <4>U, <4>o, <3>c0
        <5>2. CASE ~RescopeUnlink(s, p) /\ w[s].local[p] = Nil
          <6>1. p \in RescopeFetch(s, wfail) BY <5>2, <3>2
          <6>2. w[s].local[p] = w[s].baseline[p] BY <5>2, <6>1, <4>F
          <6>. QED BY <6>2, <4>o, <3>c0
        <5>3. CASE ~RescopeUnlink(s, p) /\ w[s].local[p] # Nil
          <6>1. p \in RescopeFetch(s, wfail) BY <5>3, <3>2
          <6>2. w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = doc[p] BY <5>3, <6>1, <4>L, <4>L1, <4>B
          <6>3. w[s].local[p] = doc[p] BY <6>2
          <6>4. w[s].local[p] # w[s].baseline[p] BY <5>3, <6>1, <4>F
          <6>5. Content(w[s].local[p]) = Content(w[s].decl[p]) BY <5>3, <6>4, <4>N, <4>o
          <6>6. doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]) BY <6>1, <6>3, <6>5, <4>F
          <6>. QED BY <6>6, <4>o, <3>c0
        <5>. QED BY <5>1, <5>2, <5>3
      <4>l. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <1>nl
      <4>c. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>d, <3>cf, <3>c0, <4>car, <4>o, <1>dk, <1>c
      <4>. QED BY <4>a, <4>b, <4>l, <4>c, <3>t DEF A1TreePath
    <3>. QED BY <3>1, <3>2
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
  <2>5. (Honorable(s) => \A q \in Paths : Broken(s, q, w[s].bound.doc) => q \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>fx DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, A1BoundKeep DEF A1IAK1
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP
\* RPullSync: takes what is owed (clean paths) to the document; every field AK1
\* reads but the tree is kept; pc pulling -> idle (outside the barrier).
LEMMA RPullSync_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1LostDead, NEW s \in Writers, RPullSync(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. UNCHANGED <<live, minted, base, conflicts, orig, doc>> BY DEF RPullSync, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
<1>g. w[s].pc = "pulling" /\ w[s].sStage # "mid"
  BY DEF RPullSync, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R5
<1>per. A1Per(s) BY A1PerAll
<1>F. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1> DEFINE R == RPullSyncW(s, fail)
<1>R. /\ R.pc = "idle"
               /\ R.pend = w[s].pend /\ R.dset = w[s].dset /\ R.decl = w[s].decl /\ R.moved = w[s].moved /\ R.dMint = w[s].dMint /\ R.car = w[s].car /\ R.bound = w[s].bound /\ R.nt = w[s].nt /\ R.lost = w[s].lost /\ R.inst = w[s].inst /\ R.cId = w[s].cId /\ R.cFold = w[s].cFold
               /\ A1LeftSet(R) = A1LeftSet(w[s]) /\ R.sStage = w[s].sStage
               /\ \A q \in Paths : \/ R.local[q] = w[s].local[q] /\ R.baseline[q] = w[s].baseline[q]
                                   \/ R.local[q] = doc[q] /\ R.baseline[q] = doc[q] /\ Owed(s, q)
  <2>p. RPullSyncW(s, fail).pc = "idle" BY DEF RPullSyncW
  <2>k0. RPullSyncW(s, fail).pend = w[s].pend /\ RPullSyncW(s, fail).dset = w[s].dset /\ RPullSyncW(s, fail).decl = w[s].decl BY DEF RPullSyncW
  <2>k1. RPullSyncW(s, fail).moved = w[s].moved /\ RPullSyncW(s, fail).dMint = w[s].dMint /\ RPullSyncW(s, fail).car = w[s].car BY DEF RPullSyncW
  <2>k2. RPullSyncW(s, fail).bound = w[s].bound /\ RPullSyncW(s, fail).nt = w[s].nt /\ RPullSyncW(s, fail).lost = w[s].lost BY DEF RPullSyncW
  <2>k3. RPullSyncW(s, fail).inst = w[s].inst /\ RPullSyncW(s, fail).cId = w[s].cId /\ RPullSyncW(s, fail).cFold = w[s].cFold BY DEF RPullSyncW
  <2>t1. RPullSyncW(s, fail).local = [p \in Paths |-> IF p \in SyncOwed(s, fail) THEN doc[p] ELSE w[s].local[p]] BY DEF RPullSyncW
  <2>t2. RPullSyncW(s, fail).baseline = SyncBl(s, fail) BY DEF RPullSyncW
  <2>t. \A q \in Paths : \/ RPullSyncW(s, fail).local[q] = w[s].local[q] /\ RPullSyncW(s, fail).baseline[q] = w[s].baseline[q]
                         \/ RPullSyncW(s, fail).local[q] = doc[q] /\ RPullSyncW(s, fail).baseline[q] = doc[q] /\ Owed(s, q)
    <3>. SUFFICES ASSUME NEW q \in Paths
                  PROVE  \/ RPullSyncW(s, fail).local[q] = w[s].local[q] /\ RPullSyncW(s, fail).baseline[q] = w[s].baseline[q]
                         \/ RPullSyncW(s, fail).local[q] = doc[q] /\ RPullSyncW(s, fail).baseline[q] = doc[q] /\ Owed(s, q) OBVIOUS
    <3>1. CASE q \in SyncOwed(s, fail) BY <3>1, <2>t1, <2>t2 DEF SyncBl, SyncOwed, SyncAll
    <3>2. CASE q \notin SyncOwed(s, fail) BY <3>2, <2>t1, <2>t2 DEF SyncBl
    <3>. QED BY <3>1, <3>2
  \* the leftover is kept: a taken path was owed, so clean (a leftover is dirty:
  \* uncited with a file) and clean after (local' = baseline')
  <2>l. A1LeftSet(RPullSyncW(s, fail)) = A1LeftSet(w[s])
    <3>0. w[s].sDrop \subseteq Paths BY <1>0, WriterFields DEF TypeOK
    <3>1. /\ RPullSyncW(s, fail).sDrop = w[s].sDrop /\ RPullSyncW(s, fail).sHeld = w[s].sHeld
          /\ RPullSyncW(s, fail).sStage = w[s].sStage /\ RPullSyncW(s, fail).sKeep = w[s].sKeep BY DEF RPullSyncW
    <3>2. \A q \in Paths : q \in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] \/ w[s].local[q] = Nil
      BY DEF SyncOwed, SyncAll, Owed
    <3>3. \A q \in Paths : (q \in A1LeftSet(RPullSyncW(s, fail))) <=> (q \in A1LeftSet(w[s]))
      <4>. SUFFICES ASSUME NEW q \in Paths
                    PROVE  (q \in A1LeftSet(RPullSyncW(s, fail))) <=> (q \in A1LeftSet(w[s])) OBVIOUS
      <4>1. CASE q \in SyncOwed(s, fail)
        <5>1. RPullSyncW(s, fail).local[q] = doc[q] /\ RPullSyncW(s, fail).baseline[q] = doc[q] BY <4>1, <2>t1, <2>t2 DEF SyncBl
        <5>2. q \notin A1LeftSet(RPullSyncW(s, fail)) BY <5>1, <3>1, <1>g DEF A1LeftSet
        <5>3. q \notin A1LeftSet(w[s]) BY <4>1, <3>2, <1>g DEF A1LeftSet
        <5>. QED BY <5>2, <5>3
      <4>2. CASE q \notin SyncOwed(s, fail)
        <5>1. RPullSyncW(s, fail).local[q] = w[s].local[q] /\ RPullSyncW(s, fail).baseline[q] = w[s].baseline[q] BY <4>2, <2>t1, <2>t2 DEF SyncBl
        <5>. QED BY <5>1, <3>1 DEF A1LeftSet
      <4>. QED BY <4>1, <4>2
    <3>4. A1LeftSet(RPullSyncW(s, fail)) \subseteq Paths /\ A1LeftSet(w[s]) \subseteq Paths BY <3>0, <3>1 DEF A1LeftSet
    <3>5. \A x : (x \in A1LeftSet(RPullSyncW(s, fail))) <=> (x \in A1LeftSet(w[s])) BY <3>3, <3>4
    <3>. QED BY <3>5, Zenon
  <2>st. RPullSyncW(s, fail).sStage = w[s].sStage BY DEF RPullSyncW
  <2>. QED BY <2>p, <2>k0, <2>k1, <2>k2, <2>k3, <2>l, <2>t, <2>st, Zenon DEF R
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0, <1>F DEF TypeOK
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>R
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
\* s's part; the old per-path facts, read once.
<1>old. \A q \in Paths :
          /\ (A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] /\ q \notin A1LeftSet(w[s])) =>
               Content(w[s].local[q]) = Content(w[s].decl[q])
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ w[s].local[q] = w[s].baseline[q]) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ q \in A1LeftSet(w[s])) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[s].decl[q])) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ w[s].decl[q] \in Opt(minted) /\ w[s].inst[q] \in Opt(minted) /\ w[s].car.pub[q] \in Opt(minted)
  BY <1>per DEF A1Per
<1>s. A1Per(s)'
  <2>w. w'[s] = R BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>R, <1>per, A1FoldsOf
  <2>2. w'[s].pc \notin {"consumed", "scanned", "claimed", "cased"} BY <2>w, <1>R, <1>g
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>R DEF A1Decl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>R, <1>old, <1>m DEF Opt
    <3>cf. CarForm(s, p, w[s].car)' <=> CarForm(s, p, w[s].car)
      <4>0. PICK c0 : c0 = w[s].car OBVIOUS
      <4>1. CarForm(s, p, c0)' <=> CarForm(s, p, c0) BY <2>w, <1>R, <1>c, <4>0 DEF CarForm
      <4>. QED BY <4>0, <4>1, <2>w, <1>R
    <3>car. w'[s].car = w[s].car /\ w'[s].pend = w[s].pend /\ w'[s].decl = w[s].decl BY <2>w, <1>R
    \* DocCar: doc, the declaration and the carrier are kept
    <3>dc. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
             (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
      BY <3>d, <3>cf, <3>car, <1>old, <1>dk, <1>c
    <3>1. CASE w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
      <4>1. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>1, <3>d, <3>car, <1>old, <1>c, <2>w, <1>R
      <4>l. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in A1LeftSet(w[s]))' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>d, <3>cf, <3>car, <1>old, <2>w, <1>R
      <4>2. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>1, <3>d, <3>cf, <3>car, <1>old
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>2. CASE w'[s].local[p] = doc[p] /\ w'[s].baseline[p] = doc[p] /\ Owed(s, p)
      \* clean now (DTree vacuous); it was clean (owed), so the old CPub3 holds
      <4>1. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>2
      <4>2. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
        BY <3>2, <3>d, <3>cf, <3>car, <1>old DEF Owed
      <4>l. p \notin A1LeftSet(w'[s]) BY <3>2, <2>w, <1>R, <1>g DEF A1LeftSet
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>. QED BY <3>1, <3>2, <2>w, <1>R
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>R, <1>per, <1>m DEF A1Per, Opt
  <2>5. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>R DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, A1BoundKeep DEF A1IAK1
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP
\* The install: pc claimed -> cased; doc' = InstallInst(s); the carrier is
\* InstallCar when something changed.  s's part: a declared FILE not withheld is
\* in carrier form after (<1>cf: clean -- the old CPub3, the carrier kept on it;
\* dirty -- MidBar makes it an upload done (A1UpsDone), InstallCar publishes its
\* snap, or nothing changed and the document holds it, DocCar); a withheld one in
\* carrier form stays so (<1>cg: dropped).  Every other tree: OMITTED below.
LEMMA Install_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1Side, NEW s \in Writers, Install(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = InstallW(s)] /\ doc' = InstallInst(s)
      /\ conflicts \subseteq conflicts' /\ UNCHANGED <<live, minted, base, orig>>
  BY DEF Install, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>w. \A u \in Writers : w'[u] = IF u = s THEN InstallW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>g. w[s].pc = "claimed" BY DEF Install
<1>per. A1Per(s) BY A1PerAll
\* no leftover in a barrier (R1: the rescope is not in flight)
<1>st. w[s].sStage = "none" BY <1>g DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>nl2. A1LeftSet(w[s]) = {} /\ A1LeftSet(w'[s]) = {}
  <2>1. InstallW(s).sStage = w[s].sStage BY DEF InstallW
  <2>. QED BY <1>st, <2>1, <1>w DEF A1LeftSet
<1>z. w[s].cId = w[s].pend.id /\ w[s].cFold = w[s].pend.fold BY <1>g, <1>per, A1FrozenOf
<1>r1. w[s].sStage = "none" BY <1>g DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>nl. A1LeftSet(w[s]) = {} BY <1>r1 DEF A1LeftSet
<1>ud. \A p \in w[s].uploads : w[s].baseline[p] # Nil => w[s].local[p] # w[s].baseline[p]
  BY <1>g DEF DInv, UD, UDR, Bar
<1>sn. \A p \in w[s].uploads : w[s].snap[p] # Nil
  BY <1>g DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Pending, Uploaded, Up
<1>mn. w[s].deletes \cap w[s].uploads = {} BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Mine
<1>ue. w[s].uploads \subseteq w[s].upDone \cup w[s].gone BY <1>g DEF A1Side, A1UpsDone
<1>tp. /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].gone \subseteq w[s].uploads
  <2>1. w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths BY TreeTy DEF IndC
  <2>2. w[s].gone \subseteq w[s].uploads BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Mine
  <2>. QED BY <2>1, <2>2
<1>db. \A p \in w[s].deletes : w[s].baseline[p] # Nil BY <1>g, <1>tp DEF A1Side, A1DelBase
<1>gu. \A p \in w[s].gone : \A c \in conflicts : c[2] # w[s].snap[p] BY <1>g, <1>tp DEF A1Side, A1GoneUnconf
<1>dm. \A q \in Paths : doc[q] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>sm. \A q \in Paths : w[s].snap[q] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, SnapMinted
\* InstallW: every field AK1 reads but pc, inst, car and lost is kept (one goal per few fields).
<1>fx. /\ InstallW(s).pc = "cased" /\ InstallW(s).local = w[s].local /\ InstallW(s).baseline = w[s].baseline
       /\ InstallW(s).pend = w[s].pend /\ InstallW(s).dset = w[s].dset /\ InstallW(s).decl = w[s].decl
       /\ InstallW(s).moved = w[s].moved /\ InstallW(s).dMint = w[s].dMint /\ InstallW(s).nt = w[s].nt
       /\ InstallW(s).uploads = w[s].uploads /\ InstallW(s).deletes = w[s].deletes /\ InstallW(s).snap = w[s].snap
       /\ InstallW(s).gone = w[s].gone /\ InstallW(s).cId = w[s].cId /\ InstallW(s).cFold = w[s].cFold
       /\ InstallW(s).bound = w[s].bound /\ InstallW(s).inst = InstallInst(s)
  <2>1. InstallW(s).pc = "cased" /\ InstallW(s).local = w[s].local /\ InstallW(s).baseline = w[s].baseline BY DEF InstallW
  <2>2. InstallW(s).pend = w[s].pend /\ InstallW(s).dset = w[s].dset /\ InstallW(s).decl = w[s].decl BY DEF InstallW
  <2>3. InstallW(s).moved = w[s].moved /\ InstallW(s).dMint = w[s].dMint /\ InstallW(s).nt = w[s].nt BY DEF InstallW
  <2>4. InstallW(s).uploads = w[s].uploads /\ InstallW(s).deletes = w[s].deletes /\ InstallW(s).snap = w[s].snap BY DEF InstallW
  <2>5. InstallW(s).gone = w[s].gone /\ InstallW(s).cId = w[s].cId /\ InstallW(s).cFold = w[s].cFold BY DEF InstallW
  <2>6. InstallW(s).bound = w[s].bound /\ InstallW(s).inst = InstallInst(s) BY DEF InstallW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6
<1>ls. A1LeftSet(InstallW(s)) = A1LeftSet(w[s]) BY DEF InstallW, A1LeftSet
<1>fc. InstallW(s).car = IF Carries(s) /\ ~InstallNothing(s) THEN InstallCar(s) ELSE w[s].car BY DEF InstallW
<1>fl. InstallW(s).lost = IF MaxTouches > 0 THEN w[s].lost \cup {w[s].snap[p] : p \in w[s].gone} ELSE w[s].lost
  BY DEF InstallW
\* the installed document, path by path
<1>ii. \A p \in Paths :
         /\ p \in w[s].gone => InstallInst(s)[p] = doc[p]
         /\ p \notin w[s].gone /\ p \in w[s].uploads \cap w[s].upDone => InstallInst(s)[p] = w[s].snap[p]
         /\ InstallInst(s)[p] \in {doc[p], w[s].snap[p], Nil}
  BY DEF InstallInst, InstallMine
\* InstallCar, field by field (its base is s's carrier when that is this record's)
<1>ic. /\ InstallCar(s).id = w[s].cId
       /\ w[s].car.id = w[s].cId => InstallCar0(s) = w[s].car
       /\ InstallCar(s).paths = InstallCar0(s).paths \cup w[s].uploads \cup w[s].deletes
       /\ InstallCar(s).deletes = (InstallCar0(s).deletes \ w[s].uploads) \cup w[s].deletes
       /\ InstallCar0(s).dropped \cup w[s].gone \subseteq InstallCar(s).dropped
       /\ InstallCar(s).uncited = InstallCar0(s).uncited
       /\ \A p \in Paths : InstallCar(s).pub[p] = IF p \in w[s].uploads \ w[s].gone THEN w[s].snap[p]
                                                  ELSE IF p \in w[s].uploads \cup w[s].deletes THEN Nil
                                                  ELSE InstallCar0(s).pub[p]
  <2>1. InstallCar(s).id = w[s].cId /\ (w[s].car.id = w[s].cId => InstallCar0(s) = w[s].car) BY DEF InstallCar, InstallCar0
  <2>2. InstallCar(s).paths = InstallCar0(s).paths \cup w[s].uploads \cup w[s].deletes BY DEF InstallCar
  <2>3. InstallCar(s).deletes = (InstallCar0(s).deletes \ w[s].uploads) \cup w[s].deletes BY DEF InstallCar
  <2>4. InstallCar0(s).dropped \cup w[s].gone \subseteq InstallCar(s).dropped BY DEF InstallCar
  <2>5. InstallCar(s).uncited = InstallCar0(s).uncited BY DEF InstallCar
  <2>6. \A p \in Paths : InstallCar(s).pub[p] = IF p \in w[s].uploads \ w[s].gone THEN w[s].snap[p]
                                               ELSE IF p \in w[s].uploads \cup w[s].deletes THEN Nil
                                               ELSE InstallCar0(s).pub[p] BY DEF InstallCar
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
\* (orig is kept: a handle's content is the same read before or after)
<1>hc. \A h : Content(h)' = Content(h) BY <1>1 DEF Content
<1>m. minted \subseteq minted' BY <1>1
\* the old per-path facts, read once
<1>old. \A q \in Paths :
          /\ (A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q] /\ q \notin A1LeftSet(w[s])) =>
               Content(w[s].local[q]) = Content(w[s].decl[q])
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ w[s].local[q] = w[s].baseline[q]) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ (A1Decl(s, q) /\ w[s].decl[q] # Nil /\ doc[q] # Nil /\ Content(doc[q]) = Content(w[s].decl[q])) =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, q, w[s].car)
          /\ w[s].decl[q] \in Opt(minted) /\ w[s].inst[q] \in Opt(minted) /\ w[s].car.pub[q] \in Opt(minted)
          /\ (w[s].cId # 0 /\ A1Decl(s, q) /\ w[s].local[q] # w[s].baseline[q]) =>
               \/ q \in w[s].uploads /\ w[s].snap[q] = w[s].local[q]
               \/ q \in w[s].deletes /\ w[s].local[q] = Nil
          /\ (w[s].cId # 0 /\ A1Decl(s, q)) =>
               /\ q \in w[s].uploads => w[s].snap[q] = w[s].local[q]
               /\ q \in w[s].deletes => w[s].local[q] = Nil
  BY <1>per, <1>g DEF A1Per
\* the new carrier as a constant, and CarForm against it in either state
<1>K. PICK k : k = InstallW(s).car OBVIOUS
<1>kc. \A q \in Paths : w'[s].decl[q] = w[s].decl[q] /\ (CarForm(s, q, k)' <=> CarForm(s, q, k))
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE w'[s].decl[q] = w[s].decl[q] /\ (CarForm(s, q, k)' <=> CarForm(s, q, k)) OBVIOUS
  <2>1. w'[s].decl[q] = w[s].decl[q] BY <1>w, <1>fx
  <2>2. k.pub[q] \in {w[s].snap[q], Nil, InstallCar0(s).pub[q], w[s].car.pub[q]} BY <1>K, <1>fc, <1>ic
  <2>3. InstallCar0(s).pub[q] \in {w[s].car.pub[q], Nil} BY DEF InstallCar0
  <2>4. Content(k.pub[q])' = Content(k.pub[q])
    <3>1. PICK h \in Opt(minted) : h = k.pub[q] BY <2>2, <2>3, <1>sm, <1>old DEF Opt
    <3>. QED BY <3>1, <1>hs, A1ContentStep
  <2>. QED BY <2>1, <2>4, <1>c DEF CarForm
\* <1>cf: a declared file not withheld is in carrier form after the install
<1>cf. \A p \in Paths : (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ p \notin w[s].gone) =>
                         k.id = w[s].pend.id /\ CarForm(s, p, k)
  <2>. SUFFICES ASSUME NEW p \in Paths, A1Decl(s, p), w[s].decl[p] # Nil, p \notin w[s].gone
                PROVE  k.id = w[s].pend.id /\ CarForm(s, p, k) OBVIOUS
  <2>0. w[s].cId # 0 /\ Carries(s) BY <1>z DEF A1Decl, Carries
  \* a dirty declared file is an upload done, its snap the tree's (declared) bytes
  <2>u. w[s].local[p] # w[s].baseline[p] =>
          /\ p \in w[s].uploads /\ p \in w[s].upDone /\ p \notin w[s].deletes
          /\ w[s].snap[p] = w[s].local[p] /\ w[s].snap[p] # Nil
          /\ Content(w[s].snap[p]) = Content(w[s].decl[p])
    <3>. SUFFICES ASSUME w[s].local[p] # w[s].baseline[p]
                  PROVE  /\ p \in w[s].uploads /\ p \in w[s].upDone /\ p \notin w[s].deletes
                         /\ w[s].snap[p] = w[s].local[p] /\ w[s].snap[p] # Nil
                         /\ Content(w[s].snap[p]) = Content(w[s].decl[p]) OBVIOUS
    <3>1. Content(w[s].local[p]) = Content(w[s].decl[p]) BY <1>old, <1>nl
    <3>2. ~(p \in w[s].deletes /\ w[s].local[p] = Nil)
      \* the declared bytes would be Nil: a declared delete, not a file
      BY <3>1 DEF Content
    <3>3. p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p] BY <2>0, <3>2, <1>old
    <3>. QED BY <3>1, <3>3, <1>sn, <1>mn, <1>ue
  \* a clean declared file is neither an upload nor a delete (UDR, M2, DelBase, MidBar)
  <2>k. w[s].local[p] = w[s].baseline[p] => p \notin w[s].uploads /\ p \notin w[s].deletes
    <3>. SUFFICES ASSUME w[s].local[p] = w[s].baseline[p] PROVE p \notin w[s].uploads /\ p \notin w[s].deletes OBVIOUS
    <3>1. p \in w[s].uploads => w[s].snap[p] = w[s].local[p] /\ w[s].snap[p] # Nil BY <2>0, <1>old, <1>sn
    <3>2. p \in w[s].deletes => w[s].local[p] = Nil /\ w[s].baseline[p] # Nil BY <2>0, <1>old, <1>db
    <3>. QED BY <3>1, <3>2, <1>ud
  <2>1. CASE InstallNothing(s)
    <3>0. k = w[s].car BY <2>1, <1>K, <1>fc
    <3>1. CASE w[s].local[p] = w[s].baseline[p] BY <3>0, <3>1, <1>old
    <3>2. CASE w[s].local[p] # w[s].baseline[p]
      \* nothing changed: the document holds the upload's (declared) bytes -- DocCar
      <4>1. doc[p] = w[s].snap[p] BY <3>2, <2>u, <2>1, <1>ii DEF InstallNothing
      <4>. QED BY <4>1, <3>0, <3>2, <2>u, <1>old
    <3>. QED BY <3>1, <3>2
  <2>2. CASE ~InstallNothing(s)
    <3>0. k = InstallCar(s) BY <2>2, <2>0, <1>K, <1>fc
    <3>1. CASE w[s].local[p] = w[s].baseline[p]
      \* the old carrier form, kept: k is built on it and leaves p alone
      <4>1. w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car) BY <3>1, <1>old
      <4>2. InstallCar0(s) = w[s].car BY <4>1, <1>z, <1>ic
      <4>3. p \notin w[s].uploads /\ p \notin w[s].deletes BY <3>1, <2>k
      <4>4. /\ p \in k.paths /\ (p \in k.deletes <=> p \in w[s].car.deletes)
            /\ w[s].car.dropped \subseteq k.dropped /\ k.pub[p] = w[s].car.pub[p]
        BY <4>1, <4>2, <4>3, <3>0, <1>ic DEF CarForm
      <4>. QED BY <4>1, <4>4, <3>0, <1>ic, <1>z DEF CarForm
    <3>2. CASE w[s].local[p] # w[s].baseline[p]
      \* an upload not withheld: k publishes its snap, the declared bytes
      <4>1. /\ p \in k.paths /\ p \notin k.deletes /\ k.pub[p] = w[s].snap[p]
        BY <3>2, <2>u, <3>0, <1>ic
      <4>. QED BY <4>1, <3>2, <2>u, <3>0, <1>ic, <1>z DEF CarForm
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2
\* <1>cg: a withheld declared file in carrier form stays so (InstallCar drops it)
<1>cg. \A p \in Paths : (A1Decl(s, p) /\ p \in w[s].gone /\ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)) =>
                         k.id = w[s].pend.id /\ CarForm(s, p, k)
  <2>. SUFFICES ASSUME NEW p \in Paths, A1Decl(s, p), p \in w[s].gone,
                       w[s].car.id = w[s].pend.id, CarForm(s, p, w[s].car)
                PROVE  k.id = w[s].pend.id /\ CarForm(s, p, k) OBVIOUS
  <2>1. CASE Carries(s) /\ ~InstallNothing(s)
    <3>1. k = InstallCar(s) /\ InstallCar0(s) = w[s].car BY <2>1, <1>K, <1>fc, <1>ic, <1>z
    <3>2. p \in k.paths /\ p \in k.dropped BY <3>1, <1>ic DEF CarForm
    <3>. QED BY <3>1, <3>2, <1>ic, <1>z DEF CarForm
  <2>2. CASE ~(Carries(s) /\ ~InstallNothing(s)) BY <2>2, <1>K, <1>fc
  <2>. QED BY <2>1, <2>2
\* Every other tree: its own fields are unchanged; the document changed only to s's
\* uploads (not another tree's private bytes: A1DeclPriv) and lost grew by s's
\* withheld uploads, which no conflict names (A1GoneUnconf): A1FrameD.
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>w. w'[u] = w[u] BY <1>w
  \* where the install changes the document to a handle, it is one of s's uploads
  <2>ch. \A q \in Paths : InstallInst(s)[q] # doc[q] /\ InstallInst(s)[q] # Nil =>
                          q \in w[s].uploads /\ InstallInst(s)[q] = w[s].snap[q]
    BY DEF InstallInst, InstallMine
  <2>sn. \A q \in w[s].uploads : w[s].snap[q] \in minted
    BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
  \* each document path: kept, cleared, or s's upload -- not u's private bytes
  <2>dd. \A q \in Paths : A1DD(u, q)
    <3>. SUFFICES ASSUME NEW q \in Paths PROVE A1DD(u, q) OBVIOUS
    <3>1. CASE doc'[q] = doc[q] \/ doc'[q] = Nil BY <3>1 DEF A1DD
    <3>2. CASE doc'[q] # doc[q] /\ doc'[q] # Nil
      <4>1. q \in w[s].uploads /\ doc'[q] = w[s].snap[q] BY <3>2, <2>ch, <1>1
      <4>2. \/ ~A1Decl(u, q) \/ w[u].decl[q] = Nil
            \/ w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)
            \/ Content(w[s].snap[q]) # Content(w[u].decl[q])
        BY <4>1 DEF A1Side, A1DeclPriv, A1Priv
      <4>. QED BY <4>1, <4>2, <2>sn DEF A1DD
    <3>. QED BY <3>1, <3>2
  <2>lo. \A c \in conflicts : c[2] \in UNION {w'[t].lost : t \in Writers} => c[2] \in UNION {w[t].lost : t \in Writers}
    BY <1>w, <1>fl, <1>gu
  <2>. QED BY <2>w, <2>dd, <2>lo, <1>1, <1>hs, A1FrameD DEF A1IAK1
<1>s. A1Per(s)'
  <2>w. w'[s] = InstallW(s) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>fx, <1>per, A1FoldsOf
  <2>2. w'[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w'[s].cId = w'[s].pend.id /\ w'[s].cFold = w'[s].pend.fold
    BY <2>w, <1>fx, <1>z
  <2>3. \A p \in Paths : A1TreePath(s, p)' /\
          (/\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p]) =>
                \/ p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p]
                \/ p \in w[s].deletes /\ w[s].local[p] = Nil
           /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p)) =>
                /\ p \in w[s].uploads => w[s].snap[p] = w[s].local[p]
                /\ p \in w[s].deletes => w[s].local[p] = Nil
           /\ (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, p) /\ w[s].decl[p] # Nil) =>
                \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
                \/ p \in w[s].gone /\ p \notin w[s].upDone)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' /\
          (/\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p]) =>
                \/ p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p]
                \/ p \in w[s].deletes /\ w[s].local[p] = Nil
           /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p)) =>
                /\ p \in w[s].uploads => w[s].snap[p] = w[s].local[p]
                /\ p \in w[s].deletes => w[s].local[p] = Nil
           /\ (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, p) /\ w[s].decl[p] # Nil) =>
                \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
                \/ p \in w[s].gone /\ p \notin w[s].upDone)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>fx DEF A1Decl
    <3>e. /\ w'[s].car = k /\ w'[s].pend = w[s].pend /\ w'[s].decl = w[s].decl
          /\ w'[s].local = w[s].local /\ w'[s].baseline = w[s].baseline /\ w'[s].gone = w[s].gone
          /\ w'[s].uploads = w[s].uploads /\ w'[s].deletes = w[s].deletes /\ w'[s].snap = w[s].snap
          /\ w'[s].cId = w[s].cId /\ w'[s].pc = "cased" /\ w'[s].inst = InstallInst(s)
      BY <2>w, <1>fx, <1>K
    <3>kc. CarForm(s, p, k)' <=> CarForm(s, p, k) BY <1>kc
    \* MidBar: the fields it reads are kept
    <3>mb. (/\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p]) =>
                \/ p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p]
                \/ p \in w[s].deletes /\ w[s].local[p] = Nil
            /\ (w[s].pc \in {"scanned", "claimed", "cased"} /\ w[s].cId # 0 /\ A1Decl(s, p)) =>
                /\ p \in w[s].uploads => w[s].snap[p] = w[s].local[p]
                /\ p \in w[s].deletes => w[s].local[p] = Nil)'
      BY <3>d, <3>e, <1>old
    \* Cased: <1>cf, or withheld
    <3>cs. (w[s].pc = "cased" /\ w[s].cId # 0 /\ A1Decl(s, p) /\ w[s].decl[p] # Nil =>
               \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
               \/ p \in w[s].gone /\ p \notin w[s].upDone)'
      <4>1. w'[s].upDone = w[s].upDone \ w[s].gone BY <2>w DEF InstallW
      <4>. QED BY <4>1, <3>d, <3>e, <3>kc, <1>cf
    \* DTree: the tree and the declaration are kept; no leftover in a barrier
    <3>a. (A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin A1LeftSet(w[s]))' =>
            (Content(w[s].local[p]) = Content(w[s].decl[p]))'
      BY <3>d, <3>e, <1>old, <1>c, <1>ls, <2>w
    \* CPub3: a clean declared file is in carrier form (old CPub3); after: <1>cf, or <1>cg
    <3>b. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p])' =>
            (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
      BY <3>d, <3>e, <3>kc, <1>old, <1>cf, <1>cg
    \* DocCar: doc' = inst; withheld: inst = doc (old DocCar, then <1>cg); else <1>cf
    <3>c. (A1Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
            (w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car))'
      <4>. SUFFICES ASSUME A1Decl(s, p), w[s].decl[p] # Nil
                    PROVE  (doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))' =>
                             k.id = w[s].pend.id /\ CarForm(s, p, k)
        BY <3>d, <3>e, <3>kc
      <4>1. CASE p \notin w[s].gone BY <4>1, <1>cf
      <4>2. CASE p \in w[s].gone
        <5>. SUFFICES ASSUME (doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]))'
                      PROVE  k.id = w[s].pend.id /\ CarForm(s, p, k) OBVIOUS
        <5>1. doc'[p] = doc[p] BY <4>2, <1>1, <1>ii
        <5>2. doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]) BY <5>1, <1>hc, <1>c, <3>e
        <5>. QED BY <5>2, <4>2, <1>old, <1>cg
      <4>. QED BY <4>1, <4>2
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      <4>1. w'[s].inst[p] \in {doc[p], w[s].snap[p], Nil} BY <3>e, <1>ii
      <4>2. k.pub[p] \in {w[s].snap[p], Nil, InstallCar0(s).pub[p], w[s].car.pub[p]} BY <1>K, <1>fc, <1>ic
      <4>3. InstallCar0(s).pub[p] \in {w[s].car.pub[p], Nil} BY DEF InstallCar0
      <4>. QED BY <4>1, <4>2, <4>3, <3>e, <1>old, <1>dm, <1>sm, <1>m DEF Opt
    <3>. QED BY <3>a, <3>b, <3>c, <3>t, <3>mb, <3>cs, <1>nl2 DEF A1TreePath
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>fx, <1>per, <1>m DEF A1Per, Opt
  \* AK1: the bound is kept; s's dropped set only grows; lost grew by no handle a
  \* conflict names (A1GoneUnconf), so a declared file broken now was broken before.
  <2>5. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>. SUFFICES ASSUME Honorable(s)', NEW p \in Paths, Broken(s, p, w[s].bound.doc)'
                  PROVE  p \in HDropped(s)' OBVIOUS
    <3>0. PICK d \in [Paths -> Opt(minted)] : d = w[s].bound.doc BY <1>per DEF A1Per
    <3>1. Broken(s, p, d)' BY <3>0, <2>w, <1>fx
    <3>lo. \A c \in conflicts : c[2] \in UNION {w'[t].lost : t \in Writers} => c[2] \in UNION {w[t].lost : t \in Writers}
      BY <1>w, <1>fl, <1>gu
    <3>2. Broken(s, p, d)
      BY <3>0, <3>1, <3>lo, <1>1, <1>hs, <2>w, <1>fx, <1>per, A1BrokenKeepL DEF A1IAK1, A1Per
    <3>3. Honorable(s) BY <2>w, <1>fx DEF Honorable
    <3>4. p \in HDropped(s) BY <3>0, <3>2, <3>3, <1>per DEF A1Per
    \* HDropped only grows: wd kept; the carrier's part kept or built on the old one
    <3>5. HDropped(s) \subseteq HDropped(s)'
      <4>1. CASE Carries(s) /\ ~InstallNothing(s) /\ w[s].car.id = w[s].pend.id
        <5>1. k = InstallCar(s) /\ InstallCar0(s) = w[s].car BY <4>1, <1>K, <1>fc, <1>ic, <1>z
        <5>. QED BY <5>1, <1>ic, <2>w, <1>fx, <1>K, <1>z DEF HDropped
      <4>2. CASE ~(Carries(s) /\ ~InstallNothing(s)) BY <4>2, <1>K, <1>fc, <2>w, <1>fx DEF HDropped
      <4>3. CASE w[s].car.id # w[s].pend.id BY <4>3, <2>w, <1>fx DEF HDropped
      <4>. QED BY <4>1, <4>2, <4>3
    <3>. QED BY <3>4, <3>5
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <1>nl2 DEF A1Per, A1TreePath
\* the new conflicts: s's withheld snaps, document versions, and what a delete
\* override deleted (a tombstone, or s's baseline) -- all minted, and minted is kept
<1>cm. A1ConflictsMinted'
  <2>1. conflicts' = conflicts \cup {<<p, w[s].snap[p]>> : p \in w[s].gone}
                               \cup {<<p, doc[p]>> : p \in InstallContested(s)}
                               \cup {<<p, DeletedAt(s, p)>> : p \in InstallDelOverridden(s)}
                               \cup {<<p, doc[p]>> : p \in InstallDelOver(s)}
    BY DEF Install
  <2>gs. \A q \in w[s].gone : w[s].snap[q] \in minted
    BY <1>g DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Mine, IndTypeOK, Minted
  <2>tb. \A q \in Paths : tomb[q] \in Opt(minted)
    BY DEF A1Side, A1TombUpped, IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh, Opt
  <2>bl. \A q \in Paths : w[s].baseline[q] \in Opt(minted) BY TreeTy DEF IndC
  <2>da. \A q \in Paths : DeletedAt(s, q) \in Opt(minted) BY <2>tb, <2>bl DEF DeletedAt
  <2>. SUFFICES ASSUME NEW c \in conflicts' PROVE c[2] \in Opt(minted') BY DEF A1ConflictsMinted
  <2>2. CASE c \in conflicts BY <2>2, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
  <2>3. CASE c \in {<<p, w[s].snap[p]>> : p \in w[s].gone} BY <2>3, <2>gs, <1>m DEF Opt
  <2>ps. InstallContested(s) \cup InstallDelOver(s) \cup InstallDelOverridden(s) \subseteq Paths
    <3>1. w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths BY TreeTy DEF IndC
    <3>. QED BY <3>1 DEF InstallContested, InstallDelOver, InstallDelOverridden, InstallMine
  <2>4. CASE c \in {<<p, doc[p]>> : p \in InstallContested(s)} \cup {<<p, doc[p]>> : p \in InstallDelOver(s)}
    <3>1. PICK p \in InstallContested(s) \cup InstallDelOver(s) : c = <<p, doc[p]>> BY <2>4
    <3>2. p \in Paths BY <3>1, <2>ps
    <3>. QED BY <3>1, <3>2, <1>dm, <1>m DEF Opt
  <2>5. CASE c \in {<<p, DeletedAt(s, p)>> : p \in InstallDelOverridden(s)}
    <3>1. PICK p \in InstallDelOverridden(s) : c = <<p, DeletedAt(s, p)>> BY <2>5
    <3>2. p \in Paths BY <3>1, <2>ps
    <3>. QED BY <3>1, <3>2, <2>da, <1>m DEF Opt
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP
LEMMA Skip_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1LostDead, NEW s \in Writers, Skip(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY DEF Skip, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
<1>w. \A u \in Writers : w'[u] = IF u = s THEN SkipW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>g. w[s].pc = "consumed" /\ \A q \in Paths : w[s].local[q] = w[s].baseline[q] BY DEF Skip
<1>per. A1Per(s) BY A1PerAll
<1>W. w[s] \in Writer BY <1>0 DEF TypeOK
<1>n. w[s].nt \in Nat /\ w[s].inst \in [Paths -> Opt(Handles)] BY <1>W, WriterFields
<1>z. w[s].cId = w[s].pend.id /\ w[s].cFold = w[s].pend.fold BY <1>g, <1>per, A1FrozenOf
<1>dm. \A q \in Paths : doc[q] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>dt. doc \in [Paths -> Opt(minted)] BY <1>0, <1>dm DEF TypeOK
\* one goal per few fields (a whole-record goal times out)
<1>fx. /\ SkipW(s).pc = "idle" /\ SkipW(s).local = w[s].local /\ SkipW(s).baseline = w[s].baseline
       /\ SkipW(s).pend = w[s].pend /\ SkipW(s).dset = w[s].dset /\ SkipW(s).decl = w[s].decl
       /\ SkipW(s).moved = w[s].moved /\ SkipW(s).dMint = w[s].dMint
       /\ SkipW(s).nt = w[s].nt /\ SkipW(s).lost = w[s].lost /\ SkipW(s).inst = w[s].inst
       /\ SkipW(s).cId = 0 /\ SkipW(s).cFold = 0
  <2>1. SkipW(s).pc = "idle" /\ SkipW(s).local = w[s].local /\ SkipW(s).baseline = w[s].baseline BY DEF SkipW
  <2>2. SkipW(s).pend = w[s].pend /\ SkipW(s).dset = w[s].dset /\ SkipW(s).decl = w[s].decl BY DEF SkipW
  <2>3. SkipW(s).moved = w[s].moved /\ SkipW(s).dMint = w[s].dMint BY DEF SkipW
  <2>4. SkipW(s).nt = w[s].nt /\ SkipW(s).lost = w[s].lost /\ SkipW(s).inst = w[s].inst BY DEF SkipW
  <2>5. SkipW(s).cId = 0 /\ SkipW(s).cFold = 0 BY DEF SkipW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5
<1>ls. A1LeftSet(SkipW(s)) = A1LeftSet(w[s]) BY DEF SkipW, A1LeftSet
<1>fc. SkipW(s).car = IF w[s].car.id # 0 THEN CarUncited(w[s].car, doc) ELSE w[s].car
  BY DEF SkipW
<1>fd. /\ w[s].cId # 0 => /\ SkipW(s).bound.id = w[s].cId /\ SkipW(s).bound.fold = w[s].cFold
                        /\ SkipW(s).bound.doc = doc /\ SkipW(s).bound.wd = {}
       /\ w[s].cId = 0 => SkipW(s).bound = w[s].bound
  <2>1. w[s].cId # 0 => SkipW(s).bound.id = w[s].cId /\ SkipW(s).bound.fold = w[s].cFold BY DEF SkipW
  <2>2. w[s].cId # 0 => SkipW(s).bound.doc = doc BY DEF SkipW
  <2>3. w[s].cId # 0 => SkipW(s).bound.wd = {} BY DEF SkipW
  <2>4. w[s].cId = 0 => SkipW(s).bound = w[s].bound BY DEF SkipW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
\* The carrier keeps every field CarForm reads; its id is kept.
<1>cr. /\ SkipW(s).car.id = w[s].car.id /\ SkipW(s).car.paths = w[s].car.paths
       /\ SkipW(s).car.deletes = w[s].car.deletes /\ SkipW(s).car.dropped = w[s].car.dropped
       /\ SkipW(s).car.pub = w[s].car.pub
  BY <1>fc DEF CarUncited
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
<1>s. A1Per(s)'
  <2>w. w'[s] = SkipW(s) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    <3>1. w'[s].bound.fold = w[s].cFold \/ w'[s].bound.fold = w[s].bound.fold BY <2>w, <1>fd
    <3>. QED BY <3>1, <2>w, <1>fx, <1>n, <1>per, A1FoldsOf
  <2>2. w'[s].pc = "idle" BY <2>w, <1>fx
  \* the tree, the declaration and the record are kept; the carrier keeps CarForm's fields
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>fx DEF A1Decl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>fx, <1>cr, <1>per, <1>m, <1>dm DEF A1Per, Opt
    \* the carriers as constants (CarForm's prime would double-prime w'[s].car)
    <3>0. PICK c0, c1 : c0 = w[s].car /\ c1 = SkipW(s).car OBVIOUS
    <3>1. CarForm(s, p, c1)' <=> CarForm(s, p, c0)'
      BY <3>0, <1>cr DEF CarForm
    <3>2. CarForm(s, p, c0)' <=> CarForm(s, p, c0)
      BY <2>w, <1>fx, <1>cr, <1>c, <3>0 DEF CarForm
    <3>. QED BY <3>d, <3>t, <3>0, <3>1, <3>2, <2>w, <1>fx, <1>ls, <1>cr, <1>c, <1>per, <1>dk DEF A1TreePath, A1Per
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')]
    <3>1. CASE w[s].cId # 0 BY <3>1, <2>w, <1>fd, <1>dt, <1>m DEF Opt
    <3>2. CASE w[s].cId = 0 BY <3>2, <2>w, <1>fd, <1>per, <1>m DEF A1Per, Opt
    <3>. QED BY <3>1, <3>2
  \* AK1: the bound binds doc; a broken declared file was published (CPub3) and the
  \* document no longer holds it, so the uncite names it.
  <2>5. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>. SUFFICES ASSUME Honorable(s)', NEW p \in Paths, Broken(s, p, w[s].bound.doc)'
                  PROVE  p \in HDropped(s)' OBVIOUS
    <3>0. w[s].pend.id # 0 BY <2>w, <1>fx DEF Honorable
    <3>1. w[s].cId # 0 BY <3>0, <1>z
    <3>2. w'[s].bound.doc = doc /\ w'[s].bound.wd = {} BY <3>1, <2>w, <1>fd
    <3>3. PICK d \in [Paths -> Opt(minted)] : d = doc BY <1>dt
    <3>4. Broken(s, p, d)' BY <3>2, <3>3, <1>1
    <3>5. Broken(s, p, d)
      BY <3>3, <3>4, <1>1, <1>hs, <1>lost, <2>w, <1>fx, <1>per, A1BrokenKeep DEF A1IAK1, A1Per
    <3>6. A1Decl(s, p) BY <3>0, <3>5 DEF A1Decl, Broken
    <3>7. w[s].decl[p] # Nil
      \* a declared delete is never broken: present is a later version, absent carries it
      <4>. SUFFICES ASSUME w[s].decl[p] = Nil PROVE FALSE OBVIOUS
      <4>1. d[p] = Nil BY <3>5 DEF Broken, LaterVersion
      <4>. QED BY <4>1, <3>5 DEF Broken, Content
    <3>8. HDropped(s)' = {} \cup (IF w[s].car.id = w[s].pend.id THEN w[s].car.dropped \cup Uncited(w[s].car, doc) ELSE {})
      <4>1. w[s].car.id = w[s].pend.id => w'[s].car = CarUncited(w[s].car, doc) BY <3>0, <2>w, <1>fc
      <4>. QED BY <4>1, <3>2, <2>w, <1>fx, <1>cr DEF HDropped, CarUncited
    \* CPub3: every path is clean
    <3>9. w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car) BY <3>6, <3>7, <1>g, <1>per DEF A1Per
    <3>10. p \in w[s].car.dropped \cup Uncited(w[s].car, d) BY <3>9, <3>5, UncitedCatches
    <3>. QED BY <3>10, <3>3, <3>8, <3>9
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <1>dk, <1>ls DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP
LEMMA PullOnly_A1 ==
  ASSUME IndC, IndC', DInv, A1IAK1, A1LostDead, NEW s \in Writers, PullOnly(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig, doc>>
  BY DEF PullOnly, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
\* the document is unchanged, its content kept (DocCar)
<1>dk. \A q \in Paths : doc'[q] = doc[q] /\ Content(doc[q])' = Content(doc[q]) BY <1>1, <1>hs, A1DocContent
<1>w. \A u \in Writers : w'[u] = IF u = s THEN PullOnlyW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>g. w[s].pc = "scanned" /\ w[s].uploads = {} /\ w[s].deletes = {} BY DEF PullOnly
<1>per. A1Per(s) BY A1PerAll
<1>W. w[s] \in Writer BY <1>0 DEF TypeOK
<1>n. w[s].nt \in Nat /\ w[s].inst \in [Paths -> Opt(Handles)] BY <1>W, WriterFields
<1>z. w[s].cId = w[s].pend.id /\ w[s].cFold = w[s].pend.fold BY <1>g, <1>per, A1FrozenOf
<1>dm. \A q \in Paths : doc[q] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>dt. doc \in [Paths -> Opt(minted)] BY <1>0, <1>dm DEF TypeOK
\* one goal per few fields (a whole-record goal times out)
<1>fx. /\ PullOnlyW(s).pc = "idle" /\ PullOnlyW(s).local = w[s].local /\ PullOnlyW(s).baseline = w[s].baseline
       /\ PullOnlyW(s).pend = w[s].pend /\ PullOnlyW(s).dset = w[s].dset /\ PullOnlyW(s).decl = w[s].decl
       /\ PullOnlyW(s).moved = w[s].moved /\ PullOnlyW(s).dMint = w[s].dMint
       /\ PullOnlyW(s).nt = w[s].nt /\ PullOnlyW(s).lost = w[s].lost /\ PullOnlyW(s).inst = doc
       /\ PullOnlyW(s).cId = 0 /\ PullOnlyW(s).cFold = 0
  <2>1. PullOnlyW(s).pc = "idle" /\ PullOnlyW(s).local = w[s].local /\ PullOnlyW(s).baseline = w[s].baseline BY DEF PullOnlyW
  <2>2. PullOnlyW(s).pend = w[s].pend /\ PullOnlyW(s).dset = w[s].dset /\ PullOnlyW(s).decl = w[s].decl BY DEF PullOnlyW
  <2>3. PullOnlyW(s).moved = w[s].moved /\ PullOnlyW(s).dMint = w[s].dMint BY DEF PullOnlyW
  <2>4. PullOnlyW(s).nt = w[s].nt /\ PullOnlyW(s).lost = w[s].lost /\ PullOnlyW(s).inst = doc BY DEF PullOnlyW
  <2>5. PullOnlyW(s).cId = 0 /\ PullOnlyW(s).cFold = 0 BY DEF PullOnlyW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5
<1>ls. A1LeftSet(PullOnlyW(s)) = A1LeftSet(w[s]) BY DEF PullOnlyW, A1LeftSet
<1>fc. PullOnlyW(s).car = IF w[s].car.id # 0 THEN CarUncited(w[s].car, doc) ELSE w[s].car
  BY DEF PullOnlyW
<1>fd. /\ w[s].cId # 0 => /\ PullOnlyW(s).bound.id = w[s].cId /\ PullOnlyW(s).bound.fold = w[s].cFold
                        /\ PullOnlyW(s).bound.doc = doc /\ PullOnlyW(s).bound.wd = {}
       /\ w[s].cId = 0 => PullOnlyW(s).bound = w[s].bound
  <2>1. w[s].cId # 0 => PullOnlyW(s).bound.id = w[s].cId /\ PullOnlyW(s).bound.fold = w[s].cFold BY DEF PullOnlyW
  <2>2. w[s].cId # 0 => PullOnlyW(s).bound.doc = doc BY DEF PullOnlyW
  <2>3. w[s].cId # 0 => PullOnlyW(s).bound.wd = {} BY DEF PullOnlyW
  <2>4. w[s].cId = 0 => PullOnlyW(s).bound = w[s].bound BY DEF PullOnlyW
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
\* The carrier keeps every field CarForm reads; its id is kept.
<1>cr. /\ PullOnlyW(s).car.id = w[s].car.id /\ PullOnlyW(s).car.paths = w[s].car.paths
       /\ PullOnlyW(s).car.deletes = w[s].car.deletes /\ PullOnlyW(s).car.dropped = w[s].car.dropped
       /\ PullOnlyW(s).car.pub = w[s].car.pub
  BY <1>fc DEF CarUncited
<1>c. \A q \in Paths :
        /\ (w'[s].local[q] = w[s].local[q] => Content(w[s].local[q])' = Content(w[s].local[q]))
        /\ (w'[s].baseline[q] = w[s].baseline[q] => Content(w[s].baseline[q])' = Content(w[s].baseline[q]))
        /\ (w'[s].decl[q] = w[s].decl[q] => Content(w[s].decl[q])' = Content(w[s].decl[q]))
        /\ (w'[s].inst[q] = w[s].inst[q] => Content(w[s].inst[q])' = Content(w[s].inst[q]))
        /\ (w'[s].car.pub[q] = w[s].car.pub[q] => Content(w[s].car.pub[q])' = Content(w[s].car.pub[q]))
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lost. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>. QED BY <1>w, <1>1, <1>hs, <1>lost, A1Frame DEF A1IAK1
<1>s. A1Per(s)'
  <2>w. w'[s] = PullOnlyW(s) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    <3>1. w'[s].bound.fold = w[s].cFold \/ w'[s].bound.fold = w[s].bound.fold BY <2>w, <1>fd
    <3>. QED BY <3>1, <2>w, <1>fx, <1>n, <1>per, A1FoldsOf
  <2>2. w'[s].pc = "idle" BY <2>w, <1>fx
  \* the tree, the declaration and the record are kept; the carrier keeps CarForm's fields
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>d. A1Decl(s, p)' <=> A1Decl(s, p) BY <2>w, <1>fx DEF A1Decl
    <3>t. w'[s].decl[p] \in Opt(minted') /\ w'[s].inst[p] \in Opt(minted') /\ w'[s].car.pub[p] \in Opt(minted')
      BY <2>w, <1>fx, <1>cr, <1>per, <1>m, <1>dm DEF A1Per, Opt
    \* the carriers as constants (CarForm's prime would double-prime w'[s].car)
    <3>0. PICK c0, c1 : c0 = w[s].car /\ c1 = PullOnlyW(s).car OBVIOUS
    <3>1. CarForm(s, p, c1)' <=> CarForm(s, p, c0)'
      BY <3>0, <1>cr DEF CarForm
    <3>2. CarForm(s, p, c0)' <=> CarForm(s, p, c0)
      BY <2>w, <1>fx, <1>cr, <1>c, <3>0 DEF CarForm
    <3>. QED BY <3>d, <3>t, <3>0, <3>1, <3>2, <2>w, <1>fx, <1>ls, <1>cr, <1>c, <1>per, <1>dk DEF A1TreePath, A1Per
  <2>4. w'[s].bound.doc \in [Paths -> Opt(minted')]
    <3>1. CASE w[s].cId # 0 BY <3>1, <2>w, <1>fd, <1>dt, <1>m DEF Opt
    <3>2. CASE w[s].cId = 0 BY <3>2, <2>w, <1>fd, <1>per, <1>m DEF A1Per, Opt
    <3>. QED BY <3>1, <3>2
  \* AK1: the bound binds doc; a broken declared file was published (CPub3) and the
  \* document no longer holds it, so the uncite names it.
  <2>5. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>. SUFFICES ASSUME Honorable(s)', NEW p \in Paths, Broken(s, p, w[s].bound.doc)'
                  PROVE  p \in HDropped(s)' OBVIOUS
    <3>0. w[s].pend.id # 0 BY <2>w, <1>fx DEF Honorable
    <3>1. w[s].cId # 0 BY <3>0, <1>z
    <3>2. w'[s].bound.doc = doc /\ w'[s].bound.wd = {} BY <3>1, <2>w, <1>fd
    <3>3. PICK d \in [Paths -> Opt(minted)] : d = doc BY <1>dt
    <3>4. Broken(s, p, d)' BY <3>2, <3>3, <1>1
    <3>5. Broken(s, p, d)
      BY <3>3, <3>4, <1>1, <1>hs, <1>lost, <2>w, <1>fx, <1>per, A1BrokenKeep DEF A1IAK1, A1Per
    <3>6. A1Decl(s, p) BY <3>0, <3>5 DEF A1Decl, Broken
    <3>7. w[s].decl[p] # Nil
      \* a declared delete is never broken: present is a later version, absent carries it
      <4>. SUFFICES ASSUME w[s].decl[p] = Nil PROVE FALSE OBVIOUS
      <4>1. d[p] = Nil BY <3>5 DEF Broken, LaterVersion
      <4>. QED BY <4>1, <3>5 DEF Broken, Content
    <3>8. HDropped(s)' = {} \cup (IF w[s].car.id = w[s].pend.id THEN w[s].car.dropped \cup Uncited(w[s].car, doc) ELSE {})
      <4>1. w[s].car.id = w[s].pend.id => w'[s].car = CarUncited(w[s].car, doc) BY <3>0, <2>w, <1>fc
      <4>. QED BY <4>1, <3>2, <2>w, <1>fx, <1>cr DEF HDropped, CarUncited
    \* MidBar: no uploads and no deletes, so a declared path is clean
    <3>9a. w[s].local[p] = w[s].baseline[p] BY <3>1, <3>6, <1>g, <1>per DEF A1Per
    <3>9. w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car) BY <3>6, <3>7, <3>9a, <1>per DEF A1Per
    <3>10. p \in w[s].car.dropped \cup Uncited(w[s].car, d) BY <3>9, <3>5, UncitedCatches
    <3>. QED BY <3>10, <3>3, <3>8, <3>9
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <1>dk, <1>ls DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>m DEF A1IAK1, A1ConflictsMinted, Opt
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

------------------------------------------------------------------------------
(* The reap in Next's second disjunct: no Frame there (UNCHANGED anc), so   *)
(* the history step comes from AncUpdateSame (Reap keeps base).  Reap_A1   *)
(* above assumes Frame, which Next does not give for a reap.               *)

LEMMA ReapQ_A1 ==
  ASSUME IndC, IndC', A1IAK1, A1LostDead, NEW s \in Writers, NEW h \in Handles, Reap(s, h),
         UNCHANGED anc
  PROVE  A1IAK1'
<1>1. /\ w' = w /\ live' = live \ {h} /\ h \in live
      /\ UNCHANGED <<minted, base, conflicts, orig, doc>>
  BY DEF Reap, aux
<1>u. AncUpdate BY <1>1, AncUpdateSame DEF IndC
<1>hs. HStep BY <1>1, <1>u, HStepQ
<1>2. \A c \in conflicts : c[2] \in live => c[2] \in live' \/ c[2] \notin UNION {w[t].lost : t \in Writers}
  BY <1>1 DEF A1LostDead
<1>. QED BY <1>1, <1>hs, <1>2, A1NoTree

------------------------------------------------------------------------------
(* Init, the step, and the theorems.                                        *)

LEMMA Init_A1 == Init => A1IAK1
<1>. SUFFICES ASSUME Init PROVE A1IAK1 OBVIOUS
<1>0. \A u \in Writers : w[u] = WriterInit BY DEF Init
\* one conjunct each (M5D Init_D: a one-step QED expanding WriterInit timed out)
<1>1. \A u \in Writers : /\ w[u].pc = "idle" /\ w[u].pend = NoPend /\ w[u].nt = 0 /\ w[u].cFold = 0
                         /\ w[u].bound.fold = 0 /\ w[u].bound.doc = [p \in Paths |-> Nil]
  BY <1>0 DEF WriterInit
<1>2. \A u \in Writers : /\ w[u].decl = [p \in Paths |-> Nil] /\ w[u].inst = [p \in Paths |-> Nil]
                         /\ w[u].car = NoCar
  BY <1>0 DEF WriterInit
<1>3. conflicts = {} BY DEF Init
<1>4. A1Folds BY <1>1 DEF A1Folds, NoPend
<1>5. A1Frozen /\ A1MidBar /\ A1Cased BY <1>1 DEF A1Frozen, A1MidBar, A1Cased
<1>6. A1DTree /\ A1CPub3 /\ A1CPubL /\ A1DocCar BY <1>1 DEF A1DTree, A1CPub3, A1CPubL, A1DocCar, A1Decl, NoPend
<1>7. A1DeclMinted /\ A1BoundMinted /\ A1InstMinted /\ A1CarPubMinted
  BY <1>1, <1>2 DEF A1DeclMinted, A1BoundMinted, A1InstMinted, A1CarPubMinted, NoCar, Opt
<1>8. A1ConflictsMinted BY <1>3 DEF A1ConflictsMinted
<1>9. BoundOK BY <1>1 DEF BoundOK, Honorable, NoPend
<1>. QED BY <1>4, <1>5, <1>6, <1>7, <1>8, <1>9 DEF A1IAK1

LEMMA Next_A1 == IndC /\ IndC' /\ DInv /\ A1IAK1 /\ A1Side /\ Next => A1IAK1'
<1>. SUFFICES ASSUME IndC, IndC', DInv, A1IAK1, A1Side, A1LostDead, A1KeepCited, A1HeldSame, Next PROVE A1IAK1' BY DEF A1Side
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE A1IAK1' BY <3>1, GPut_A1
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE A1IAK1' BY <3>2, GCas_A1
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE A1IAK1' BY <3>3, GDelete_A1
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE A1IAK1' BY <3>4, GRename_A1
    <3>5. ASSUME GRenameFinish PROVE A1IAK1' BY <3>5, GRenameFinish_A1
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE A1IAK1' BY <3>1, Checkout_A1
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE A1IAK1' BY <3>2, Consume_A1
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE A1IAK1' BY <3>3, Scan_A1
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE A1IAK1' BY <3>4, Skip_A1
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE A1IAK1' BY <3>5, PullOnly_A1
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE A1IAK1' BY <3>6, Claim_A1
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE A1IAK1' BY <3>7, Verify_A1
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE A1IAK1' BY <3>8, Install_A1
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE A1IAK1' BY <3>9, Collect_A1
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE A1IAK1' BY <3>10, Finish_A1
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE A1IAK1' BY <3>11, Restart_A1
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE A1IAK1' BY <3>12, Sync_A1
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE A1IAK1' BY <3>13, RescopeBegin_A1
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE A1IAK1' BY <3>14, RescopeFirst_A1
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE A1IAK1' BY <3>15, RescopeSecond_A1
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE A1IAK1' BY <3>16, RPullRead_A1
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE A1IAK1' BY <3>17, RPullSync_A1
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE A1IAK1' BY <3>18, Edit_A1
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE A1IAK1' BY <3>19, Delete_A1
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE A1IAK1'
      <4>1. CASE w[s].snap[p] \notin upped BY <3>20, <4>1, UploadFirst_A1
      <4>2. CASE w[s].snap[p] \in upped BY <3>20, <4>2, UploadCopy_A1
      <4>. QED BY <4>1, <4>2
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE A1IAK1' BY <3>21, Sweep_A1
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE A1IAK1' BY <3>22, Touch_A1
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE A1IAK1' BY <3>23, Honor_A1
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE A1IAK1' BY <3>24, HonorReader_A1
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE A1IAK1' BY <3>25, RetireByAck_A1
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE A1IAK1' BY <3>26, Depose_A1
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE A1IAK1' BY <3>27, Fence_A1
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>1. CASE Age \/ RLoad
    <3>1. UNCHANGED <<anc, minted, base, orig, conflicts, w, live, doc>> BY <1>2, <2>1 DEF Age, RLoad, aux
    <3>. QED BY <3>1, A1Quiet
  <2>2. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE A1IAK1'
    BY <1>2, <2>2, ReapQ_A1
  <2>. QED BY <1>2, <2>1, <2>2
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA A1Step == IndC /\ IndC' /\ DInv /\ A1IAK1 /\ A1Side /\ [Next]_vars => A1IAK1'
<1>. SUFFICES ASSUME IndC, IndC', DInv, A1IAK1, A1Side, [Next]_vars PROVE A1IAK1' OBVIOUS
<1>1. CASE Next BY <1>1, Next_A1
<1>2. CASE UNCHANGED vars
  <2>1. UNCHANGED <<anc, minted, base, orig, conflicts, w, live, doc>> BY <1>2 DEF vars, bucket, aux
  <2>. QED BY <2>1, A1Quiet
<1>. QED BY <1>1, <1>2

\* A1Side's invariance (A1LostDead, A1UpsDone, A1DelBase, A1GoneUnconf, A1TombUpped) is
\* M12's (it EXTENDS this one); the unconditional THEOREM Spec => AK1 then lives
\* there: BY CInvariant, DInvariant, SideInvariant, A1Inductive... PTL.
THEOREM A1Inductive == Spec /\ []A1Side => []A1IAK1
BY Init_A1, A1Step, CInvariant, DInvariant, PTL DEF Spec

LEMMA A1BoundOK == A1IAK1 => BoundOK BY DEF A1IAK1

THEOREM AK1FromSide == Spec /\ []A1Side => AK1
<1>1. Spec /\ []A1Side => []BoundOK BY A1Inductive, A1BoundOK, PTL
<1>. QED BY <1>1, AK1FromBoundOK, PTL

==============================================================================
