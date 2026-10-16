--------------------------- MODULE LeanP1ProofM5A ----------------------------
EXTENDS LeanP1ProofM4

------------------------------------------------------------------------------
(* M5, part A: the history.                                                 *)

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
M5 == Hist
IndM5 == IndM4 /\ M5

\* What a step does to the history: it keeps every minted handle's, and
\* writes a minted history only at what it mints.
HistEv ==
  /\ minted \subseteq minted'
  /\ \A m \in minted : base'[m] = base[m] /\ orig'[m] = orig[m]
  /\ \A h \in minted' \ minted : /\ base'[h] \in Opt(minted) /\ orig'[h] \in Opt(minted)
                                 /\ (orig'[h] # Nil => orig[orig'[h]] = Nil)
  /\ \A h \in Handles \ minted' : base'[h] = Nil /\ orig'[h] = Nil
\* Where a step takes a tree's new baseline from.
BlEv == \A u \in Writers, p \in Paths :
          w'[u].baseline[p] \in {w[u].baseline[p], Nil, doc[p], w[u].snap[p]}
\* The typing HistWrite reads.
HistTy ==
  /\ minted \subseteq Handles /\ minted' \subseteq Handles
  /\ base \in [Handles -> Opt(Handles)] /\ base' \in [Handles -> Opt(Handles)]
  /\ orig \in [Handles -> Opt(Handles)] /\ orig' \in [Handles -> Opt(Handles)]
  /\ anc \in [Handles -> SUBSET Handles]

------------------------------------------------------------------------------
(* What the events give the history.                                        *)

LEMMA HistSame ==
  ASSUME Hist, UNCHANGED <<minted, base, orig>>
  PROVE  HistEv
BY DEF Hist, HistNew, HistEv

LEMMA BlNone == w' = w => BlEv
BY DEF BlEv

LEMMA BlWrite ==
  ASSUME NEW t, NEW R, Wr(t, R),
         t \in Writers => \A p \in Paths : R.baseline[p] \in {w[t].baseline[p], Nil, doc[p], w[t].snap[p]}
  PROVE  BlEv
BY DEF Wr, BlEv

\* A minted handle keeps its `anc` across a step.
LEMMA AncKeep ==
  ASSUME HistTy, HistEv, AncUpdate, NEW m \in minted
  PROVE  anc'[m] = anc[m]
BY DEF HistEv, AncUpdate, HistTy

LEMMA HistWrite ==
  ASSUME Hist, HistEv, HistTy, AncUpdate, BlEv, Minted, SnapMinted
  PROVE  Hist'
<1>k. \A m \in minted : anc'[m] = anc[m] BY AncKeep
<1>1. HistNew'
  <2>. SUFFICES ASSUME NEW h \in Handles \ minted' PROVE base'[h] = Nil /\ anc'[h] = {} /\ orig'[h] = Nil
    BY DEF HistNew
  <2>1. h \notin minted BY DEF HistEv
  <2>2. base[h] = Nil /\ anc[h] = {} BY <2>1 DEF Hist, HistNew
  <2>3. base'[h] = Nil /\ orig'[h] = Nil BY DEF HistEv
  <2>4. anc'[h] = anc[h] BY <2>2, <2>3 DEF AncUpdate, HistTy
  <2>. QED BY <2>2, <2>3, <2>4
