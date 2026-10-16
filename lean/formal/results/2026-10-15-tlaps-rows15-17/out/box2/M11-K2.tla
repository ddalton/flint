------------------------------ MODULE LeanP1ProofM11 ------------------------------
(* M11 (row 15): AK1 part 2, BoundOK an invariant.  Generated:
   results/2026-10-15-tlaps-rows15-17/gen-m11.py (its docstring has the plan;
   AK1-PLAN.txt there too).                                                 *)
EXTENDS LeanP1ProofM10, LeanP1ProofM5D

\* A declared path the agent has not moved on from (MCLeanP1M10's A1Decl).
A1Decl(s, p) == w[s].pend.id # 0 /\ p \in w[s].dset /\ p \notin w[s].moved
\* (D) a dirty declared path holds the declared bytes.
A1DTree == \A s \in Writers, p \in Paths :
           A1Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] => Content(w[s].local[p]) = Content(w[s].decl[p])
\* (C) a clean declared FILE is in carrier form, or a fetch made the tree agree.
A1CPub3 == \A s \in Writers, p \in Paths :
          A1Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p] =>
            \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            \/ Content(w[s].baseline[p]) = Content(w[s].decl[p])
\* (F) folds: a touch makes the honour wait for the next barrier end.
A1Folds == \A s \in Writers : w[s].pend.fold <= w[s].nt /\ w[s].bound.fold <= w[s].nt /\ w[s].cFold <= w[s].nt
\* (Z) in a barrier the record is the one it consumed (touch, retire: idle only).
A1Frozen == \A s \in Writers : w[s].pc \in {"consumed", "scanned", "claimed", "cased"} =>
            w[s].cId = w[s].pend.id /\ w[s].cFold = w[s].pend.fold
\* (M) after a carrying scan, a dirty declared path is published by it.
A1MidBar == \A s \in Writers, p \in Paths :
            w[s].pc \in {"scanned", "claimed", "cased"} /\ Carries(s) /\ A1Decl(s, p)
            /\ w[s].local[p] # w[s].baseline[p] =>
              \/ p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p]
              \/ p \in w[s].deletes /\ w[s].local[p] = Nil
\* (I) after a carrying install, a declared file is in carrier form, or the
\* install's result carries the declaration.
A1Cased == \A s \in Writers, p \in Paths :
           w[s].pc = "cased" /\ Carries(s) /\ A1Decl(s, p) /\ w[s].decl[p] # Nil =>
             \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
             \/ Content(w[s].inst[p]) = Content(w[s].decl[p])
             \* withheld: the barrier end names it (wd = gone; M10C1/C2, two writers)
             \/ p \in w[s].gone
\* Typing the steps need: what is declared, the bound's document, what was
\* installed, what the carrier published, and what a conflict preserves, are minted.
A1ConflictsMinted == \A c \in conflicts : c[2] \in Opt(minted)
A1DeclMinted == \A s \in Writers, p \in Paths : w[s].decl[p] \in Opt(minted)
A1BoundMinted == \A s \in Writers : w[s].bound.doc \in [Paths -> Opt(minted)]
A1InstMinted == \A s \in Writers, p \in Paths : w[s].inst[p] \in Opt(minted)
A1CarPubMinted == \A s \in Writers, p \in Paths : w[s].car.pub[p] \in Opt(minted)
\* The invariant.
A1IAK1 == /\ A1Folds /\ A1Frozen /\ A1MidBar /\ A1Cased /\ A1DTree /\ A1CPub3
          /\ A1DeclMinted /\ A1BoundMinted /\ A1InstMinted /\ A1CarPubMinted /\ A1ConflictsMinted
          /\ BoundOK

------------------------------------------------------------------------------
(* Broken is kept by a step that keeps the declaration, only adds conflicts   *)
(* and live handles, and adds no lost handle.                              *)


LEMMA A1ContentStep ==
  ASSUME IndC, HStep, NEW h \in Opt(minted)
  PROVE  Content(h)' = Content(h)
