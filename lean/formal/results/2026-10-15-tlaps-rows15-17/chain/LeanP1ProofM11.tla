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
  ASSUME IndC, HStep, A1ConflictsMinted, conflicts \subseteq conflicts',
         \A c \in conflicts : c[2] \in live => c[2] \in live',
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
  ASSUME IndC, A1IAK1, HStep, w' = w, conflicts' = conflicts,
         \A c \in conflicts : c[2] \in live => c[2] \in live'
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

------------------------------------------------------------------------------
(* A step that ends a barrier without a bound write (Fence before the        *)
(* install, Restart): pc idle, the barrier's fields cleared, every field   *)
(* BoundOK, DTree and CPub3 read kept.                                     *)

A1Kept(R, W) ==
  /\ R.pend = W.pend /\ R.dset = W.dset /\ R.decl = W.decl /\ R.moved = W.moved /\ R.dMint = W.dMint
  /\ R.local = W.local /\ R.baseline = W.baseline /\ R.car = W.car /\ R.bound = W.bound
  /\ R.nt = W.nt /\ R.lost = W.lost
A1Cleared(R) == R.pc = "idle" /\ R.inst = [q \in Paths |-> Nil] /\ R.cId = 0 /\ R.cFold = 0

LEMMA A1Reset ==
  ASSUME IndC, A1IAK1, HStep, NEW s \in Writers, NEW R,
         w' = [w EXCEPT ![s] = R], A1Kept(R, w[s]), A1Cleared(R),
         conflicts' = conflicts, live \subseteq live'
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
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>6, <1>7, <1>8, <1>9 DEF A1IAK1

LEMMA Restart_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Restart(s), Frame
  PROVE  A1IAK1'
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF Restart
<1>hs. HStep BY <1>1, HStepQ DEF Frame
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11 DEF A1Kept
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
<1>u. UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF Fence, bucket, aux
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
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10, <3>11 DEF A1Kept
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
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10, <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17 DEF A1Same
  <2>3. A1PcOK(FenceAfterW(s), w[s])
    <3>1. FenceAfterW(s).pc = w[s].pc BY DEF FenceAfterW
    <3> QED BY <2>0, <3>1 DEF A1PcOK
  <2>. QED BY <2>1, <2>2, <2>3, <1>u, <1>hs, A1Keep
<1>. QED BY <1>1, <1>2

LEMMA RescopeBegin_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  A1IAK1'
<1>0. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>u. UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF RescopeBegin, bucket
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
<1>3. A1PcOK(RescopeBeginW(s, T), w[s])
  <2>1. RescopeBeginW(s, T).pc = w[s].pc BY DEF RescopeBeginW
  <2> QED BY <1>g, <2>1 DEF A1PcOK
<1>. QED BY <1>0, <1>u, <1>hs, <1>2, <1>3, A1Keep

------------------------------------------------------------------------------
(* A1Keep with the per-tree fact as the hypothesis (a step writing two trees). *)

LEMMA A1KeepF ==
  ASSUME IndC, A1IAK1, HStep,
         \A u \in Writers : A1Same(w'[u], w[u]) /\ A1PcOK(w'[u], w[u]),
         conflicts' = conflicts, live \subseteq live'
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
LEMMA Depose_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW t \in Writers, Depose(t), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<live, minted, base, conflicts, orig>>
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
<1>5. DeposeW(t).pc = "claimed" /\ DeposedW(holder).pc = w[holder].pc BY DEF DeposeW, DeposedW
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
      /\ UNCHANGED <<minted, base, conflicts, orig>>
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
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17 DEF A1Same
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
      /\ UNCHANGED <<minted, base, conflicts, orig>>
  BY DEF Sweep, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>2. \A c \in conflicts : c[2] \in live => c[2] \in live' BY <1>1 DEF Preserved
<1>. QED BY <1>1, <1>hs, <1>2, A1NoTree

\* Age and RLoad: no tree, no conflict, no live handle changes, anc kept.
LEMMA A1Quiet ==
  ASSUME IndC, IndC', A1IAK1, UNCHANGED <<anc, minted, base, orig, conflicts, w, live>>
  PROVE  A1IAK1'
<1>u. AncUpdate BY AncUpdateSame DEF IndC
<1>hs. HStep BY <1>u, HStepQ
<1>. QED BY <1>hs, A1NoTree

------------------------------------------------------------------------------
(* The invariant writer by writer: a step writing tree s argues only about s; *)
(* every other tree is unchanged (A1Frame).                                *)

\* A tree's per-path facts outside the barrier: DTree, CPub3 and the typing.
A1TreePath(u, p) ==
  /\ (A1Decl(u, p) /\ w[u].local[p] # w[u].baseline[p]) => Content(w[u].local[p]) = Content(w[u].decl[p])
  /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ w[u].local[p] = w[u].baseline[p]) =>
       \/ w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
       \/ Content(w[u].baseline[p]) = Content(w[u].decl[p])
  /\ w[u].decl[p] \in Opt(minted) /\ w[u].inst[p] \in Opt(minted) /\ w[u].car.pub[p] \in Opt(minted)

A1Per(u) ==
  /\ w[u].pend.fold <= w[u].nt /\ w[u].bound.fold <= w[u].nt /\ w[u].cFold <= w[u].nt
  /\ w[u].pc \in {"consumed", "scanned", "claimed", "cased"} => w[u].cId = w[u].pend.id /\ w[u].cFold = w[u].pend.fold
  /\ \A p \in Paths :
       /\ (w[u].pc \in {"scanned", "claimed", "cased"} /\ w[u].cId # 0 /\ A1Decl(u, p) /\ w[u].local[p] # w[u].baseline[p]) =>
            \/ p \in w[u].uploads /\ w[u].snap[p] = w[u].local[p]
            \/ p \in w[u].deletes /\ w[u].local[p] = Nil
       /\ (w[u].pc = "cased" /\ w[u].cId # 0 /\ A1Decl(u, p) /\ w[u].decl[p] # Nil) =>
            \/ w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
            \/ Content(w[u].inst[p]) = Content(w[u].decl[p])
            \/ p \in w[u].gone
       /\ (A1Decl(u, p) /\ w[u].local[p] # w[u].baseline[p]) => Content(w[u].local[p]) = Content(w[u].decl[p])
       /\ (A1Decl(u, p) /\ w[u].decl[p] # Nil /\ w[u].local[p] = w[u].baseline[p]) =>
            \/ w[u].car.id = w[u].pend.id /\ CarForm(u, p, w[u].car)
            \/ Content(w[u].baseline[p]) = Content(w[u].decl[p])
       /\ w[u].decl[p] \in Opt(minted) /\ w[u].inst[p] \in Opt(minted) /\ w[u].car.pub[p] \in Opt(minted)
  /\ w[u].bound.doc \in [Paths -> Opt(minted)]
  /\ Honorable(u) => \A p \in Paths : Broken(u, p, w[u].bound.doc) => p \in HDropped(u)

LEMMA A1PerAll == A1IAK1 <=> (\A u \in Writers : A1Per(u)) /\ A1ConflictsMinted
BY DEF A1IAK1, A1Per, A1Folds, A1Frozen, A1MidBar, A1Cased, A1DTree, A1CPub3, A1DeclMinted,
       A1BoundMinted, A1InstMinted, A1CarPubMinted, BoundOK, Carries

\* An unchanged tree keeps its part, when Content and Derives are kept (HStep),
\* conflicts only grow, a preserved handle stays live and no lost handle is added.
LEMMA A1Frame ==
  ASSUME IndC, A1IAK1, HStep, NEW u \in Writers, w'[u] = w[u],
         A1ConflictsMinted, conflicts \subseteq conflicts',
         \A c \in conflicts : c[2] \in live => c[2] \in live',
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
<1>b. Honorable(u)' => \A p \in Paths : Broken(u, p, w[u].bound.doc)' => p \in HDropped(u)'
  <2>. SUFFICES ASSUME Honorable(u)', NEW p \in Paths, Broken(u, p, w[u].bound.doc)'
                PROVE  p \in HDropped(u)' OBVIOUS
  <2>0. PICK d \in [Paths -> Opt(minted)] : d = w[u].bound.doc BY <1>0 DEF A1Per
  <2>1. Broken(u, p, d)' BY <2>0
  <2>2. Broken(u, p, d) BY <2>0, <2>1, <1>0, A1BrokenKeep DEF A1Per
  <2>3. Honorable(u) BY DEF Honorable
  <2>. QED BY <1>0, <2>0, <2>2, <2>3 DEF A1Per, HDropped
<1>. QED BY <1>0, <1>c, <1>m, <1>b DEF A1Per, A1Decl, CarForm, Honorable, HDropped, Opt

LEMMA A1PerAllP == (\A u \in Writers : A1Per(u)') /\ A1ConflictsMinted' => A1IAK1'
BY DEF A1IAK1, A1Per, A1Folds, A1Frozen, A1MidBar, A1Cased, A1DTree, A1CPub3, A1DeclMinted,
       A1BoundMinted, A1InstMinted, A1CarPubMinted, BoundOK, Carries

\* Every step keeps Content of every tree handle the conjuncts read.
LEMMA A1ContentAll ==
  ASSUME IndC, A1IAK1, HStep
  PROVE  \A u \in Writers, q \in Paths :
           /\ Content(w[u].local[q])' = Content(w[u].local[q])
           /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
           /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
           /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
           /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
<1>5. IndM5 BY DEF IndC
<1>ty. \A u \in Writers, q \in Paths :
         /\ w[u].local[q] \in Opt(minted) /\ w[u].baseline[q] \in Opt(minted)
         /\ w[u].decl[q] \in Opt(minted) /\ w[u].inst[q] \in Opt(minted) /\ w[u].car.pub[q] \in Opt(minted)
  BY <1>5, TreeTy DEF A1IAK1, A1DeclMinted, A1InstMinted, A1CarPubMinted
<1>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths
              PROVE  /\ Content(w[u].local[q])' = Content(w[u].local[q])
                     /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
                     /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
                     /\ Content(w[u].inst[q])' = Content(w[u].inst[q])
                     /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q]) OBVIOUS
<1>1. PICK h1 \in Opt(minted) : h1 = w[u].local[q] BY <1>ty
<1>2. PICK h2 \in Opt(minted) : h2 = w[u].baseline[q] BY <1>ty
<1>3. PICK h3 \in Opt(minted) : h3 = w[u].decl[q] BY <1>ty
<1>4. PICK h4 \in Opt(minted) : h4 = w[u].inst[q] BY <1>ty
<1>6. PICK h5 \in Opt(minted) : h5 = w[u].car.pub[q] BY <1>ty
<1>7. /\ Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3)
      /\ Content(h4)' = Content(h4) /\ Content(h5)' = Content(h5)
  BY A1ContentStep
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>6, <1>7

------------------------------------------------------------------------------
(* The touch: a new declaration, at the tree's bytes, of its dirty paths;   *)
(* the honour then waits for the next barrier end (the fold moved).        *)

LEMMA Touch_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Touch(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>>
  BY DEF Touch, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>w. \A u \in Writers : w'[u] = IF u = s THEN TouchW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>g. w[s].pc = "idle" BY DEF Touch
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
  <2>2. TouchW(s).pend = [id |-> IF TouchFresh(s) THEN TouchN(s) ELSE w[s].pend.id, fold |-> TouchN(s), nonces |-> TouchKept(s)]
    BY DEF TouchW
  <2>. QED BY <2>1, <2>2
<1>fd. /\ TouchW(s).dset = (IF TouchFresh(s) THEN {} ELSE w[s].dset) \cup TouchDirty(s)
       /\ TouchW(s).decl = [p \in Paths |-> IF p \in TouchDirty(s) THEN w[s].local[p]
                                           ELSE IF TouchFresh(s) THEN Nil ELSE w[s].decl[p]]
       /\ TouchW(s).moved = (IF TouchFresh(s) THEN {} ELSE w[s].moved) \ TouchDirty(s)
  <2>1. TouchW(s).dset = (IF TouchFresh(s) THEN {} ELSE w[s].dset) \cup TouchDirty(s) BY DEF TouchW
  <2>2. TouchW(s).decl = [p \in Paths |-> IF p \in TouchDirty(s) THEN w[s].local[p]
                                         ELSE IF TouchFresh(s) THEN Nil ELSE w[s].decl[p]] BY DEF TouchW
  <2>3. TouchW(s).moved = (IF TouchFresh(s) THEN {} ELSE w[s].moved) \ TouchDirty(s) BY DEF TouchW
  <2>. QED BY <2>1, <2>2, <2>3
<1>dy. TouchDirty(s) \subseteq {p \in Paths : w[s].local[p] # w[s].baseline[p]} BY DEF TouchDirty
<1>c. \A q \in Paths :
        /\ Content(w[s].local[q])' = Content(w[s].local[q])
        /\ Content(w[s].baseline[q])' = Content(w[s].baseline[q])
        /\ Content(w[s].decl[q])' = Content(w[s].decl[q])
        /\ Content(w[s].car.pub[q])' = Content(w[s].car.pub[q])
  BY <1>hs, A1ContentAll
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>lm. \A q \in Paths : w[s].local[q] \in Opt(minted) BY TreeTy DEF IndC
\* Every other tree keeps its part.
<1>o. \A u \in Writers \ {s} : A1Per(u)'
  <2>. SUFFICES ASSUME NEW u \in Writers \ {s} PROVE A1Per(u)' OBVIOUS
  <2>1. w'[u] = w[u] BY <1>w
  <2>2. UNION {w'[t].lost : t \in Writers} \subseteq UNION {w[t].lost : t \in Writers} BY <1>w, <1>fx
  <2>. QED BY <1>1, <1>hs, <2>1, <2>2, A1Frame DEF A1IAK1
\* s's part.
<1>s. A1Per(s)'
  <2>w. w'[s] = TouchW(s) BY <1>w
  <2>1. w'[s].pend.fold <= w'[s].nt /\ w'[s].bound.fold <= w'[s].nt /\ w'[s].cFold <= w'[s].nt
    BY <2>w, <1>per, <1>n, <1>fx, <1>fn DEF A1Per
  <2>2. w'[s].pc = "idle" BY <2>w, <1>g, <1>fx
  <2>3. \A p \in Paths : A1TreePath(s, p)'
    <3>. SUFFICES ASSUME NEW p \in Paths PROVE A1TreePath(s, p)' OBVIOUS
    <3>1. CASE p \in TouchDirty(s)
      \* declared now, at the tree's bytes; dirty, so not clean
      <4>1. w'[s].decl[p] = w[s].local[p] /\ w'[s].local[p] = w[s].local[p] /\ w'[s].local[p] # w'[s].baseline[p]
        BY <3>1, <2>w, <1>fx, <1>fd, <1>dy
      <4>2. w'[s].inst[p] = w[s].inst[p] /\ w'[s].car = w[s].car BY <2>w, <1>fx
      <4>. QED BY <4>1, <4>2, <1>per, <1>lm, <1>m, <1>c DEF A1TreePath, A1Per, A1Decl, Opt
    <3>2. CASE p \notin TouchDirty(s)
      <4>1. A1Decl(s, p)' => ~TouchFresh(s) /\ A1Decl(s, p)
        BY <3>2, <2>w, <1>fn, <1>fd DEF A1Decl, TouchFresh
      <4>2. ~TouchFresh(s) => w'[s].decl[p] = w[s].decl[p] /\ w'[s].pend.id = w[s].pend.id
        BY <3>2, <2>w, <1>fn, <1>fd
      <4>3. TouchFresh(s) => w'[s].decl[p] = Nil BY <3>2, <2>w, <1>fd
      <4>4. w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
            /\ w'[s].car = w[s].car /\ w'[s].inst[p] = w[s].inst[p]
        BY <2>w, <1>fx
      <4>. QED BY <4>1, <4>2, <4>3, <4>4, <1>per, <1>c, <1>m DEF A1TreePath, A1Per, CarForm, Opt
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
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] /\ UNCHANGED <<live, minted, base, conflicts, orig>>
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
    BY <2>w, <2>0, <1>per, <1>n, <1>fx DEF A1Per
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

LEMMA A1BoundKeep ==
  ASSUME IndC, A1IAK1, HStep, NEW u \in Writers, A1BoundFields(w'[u], w[u]),
         A1ConflictsMinted, conflicts \subseteq conflicts',
         \A c \in conflicts : c[2] \in live => c[2] \in live',
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
  ASSUME IndC, A1IAK1, HStep, NEW u \in Writers, A1TreeFields(w'[u], w[u])
  PROVE  \A p \in Paths : A1TreePath(u, p)'
<1>0. A1Per(u) BY A1PerAll
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>c. \A q \in Paths :
        /\ Content(w[u].local[q])' = Content(w[u].local[q])
        /\ Content(w[u].baseline[q])' = Content(w[u].baseline[q])
        /\ Content(w[u].decl[q])' = Content(w[u].decl[q])
        /\ Content(w[u].car.pub[q])' = Content(w[u].car.pub[q])
  BY A1ContentAll
<1>. QED BY <1>0, <1>c, <1>m DEF A1Per, A1TreePath, A1TreeFields, A1Decl, CarForm, Opt

------------------------------------------------------------------------------
(* The scan: a carrying one publishes every dirty declared path (MidBar).   *)

LEMMA Scan_A1 ==
  ASSUME IndC, IndC', A1IAK1, NEW s \in Writers, Scan(s), Frame
  PROVE  A1IAK1'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>d. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) :
        w' = [w EXCEPT ![s] = ScanW(s, dels)]
  BY DEF Scan
<1>1. UNCHANGED <<live, minted, base, conflicts, orig>> BY DEF Scan, bucket, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
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
    BY <2>w, <1>f, <1>per DEF A1Per
  <2>2. w'[s].cId = w'[s].pend.id /\ w'[s].cFold = w'[s].pend.fold BY <2>w, <1>f, <1>g, <1>per DEF A1Per
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
  <2>4. \A p \in Paths : A1TreePath(s, p)'
    <3>1. A1TreeFields(w'[s], w[s]) BY <2>w, <1>f DEF A1TreeFields
    <3>. QED BY <3>1, <1>hs, A1TreeKeep
  <2>5. w'[s].bound.doc \in [Paths -> Opt(minted')] BY <2>w, <1>f, <1>per, <1>hs DEF A1Per, Opt, HStep, HistEv
  <2>6. (Honorable(s) => \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s))'
    <3>1. A1BoundFields(w'[s], w[s]) BY <2>w, <1>f DEF A1BoundFields
    <3>. QED BY <3>1, <1>1, <1>hs, <1>lost, A1BoundKeep DEF A1IAK1
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>w, <1>f DEF A1Per, A1TreePath
<1>cm. A1ConflictsMinted' BY <1>1, <1>hs DEF A1IAK1, A1ConflictsMinted, Opt, HStep, HistEv
<1>. QED BY <1>o, <1>s, <1>cm, A1PerAllP

==============================================================================
