--------------------------- MODULE LeanP1ProofM5D ----------------------------
EXTENDS LeanP1ProofM5C

------------------------------------------------------------------------------
(* M5, part D: AN5, Inv_AckedNamed restated (user's choice 2026-10-14;      *)
(* NOTES.txt E38-E41).  An acked <<p, h>> is accounted for along h's rename *)
(* chain as AN3 has it, or by a conflict on the chain naming a version      *)
(* derived from h (NR5's reading of a conflict), or by an acknowledged      *)
(* version with other bytes derived from h.  AN3 is FALSE (E38, E39): a     *)
(* copy's acked successor contested away or renamed on, the copy reaped.    *)
(* The claim is LeanP1.tla's AN5 (since 2026-10-14); `Derives` here is     *)
(* LeanP1Anc's.                                                             *)

\* Chain, AccountedAt, AccountedChain, ConfAt, Desc and AN5 are LeanP1.tla's
\* (2026-10-14), via LeanP1Anc.

------------------------------------------------------------------------------
(* The strengthening (MCLeanP1M5D.tla's J5D, TLC-checked: E41).  An acked   *)
(* version is cited; or an exact record names it -- a conflict, an          *)
(* acknowledged delete on its chain; or an acknowledged strict descendant   *)
(* does (a save over it); or a tree that took it on its chain has moved on  *)
(* (a commit over it from a clean baseline).  Each way a cited version      *)
(* stops being cited leaves one of these, and a tree takes it back only     *)
(* while it is cited.  No tombstone, no later-ack clause, no derivation     *)
(* through a conflict is needed.                                            *)

ChainH(h) == {h[1]} \cup {q \in Paths : <<q, h>> \in acked}
TookAt(q, h) == \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
KD(h) == \/ Cited(h) \/ Preserved(h) \/ Desc(h)
         \/ \E q \in ChainH(h) : <<q, h>> \in udel \/ TookAt(q, h)
KDall  == \A a \in acked : KD(a[2])
AckMin == \A a \in acked : a[2] \in minted
\* A cited version is at its own path, or was acknowledged there.
CHI == \A q \in Paths : doc[q] # Nil => doc[q][1] = q \/ <<q, doc[q]>> \in acked
\* A tree's memory has its baseline.
BT  == \A s \in Writers, p \in Paths : w[s].baseline[p] # Nil => Gen(w[s].baseline[p]) \in took[s][p]
\* In a barrier, an upload's bytes are not its baseline.
UDR(R) == R.pc \in Bar => \A p \in R.uploads : R.baseline[p] # Nil => R.local[p] # R.baseline[p]
UD  == \A s \in Writers : UDR(w[s])
DInv == AckMin /\ CHI /\ BT /\ UD /\ KDall

\* What a step's trees may now hold: what they held, the document's, nothing,
\* or a handle minted by this step.
LocalOK == \A t \in Writers, q \in Paths :
             \/ w'[t].local[q] = w[t].local[q] \/ w'[t].local[q] = doc[q]
             \/ w'[t].local[q] = Nil \/ w'[t].local[q] \notin minted
\* The trees' memory only grows, and holds every new baseline.
TookOK == \A t \in Writers, q \in Paths :
            /\ took[t][q] \subseteq took'[t][q]
            /\ (w'[t].baseline[q] # Nil /\ w'[t].baseline[q] # w[t].baseline[q])
                 => Gen(w'[t].baseline[q]) \in took'[t][q]

------------------------------------------------------------------------------
(* The claim from the strengthening.                                       *)

LEMMA KDToAN5 == IndC /\ DInv => AN5
<1>. SUFFICES ASSUME IndC, DInv, NEW a \in acked
              PROVE  \/ AccountedChain(a[1], a[2]) \/ Desc(a[2])
                     \/ \E q \in Chain(a[1], a[2]) : ConfAt(q, a[2])
  BY DEF AN5
<1>ty. acked \subseteq Paths \X Handles /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>0. PICK p \in Paths, h \in Handles : a = <<p, h>> BY <1>ty
<1>n. h # Nil BY NilHandle
<1>d. Derives(h, h) BY DEF Derives
<1>c. ChainH(h) \subseteq Chain(p, h) BY <1>0 DEF ChainH, Chain
<1>k. KD(h) BY <1>0 DEF DInv, KDall
<1>. SUFFICES AccountedAt(p, h, Chain(p, h)) \/ Desc(h) BY <1>0 DEF AccountedChain
<1>1. CASE Cited(h)
  <2>1. PICK q \in Paths : doc[q] = h BY <1>1 DEF Cited
  <2>2. h[1] = q \/ <<q, h>> \in acked BY <2>1, <1>n DEF DInv, CHI
  <2>3. q \in Chain(p, h) BY <2>2 DEF Chain
  <2>. QED BY <2>1, <2>3, <1>d, <1>n DEF AccountedAt
<1>2. CASE Preserved(h) BY <1>2 DEF AccountedAt
<1>3. CASE Desc(h) BY <1>3
<1>4. CASE \E q \in ChainH(h) : <<q, h>> \in udel
  <2>1. PICK q \in ChainH(h) : <<q, h>> \in udel BY <1>4
  <2>2. \E d \in udel : d[1] = q /\ Derives(d[2], h) BY <2>1, <1>d
  <2>. QED BY <2>1, <2>2, <1>c DEF AccountedAt
<1>5. CASE \E q \in ChainH(h) : TookAt(q, h) BY <1>5, <1>c DEF AccountedAt, TookAt
<1>. QED BY <1>k, <1>1, <1>2, <1>3, <1>4, <1>5 DEF KD

------------------------------------------------------------------------------
(* Generic steps.                                                           *)

LEMMA TookUpd ==
  ASSUME TypeOK, TookUpdate PROVE TookOK
BY DEF TookUpdate, TookOK, TypeOK

LEMMA TookQuiet ==
  ASSUME TypeOK, took' = took, w' = w PROVE TookOK
BY DEF TookOK

LEMMA ContentKeep ==
  ASSUME HStep, NEW m \in minted PROVE Content(m)' = Content(m)
BY DEF HStep, HistEv, Content

LEMMA DescKeep ==
  ASSUME IndC, DInv, HStep, acked \subseteq acked', NEW h \in minted, Desc(h)
  PROVE  Desc(h)'
<1>1. PICK b \in acked : Derives(b[2], h) /\ Content(b[2]) # Content(h) BY DEF Desc
<1>2. b[2] \in minted BY <1>1 DEF DInv, AckMin
<1>t. minted \subseteq Handles BY MintedHandles DEF IndC
<1>3. Derives(b[2], h)' BY <1>1, <1>2, <1>t, DerivesStep DEF IndC, Opt
<1>4. Content(b[2])' = Content(b[2]) /\ Content(h)' = Content(h) BY <1>2, ContentKeep
<1>. QED BY <1>1, <1>3, <1>4 DEF Desc

\* A step keeps an acked version's name, given what it does to the
\* citation and to the trees.
LEMMA KDStep ==
  ASSUME IndC, DInv, HStep, conflicts \subseteq conflicts', udel \subseteq udel', acked \subseteq acked',
         TookOK, NEW h \in minted, KD(h),
         Cited(h) => KD(h)',
         \A t \in Writers, q \in Paths : w[t].local[q] # h /\ w'[t].local[q] = h => KD(h)'
  PROVE  KD(h)'
<1>c. ChainH(h) \subseteq ChainH(h)' BY DEF ChainH
<1>t. h \in Handles /\ h[1] \in Paths BY MintedHandles DEF IndC, Handles
<1>1. CASE Cited(h) BY <1>1
<1>2. CASE Preserved(h) BY <1>2 DEF KD, Preserved
<1>3. CASE Desc(h) BY <1>3, DescKeep DEF KD
<1>4. CASE \E q \in ChainH(h) : <<q, h>> \in udel BY <1>4, <1>c DEF KD
<1>5. CASE \E q \in ChainH(h) : TookAt(q, h)
  <2>1. PICK q \in ChainH(h) : TookAt(q, h) BY <1>5
  <2>2. PICK t \in Writers : Gen(h) \in took[t][q] /\ w[t].local[q] # h BY <2>1 DEF TookAt
  <2>q. q \in Paths BY <1>t DEF ChainH
  <2>3. CASE w'[t].local[q] = h BY <2>2, <2>3, <2>q
  <2>4. CASE w'[t].local[q] # h
    <3>1. Gen(h) \in took'[t][q] BY <2>2, <2>q DEF TookOK
    <3>2. TookAt(q, h)' BY <3>1, <2>4 DEF TookAt
    <3>. QED BY <3>2, <2>1, <1>c DEF KD
  <2>. QED BY <2>3, <2>4
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF KD

LEMMA BTKeep ==
  ASSUME BT, TookOK PROVE BT'
<1>. SUFFICES ASSUME NEW s \in Writers, NEW p \in Paths, w'[s].baseline[p] # Nil
              PROVE  Gen(w'[s].baseline[p]) \in took'[s][p]
  BY DEF BT
<1>1. CASE w'[s].baseline[p] = w[s].baseline[p] BY <1>1 DEF BT, TookOK
<1>2. CASE w'[s].baseline[p] # w[s].baseline[p] BY <1>2 DEF TookOK
<1>. QED BY <1>1, <1>2

\* A step that keeps the document and the records: the conjuncts follow
\* from what it does to the trees.
LEMMA DCore ==
  ASSUME IndC, IndC', DInv, HStep, UNCHANGED <<doc, acked, conflicts, udel>>,
         TookOK, LocalOK, UD'
  PROVE  DInv'
<1>m. minted \subseteq minted' BY DEF HStep, HistEv
<1>1. AckMin' BY <1>m DEF DInv, AckMin
<1>2. CHI' BY DEF DInv, CHI
<1>3. BT' BY BTKeep DEF DInv
<1>4. KDall'
  <2>. SUFFICES ASSUME NEW a \in acked PROVE KD(a[2])' BY DEF KDall
  <2>h. a[2] \in minted BY DEF DInv, AckMin
  <2>k. KD(a[2]) BY DEF DInv, KDall
  <2>n. a[2] # Nil BY <2>h, NilHandle, MintedHandles DEF IndC
  <2>1. Cited(a[2]) => KD(a[2])' BY DEF Cited, KD
  <2>2. \A t \in Writers, q \in Paths : w[t].local[q] # a[2] /\ w'[t].local[q] = a[2] => KD(a[2])'
    <3>. SUFFICES ASSUME NEW t \in Writers, NEW q \in Paths, w[t].local[q] # a[2], w'[t].local[q] = a[2]
                  PROVE  KD(a[2])'
      OBVIOUS
    <3>1. doc[q] = a[2] BY <2>h, <2>n, <1>m DEF LocalOK
    <3>. QED BY <3>1 DEF KD, Cited
  <2>. QED BY <2>h, <2>k, <2>1, <2>2, KDStep
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF DInv

\* A step that writes no tree.
LEMMA DNoTree ==
  ASSUME IndC, IndC', DInv, HStep, UNCHANGED <<doc, acked, conflicts, udel>>, w' = w, TookOK
  PROVE  DInv'
<1>1. LocalOK BY DEF LocalOK
<1>2. UD' BY DEF DInv, UD
<1>. QED BY <1>1, <1>2, DCore

\* A step that writes tree s only (as the record R).
LEMMA DTree ==
  ASSUME IndC, IndC', DInv, HStep, UNCHANGED <<doc, acked, conflicts, udel>>, TookOK,
         NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         \A q \in Paths : \/ R.local[q] = w[s].local[q] \/ R.local[q] = doc[q]
                          \/ R.local[q] = Nil \/ R.local[q] \notin minted,
         UDR(R)
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY <1>0, WriteAny
<1>1. LocalOK BY <1>w DEF LocalOK
<1>2. UD' BY <1>w DEF DInv, UD
<1>. QED BY <1>1, <1>2, DCore

\* A record keeping the fields UD reads keeps UD.
LEMMA UDSame ==
  ASSUME DInv, NEW s \in Writers, NEW R,
         R.local = w[s].local, R.baseline = w[s].baseline, R.uploads = w[s].uploads,
         R.pc \in Bar => w[s].pc \in Bar
  PROVE  UDR(R)
BY DEF DInv, UD, UDR

LEMMA HStepQ ==
  ASSUME IndC, IndC', UNCHANGED <<minted, base, orig>>, AncUpdate PROVE HStep
BY HStepSame DEF IndC

------------------------------------------------------------------------------
(* Part D: the gateway.                                                     *)

LEMMA GPut_D ==
  ASSUME IndC, IndC', DInv, NEW p \in Paths, GPut(p), Frame
  PROVE  DInv'
<1>m. IndM5' BY DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. DEFINE h == <<p, nextGen>>
<1>1. /\ minted' = minted \cup {h} /\ base' = [base EXCEPT ![h] = doc[p]] /\ orig' = orig
      /\ nextGen <= MaxMint /\ nextGen' = nextGen + 1
      /\ w' = w /\ UNCHANGED <<doc, conflicts, udel, acked>>
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
<1>t. TookOK BY <1>0, TookUpd DEF Frame
<1>. QED BY <1>1, <1>hs, <1>t, DNoTree

LEMMA Sweep_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. UNCHANGED <<minted, base, orig, conflicts, udel, acked, doc>> /\ w' = w BY DEF Sweep, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>t. TookOK BY <1>0, TookUpd DEF Frame
<1>. QED BY <1>1, <1>hs, <1>t, DNoTree

LEMMA GRenameFinish_D ==
  ASSUME IndC, GRenameFinish PROVE DInv'
<1>1. mv = Nil BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

\* A save lands over the version it read: that version gains an acknowledged
\* strict descendant.
LEMMA GCas_D ==
  ASSUME IndC, IndC', DInv, NEW p \in Paths, GCas(p), Frame
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. UNCHANGED <<minted, base, orig, conflicts, udel>> /\ w' = w BY DEF GCas, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>t. minted \subseteq Handles BY MintedHandles DEF IndC
<1>2. PICK g \in minted : g = gw[p]
  BY DEF GCas, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted, Opt
<1>dt. doc \in [Paths -> Opt(Handles)] BY <1>0 DEF TypeOK
<1>3. CASE doc[p] # base[g]
  <2>1. doc' = doc /\ acked' = acked BY <1>3, <1>2, ShippedShape DEF GCas, Shipped
  <2>. QED BY <2>1, <1>1, <1>hs, <1>tk, DNoTree
<1>4. CASE doc[p] = base[g]
  <2>1. doc' = [doc EXCEPT ![p] = g] /\ acked' = acked \cup {<<p, g>>} BY <1>4, <1>2 DEF GCas
  <2>d. \A q \in Paths : doc'[q] = IF q = p THEN g ELSE doc[q] BY <2>1, <1>dt
  <2>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
  <2>a. AckMin' BY <2>1, <2>m DEF DInv, AckMin
  <2>gn. g # Nil /\ g[1] = p
    <3>1. gw[p] # Nil /\ gw[p][1] = p BY DEF GCas, IndC, IndM5, IndM4, IndM3, IndM2, M2, Flight
    <3>. QED BY <3>1, <1>2
  <2>b. CHI' BY <2>1, <2>d, <2>gn DEF DInv, CHI
  <2>c. BT' BY <1>tk, BTKeep DEF DInv
  <2>u. UD' BY <1>1 DEF DInv, UD
  <2>k. KDall'
    <3>. SUFFICES ASSUME NEW a \in acked' PROVE KD(a[2])' BY DEF KDall
    <3>1. CASE a = <<p, g>>
      <4>1. doc'[p] = g BY <2>d
      <4>. QED BY <4>1, <3>1 DEF KD, Cited
    <3>2. CASE a \in acked
      <4>h. a[2] \in minted BY <3>2 DEF DInv, AckMin
      <4>k. KD(a[2]) BY <3>2 DEF DInv, KDall
      <4>n. a[2] # Nil BY <4>h, <1>t, NilHandle
      <4>1. Cited(a[2]) => KD(a[2])'
        <5>. SUFFICES ASSUME NEW q \in Paths, doc[q] = a[2] PROVE KD(a[2])' BY DEF Cited
        <5>1. CASE q # p BY <5>1, <2>d DEF KD, Cited
        <5>2. CASE q = p
          <6>. DEFINE h == a[2]
          <6>1. base[g] = h BY <5>2, <1>4
          <6>2. anc[g] = {h} \cup anc[h] BY <6>1, <4>n, <1>t DEF IndC, IndM5, M5, Hist, HistAnc, AncOf
          <6>3. Derives(h, h) BY DEF Derives
          <6>4. Derives(g, h) BY <6>2, <6>3, <4>h, <1>t, DerivesBase
          <6>5a. IndM5 /\ g \in Opt(Handles) BY <1>2, <1>t DEF IndC, Opt
          <6>5. Derives(g, h)' BY <6>4, <6>5a, <4>h, <1>hs, DerivesStep
          <6>6a. h \in anc[g] BY <6>2, Zenon
          <6>6. CR(h) < CR(g) BY <6>6a, <1>2 DEF IndC, HistC, RankC
          <6>7. Content(g) # Content(h) BY <6>6 DEF CR
          <6>8. Content(g)' = Content(g) /\ Content(h)' = Content(h) BY <4>h, ContentKeep, <1>hs
          <6>9. <<p, g>> \in acked' BY <2>1
          <6>10. Desc(h)' BY <6>5, <6>7, <6>8, <6>9 DEF Desc
          <6>. QED BY <6>10 DEF KD
        <5>. QED BY <5>1, <5>2
      <4>2. \A t \in Writers, q \in Paths : w[t].local[q] # a[2] /\ w'[t].local[q] = a[2] => KD(a[2])'
        BY <1>1
      <4>. QED BY <4>h, <4>k, <4>1, <4>2, <1>1, <1>hs, <1>tk, <2>1, KDStep
    <3>. QED BY <3>1, <3>2, <2>1
  <2>. QED BY <2>a, <2>b, <2>c, <2>u, <2>k DEF DInv
<1>. QED BY <1>3, <1>4

\* A delete is acknowledged at the path it cleared.
LEMMA GDelete_D ==
  ASSUME IndC, IndC', DInv, NEW p \in Paths, GDelete(p), Frame
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ UNCHANGED <<minted, base, orig, conflicts, acked>> /\ w' = w
      /\ doc' = [doc EXCEPT ![p] = Nil] /\ udel' = udel \cup {<<p, doc[p]>>}
  BY DEF GDelete, aux
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>t. minted \subseteq Handles BY MintedHandles DEF IndC
<1>dt. doc \in [Paths -> Opt(Handles)] BY <1>0 DEF TypeOK
<1>d. \A q \in Paths : doc'[q] = IF q = p THEN Nil ELSE doc[q] BY <1>1, <1>dt
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>a. AckMin' BY <1>1, <1>m DEF DInv, AckMin
<1>b. CHI' BY <1>1, <1>d DEF DInv, CHI
<1>c. BT' BY <1>tk, BTKeep DEF DInv
<1>u. UD' BY <1>1 DEF DInv, UD
<1>k. KDall'
  <2>. SUFFICES ASSUME NEW a \in acked PROVE KD(a[2])' BY <1>1 DEF KDall
  <2>h. a[2] \in minted BY DEF DInv, AckMin
  <2>k. KD(a[2]) BY DEF DInv, KDall
  <2>n. a[2] # Nil BY <2>h, <1>t, NilHandle
  <2>1. Cited(a[2]) => KD(a[2])'
    <3>. SUFFICES ASSUME NEW q \in Paths, doc[q] = a[2] PROVE KD(a[2])' BY DEF Cited
    <3>1. CASE q # p BY <3>1, <1>d DEF KD, Cited
    <3>2. CASE q = p
      <4>1. a[2][1] = p \/ <<p, a[2]>> \in acked BY <3>2, <2>n DEF DInv, CHI
      <4>2. p \in ChainH(a[2])' BY <4>1, <1>1 DEF ChainH
      <4>3. <<p, a[2]>> \in udel' BY <3>2, <1>1
      <4>. QED BY <4>2, <4>3 DEF KD
    <3>. QED BY <3>1, <3>2
  <2>2. \A t \in Writers, q \in Paths : w[t].local[q] # a[2] /\ w'[t].local[q] = a[2] => KD(a[2])'
    BY <1>1
  <2>. QED BY <2>h, <2>k, <2>1, <2>2, <1>1, <1>hs, <1>tk, KDStep
<1>. QED BY <1>a, <1>b, <1>c, <1>u, <1>k DEF DInv

\* A rename keeps the version cited, and acknowledges it at its new path.
LEMMA GRename_D ==
  ASSUME IndC, IndC', DInv, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>z. p # q /\ doc[p] # Nil /\ doc[q] = Nil BY DEF GRename
<1>1. /\ UNCHANGED <<minted, base, orig, conflicts, udel>> /\ w' = w
      /\ doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil] /\ acked' = acked \cup {<<q, doc[p]>>}
  BY ShippedShape DEF GRename, aux, Shipped
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>t. minted \subseteq Handles BY MintedHandles DEF IndC
<1>dt. doc \in [Paths -> Opt(Handles)] BY <1>0 DEF TypeOK
<1>d. \A r \in Paths : doc'[r] = IF r = q THEN doc[p] ELSE IF r = p THEN Nil ELSE doc[r] BY <1>1, <1>dt, <1>z
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>dm. doc[p] \in minted BY <1>z DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted, Opt
<1>a. AckMin' BY <1>1, <1>m, <1>dm DEF DInv, AckMin
<1>b. CHI' BY <1>1, <1>d DEF DInv, CHI
<1>c. BT' BY <1>tk, BTKeep DEF DInv
<1>u. UD' BY <1>1 DEF DInv, UD
<1>k. KDall'
  <2>. SUFFICES ASSUME NEW a \in acked' PROVE KD(a[2])' BY DEF KDall
  <2>1. CASE a = <<q, doc[p]>>
    <3>1. doc'[q] = doc[p] BY <1>d
    <3>. QED BY <3>1, <2>1 DEF KD, Cited
  <2>2. CASE a \in acked
    <3>h. a[2] \in minted BY <2>2 DEF DInv, AckMin
    <3>k. KD(a[2]) BY <2>2 DEF DInv, KDall
    <3>n. a[2] # Nil BY <3>h, <1>t, NilHandle
    <3>1. Cited(a[2]) => KD(a[2])'
      <4>. SUFFICES ASSUME NEW r \in Paths, doc[r] = a[2] PROVE KD(a[2])' BY DEF Cited
      <4>1. CASE r = p
        <5>1. doc'[q] = a[2] BY <4>1, <1>d
        <5>. QED BY <5>1 DEF KD, Cited
      <4>2. CASE r = q BY <4>2, <1>z, <3>n
      <4>3. CASE r # p /\ r # q BY <4>3, <1>d DEF KD, Cited
      <4>. QED BY <4>1, <4>2, <4>3
    <3>2. \A t \in Writers, r \in Paths : w[t].local[r] # a[2] /\ w'[t].local[r] = a[2] => KD(a[2])'
      BY <1>1
    <3>. QED BY <3>h, <3>k, <3>1, <3>2, <1>1, <1>hs, <1>tk, KDStep
  <2>. QED BY <2>1, <2>2, <1>1
<1>. QED BY <1>a, <1>b, <1>c, <1>u, <1>k DEF DInv

------------------------------------------------------------------------------
(* Part D: the commit.  What it uncites: its own delete (acknowledged), a   *)
(* version it contests (preserved), or its clean baseline -- the tree took  *)
(* that and its bytes are no longer it.                                     *)

LEMMA Install_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Install(s), Frame
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. DEFINE I == InstallInst(s)
<1>1. /\ w' = [w EXCEPT ![s] = InstallW(s)] /\ UNCHANGED <<minted, base, orig, acked>> /\ doc' = I
      /\ conflicts' = conflicts \cup {<<p, w[s].snap[p]>> : p \in w[s].gone}
                                \cup {<<p, doc[p]>> : p \in InstallContested(s)}
                                \cup {<<p, DeletedAt(s, p)>> : p \in InstallDelOverridden(s)}
                                \cup {<<p, doc[p]>> : p \in InstallDelOver(s)}
      /\ udel' = udel \cup {<<p, doc[p]>> : p \in {q \in Paths : doc[q] # Nil /\ I[q] = Nil}}
  BY DEF Install, aux
<1>pc. w[s].pc = "claimed" BY DEF Install
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>t. minted \subseteq Handles BY MintedHandles DEF IndC
<1>y. /\ w[s] \in Writer /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths
      /\ \A q \in Paths : w[s].baseline[q] \in Opt(minted)
  BY TreeTy DEF IndC
<1>sh. CommitSurfacesForeign /\ DeleteWinsPreserved BY ShippedShape DEF Shipped
<1>w. \A t \in Writers : w'[t] = IF t = s THEN InstallW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>r. /\ w'[s].local = w[s].local /\ w'[s].baseline = w[s].baseline /\ w'[s].uploads = w[s].uploads
      /\ w'[s].pc = "cased"
  BY <1>w DEF InstallW
\* What the merge installs at each path.
<1>i. \A q \in Paths : \/ I[q] = doc[q] \/ I[q] = Nil
                       \/ q \in InstallMine(s) \ w[s].gone /\ I[q] = w[s].snap[q]
  BY DEF InstallInst
<1>m. minted \subseteq minted' BY <1>hs DEF HStep, HistEv
<1>a. AckMin' BY <1>1, <1>m DEF DInv, AckMin
<1>b. CHI'
  <2>. SUFFICES ASSUME NEW q \in Paths, I[q] # Nil PROVE I[q][1] = q \/ <<q, I[q]>> \in acked
    BY <1>1 DEF CHI
  <2>1. CASE I[q] = doc[q] BY <2>1 DEF DInv, CHI
  <2>2. CASE q \in InstallMine(s) \ w[s].gone /\ I[q] = w[s].snap[q]
    <3>1. w[s].snap[q][1] = q BY <2>2, <1>pc DEF InstallMine, IndC, IndM5, IndM4, IndM3, IndM2, M2, Uploaded
    <3>. QED BY <3>1, <2>2
  <2>. QED BY <1>i, <2>1, <2>2
<1>c. BT' BY <1>tk, BTKeep DEF DInv
<1>u. UD'
  <2>1. UDR(InstallW(s)) BY <1>r, <1>w, <1>pc, UDSame DEF Bar
  <2>. QED BY <2>1, <1>w DEF DInv, UD
<1>k. KDall'
  <2>. SUFFICES ASSUME NEW a \in acked PROVE KD(a[2])' BY <1>1 DEF KDall
  <2>. DEFINE h == a[2]
  <2>h. h \in minted BY DEF DInv, AckMin
  <2>k. KD(h) BY DEF DInv, KDall
  <2>n. h # Nil BY <2>h, <1>t, NilHandle
  <2>g. conflicts \subseteq conflicts' /\ udel \subseteq udel' BY <1>1
  <2>1. Cited(h) => KD(h)'
    <3>. SUFFICES ASSUME NEW q \in Paths, doc[q] = h PROVE KD(h)' BY DEF Cited
    <3>ch. q \in ChainH(h)' BY <2>n, <1>1 DEF DInv, CHI, ChainH
    <3>1. CASE I[q] = doc[q] BY <3>1, <1>1 DEF KD, Cited
    <3>2. CASE I[q] = Nil
      <4>1. <<q, h>> \in udel' BY <3>2, <2>n, <1>1
      <4>. QED BY <4>1, <3>ch DEF KD
    <3>3. CASE I[q] # doc[q] /\ I[q] # Nil
      <4>0. q \in InstallMine(s) \ w[s].gone BY <3>3, <1>i
      <4>1. CASE Foreign(s, q)
        <5>1. q \in InstallContested(s) BY <4>0, <4>1, <2>n, <1>sh DEF InstallContested
        <5>2. <<q, h>> \in conflicts' BY <5>1, <1>1
        <5>. QED BY <5>2 DEF KD, Preserved
      <4>2. CASE ~Foreign(s, q)
        <5>1. w[s].baseline[q] = h BY <4>2 DEF Foreign
        <5>2. Gen(h) \in took[s][q] BY <5>1, <2>n DEF DInv, BT
        <5>3. Gen(h) \in took'[s][q] BY <5>2, <1>tk DEF TookOK
        <5>4. q \in w[s].uploads BY <4>0 DEF InstallMine
        <5>5. w[s].local[q] # h BY <5>1, <5>4, <2>n, <1>pc DEF DInv, UD, UDR, Bar
        <5>6. w'[s].local[q] # h BY <5>5, <1>r
        <5>7. TookAt(q, h)' BY <5>3, <5>6 DEF TookAt
        <5>. QED BY <5>7, <3>ch DEF KD
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <3>1, <3>2, <3>3
  <2>2. \A t \in Writers, q \in Paths : w[t].local[q] # h /\ w'[t].local[q] = h => KD(h)'
    BY <1>w, <1>r
  <2>. QED BY <2>h, <2>k, <2>1, <2>2, <2>g, <1>1, <1>hs, <1>tk, KDStep
<1>. QED BY <1>a, <1>b, <1>c, <1>u, <1>k DEF DInv

------------------------------------------------------------------------------
(* Part D: the other tree steps.  Each writes one tree, taking the          *)
(* document's version, nothing, or a handle it mints.                       *)

\* The shape of a writer step with no history written.
WStep(s, R) ==
  /\ w' = [w EXCEPT ![s] = R]
  /\ UNCHANGED <<minted, base, orig, doc, acked, conflicts, udel>>

LEMMA DWriter ==
  ASSUME IndC, IndC', DInv, Frame, NEW s \in Writers, NEW R, WStep(s, R),
         \A q \in Paths : \/ R.local[q] = w[s].local[q] \/ R.local[q] = doc[q]
                          \/ R.local[q] = Nil \/ R.local[q] \notin minted,
         UDR(R)
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>hs. HStep BY HStepQ DEF Frame, WStep
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>. QED BY <1>hs, <1>tk, DTree DEF WStep

LEMMA Claim_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Claim(s), Frame PROVE DInv'
<1>1. WStep(s, ClaimW(s)) BY DEF Claim, WStep, aux
<1>2. UDR(ClaimW(s)) BY UDSame DEF ClaimW, Claim, Bar
<1>. QED BY <1>1, <1>2, DWriter DEF ClaimW

LEMMA Verify_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Verify(s), Frame PROVE DInv'
<1>1. WStep(s, VerifyW(s)) BY DEF Verify, WStep, bucket, aux
<1>2. UDR(VerifyW(s)) BY UDSame DEF VerifyW
<1>. QED BY <1>1, <1>2, DWriter DEF VerifyW

LEMMA Collect_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Collect(s), Frame PROVE DInv'
<1>1. WStep(s, CollectW(s)) BY DEF Collect, WStep, aux
<1>2. UDR(CollectW(s)) BY UDSame DEF CollectW
<1>. QED BY <1>1, <1>2, DWriter DEF CollectW

LEMMA Skip_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Skip(s), Frame PROVE DInv'
<1>1. WStep(s, SkipW(s)) BY DEF Skip, WStep, bucket, aux
<1>2. UDR(SkipW(s)) BY DEF UDR, SkipW, Bar
<1>. QED BY <1>1, <1>2, DWriter DEF SkipW

LEMMA PullOnly_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, PullOnly(s), Frame PROVE DInv'
<1>1. WStep(s, PullOnlyW(s)) BY DEF PullOnly, WStep, bucket, aux
<1>2. UDR(PullOnlyW(s)) BY DEF UDR, PullOnlyW, Bar
<1>. QED BY <1>1, <1>2, DWriter DEF PullOnlyW

LEMMA Finish_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Finish(s), Frame PROVE DInv'
<1>1. WStep(s, FinishW(s)) BY DEF Finish, WStep, aux
<1>2. UDR(FinishW(s)) BY DEF UDR, FinishW, Bar
<1>. QED BY <1>1, <1>2, DWriter DEF FinishW

LEMMA Restart_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Restart(s), Frame PROVE DInv'
<1>1. WStep(s, RestartW(s)) BY DEF Restart, WStep, aux
<1>2. UDR(RestartW(s)) BY DEF UDR, RestartW, Bar
<1>. QED BY <1>1, <1>2, DWriter DEF RestartW

LEMMA RescopeBegin_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, RescopeBegin(s), Frame PROVE DInv'
<1>1. PICK T \in Scopes \ {w[s].scope} : WStep(s, RescopeBeginW(s, T)) BY DEF RescopeBegin, WStep, bucket, aux
<1>2. UDR(RescopeBeginW(s, T)) BY DEF UDR, RescopeBeginW, RescopeBegin, Bar
<1>. QED BY <1>1, <1>2, DWriter DEF RescopeBeginW

LEMMA RPullRead_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, RPullRead(s), Frame PROVE DInv'
<1>1. WStep(s, RPullReadW(s)) BY DEF RPullRead, WStep, bucket, aux
<1>2. UDR(RPullReadW(s)) BY DEF UDR, RPullReadW, Bar
<1>. QED BY <1>1, <1>2, DWriter DEF RPullReadW

LEMMA RescopeFirst_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, RescopeFirst(s), Frame PROVE DInv'
<1>sh. RescopeUnciteFirst BY ShippedShape DEF Shipped
<1>1. WStep(s, RescopeFirstW(s)) BY DEF RescopeFirst, WStep, bucket, aux
<1>2. RescopeFirstW(s).local = w[s].local BY <1>sh DEF RescopeFirstW
<1>3. UDR(RescopeFirstW(s)) BY DEF UDR, RescopeFirstW, RescopeFirst, Bar
<1>. QED BY <1>1, <1>2, <1>3, DWriter

LEMMA Checkout_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Checkout(s), Frame PROVE DInv'
<1>1. PICK T \in Scopes : WStep(s, CheckoutW(s, T)) BY DEF Checkout, WStep, bucket, aux
<1>0. w[s].pc = "idle"
  <2>1. w[s] = WriterInit BY DEF Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
  <2>. QED BY <2>1 DEF WriterInit
<1>2. UDR(CheckoutW(s, T)) BY <1>0 DEF UDR, CheckoutW, Bar
<1>3. \A q \in Paths : CheckoutW(s, T).local[q] = doc[q] \/ CheckoutW(s, T).local[q] = Nil
  BY DEF CheckoutW, CheckoutHeld
<1>. QED BY <1>1, <1>2, <1>3, DWriter

LEMMA Consume_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Consume(s), Frame PROVE DInv'
<1>k. UNCHANGED <<minted, base, orig, doc, acked, conflicts, udel>> BY DEF Consume, bucket, aux
<1>1. CASE CheapPath(s)
  <2>1. WStep(s, ConsumeCheapW(s)) BY <1>1, <1>k DEF Consume, WStep
  <2>2. UDR(ConsumeCheapW(s)) BY DEF UDR, ConsumeCheapW, Bar
  <2>. QED BY <2>1, <2>2, DWriter DEF ConsumeCheapW
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : WStep(s, ConsumeW(s, fail)) BY <1>2, <1>k DEF Consume, WStep
  <2>2. UDR(ConsumeW(s, fail)) BY DEF UDR, ConsumeW, Bar
  <2>3. \A q \in Paths : ConsumeW(s, fail).local[q] = w[s].local[q] \/ ConsumeW(s, fail).local[q] = doc[q]
    BY DEF ConsumeW
  <2>. QED BY <2>1, <2>2, <2>3, DWriter
<1>. QED BY <1>1, <1>2

LEMMA Sync_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Sync(s), Frame PROVE DInv'
<1>1. PICK fail \in SUBSET SyncAll(s) : WStep(s, SyncW(s, fail)) BY DEF Sync, WStep, bucket, aux
<1>2. UDR(SyncW(s, fail)) BY DEF UDR, SyncW, Sync, Bar
<1>3. \A q \in Paths : SyncW(s, fail).local[q] = w[s].local[q] \/ SyncW(s, fail).local[q] = doc[q]
  BY DEF SyncW
<1>. QED BY <1>1, <1>2, <1>3, DWriter

LEMMA RPullSync_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, RPullSync(s), Frame PROVE DInv'
<1>1. PICK fail \in SUBSET SyncAll(s) : WStep(s, RPullSyncW(s, fail)) BY DEF RPullSync, WStep, bucket, aux
<1>2. UDR(RPullSyncW(s, fail)) BY DEF UDR, RPullSyncW, Bar
<1>3. \A q \in Paths : RPullSyncW(s, fail).local[q] = w[s].local[q] \/ RPullSyncW(s, fail).local[q] = doc[q]
  BY DEF RPullSyncW
<1>. QED BY <1>1, <1>2, <1>3, DWriter

LEMMA RescopeSecond_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, RescopeSecond(s), Frame PROVE DInv'
<1>sh. RescopeUnciteFirst BY ShippedShape DEF Shipped
<1>1. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        WStep(s, RescopeSecondW(s, wfail))
  BY DEF RescopeSecond, WStep, bucket, aux
<1>2. UDR(RescopeSecondW(s, wfail)) BY DEF UDR, RescopeSecondW, RescopeSecond, Bar
<1>3. \A q \in Paths : \/ RescopeSecondW(s, wfail).local[q] = w[s].local[q]
                       \/ RescopeSecondW(s, wfail).local[q] = doc[q]
                       \/ RescopeSecondW(s, wfail).local[q] = Nil
  <2>. SUFFICES ASSUME NEW q \in Paths
                PROVE  \/ RescopeSecondW(s, wfail).local[q] = w[s].local[q]
                       \/ RescopeSecondW(s, wfail).local[q] = doc[q]
                       \/ RescopeSecondW(s, wfail).local[q] = Nil
    OBVIOUS
  <2>1. RescopeLocal1(s)[q] = w[s].local[q] \/ RescopeLocal1(s)[q] = Nil BY <1>sh DEF RescopeLocal1
  <2>. QED BY <2>1 DEF RescopeSecondW
<1>. QED BY <1>1, <1>2, <1>3, DWriter

LEMMA Scan_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Scan(s), Frame PROVE DInv'
<1>1. PICK dels \in SUBSET ScanAbsent(s) : WStep(s, ScanW(s, dels)) BY DEF Scan, WStep, bucket, aux
<1>2. UDR(ScanW(s, dels)) BY DEF UDR, ScanW, ScanUps, ScanDirty
<1>. QED BY <1>1, <1>2, DWriter DEF ScanW

LEMMA Delete_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame PROVE DInv'
<1>y. w[s] \in Writer BY TreeTy DEF IndC
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
<1>1. WStep(s, DeleteW(s, p)) BY DEF Delete, WStep, bucket, aux
<1>l. \A q \in Paths : DeleteW(s, p).local[q] = IF q = p THEN Nil ELSE w[s].local[q] BY <1>lt DEF DeleteW
<1>2. UDR(DeleteW(s, p))
  <2>. SUFFICES ASSUME w[s].pc \in Bar, NEW q \in w[s].uploads, w[s].baseline[q] # Nil
                PROVE  DeleteW(s, p).local[q] # w[s].baseline[q]
    BY DEF UDR, DeleteW
  <2>q. q \in Paths BY TreeTy DEF IndC
  <2>1. CASE q = p BY <2>1, <2>q, <1>l
  <2>2. CASE q # p BY <2>2, <2>q, <1>l DEF DInv, UD, UDR
  <2>. QED BY <2>1, <2>2
<1>3. \A q \in Paths : DeleteW(s, p).local[q] = w[s].local[q] \/ DeleteW(s, p).local[q] = Nil BY <1>l
<1>. QED BY <1>1, <1>2, <1>3, DWriter

LEMMA Edit_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame PROVE DInv'
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
      /\ UNCHANGED <<doc, conflicts, udel, acked>>
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
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
<1>l. \A q \in Paths : EditW(s, p).local[q] = IF q = p THEN hh ELSE w[s].local[q] BY <1>lt, <1>h DEF EditW
<1>2. UDR(EditW(s, p))
  <2>. SUFFICES ASSUME w[s].pc \in Bar, NEW q \in w[s].uploads, w[s].baseline[q] # Nil
                PROVE  EditW(s, p).local[q] # w[s].baseline[q]
    BY DEF UDR, EditW
  <2>q. q \in Paths BY <1>y
  <2>1. CASE q = p
    <3>1. w[s].baseline[q] \in minted BY <2>q, <1>y DEF Opt
    <3>. QED BY <2>1, <2>q, <1>l, <1>4, <3>1
  <2>2. CASE q # p BY <2>2, <2>q, <1>l DEF DInv, UD, UDR
  <2>. QED BY <2>1, <2>2
<1>3. \A q \in Paths : EditW(s, p).local[q] = w[s].local[q] \/ EditW(s, p).local[q] \notin minted
  BY <1>l, <1>4
<1>. QED BY <1>1, <1>hs, <1>tk, <1>2, <1>3, DTree

LEMMA Upload_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame PROVE DInv'
<1>m. IndM5' BY DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles /\ w[s] \in Writer
  BY TreeTy DEF IndC
<1>k. w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
<1>e. UNCHANGED <<doc, conflicts, udel, acked>> BY DEF Upload
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>h0. PICK h0 \in minted : h0 = w[s].snap[p] BY <1>k, <1>y
<1>1. CASE h0 \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ UNCHANGED <<minted, base, orig>> BY <1>1, <1>h0 DEF Upload
  <2>2. WStep(s, UploadW(s, p)) BY <2>1, <1>e DEF WStep
  <2>3. UDR(UploadW(s, p)) BY UDSame DEF UploadW
  <2>. QED BY <2>2, <2>3, DWriter DEF UploadW
<1>2. CASE h0 \in upped
  <2>g. copies \in Nat /\ copies < MaxCopies BY <1>2, <1>h0 DEF Upload, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
  <2>c. PICK c \in Handles : c = <<p, MaxMint + copies + 1>>
    BY <2>g, MaxMintNat, MaxCopiesNat DEF Handles, Gens, Seed
  <2>1. /\ minted' = minted \cup {c} /\ base' = [base EXCEPT ![c] = base[h0]]
        /\ orig' = [orig EXCEPT ![c] = Content(h0)] /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
    BY <1>2, <1>h0, <2>c DEF Upload
  <2>4. c \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
      BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>2. nextGen <= MaxMint + 1 BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>3. nextGen \in Nat BY <1>0 DEF TypeOK
    <3>. QED BY <3>1, <3>2, <3>3, <2>c, <2>g, MaxMintNat DEF Gen
  <2>ty. base \in [Handles -> Opt(Handles)] /\ orig \in [Handles -> Opt(Handles)]
    BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
  <2>f. MintFacts(c) BY <2>1, <2>4, <2>ty, <1>y DEF MintFacts
  <2>hm. base[h0] \in Opt(minted) /\ Content(h0) \in minted /\ orig[Content(h0)] = Nil
    BY ContentMinted DEF IndC, IndM5, M5, Hist, HistMinted
  <2>7. base'[c] = base[h0] /\ orig'[c] = Content(h0) BY <2>1, <2>ty
  <2>8. \A z \in Handles : z # c => base'[z] = base[z] /\ orig'[z] = orig[z] BY <2>1, <2>ty
  <2>hs. HStep BY <1>m, <1>u, <2>f, <2>7, <2>8, <2>hm, HStepMint DEF IndC, Opt
  <2>lt. w[s].local \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
  <2>l. \A q \in Paths : UploadCopyW(s, p, c).local[q] = IF q = p /\ w[s].local[p] = h0 THEN c ELSE w[s].local[q]
    BY <2>lt, <1>h0 DEF UploadCopyW
  <2>2. UDR(UploadCopyW(s, p, c))
    <3>. SUFFICES ASSUME NEW q \in w[s].uploads, w[s].baseline[q] # Nil
                  PROVE  UploadCopyW(s, p, c).local[q] # w[s].baseline[q]
      BY DEF UDR, UploadCopyW
    <3>q. q \in Paths BY <1>y
    <3>1. CASE q = p /\ w[s].local[p] = h0
      <4>1. w[s].baseline[q] \in minted BY <3>q, <1>y DEF Opt
      <4>. QED BY <3>1, <3>q, <2>l, <2>4, <4>1
    <3>2. CASE ~(q = p /\ w[s].local[p] = h0) BY <3>2, <3>q, <2>l, <1>k DEF DInv, UD, UDR, Bar
    <3>. QED BY <3>1, <3>2
  <2>3. \A q \in Paths : UploadCopyW(s, p, c).local[q] = w[s].local[q] \/ UploadCopyW(s, p, c).local[q] \notin minted
    BY <2>l, <2>4
  <2>. QED BY <2>1, <1>e, <2>hs, <1>tk, <2>2, <2>3, DTree
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* Part D: G elapses, a reader loads, a writer reaps, a stutter.            *)

LEMMA Quiet_D ==
  ASSUME IndC, IndC', DInv, UNCHANGED <<anc, minted, base, orig, doc, acked, conflicts, udel, w, took>>
  PROVE  DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>u. AncUpdate BY AncUpdateSame DEF IndC
<1>hs. HStep BY <1>u, HStepQ
<1>tk. TookOK BY <1>0, TookQuiet
<1>. QED BY <1>hs, <1>tk, DNoTree

------------------------------------------------------------------------------
(* Part D: the invariant, and AN5.                                         *)

LEMMA Init_D == Init => DInv
<1>. SUFFICES ASSUME Init PROVE DInv OBVIOUS
<1>1. acked = {} BY DEF Init
<1>2. \A q \in Paths : doc[q] # Nil => doc[q][1] = q BY DEF Init
<1>3. \A u \in Writers : w[u] = WriterInit BY DEF Init
<1>. QED BY <1>1, <1>2, <1>3 DEF DInv, AckMin, CHI, BT, UD, UDR, KDall, WriterInit, Bar

LEMMA Next_D == IndC /\ IndC' /\ DInv /\ Next => DInv'
<1>. SUFFICES ASSUME IndC, IndC', DInv, Next PROVE DInv' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE DInv' BY <3>1, GPut_D
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE DInv' BY <3>2, GCas_D
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE DInv' BY <3>3, GDelete_D
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE DInv' BY <3>4, GRename_D
    <3>5. ASSUME GRenameFinish PROVE DInv' BY <3>5, GRenameFinish_D
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE DInv' BY <3>1, Checkout_D
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE DInv' BY <3>2, Consume_D
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE DInv' BY <3>3, Scan_D
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE DInv' BY <3>4, Skip_D
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE DInv' BY <3>5, PullOnly_D
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE DInv' BY <3>6, Claim_D
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE DInv' BY <3>7, Verify_D
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE DInv' BY <3>8, Install_D
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE DInv' BY <3>9, Collect_D
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE DInv' BY <3>10, Finish_D
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE DInv' BY <3>11, Restart_D
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE DInv' BY <3>12, Sync_D
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE DInv' BY <3>13, RescopeBegin_D
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE DInv' BY <3>14, RescopeFirst_D
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE DInv' BY <3>15, RescopeSecond_D
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE DInv' BY <3>16, RPullRead_D
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE DInv' BY <3>17, RPullSync_D
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE DInv' BY <3>18, Edit_D
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE DInv' BY <3>19, Delete_D
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE DInv' BY <3>20, Upload_D
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE DInv' BY <3>21, Sweep_D
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>1. UNCHANGED <<anc, minted, base, orig, doc, acked, conflicts, udel, w, took>>
    BY <1>2 DEF Age, RLoad, Reap, aux
  <2>. QED BY <2>1, Quiet_D
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA DStep == IndC /\ IndC' /\ DInv /\ [Next]_vars => DInv'
<1>. SUFFICES ASSUME IndC, IndC', DInv, [Next]_vars PROVE DInv' OBVIOUS
<1>1. CASE Next BY <1>1, Next_D
<1>2. CASE UNCHANGED vars
  <2>1. UNCHANGED <<anc, minted, base, orig, doc, acked, conflicts, udel, w, took>> BY <1>2 DEF vars, bucket, aux
  <2>. QED BY <2>1, Quiet_D
<1>. QED BY <1>1, <1>2

LEMMA DInvariant == Spec => []DInv
BY Init_D, DStep, CInvariant, PTL DEF Spec

THEOREM AckedNamed == Spec => []AN5
BY DInvariant, CInvariant, KDToAN5, PTL
==============================================================================