<1>1. CASE h = Nil BY <1>1 DEF Content
<1>2. CASE h \in minted BY <1>2, ContentKeep
<1>. QED BY <1>1, <1>2 DEF Opt

LEMMA A1BrokenKeep ==
  ASSUME IndC, HStep, A1ConflictsMinted, conflicts \subseteq conflicts', live \subseteq live',
         UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers},
         NEW s \in Writers, NEW p \in Paths, NEW d,
         w'[s].dset = w[s].dset, w'[s].moved = w[s].moved,
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
(* handles keeps every conjunct (GPut, GCas, GDelete, GRename, GRenameFinish).*)

LEMMA A1NoTree ==
  ASSUME IndC, A1IAK1, HStep, w' = w, conflicts' = conflicts, live \subseteq live'
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
<1>4. A1Cased' BY <1>c DEF A1IAK1, A1Cased, A1Decl, CarForm, Carries
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
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>6, <1>7 DEF A1IAK1
------------------------------------------------------------------------------
(* The gateway steps.                                                       *)

LEMMA GCas_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW p \in Paths, GCas(p), Frame
  PROVE  A1IAK1'
<1>1. w' = w /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF GCas, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>. QED BY <1>1, <1>hs, A1NoTree

LEMMA GDelete_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW p \in Paths, GDelete(p), Frame
  PROVE  A1IAK1'
<1>1. w' = w /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF GDelete, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>. QED BY <1>1, <1>hs, A1NoTree

LEMMA GRename_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  A1IAK1'
<1>1. w' = w /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF GRename, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>. QED BY <1>1, <1>hs, A1NoTree

LEMMA GRenameFinish_A1 ==
  ASSUME IndC, IndC', A1IAK1, GRenameFinish, Frame
  PROVE  A1IAK1'
<1>1. w' = w /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF GRenameFinish, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>. QED BY <1>1, <1>hs, A1NoTree

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
A1PcOK(R, W) ==
  /\ R.pc \in {"consumed", "scanned", "claimed", "cased"} => W.pc \in {"consumed", "scanned", "claimed", "cased"}
  /\ R.pc \in {"scanned", "claimed", "cased"} => W.pc \in {"scanned", "claimed", "cased"}
  /\ R.pc = "cased" => W.pc = "cased"

LEMMA A1Keep ==
  ASSUME IndC, A1IAK1, HStep, NEW s \in Writers, NEW R,
         w' = [w EXCEPT ![s] = R], A1Same(R, w[s]), A1PcOK(R, w[s]),
         conflicts' = conflicts, live \subseteq live'
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
<1>7. A1Cased' BY <1>f, <1>c DEF A1IAK1, A1Cased, A1Same, A1PcOK, A1Decl, CarForm, Carries
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
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>6, <1>7, <1>8, <1>9 DEF A1IAK1
------------------------------------------------------------------------------
(* The writer steps that keep AK1's fields (A1Keep).                       *)

LEMMA Claim_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Claim(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF Claim, bucket, aux
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
<1>3. A1PcOK(ClaimW(s), w[s])
  <2>1. ClaimW(s).pc = "claimed" BY DEF ClaimW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA Verify_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Verify(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF Verify, bucket, aux
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
<1>3. A1PcOK(VerifyW(s), w[s])
  <2>1. VerifyW(s).pc = w[s].pc BY DEF VerifyW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA RPullRead_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, RPullRead(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF RPullRead, bucket, aux
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
<1>3. A1PcOK(RPullReadW(s), w[s])
  <2>1. RPullReadW(s).pc = "pulling" BY DEF RPullReadW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA Honor_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Honor(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF Honor, bucket, aux
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
<1>3. A1PcOK(HonorW(s), w[s])
  <2>1. HonorW(s).pc = w[s].pc BY DEF HonorW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

LEMMA HonorReader_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, HonorReader(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF HonorReader, bucket, aux
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
<1>3. A1PcOK(HonorReaderW(s), w[s])
  <2>1. HonorReaderW(s).pc = w[s].pc BY DEF HonorReaderW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>1, <1>hs, <1>2, <1>3, A1Keep

==============================================================================