<1>2. HistAnc'
  <2>. SUFFICES ASSUME NEW h \in Handles
                PROVE  anc'[h] = IF base'[h] = Nil THEN {} ELSE {base'[h]} \cup anc'[base'[h]]
    BY DEF HistAnc, AncOf
  <2>1. CASE h \in minted
    <3>1. base'[h] = base[h] /\ anc'[h] = anc[h] BY <2>1, <1>k DEF HistEv
    <3>2. anc[h] = IF base[h] = Nil THEN {} ELSE {base[h]} \cup anc[base[h]] BY DEF Hist, HistAnc, AncOf
    <3>3. base[h] \in Opt(minted) BY <2>1 DEF Hist, HistMinted
    <3>4. base[h] # Nil => anc'[base[h]] = anc[base[h]] BY <3>3, <1>k DEF Opt
    <3>. QED BY <3>1, <3>2, <3>4
  <2>2. CASE h \notin minted
    <3>1. base[h] = Nil /\ anc[h] = {} BY <2>2 DEF Hist, HistNew
    <3>2. CASE base'[h] = Nil
      <4>1. anc'[h] = anc[h] BY <3>1, <3>2 DEF AncUpdate, HistTy
      <4>. QED BY <3>1, <3>2, <4>1
    <3>3. CASE base'[h] # Nil
      <4>1. h \in minted' BY <3>3 DEF HistEv
      <4>2. base'[h] \in minted BY <2>2, <3>3, <4>1 DEF HistEv, Opt
      <4>3. anc'[h] = {base'[h]} \cup anc[base'[h]] BY <3>1, <3>3 DEF AncUpdate, AncOf, HistTy
      <4>. QED BY <4>2, <4>3, <3>3, <1>k
    <3>. QED BY <3>2, <3>3
  <2>. QED BY <2>1, <2>2
<1>3. HistMinted'
  <2>. SUFFICES ASSUME NEW h \in minted'
                PROVE  /\ base'[h] \in Opt(minted') /\ anc'[h] \subseteq minted'
                       /\ orig'[h] \in Opt(minted') /\ (orig'[h] # Nil => orig'[orig'[h]] = Nil)
    BY DEF HistMinted
  <2>0. minted \subseteq minted' BY DEF HistEv
  <2>1. CASE h \in minted
    <3>1. base'[h] = base[h] /\ orig'[h] = orig[h] /\ anc'[h] = anc[h] BY <2>1, <1>k DEF HistEv
    <3>2. /\ base[h] \in Opt(minted) /\ anc[h] \subseteq minted
          /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
      BY <2>1 DEF Hist, HistMinted
    <3>3. orig[h] # Nil => orig'[orig[h]] = orig[orig[h]] BY <3>2 DEF HistEv, Opt
    <3>. QED BY <2>0, <3>1, <3>2, <3>3 DEF Opt
  <2>2. CASE h \notin minted
    <3>1. /\ base'[h] \in Opt(minted) /\ orig'[h] \in Opt(minted)
          /\ (orig'[h] # Nil => orig[orig'[h]] = Nil)
      BY <2>2 DEF HistEv
    <3>2. base[h] = Nil /\ anc[h] = {} BY <2>2 DEF Hist, HistNew, HistTy
    <3>3. anc'[h] \subseteq minted
      <4>1. CASE base'[h] = Nil BY <3>2, <4>1 DEF AncUpdate, HistTy
      <4>2. CASE base'[h] # Nil
        <5>1. base'[h] \in minted BY <3>1, <4>2 DEF Opt
        <5>2. anc'[h] = {base'[h]} \cup anc[base'[h]] BY <3>2, <4>2 DEF AncUpdate, AncOf, HistTy
        <5>3. anc[base'[h]] \subseteq minted BY <5>1 DEF Hist, HistMinted
        <5>. QED BY <5>1, <5>2, <5>3
      <4>. QED BY <4>1, <4>2
    <3>4. orig'[h] # Nil => orig'[orig'[h]] = orig[orig'[h]] BY <3>1 DEF HistEv, Opt
    <3>. QED BY <2>0, <3>1, <3>3, <3>4 DEF Opt
  <2>. QED BY <2>1, <2>2
<1>4. BaselineMinted'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths PROVE w'[u].baseline[p] \in Opt(minted')
    BY DEF BaselineMinted
  <2>1. w'[u].baseline[p] \in {w[u].baseline[p], Nil, doc[p], w[u].snap[p]} BY DEF BlEv
  <2>2. w[u].baseline[p] \in Opt(minted) BY DEF Hist, BaselineMinted
  <2>3. doc[p] \in Opt(minted) /\ w[u].snap[p] \in Opt(minted) BY DEF Minted, SnapMinted
  <2>. QED BY <2>1, <2>2, <2>3 DEF HistEv, Opt
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF Hist

\* THE POINT: Derives and Content between minted handles never move.
LEMMA DerivesKeep ==
  ASSUME Hist, HistEv, HistTy, AncUpdate, NEW k \in minted, NEW x \in minted
  PROVE  /\ Content(k)' = Content(k)
         /\ Derives(k, x)' = Derives(k, x)
<1>0. Nil \notin Handles BY NilHandle
<1>k. anc'[k] = anc[k] BY AncKeep
<1>a. anc[k] \subseteq minted BY DEF Hist, HistMinted
<1>c. \A m \in minted : Content(m)' = Content(m) BY DEF HistEv, Content
<1>1. Content(k)' = Content(k) BY <1>c
<1>2. Derives(k, x)' = Derives(k, x)
  <2>1. k # Nil /\ x # Nil BY <1>0 DEF HistTy
  <2>2. (\E a \in anc'[k] : a = x \/ (x # Nil /\ Content(a)' = Content(x)'))
        = (\E a \in anc[k] : a = x \/ (x # Nil /\ Content(a) = Content(x)))
    BY <1>k, <1>a, <1>c
  <2>. QED BY <2>1, <2>2, <1>c DEF Derives
<1>. QED BY <1>1, <1>2

\* The history after a step that writes none of it and no tree.
LEMMA HistAll ==
  ASSUME Hist, UNCHANGED <<minted, base, orig, anc, w>>
  PROVE  Hist'
BY DEF Hist, HistNew, HistAnc, AncOf, HistMinted, BaselineMinted

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M5 == Init => IndM5
<1>. SUFFICES ASSUME Init PROVE IndM5 OBVIOUS
<1>1. IndM4 BY Init_M4
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>t. minted \subseteq Handles BY <1>1 DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>b. base = [h \in Handles |-> Nil] /\ anc = [h \in Handles |-> {}] /\ orig = [h \in Handles |-> Nil] BY DEF Init
<1>3a. HistNew BY <1>b DEF HistNew
<1>3b. HistAnc BY <1>b DEF HistAnc, AncOf
<1>3c. HistMinted BY <1>b, <1>t DEF HistMinted, Opt
<1>3. HistNew /\ HistAnc /\ HistMinted BY <1>3a, <1>3b, <1>3c
<1>4. BaselineMinted BY <1>2 DEF BaselineMinted, WriterInit, Opt
<1>. QED BY <1>1, <1>3, <1>4 DEF IndM5, M5, Hist

------------------------------------------------------------------------------
(* M5: the gateway.                                                         *)

LEMMA GPut_M5 ==
  ASSUME IndM5, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GPut_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv
  <2>. DEFINE h == <<p, nextGen>>
  <2>1. /\ minted' = minted \cup {h} /\ base' = [base EXCEPT ![h] = doc[p]] /\ orig' = orig
        /\ nextGen <= MaxMint
    BY DEF GPut, aux
  <2>2. nextGen \in Nat /\ nextGen > Seed BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>3. h \in Handles BY <2>1, <2>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
  <2>4. h \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>. QED BY <3>1, <2>1, <2>2, MaxMintNat DEF Gen
  <2>5. doc[p] \in Opt(minted) BY <1>y DEF Minted
  <2>6. orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil) BY <2>3, <2>4, <1>c DEF Hist, HistNew, Opt
<2>q1. minted \subseteq minted' BY <2>1
<2>q2. \A x \in minted : base'[x] = base[x] /\ orig'[x] = orig[x]
  BY <2>1, <2>4, <1>ty DEF HistTy
<2>q3. \A x \in minted' \ minted : /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                                     /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
  <3>1. SUFFICES ASSUME NEW x \in minted' \ minted
                PROVE  /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                       /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
    OBVIOUS
  <3>2. x = h BY <2>1
  <3>3. base'[h] = doc[p] /\ orig'[h] = orig[h] BY <2>1, <2>3, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <2>5, <2>6
<2>q4. \A x \in Handles \ minted' : base'[x] = Nil /\ orig'[x] = Nil
  <3>1. SUFFICES ASSUME NEW x \in Handles \ minted' PROVE base'[x] = Nil /\ orig'[x] = Nil
    OBVIOUS
  <3>2. x # h /\ x \notin minted BY <2>1
  <3>3. base'[x] = base[x] /\ orig'[x] = orig[x] BY <3>2, <2>1, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <1>c DEF Hist, HistNew
<2>. QED BY <2>q1, <2>q2, <2>q3, <2>q4 DEF HistEv
<1>b. BlEv BY BlNone DEF GPut
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA GCas_M5 ==
  ASSUME IndM5, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GCas_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF GCas, bucket, aux
<1>b. BlEv BY BlNone DEF GCas
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA GRename_M5 ==
  ASSUME IndM5, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GRename_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF GRename, bucket, aux
<1>b. BlEv BY BlNone DEF GRename
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA GRenameFinish_M5 ==
  ASSUME IndM5, GRenameFinish, Frame
  PROVE  IndM5'
<1>1. mv = Nil BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

LEMMA GDelete_M5 ==
  ASSUME IndM5, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GDelete_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF GDelete, bucket, aux
<1>b. BlEv BY BlNone DEF GDelete
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Sweep_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Sweep_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF Sweep, bucket, aux
<1>b. BlEv BY BlNone DEF Sweep
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

------------------------------------------------------------------------------
(* M5: the agent, the commit section, the sync, the rescope, the reader.    *)

LEMMA Edit_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Edit_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>2. Wr(s, EditW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. EditW(s, p).baseline = w[s].baseline BY DEF EditW
<1>4. \A p0 \in Paths : EditW(s, p).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv
  <2>. DEFINE h == <<p, nextGen>>
  <2>1. /\ minted' = minted \cup {h} /\ base' = [base EXCEPT ![h] = IF w[s].local[p] # Nil THEN w[s].local[p] ELSE IF p \in w[s].uploads THEN w[s].snap[p] ELSE w[s].baseline[p]] /\ orig' = orig
        /\ nextGen <= MaxMint
    BY DEF Edit, aux
  <2>2. nextGen \in Nat /\ nextGen > Seed BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>3. h \in Handles BY <2>1, <2>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
  <2>4. h \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>. QED BY <3>1, <2>1, <2>2, MaxMintNat DEF Gen
  <2>5a. w[s].baseline[p] \in Opt(minted) BY <1>c DEF Hist, BaselineMinted
  <2>5b. w[s].local[p] \in Opt(minted) BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
  <2>5c. w[s].snap[p] \in Opt(minted) BY DEF IndM5, IndM4, IndM3, IndM2, M2, SnapMinted
  <2>5. (IF w[s].local[p] # Nil THEN w[s].local[p] ELSE IF p \in w[s].uploads THEN w[s].snap[p] ELSE w[s].baseline[p]) \in Opt(minted) BY <2>5a, <2>5b, <2>5c
  <2>6. orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil) BY <2>3, <2>4, <1>c DEF Hist, HistNew, Opt
<2>q1. minted \subseteq minted' BY <2>1
<2>q2. \A x \in minted : base'[x] = base[x] /\ orig'[x] = orig[x]
  BY <2>1, <2>4, <1>ty DEF HistTy
<2>q3. \A x \in minted' \ minted : /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                                     /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
  <3>1. SUFFICES ASSUME NEW x \in minted' \ minted
                PROVE  /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                       /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
    OBVIOUS
  <3>2. x = h BY <2>1
  <3>3. base'[h] = (IF w[s].local[p] # Nil THEN w[s].local[p] ELSE IF p \in w[s].uploads THEN w[s].snap[p] ELSE w[s].baseline[p]) /\ orig'[h] = orig[h] BY <2>1, <2>3, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <2>5, <2>6
<2>q4. \A x \in Handles \ minted' : base'[x] = Nil /\ orig'[x] = Nil
  <3>1. SUFFICES ASSUME NEW x \in Handles \ minted' PROVE base'[x] = Nil /\ orig'[x] = Nil
    OBVIOUS
  <3>2. x # h /\ x \notin minted BY <2>1
  <3>3. base'[x] = base[x] /\ orig'[x] = orig[x] BY <3>2, <2>1, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <1>c DEF Hist, HistNew
<2>. QED BY <2>q1, <2>q2, <2>q3, <2>q4 DEF HistEv
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Delete_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Delete_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Delete, bucket, aux
<1>2. Wr(s, DeleteW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. DeleteW(s, p).baseline = w[s].baseline BY DEF DeleteW
<1>4. \A p0 \in Paths : DeleteW(s, p).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Checkout_M5 ==
  ASSUME IndM5, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Checkout_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Checkout, bucket, aux
<1>2. Wr(s, CheckoutW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. CheckoutW(s, T).baseline = CheckoutHeld(s, T) BY DEF CheckoutW
<1>4. \A p0 \in Paths : CheckoutW(s, T).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF CheckoutHeld
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Consume_M5 ==
  ASSUME IndM5, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Consume_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] /\ UNCHANGED <<regressed, fails>>
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] /\ UNCHANGED <<minted, base, orig, doc>> BY <1>1, <1>ck DEF Consume, bucket, aux
  <2>2. Wr(s, ConsumeCheapW(s, ck)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. ConsumeCheapW(s, ck).baseline = w[s].baseline BY DEF ConsumeCheapW
  <2>4. \A p0 \in Paths : ConsumeCheapW(s, ck).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
  <2>h. HistEv BY <2>1, <1>c, HistSame DEF bucket, aux
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] /\ UNCHANGED <<minted, base, orig, doc>> BY <1>2, <1>ck DEF Consume, bucket, aux
  <2>2. Wr(s, ConsumeW(s, fail, ck)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. ConsumeW(s, fail, ck).baseline = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]] BY DEF ConsumeW
  <2>4. \A p0 \in Paths : ConsumeW(s, fail, ck).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
  <2>h. HistEv BY <2>1, <1>c, HistSame DEF bucket, aux
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>. QED BY <1>1, <1>2

LEMMA Scan_M5 ==
  ASSUME IndM5, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Scan_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Scan, bucket, aux
<1>2. Wr(s, ScanW(s, dels)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. ScanW(s, dels).baseline = w[s].baseline BY DEF ScanW
<1>4. \A p0 \in Paths : ScanW(s, dels).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Skip_M5 ==
  ASSUME IndM5, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Skip_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Skip, bucket, aux
<1>2. Wr(s, SkipW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. SkipW(s).baseline = w[s].baseline BY DEF SkipW
<1>4. \A p0 \in Paths : SkipW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Upload_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Upload_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ UNCHANGED <<minted, base, orig, doc>> BY <1>1 DEF Upload
  <2>2. Wr(s, UploadW(s, p)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. UploadW(s, p).baseline = w[s].baseline BY DEF UploadW
  <2>4. \A p0 \in Paths : UploadW(s, p).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
  <2>h. HistEv BY <2>1, <1>c, HistSame DEF bucket, aux
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>. DEFINE h == w[s].snap[p]
  <2>1. /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\ doc' = doc
      /\ minted' = minted \cup {c} /\ base' = [base EXCEPT ![c] = base[h]]
      /\ orig' = [orig EXCEPT ![c] = Content(h)] /\ copies < MaxCopies
    BY <1>2 DEF Upload
  <2>2. Wr(s, UploadCopyW(s, p, c)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. UploadCopyW(s, p, c).baseline = w[s].baseline BY DEF UploadCopyW
  <2>4. \A p0 \in Paths : UploadCopyW(s, p, c).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
<2>h. HistEv
    <3>1. /\ minted' = minted \cup {c} /\ base' = [base EXCEPT ![c] = base[h]]
          /\ orig' = [orig EXCEPT ![c] = Content(h)] /\ copies < MaxCopies
      BY <2>1
    <3>2. copies \in Nat /\ nextGen \in Nat /\ nextGen <= MaxMint + 1
      BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh, Ghosts, IndM1, IndTypeOK, TypeOK
    <3>3. c \in Handles BY <3>1, <3>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
    <3>4. c \notin minted
      <4>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
        BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
      <4>. QED BY <4>1, <3>2, MaxMintNat DEF Gen
    <3>h0. h \in minted BY <1>2 DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>h1. base[h] \in Opt(minted) /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
      BY <3>h0, <1>c DEF Hist, HistMinted
    <3>5. base[h] \in Opt(minted) BY <3>h1
    <3>6. Content(h) \in Opt(minted) /\ (Content(h) # Nil => orig[Content(h)] = Nil)
      <4>0. h # Nil BY <3>h0, NilHandle, <1>ty DEF HistTy
      <4>1. CASE orig[h] # Nil BY <4>0, <4>1, <3>h1 DEF Content, Opt
      <4>2. CASE orig[h] = Nil BY <4>0, <4>2, <3>h0 DEF Content, Opt
      <4>. QED BY <4>1, <4>2
  <3>q1. minted \subseteq minted' BY <3>1
  <3>q2. \A x \in minted : base'[x] = base[x] /\ orig'[x] = orig[x]
    BY <3>1, <3>4, <1>ty DEF HistTy
  <3>q3. \A x \in minted' \ minted : /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                                       /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
    <4>1. SUFFICES ASSUME NEW x \in minted' \ minted
                  PROVE  /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                         /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
      OBVIOUS
    <4>2. x = c BY <3>1
    <4>3. base'[c] = base[h] /\ orig'[c] = Content(h) BY <3>1, <3>3, <1>ty DEF HistTy
    <4>. QED BY <4>2, <4>3, <3>5, <3>6
  <3>q4. \A x \in Handles \ minted' : base'[x] = Nil /\ orig'[x] = Nil
    <4>1. SUFFICES ASSUME NEW x \in Handles \ minted' PROVE base'[x] = Nil /\ orig'[x] = Nil
      OBVIOUS
    <4>2. x # c /\ x \notin minted BY <3>1
    <4>3. base'[x] = base[x] /\ orig'[x] = orig[x] BY <4>2, <3>1, <1>ty DEF HistTy
    <4>. QED BY <4>2, <4>3, <1>c DEF Hist, HistNew
  <3>. QED BY <3>q1, <3>q2, <3>q3, <3>q4 DEF HistEv
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>. QED BY <1>1, <1>2

LEMMA PullOnly_M5 ==
  ASSUME IndM5, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY PullOnly_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF PullOnly, bucket, aux
<1>2. Wr(s, PullOnlyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. PullOnlyW(s).baseline = w[s].baseline BY DEF PullOnlyW
<1>4. \A p0 \in Paths : PullOnlyW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Claim_M5 ==
  ASSUME IndM5, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Claim_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Claim, bucket, aux
<1>2. Wr(s, ClaimW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. ClaimW(s).baseline = w[s].baseline BY DEF ClaimW
<1>4. \A p0 \in Paths : ClaimW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Verify_M5 ==
  ASSUME IndM5, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Verify_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Verify, bucket, aux
<1>2. Wr(s, VerifyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. VerifyW(s).baseline = w[s].baseline BY DEF VerifyW
<1>4. \A p0 \in Paths : VerifyW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Install_M5 ==
  ASSUME IndM5, NEW s \in Writers, Install(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Install_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Install, bucket, aux
<1>2. Wr(s, InstallW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. InstallW(s).baseline = w[s].baseline BY DEF InstallW
<1>4. \A p0 \in Paths : InstallW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Collect_M5 ==
  ASSUME IndM5, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Collect_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Collect, bucket, aux
<1>2. Wr(s, CollectW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. CollectW(s).baseline = w[s].baseline BY DEF CollectW
<1>4. \A p0 \in Paths : CollectW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Finish_M5 ==
  ASSUME IndM5, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Finish_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Finish, bucket, aux
<1>2. Wr(s, FinishW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. FinishW(s).baseline = [p0 \in Paths |-> IF p0 \in w[s].uploads \cap w[s].upDone THEN w[s].snap[p0]
                         ELSE IF p0 \in w[s].deletes /\ w[s].inst[p0] = Nil THEN Nil
                         ELSE w[s].baseline[p0]] BY DEF FinishW
<1>4. \A p0 \in Paths : FinishW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Restart_M5 ==
  ASSUME IndM5, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Restart_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Restart, bucket, aux
<1>2. Wr(s, RestartW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RestartW(s).baseline = w[s].baseline BY DEF RestartW
<1>4. \A p0 \in Paths : RestartW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Sync_M5 ==
  ASSUME IndM5, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Sync_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\ UNCHANGED <<minted, base, orig>> BY DEF Sync, bucket, aux
<1>2. Wr(s, SyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. SyncW(s, fail).baseline = SyncBl(s, fail) BY DEF SyncW
<1>4. \A p0 \in Paths : SyncW(s, fail).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF SyncBl
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RescopeBegin_M5 ==
  ASSUME IndM5, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RescopeBegin_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\ UNCHANGED <<minted, base, orig>> BY DEF RescopeBegin, bucket, aux
<1>2. Wr(s, RescopeBeginW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RescopeBeginW(s, T).baseline = w[s].baseline BY DEF RescopeBeginW
<1>4. \A p0 \in Paths : RescopeBeginW(s, T).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RescopeFirst_M5 ==
  ASSUME IndM5, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RescopeFirst_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF RescopeFirst, bucket, aux
<1>2. Wr(s, RescopeFirstW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RescopeFirstW(s).baseline = IF RescopeUnciteFirst
                THEN [p0 \in Paths |-> IF p0 \in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[p0]]
                ELSE w[s].baseline BY DEF RescopeFirstW
<1>4. \A p0 \in Paths : RescopeFirstW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RescopeSecond_M5 ==
  ASSUME IndM5, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RescopeSecond_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
      w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\ UNCHANGED <<minted, base, orig>>
    BY DEF RescopeSecond, bucket, aux
<1>2. Wr(s, RescopeSecondW(s, wfail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RescopeSecondW(s, wfail).baseline = [p0 \in Paths |-> IF p0 \in RescopeFetch(s, wfail) THEN doc[p0] ELSE RescopeBase1(s)[p0]] BY DEF RescopeSecondW
<1>4. \A p0 \in Paths : RescopeSecondW(s, wfail).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF RescopeBase1
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RPullRead_M5 ==
  ASSUME IndM5, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RPullRead_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF RPullRead, bucket, aux
<1>2. Wr(s, RPullReadW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RPullReadW(s).baseline = w[s].baseline BY DEF RPullReadW
<1>4. \A p0 \in Paths : RPullReadW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RPullSync_M5 ==
  ASSUME IndM5, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RPullSync_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\ UNCHANGED <<minted, base, orig>> BY DEF RPullSync, bucket, aux
<1>2. Wr(s, RPullSyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RPullSyncW(s, fail).baseline = SyncBl(s, fail) BY DEF RPullSyncW
<1>4. \A p0 \in Paths : RPullSyncW(s, fail).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF SyncBl
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

------------------------------------------------------------------------------
(* M5: the retire age and the reader's load.                                *)

LEMMA Age_M5 ==
  ASSUME IndM5, Age, UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY Age_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, Age, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

LEMMA Reap_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY Reap_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, Reap, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

LEMMA RLoad_M5 ==
  ASSUME IndM5, RLoad, UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY RLoad_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, RLoad, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

------------------------------------------------------------------------------
(* M5: the answer (row 15) and deposal (row 17): no baseline moves.        *)

LEMMA Touch_M5 ==
  ASSUME IndM5, NEW s \in Writers, Touch(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Touch_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Touch, bucket, aux
<1>2. Wr(s, TouchW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. TouchW(s).baseline = w[s].baseline BY DEF TouchW
<1>4. \A p0 \in Paths : TouchW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Honor_M5 ==
  ASSUME IndM5, NEW s \in Writers, Honor(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Honor_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Honor, bucket, aux
<1>2. Wr(s, HonorW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. HonorW(s).baseline = w[s].baseline BY DEF HonorW
<1>4. \A p0 \in Paths : HonorW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA HonorReader_M5 ==
  ASSUME IndM5, NEW s \in Writers, HonorReader(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY HonorReader_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF HonorReader, bucket, aux
<1>2. Wr(s, HonorReaderW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. HonorReaderW(s).baseline = w[s].baseline BY DEF HonorReaderW
<1>4. \A p0 \in Paths : HonorReaderW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RetireByAck_M5 ==
  ASSUME IndM5, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RetireByAck_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF RetireByAck, bucket, aux
<1>2. Wr(s, RetireByAckW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RetireByAckW(s).baseline = w[s].baseline BY DEF RetireByAckW
<1>4. \A p0 \in Paths : RetireByAckW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Depose_M5 ==
  ASSUME IndM5, NEW t \in Writers, Depose(t), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Depose_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<minted, base, orig>>
  BY DEF Depose, bucket, aux
<1>h. HistEv BY <1>1, <1>c, HistSame
<1>2. \A u \in Writers : w'[u].baseline = w[u].baseline BY <1>1, <1>y DEF DeposeW, DeposedW
<1>b. BlEv BY <1>2 DEF BlEv
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Fence_M5 ==
  ASSUME IndM5, NEW s \in Writers, Fence(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Fence_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. /\ \/ w' = [w EXCEPT ![s] = FenceBeforeW(s)]
         \/ w' = [w EXCEPT ![s] = FenceAfterW(s)]
      /\ UNCHANGED <<minted, base, orig>>
  BY DEF Fence, bucket, aux
<1>h. HistEv BY <1>1, <1>c, HistSame
<1>2. \A u \in Writers : w'[u].baseline = w[u].baseline BY <1>1, <1>y DEF FenceBeforeW, FenceAfterW
<1>b. BlEv BY <1>2 DEF BlEv
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

------------------------------------------------------------------------------
(* M5: the step, and the invariant.                                         *)

LEMMA Next_M5 == IndM5 /\ Next => IndM5'
<1>. SUFFICES ASSUME IndM5, Next PROVE IndM5' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM5' BY <3>1, GPut_M5
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM5' BY <3>2, GCas_M5
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM5' BY <3>3, GDelete_M5
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM5' BY <3>4, GRename_M5
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M5
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM5' BY <3>1, Checkout_M5
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM5' BY <3>2, Consume_M5
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM5' BY <3>3, Scan_M5
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM5' BY <3>4, Skip_M5
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM5' BY <3>5, PullOnly_M5
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM5' BY <3>6, Claim_M5
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM5' BY <3>7, Verify_M5
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM5' BY <3>8, Install_M5
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM5' BY <3>9, Collect_M5
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM5' BY <3>10, Finish_M5
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM5' BY <3>11, Restart_M5
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM5' BY <3>12, Sync_M5
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM5' BY <3>13, RescopeBegin_M5
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM5' BY <3>14, RescopeFirst_M5
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM5' BY <3>15, RescopeSecond_M5
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM5' BY <3>16, RPullRead_M5
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM5' BY <3>17, RPullSync_M5
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM5' BY <3>18, Edit_M5
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM5' BY <3>19, Delete_M5
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM5' BY <3>20, Upload_M5
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM5' BY <3>21, Sweep_M5
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE IndM5' BY <3>22, Touch_M5
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE IndM5' BY <3>23, Honor_M5
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE IndM5' BY <3>24, HonorReader_M5
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE IndM5' BY <3>25, RetireByAck_M5
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE IndM5' BY <3>26, Depose_M5
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE IndM5' BY <3>27, Fence_M5
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M5
  <2>2. CASE RLoad BY <2>2, RLoad_M5
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM5' BY <2>3, Reap_M5
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M5Invariant == Spec => []IndM5
<1>1. Init => IndM5 BY Init_M5
<1>2. IndM5 /\ [Next]_vars => IndM5'
  <2>1. IndM5 /\ Next => IndM5' BY Next_M5
  <2>2. IndM5 /\ UNCHANGED vars => IndM5'
    <3>1. IndM5 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM5 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM5, IndM4, IndM3, IndM2, IndM1, vars
    <3>3. IndM5 /\ UNCHANGED vars => M2' BY M2Same DEF IndM5, IndM4, IndM3, IndM2, vars, aux, ret
    <3>4. IndM5 /\ UNCHANGED vars => M3' BY M3Same DEF IndM5, IndM4, IndM3, vars, bucket
    <3>5. IndM5 /\ UNCHANGED vars => M4' BY M4Same DEF IndM5, IndM4, vars, ret
    <3>6. IndM5 /\ UNCHANGED vars => M5' BY HistAll DEF IndM5, M5, vars, bucket, aux
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6 DEF IndM5, IndM4, IndM3, IndM2, IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

==============================================================================
