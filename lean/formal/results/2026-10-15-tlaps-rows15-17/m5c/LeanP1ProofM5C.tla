--------------------------- MODULE LeanP1ProofM5C ----------------------------
EXTENDS LeanP1ProofM5B

------------------------------------------------------------------------------
(* M5, part C: NR5, Inv_NoRegress restated as an action (RESTATEMENT-      *)
(* PROPOSAL.txt; NOTES.txt E27-E35).  A tree never takes the document's    *)
(* version at p while that steps BACK -- to a version its baseline derives *)
(* from with other bytes -- unless a record at p names a version derived   *)
(* from the baseline: a conflict, an acknowledged delete, or an            *)
(* acknowledgement at another path.  The claim is LeanP1.tla's NR5        *)
(* (since 2026-10-14); `Derives` here is LeanP1Anc's.                      *)

\* NamedAt, BackR5, Takes and NR5 are LeanP1.tla's (2026-10-14), via LeanP1Anc.

------------------------------------------------------------------------------
(* The history's order.  A handle's RANK is the generation of the bytes it  *)
(* holds; everything on its base chain ranks strictly lower, so a version   *)
(* deriving from another is never strictly older than it.                   *)

CR(h) == Gen(Content(h))
DS(k) == {k} \cup anc[k]
RankC   == \A m \in minted : \A a \in anc[m] : CR(a) < CR(m)
\* An original (no copy) was minted before the next mint.
OrigC   == \A m \in minted : orig[m] = Nil => Gen(m) < nextGen
\* A copy shares its original's history.
SameC   == \A m, n \in minted : Content(m) = Content(n) => anc[m] = anc[n]
\* The chain is closed under its own chains.
ClosedC == \A m \in minted : \A a \in anc[m] : anc[a] \subseteq anc[m]
HistC == RankC /\ OrigC /\ SameC /\ ClosedC

LEMMA GensNat == Gens \subseteq Nat
BY MaxMintNat, MaxCopiesNat DEF Gens, Seed

LEMMA MintedHandles == IndM5 => minted \subseteq Handles
BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK

\* What a minted handle's bytes and chain are.
LEMMA ContentMinted ==
  ASSUME IndM5, NEW m \in minted
  PROVE  /\ Content(m) \in minted /\ orig[Content(m)] = Nil /\ Content(Content(m)) = Content(m)
         /\ anc[m] \subseteq minted /\ DS(m) \subseteq minted
         /\ CR(m) \in Nat
<1>0. Nil \notin Handles BY NilHandle
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. orig[m] \in Opt(minted) /\ (orig[m] # Nil => orig[orig[m]] = Nil) /\ anc[m] \subseteq minted
  BY DEF IndM5, M5, Hist, HistMinted
<1>2. m # Nil BY <1>0, <1>t
<1>3. CASE orig[m] = Nil
  <2>1. Content(m) = m BY <1>2, <1>3 DEF Content
  <2>. QED BY <1>1, <1>3, <2>1, <1>t, GensNat DEF DS, CR, Gen, Handles
<1>4. CASE orig[m] # Nil
  <2>1. Content(m) = orig[m] BY <1>2, <1>4 DEF Content
  <2>2. orig[m] \in minted /\ orig[orig[m]] = Nil BY <1>1, <1>4 DEF Opt
  <2>3. orig[m] # Nil BY <1>4
  <2>4. Content(orig[m]) = orig[m] BY <2>2, <2>3 DEF Content
  <2>5. Gen(orig[m]) \in Nat BY <2>2, <1>t, GensNat DEF Gen, Handles
  <2>. QED BY <1>1, <2>1, <2>2, <2>4, <2>5 DEF DS, CR
<1>. QED BY <1>3, <1>4

\* Derives between handles: some handle on k's chain holds h's bytes.
LEMMA DerivesDS ==
  ASSUME NEW k \in Handles, NEW h \in Handles
  PROVE  Derives(k, h) <=> \E u \in DS(k) : Content(u) = Content(h)
<1>0. k # Nil /\ h # Nil BY NilHandle
<1>. QED BY <1>0 DEF Derives, DS

LEMMA DerivesTrans ==
  ASSUME IndM5, HistC, NEW k \in minted, NEW h \in minted, NEW x \in minted,
         Derives(k, h), Derives(h, x)
  PROVE  Derives(k, x)
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. PICK u \in DS(k) : Content(u) = Content(h) BY <1>t, DerivesDS
<1>2. PICK v \in DS(h) : Content(v) = Content(x) BY <1>t, DerivesDS
<1>3. u \in minted BY ContentMinted
<1>4. anc[u] = anc[h] BY <1>1, <1>3 DEF HistC, SameC
<1>5. anc[u] \subseteq anc[k]
  <2>1. CASE u = k BY <2>1
  <2>2. CASE u \in anc[k] BY <2>2 DEF HistC, ClosedC
  <2>. QED BY <2>1, <2>2 DEF DS
<1>6. \E z \in DS(k) : Content(z) = Content(x)
  <2>1. CASE v = h BY <2>1, <1>1, <1>2
  <2>2. CASE v \in anc[h] BY <2>2, <1>2, <1>4, <1>5 DEF DS
  <2>. QED BY <2>1, <2>2 DEF DS
<1>. QED BY <1>6, <1>t, DerivesDS

\* A version deriving from another never ranks above it; with other bytes,
\* strictly below.
LEMMA DerivesRank ==
  ASSUME IndM5, HistC, NEW k \in minted, NEW h \in minted, Derives(k, h)
  PROVE  /\ CR(h) <= CR(k)
         /\ Content(h) # Content(k) => CR(h) < CR(k)
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. PICK u \in DS(k) : Content(u) = Content(h) BY <1>t, DerivesDS
<1>2. u \in minted BY ContentMinted
<1>3. CR(h) = CR(u) BY <1>1 DEF CR
<1>n. CR(h) \in Nat /\ CR(k) \in Nat /\ CR(u) \in Nat BY <1>2, ContentMinted
<1>4. CASE u = k BY <1>1, <1>3, <1>4, <1>n
<1>5. CASE u \in anc[k]
  <2>1. CR(u) < CR(k) BY <1>5 DEF HistC, RankC
  <2>. QED BY <1>3, <1>n, <2>1
<1>. QED BY <1>4, <1>5 DEF DS

\* THE POINT: a version deriving from x is never strictly older than x.
LEMMA NotOlder ==
  ASSUME IndM5, HistC, NEW x \in minted, NEW y \in minted, Derives(y, x)
  PROVE  ~(Derives(x, y) /\ Content(y) # Content(x))
<1>1. CR(x) <= CR(y) BY DerivesRank
<1>2. (Derives(x, y) /\ Content(y) # Content(x)) => CR(y) < CR(x) BY DerivesRank
<1>n. CR(x) \in Nat /\ CR(y) \in Nat BY ContentMinted
<1>. QED BY <1>1, <1>2, <1>n

------------------------------------------------------------------------------
(* What the steps do to the order.  A step mints at most one handle: an     *)
(* original (GPut, Edit) whose base was minted before it, or a copy         *)
(* (Upload) holding a minted handle's bytes on that handle's base.          *)

\* What every minting step leaves of the history.
MintFacts(x) ==
  /\ x \in Handles /\ x \notin minted /\ minted' = minted \cup {x}
  /\ \A m \in minted : base'[m] = base[m] /\ orig'[m] = orig[m]

LEMMA MintAnc ==
  ASSUME IndM5, AncUpdate, NEW x, MintFacts(x)
  PROVE  /\ \A m \in minted : anc'[m] = anc[m]
         /\ anc'[x] = AncOf(base'[x])
         /\ \A m \in minted : Content(m)' = Content(m) /\ CR(m)' = CR(m)
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. \A m \in minted : anc'[m] = anc[m] BY <1>t DEF MintFacts, AncUpdate
<1>2. base[x] = Nil /\ anc[x] = {} BY DEF MintFacts, IndM5, M5, Hist, HistNew
<1>3. anc'[x] = AncOf(base'[x])
  <2>1. CASE base'[x] = base[x] BY <1>2, <2>1 DEF MintFacts, AncUpdate, AncOf
  <2>2. CASE base'[x] # base[x] BY <2>2 DEF MintFacts, AncUpdate
  <2>. QED BY <2>1, <2>2
<1>4. \A m \in minted : Content(m)' = Content(m) /\ CR(m)' = CR(m) BY DEF MintFacts, Content, CR
<1>. QED BY <1>1, <1>3, <1>4

\* An original: its rank is the next generation, above everything minted.
LEMMA HCOrig ==
  ASSUME IndM5, HistC, IndM5', AncUpdate, NEW x, MintFacts(x),
         orig'[x] = Nil, Gen(x) = nextGen, nextGen' = nextGen + 1, base'[x] \in Opt(minted)
  PROVE  HistC'
<1>0. Nil \notin Handles BY NilHandle
<1>t. minted \subseteq Handles BY MintedHandles
<1>n. nextGen \in Nat BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>a. /\ \A m \in minted : anc'[m] = anc[m]
      /\ anc'[x] = AncOf(base'[x])
      /\ \A m \in minted : Content(m)' = Content(m) /\ CR(m)' = CR(m)
  BY MintAnc
<1>x. x \in Handles /\ x \notin minted /\ minted' = minted \cup {x} BY DEF MintFacts
<1>c. Content(x)' = x /\ CR(x)' = nextGen BY <1>x, <1>0 DEF Content, CR
\* What x's base chain ranks: below the next generation.
<1>b. \A a \in AncOf(base'[x]) : a \in minted /\ CR(a) < nextGen
  <2>. SUFFICES ASSUME NEW a \in AncOf(base'[x]) PROVE a \in minted /\ CR(a) < nextGen OBVIOUS
  <2>1. base'[x] # Nil /\ base'[x] \in minted BY DEF AncOf, Opt
  <2>. DEFINE b == base'[x]
  <2>2. Content(b) \in minted /\ orig[Content(b)] = Nil /\ anc[b] \subseteq minted /\ CR(b) \in Nat
    BY <2>1, ContentMinted
  <2>3. CR(b) < nextGen BY <2>2 DEF HistC, OrigC, CR
  <2>4. CASE a = b BY <2>1, <2>3, <2>4
  <2>5. CASE a \in anc[b]
    <3>1. CR(a) < CR(b) BY <2>1, <2>5 DEF HistC, RankC
    <3>2. a \in minted BY <2>2, <2>5
    <3>3. CR(a) \in Nat BY <3>2, ContentMinted
    <3>. QED BY <3>1, <3>2, <3>3, <2>2, <2>3, <1>n
  <2>. QED BY <2>1, <2>4, <2>5 DEF AncOf
<1>1. RankC'
  <2>. SUFFICES ASSUME NEW m \in minted', NEW a \in anc'[m] PROVE CR(a)' < CR(m)' BY DEF RankC
  <2>1. CASE m \in minted
    <3>1. a \in anc[m] /\ a \in minted BY <2>1, <1>a, ContentMinted
    <3>. QED BY <2>1, <3>1, <1>a DEF HistC, RankC
  <2>2. CASE m = x
    <3>1. a \in AncOf(base'[x]) BY <2>2, <1>a
    <3>. QED BY <3>1, <1>b, <1>a, <1>c, <2>2
  <2>. QED BY <2>1, <2>2, <1>x
<1>2. OrigC'
  <2>. SUFFICES ASSUME NEW m \in minted', orig'[m] = Nil PROVE Gen(m) < nextGen' BY DEF OrigC
  <2>1. CASE m \in minted
    <3>1. Gen(m) < nextGen BY <2>1 DEF MintFacts, HistC, OrigC
    <3>2. Gen(m) \in Nat BY <2>1, <1>t, GensNat DEF Gen, Handles
    <3>. QED BY <3>1, <3>2, <1>n
  <2>2. CASE m = x BY <2>2, <1>n
  <2>. QED BY <2>1, <2>2, <1>x
<1>3. SameC'
  <2>. SUFFICES ASSUME NEW m \in minted', NEW k \in minted', Content(m)' = Content(k)' PROVE anc'[m] = anc'[k]
    BY DEF SameC
  <2>1. CASE m \in minted /\ k \in minted BY <2>1, <1>a DEF HistC, SameC
  <2>2. CASE m = x /\ k \in minted
    <3>1. Content(k) \in minted BY <2>2, ContentMinted
    <3>. QED BY <2>2, <3>1, <1>a, <1>c, <1>x
  <2>3. CASE m \in minted /\ k = x
    <3>1. Content(m) \in minted BY <2>3, ContentMinted
    <3>. QED BY <2>3, <3>1, <1>a, <1>c, <1>x
  <2>4. CASE m = x /\ k = x BY <2>4
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <1>x
<1>4. ClosedC'
  <2>. SUFFICES ASSUME NEW m \in minted', NEW a \in anc'[m] PROVE anc'[a] \subseteq anc'[m] BY DEF ClosedC
  <2>1. CASE m \in minted
    <3>1. a \in anc[m] /\ a \in minted BY <2>1, <1>a, ContentMinted
    <3>. QED BY <2>1, <3>1, <1>a DEF HistC, ClosedC
  <2>2. CASE m = x
    <3>1. a \in AncOf(base'[x]) BY <2>2, <1>a
    <3>2. base'[x] # Nil /\ base'[x] \in minted BY <3>1 DEF AncOf, Opt
    <3>3. anc'[x] = {base'[x]} \cup anc[base'[x]] BY <2>2, <3>2, <1>a DEF AncOf
    <3>4. a \in minted BY <3>1, <1>b
    <3>5. anc'[a] = anc[a] BY <3>4, <1>a
    <3>6. CASE a = base'[x] BY <3>3, <3>5, <3>6, <2>2
    <3>7. CASE a \in anc[base'[x]]
      <4>1. anc[a] \subseteq anc[base'[x]] BY <3>2, <3>7 DEF HistC, ClosedC
      <4>. QED BY <3>3, <3>5, <4>1, <2>2
    <3>. QED BY <3>1, <3>3, <3>6, <3>7, <2>2
  <2>. QED BY <2>1, <2>2, <1>x
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF HistC

\* A copy: h's bytes on h's base -- h's rank, h's chain.
LEMMA HCCopy ==
  ASSUME IndM5, HistC, IndM5', AncUpdate, NEW x, MintFacts(x),
         NEW h \in minted, orig'[x] = Content(h), base'[x] = base[h], nextGen' = nextGen
  PROVE  HistC'
<1>0. Nil \notin Handles BY NilHandle
<1>t. minted \subseteq Handles BY MintedHandles
<1>a. /\ \A m \in minted : anc'[m] = anc[m]
      /\ anc'[x] = AncOf(base'[x])
      /\ \A m \in minted : Content(m)' = Content(m) /\ CR(m)' = CR(m)
  BY MintAnc
<1>x. x \in Handles /\ x \notin minted /\ minted' = minted \cup {x} BY DEF MintFacts
<1>h. Content(h) \in minted /\ anc[h] \subseteq minted /\ CR(h) \in Nat BY ContentMinted
<1>c. Content(x)' = Content(h) /\ CR(x)' = CR(h) BY <1>x, <1>h, <1>t, <1>0 DEF Content, CR
<1>g. anc'[x] = anc[h]
  <2>1. anc[h] = AncOf(base[h]) BY <1>t DEF IndM5, M5, Hist, HistAnc
  <2>. QED BY <1>a, <2>1
<1>1. RankC'
  <2>. SUFFICES ASSUME NEW m \in minted', NEW a \in anc'[m] PROVE CR(a)' < CR(m)' BY DEF RankC
  <2>1. CASE m \in minted
    <3>1. a \in anc[m] /\ a \in minted BY <2>1, <1>a, ContentMinted
    <3>. QED BY <2>1, <3>1, <1>a DEF HistC, RankC
  <2>2. CASE m = x
    <3>1. a \in anc[h] /\ a \in minted BY <2>2, <1>g, <1>h
    <3>. QED BY <2>2, <3>1, <1>a, <1>c DEF HistC, RankC
  <2>. QED BY <2>1, <2>2, <1>x
<1>2. OrigC'
  <2>. SUFFICES ASSUME NEW m \in minted', orig'[m] = Nil PROVE Gen(m) < nextGen' BY DEF OrigC
  <2>1. CASE m \in minted BY <2>1 DEF MintFacts, HistC, OrigC
  <2>2. CASE m = x BY <2>2, <1>h, <1>t, <1>0
  <2>. QED BY <2>1, <2>2, <1>x
<1>3. SameC'
  <2>. SUFFICES ASSUME NEW m \in minted', NEW k \in minted', Content(m)' = Content(k)' PROVE anc'[m] = anc'[k]
    BY DEF SameC
  <2>1. CASE m \in minted /\ k \in minted BY <2>1, <1>a DEF HistC, SameC
  <2>2. CASE m = x /\ k \in minted
    <3>1. Content(h) = Content(k) BY <2>2, <1>a, <1>c
    <3>2. anc[h] = anc[k] BY <2>2, <3>1 DEF HistC, SameC
    <3>. QED BY <2>2, <3>2, <1>a, <1>g
  <2>3. CASE m \in minted /\ k = x
    <3>1. Content(h) = Content(m) BY <2>3, <1>a, <1>c
    <3>2. anc[h] = anc[m] BY <2>3, <3>1 DEF HistC, SameC
    <3>. QED BY <2>3, <3>2, <1>a, <1>g
  <2>4. CASE m = x /\ k = x BY <2>4
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <1>x
<1>4. ClosedC'
  <2>. SUFFICES ASSUME NEW m \in minted', NEW a \in anc'[m] PROVE anc'[a] \subseteq anc'[m] BY DEF ClosedC
  <2>1. CASE m \in minted
    <3>1. a \in anc[m] /\ a \in minted BY <2>1, <1>a, ContentMinted
    <3>. QED BY <2>1, <3>1, <1>a DEF HistC, ClosedC
  <2>2. CASE m = x
    <3>1. a \in anc[h] /\ a \in minted BY <2>2, <1>g, <1>h
    <3>2. anc[a] \subseteq anc[h] BY <3>1 DEF HistC, ClosedC
    <3>. QED BY <2>2, <3>1, <3>2, <1>a, <1>g
  <2>. QED BY <2>1, <2>2, <1>x
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF HistC

\* A step that writes no history.
LEMMA HCSame ==
  ASSUME HistC, minted \subseteq Handles, UNCHANGED <<minted, base, orig, anc>>, nextGen \in Nat, nextGen' \in Nat, nextGen <= nextGen'
  PROVE  HistC'
<1>1. \A m \in minted : CR(m)' = CR(m) BY DEF CR, Content
<1>2. \A m \in minted : orig[m] = Nil => Gen(m) < nextGen'
  <2>1. \A m \in minted : Gen(m) \in Nat BY GensNat DEF Gen, Handles
  <2>. QED BY <2>1 DEF HistC, OrigC
<1>. QED BY <1>1, <1>2 DEF HistC, RankC, OrigC, SameC, ClosedC, Content, CR

\* AncUpdate keeps anc where base is kept.
LEMMA AncSame ==
  ASSUME IndM5, AncUpdate, UNCHANGED base PROVE UNCHANGED anc
BY DEF AncUpdate, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK

------------------------------------------------------------------------------
(* The trees.  A tree's bytes derive from its baseline (LDB); in a barrier  *)
(* the uploads' snapshots do too (SDB), and the bytes from the snapshots    *)
(* (LDS).  Where the tree holds its baseline clean, the document at p       *)
(* derives from it or a record names it (PC1); between the CAS and the      *)
(* finish, the same for what it installed (PC2).                            *)

Bar == {"scanned", "claimed", "cased"}
DorN(p, x) == (doc[p] # Nil /\ Derives(doc[p], x)) \/ NamedAt(p, x)
UBt(u)  == w[u].pc \notin Bar => w[u].uploads = {}
LDBt(u) == \A p \in Paths : (w[u].local[p] # Nil /\ w[u].baseline[p] # Nil) => Derives(w[u].local[p], w[u].baseline[p])
LDSt(u) == w[u].pc \in Bar => \A p \in w[u].uploads : w[u].local[p] # Nil => Derives(w[u].local[p], w[u].snap[p])
SDBt(u) == w[u].pc \in Bar => \A p \in w[u].uploads : w[u].baseline[p] # Nil => Derives(w[u].snap[p], w[u].baseline[p])
PC1t(u) == \A p \in Paths : (w[u].baseline[p] # Nil /\ w[u].local[p] = w[u].baseline[p]) => DorN(p, w[u].baseline[p])
PC2t(u) == w[u].pc = "cased" => \A p \in w[u].uploads \cap w[u].upDone : DorN(p, w[u].snap[p])
TC(u) == UBt(u) /\ LDBt(u) /\ LDSt(u) /\ SDBt(u) /\ PC1t(u) /\ PC2t(u)
TreeC == \A u \in Writers : TC(u)
IndC == IndM5 /\ HistC /\ TreeC

\* What every Next step gives the history it does not write (part A).
HStep == Hist /\ HistEv /\ HistTy /\ AncUpdate

\* A version a record or a tree names derives only from what is minted.
LEMMA DerivesFromMinted ==
  ASSUME IndM5, NEW k \in Opt(Handles), NEW x \in minted, Derives(k, x)
  PROVE  k \in minted
<1>0. Nil \notin Handles BY NilHandle
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. x # Nil /\ Content(x) \in minted BY <1>0, <1>t, ContentMinted
<1>2. k # Nil BY <1>1 DEF Derives
<1>. SUFFICES ASSUME k \notin minted PROVE FALSE OBVIOUS
<1>3. anc[k] = {} /\ orig[k] = Nil BY <1>2 DEF Opt, IndM5, M5, Hist, HistNew
<1>4. Content(k) = k BY <1>2, <1>3 DEF Content
<1>. QED BY <1>1, <1>3, <1>4 DEF Derives

\* Across a step: Derives into a minted handle keeps its value, for any k.
LEMMA DerivesStep ==
  ASSUME IndM5, HStep, NEW k \in Opt(Handles), NEW x \in minted, Derives(k, x)
  PROVE  Derives(k, x)'
<1>1. k \in minted BY DerivesFromMinted
<1>. QED BY <1>1, DerivesKeep DEF HStep

\* Records only grow: a name stays a name.
LEMMA NamedKeep ==
  ASSUME IndM5, HStep, conflicts \subseteq conflicts', udel \subseteq udel', acked \subseteq acked',
         NEW p \in Paths, NEW x \in minted, NamedAt(p, x)
  PROVE  NamedAt(p, x)'
<1>t. /\ conflicts \subseteq Paths \X Opt(Handles) /\ udel \subseteq Paths \X Handles
      /\ acked \subseteq Paths \X Handles
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. CASE \E c \in conflicts : c[1] = p /\ Derives(c[2], x)
  <2>1. PICK c \in conflicts : c[1] = p /\ Derives(c[2], x) BY <1>1
  <2>2. c[2] \in Opt(Handles) BY <1>t
  <2>3. Derives(c[2], x)' BY <2>1, <2>2, DerivesStep
  <2>. QED BY <2>1, <2>3 DEF NamedAt
<1>2. CASE \E d \in udel : d[1] = p /\ Derives(d[2], x)
  <2>1. PICK d \in udel : d[1] = p /\ Derives(d[2], x) BY <1>2
  <2>2. d[2] \in Opt(Handles) BY <1>t DEF Opt
  <2>3. Derives(d[2], x)' BY <2>1, <2>2, DerivesStep
  <2>. QED BY <2>1, <2>3 DEF NamedAt
<1>3. CASE \E a \in acked : a[1] # p /\ Derives(a[2], x)
  <2>1. PICK a \in acked : a[1] # p /\ Derives(a[2], x) BY <1>3
  <2>2. a[2] \in Opt(Handles) BY <1>t DEF Opt
  <2>3. Derives(a[2], x)' BY <2>1, <2>2, DerivesStep
  <2>. QED BY <2>1, <2>3 DEF NamedAt
<1>. QED BY <1>1, <1>2, <1>3 DEF NamedAt

\* A step that keeps the document and grows the records keeps every DorN.
LEMMA DorNKeep ==
  ASSUME IndM5, HStep, conflicts \subseteq conflicts', udel \subseteq udel', acked \subseteq acked',
         NEW p \in Paths, doc'[p] = doc[p], NEW x \in minted, DorN(p, x)
  PROVE  DorN(p, x)'
<1>t. doc[p] \in Opt(minted) BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>h. minted \subseteq Handles BY MintedHandles
<1>1. CASE doc[p] # Nil /\ Derives(doc[p], x)
  <2>1. doc[p] \in Opt(Handles) BY <1>t, <1>h DEF Opt
  <2>2. Derives(doc[p], x)' BY <1>1, <2>1, DerivesStep
  <2>. QED BY <1>1, <2>2 DEF DorN
<1>2. CASE NamedAt(p, x) BY <1>2, NamedKeep DEF DorN
<1>. QED BY <1>1, <1>2 DEF DorN

\* Through base: what a handle's base derives from, the handle derives from.
LEMMA DerivesBase ==
  ASSUME NEW h \in Handles, NEW y \in Handles, NEW x \in Handles,
         anc[h] = {y} \cup anc[y], Derives(y, x)
  PROVE  Derives(h, x)
<1>1. PICK u \in DS(y) : Content(u) = Content(x) BY DerivesDS
<1>2. u \in DS(h) BY <1>1 DEF DS
<1>. QED BY <1>1, <1>2, DerivesDS

\* The history's typing across a step, from the invariant on both sides.
LEMMA HTy == ASSUME IndM5, IndM5' PROVE HistTy
BY DEF HistTy, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts

LEMMA HStepSame ==
  ASSUME IndM5, IndM5', UNCHANGED <<minted, base, orig>>, AncUpdate
  PROVE  HStep
BY HistSame, HTy DEF HStep, IndM5, M5

\* A step minting x, and writing no other handle's history.
LEMMA HStepMint ==
  ASSUME IndM5, IndM5', AncUpdate, NEW x, MintFacts(x),
         base'[x] \in Opt(minted), orig'[x] \in Opt(minted), orig'[x] # Nil => orig[orig'[x]] = Nil,
         \A y \in Handles : y # x => base'[y] = base[y] /\ orig'[y] = orig[y]
  PROVE  HStep
<1>1. HistEv
  <2>1. \A h \in Handles \ minted' : base'[h] = Nil /\ orig'[h] = Nil
    <3>. SUFFICES ASSUME NEW h \in Handles \ minted' PROVE base'[h] = Nil /\ orig'[h] = Nil OBVIOUS
    <3>1. h # x /\ h \notin minted BY DEF MintFacts
    <3>. QED BY <3>1 DEF IndM5, M5, Hist, HistNew
  <2>. QED BY <2>1 DEF HistEv, MintFacts
<1>. QED BY <1>1, HTy DEF HStep, IndM5, M5

\* A step that writes no tree and keeps every DorN keeps every tree's.
LEMMA TCKeepAll ==
  ASSUME IndC, IndM5', HStep, w' = w,
         \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)'
  PROVE  TreeC'
<1>. SUFFICES ASSUME NEW u \in Writers PROVE TC(u)' BY DEF TreeC
<1>t. minted \subseteq Handles BY MintedHandles DEF IndC
\* rows 15-17: Writer has 48 fields; expanding it to type `p` timed out (record 10-14 too).
<1>w. w[u] \in Writer BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>y. /\ \A p \in Paths : w[u].local[p] \in Opt(minted) /\ w[u].baseline[p] \in Opt(minted)
                          /\ w[u].snap[p] \in Opt(minted)
      /\ \A p \in w[u].uploads : w[u].snap[p] \in minted
  BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted, M5, Hist, BaselineMinted, M2, SnapMinted
<1>o. \A p \in Paths : w[u].local[p] \in Opt(Handles) /\ w[u].snap[p] \in Opt(Handles) BY <1>y, <1>t DEF Opt
<1>c. TC(u) BY DEF IndC, TreeC
<1>1. UBt(u)' BY <1>c DEF TC, UBt
<1>2. LDBt(u)'
  <2>. SUFFICES ASSUME NEW p \in Paths, w[u].local[p] # Nil, w[u].baseline[p] # Nil
                PROVE  Derives(w[u].local[p], w[u].baseline[p])' BY DEF LDBt
  <2>1. Derives(w[u].local[p], w[u].baseline[p]) BY <1>c DEF TC, LDBt
  <2>2. w[u].baseline[p] \in minted BY <1>y DEF Opt
  <2>. QED BY <2>1, <2>2, <1>o, DerivesStep DEF IndC
<1>3. LDSt(u)'
  <2>. SUFFICES ASSUME w[u].pc \in Bar, NEW p \in w[u].uploads, w[u].local[p] # Nil
                PROVE  Derives(w[u].local[p], w[u].snap[p])' BY DEF LDSt
  <2>1. Derives(w[u].local[p], w[u].snap[p]) BY <1>c DEF TC, LDSt
  <2>2. p \in Paths BY <1>w, WriterFields
  <2>. QED BY <2>1, <2>2, <1>y, <1>o, DerivesStep DEF IndC
<1>4. SDBt(u)'
  <2>. SUFFICES ASSUME w[u].pc \in Bar, NEW p \in w[u].uploads, w[u].baseline[p] # Nil
                PROVE  Derives(w[u].snap[p], w[u].baseline[p])' BY DEF SDBt
  <2>1. Derives(w[u].snap[p], w[u].baseline[p]) BY <1>c DEF TC, SDBt
  <2>2. p \in Paths BY <1>w, WriterFields
  <2>. QED BY <2>1, <2>2, <1>y, <1>o, DerivesStep DEF Opt, IndC
<1>5. PC1t(u)'
  <2>. SUFFICES ASSUME NEW p \in Paths, w[u].baseline[p] # Nil, w[u].local[p] = w[u].baseline[p]
                PROVE  DorN(p, w[u].baseline[p])' BY DEF PC1t
  <2>1. DorN(p, w[u].baseline[p]) BY <1>c DEF TC, PC1t
  <2>. QED BY <2>1, <1>y DEF Opt
<1>6. PC2t(u)'
  <2>. SUFFICES ASSUME w[u].pc = "cased", NEW p \in w[u].uploads \cap w[u].upDone
                PROVE  DorN(p, w[u].snap[p])' BY DEF PC2t
  <2>1. DorN(p, w[u].snap[p]) BY <1>c DEF TC, PC2t
  <2>2. p \in Paths BY <1>w, WriterFields
  <2>. QED BY <2>1, <2>2, <1>y
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF TC

------------------------------------------------------------------------------
(* Part C: the gateway.                                                     *)

LEMMA GPut_C ==
  ASSUME IndC, NEW p \in Paths, GPut(p), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY GPut_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
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
<1>a. HistC' BY <1>m, <1>u, <1>f, <1>1, <1>5, <1>7, HCOrig DEF IndC, Gen
<1>b. TreeC'
  <2>1. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
  <2>. QED BY <1>m, <1>hs, <1>1, <2>1, TCKeepAll
<1>. QED BY <1>a, <1>b

LEMMA GCas_C ==
  ASSUME IndC, NEW p \in Paths, GCas(p), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY GCas_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>1. UNCHANGED <<minted, base, orig, nextGen, conflicts, udel>> /\ w' = w /\ acked \subseteq acked'
  BY DEF GCas, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>t. minted \subseteq Handles BY MintedHandles DEF IndC
<1>2. PICK h \in minted : h = gw[p]
  BY DEF GCas, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted, Opt
<1>dt. doc \in [Paths -> Opt(Handles)] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>5. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)'
  <2>. SUFFICES ASSUME NEW q \in Paths, NEW x \in minted, DorN(q, x) PROVE DorN(q, x)' OBVIOUS
  <2>0. CASE doc[p] # base[h]
    <3>1. doc' = doc BY <2>0, <1>2, ShippedShape DEF GCas, Shipped
    <3>. QED BY <3>1, <1>1, <1>hs, <1>dt, DorNKeep DEF IndC
  <2>9. CASE doc[p] = base[h]
    <3>0. doc' = [doc EXCEPT ![p] = h] BY <2>9, <1>2 DEF GCas
    <3>1. CASE q # p BY <3>1, <3>0, <1>1, <1>hs, <1>dt, DorNKeep DEF IndC
    <3>2. CASE q = p
      <4>1. CASE doc[p] # Nil /\ Derives(doc[p], x)
        <5>0. PICK d \in minted : d = doc[p] BY <4>1, <1>dt, DerivesFromMinted DEF IndC
        <5>1. anc[h] = {d} \cup anc[d] BY <2>9, <5>0, <4>1, <1>t DEF IndC, IndM5, M5, Hist, HistAnc, AncOf
        <5>2. Derives(h, x) BY <5>0, <5>1, <4>1, <1>t, DerivesBase
        <5>3. Derives(h, x)' BY <5>2, <1>hs, <1>t, DerivesStep DEF IndC, Opt
        <5>4. doc'[p] = h /\ h # Nil BY <3>0, <1>dt, <1>t, NilHandle
        <5>. QED BY <5>3, <5>4, <3>2 DEF DorN
      <4>2. CASE NamedAt(p, x) BY <4>2, <3>2, <1>1, <1>hs, NamedKeep DEF IndC, DorN
      <4>. QED BY <4>1, <4>2, <3>2 DEF DorN
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>0, <2>9
<1>b. TreeC' BY <1>m, <1>hs, <1>1, <1>5, TCKeepAll
<1>. QED BY <1>a, <1>b

LEMMA GDelete_C ==
  ASSUME IndC, NEW p \in Paths, GDelete(p), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY GDelete_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>1. /\ UNCHANGED <<minted, base, orig, nextGen, conflicts, acked>> /\ w' = w
      /\ doc' = [doc EXCEPT ![p] = Nil] /\ udel' = udel \cup {<<p, doc[p]>>}
  BY DEF GDelete, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>dt. doc \in [Paths -> Opt(Handles)] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>5. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)'
  <2>. SUFFICES ASSUME NEW q \in Paths, NEW x \in minted, DorN(q, x) PROVE DorN(q, x)' OBVIOUS
  <2>1. CASE q # p BY <2>1, <1>1, <1>hs, <1>dt, DorNKeep DEF IndC
  <2>2. CASE q = p
    <3>1. CASE doc[p] # Nil /\ Derives(doc[p], x)
      <4>0. PICK d \in minted : d = doc[p] BY <3>1, <1>dt, DerivesFromMinted DEF IndC
      <4>1. Derives(d, x)' BY <3>1, <4>0, <1>hs, MintedHandles, DerivesStep DEF IndC, Opt
      <4>2. <<p, d>> \in udel' BY <1>1, <4>0
      <4>. QED BY <4>1, <4>2, <2>2 DEF DorN, NamedAt
    <3>2. CASE NamedAt(p, x) BY <3>2, <2>2, <1>1, <1>hs, NamedKeep DEF IndC, DorN
    <3>. QED BY <3>1, <3>2, <2>2 DEF DorN
  <2>. QED BY <2>1, <2>2
<1>b. TreeC' BY <1>m, <1>hs, <1>1, <1>5, TCKeepAll
<1>. QED BY <1>a, <1>b

LEMMA GRename_C ==
  ASSUME IndC, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY GRename_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. p # q /\ doc[p] # Nil /\ doc[q] = Nil BY DEF GRename
<1>1. /\ UNCHANGED <<minted, base, orig, nextGen, conflicts, udel>> /\ w' = w
      /\ doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil] /\ acked' = acked \cup {<<q, doc[p]>>}
  BY ShippedShape DEF GRename, aux, Shipped
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>dt. doc \in [Paths -> Opt(Handles)] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>5. \A r \in Paths, x \in minted : DorN(r, x) => DorN(r, x)'
  <2>. SUFFICES ASSUME NEW r \in Paths, NEW x \in minted, DorN(r, x) PROVE DorN(r, x)' OBVIOUS
  <2>1. CASE r # p /\ r # q BY <2>1, <1>1, <1>hs, <1>dt, DorNKeep DEF IndC
  <2>2. CASE r = p
    <3>1. CASE doc[p] # Nil /\ Derives(doc[p], x)
      <4>0. PICK d \in minted : d = doc[p] BY <3>1, <1>dt, DerivesFromMinted DEF IndC
      <4>1. Derives(d, x)' BY <3>1, <4>0, <1>hs, MintedHandles, DerivesStep DEF IndC, Opt
      <4>2. <<q, d>> \in acked' /\ q # p BY <1>0, <1>1, <4>0
      <4>. QED BY <4>1, <4>2, <2>2 DEF DorN, NamedAt
    <3>2. CASE NamedAt(p, x) BY <3>2, <2>2, <1>1, <1>hs, NamedKeep DEF IndC, DorN
    <3>. QED BY <3>1, <3>2, <2>2 DEF DorN
  <2>3. CASE r = q
    <3>1. NamedAt(q, x) BY <2>3, <1>0 DEF DorN
    <3>. QED BY <3>1, <2>3, <1>1, <1>hs, NamedKeep DEF IndC, DorN
  <2>. QED BY <2>1, <2>2, <2>3
<1>b. TreeC' BY <1>m, <1>hs, <1>1, <1>5, TCKeepAll
<1>. QED BY <1>a, <1>b

LEMMA GRenameFinish_C ==
  ASSUME IndC, GRenameFinish, Frame
  PROVE  HistC' /\ TreeC'
<1>1. mv = Nil BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

LEMMA Sweep_C ==
  ASSUME IndC, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Sweep_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>1. UNCHANGED <<minted, base, orig, nextGen, conflicts, udel, acked, doc>> /\ w' = w BY DEF Sweep, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>5. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>b. TreeC' BY <1>m, <1>hs, <1>1, <1>5, TCKeepAll
<1>. QED BY <1>a, <1>b

------------------------------------------------------------------------------
(* Part C: the writers.  What a tree step leaves of the conjuncts.          *)

\* The typing the conjuncts read.
LEMMA TreeTy ==
  ASSUME IndM5, NEW u \in Writers
  PROVE  /\ w[u] \in Writer
         /\ \A p \in Paths : /\ w[u].local[p] \in Opt(minted) /\ w[u].baseline[p] \in Opt(minted)
                             /\ w[u].snap[p] \in Opt(minted)
         /\ \A p \in w[u].uploads : w[u].snap[p] \in minted
         /\ w[u].uploads \subseteq Paths /\ w[u].upDone \subseteq Paths /\ w[u].deletes \subseteq Paths
         /\ minted \subseteq Handles /\ doc \in [Paths -> Opt(Handles)]
         /\ \A p \in Paths : doc[p] \in Opt(minted)
<1>1. w[u] \in Writer BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
\* rows 15-17: one goal per fact (the whole QED timed out, record 10-14 too).
<1>2. w[u].uploads \subseteq Paths /\ w[u].upDone \subseteq Paths /\ w[u].deletes \subseteq Paths
  BY <1>1, WriterFields
<1>3. minted \subseteq Handles BY MintedHandles
<1>4. doc \in [Paths -> Opt(Handles)] /\ \A p \in Paths : doc[p] \in Opt(minted)
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Minted, M5, Hist, BaselineMinted, M2, SnapMinted
<1>5. \A p \in Paths : w[u].local[p] \in Opt(minted) BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Minted, M5, Hist, BaselineMinted, M2, SnapMinted
<1>6. \A p \in Paths : w[u].baseline[p] \in Opt(minted) BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Minted, M5, Hist, BaselineMinted, M2, SnapMinted
<1>7. \A p \in Paths : w[u].snap[p] \in Opt(minted) BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Minted, M5, Hist, BaselineMinted, M2, SnapMinted
<1>8. \A p \in w[u].uploads : w[u].snap[p] \in minted BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Minted, M5, Hist, BaselineMinted, M2, SnapMinted
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8

\* A step whose tree u keeps the fields the conjuncts read (pc in or out
\* of the barrier as before, cased only from cased).  A tree the step does
\* not write is the case w'[u] = w[u].
LEMMA TCFields ==
  ASSUME IndC, IndM5', HStep, NEW u \in Writers,
         \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)',
         w'[u].local = w[u].local, w'[u].baseline = w[u].baseline, w'[u].snap = w[u].snap,
         w'[u].uploads = w[u].uploads, w'[u].upDone = w[u].upDone,
         w'[u].pc \in Bar => w[u].pc \in Bar, w'[u].pc = "cased" => w[u].pc = "cased",
         w'[u].pc \notin Bar => w'[u].uploads = {}
  PROVE  TC(u)'
<1>y. /\ \A p \in Paths : /\ w[u].local[p] \in Opt(minted) /\ w[u].baseline[p] \in Opt(minted)
                          /\ w[u].snap[p] \in Opt(minted)
      /\ \A p \in w[u].uploads : w[u].snap[p] \in minted
      /\ w[u].uploads \subseteq Paths /\ w[u].upDone \subseteq Paths /\ minted \subseteq Handles
  BY TreeTy DEF IndC
<1>c. TC(u) BY DEF IndC, TreeC
<1>1. UBt(u)' BY DEF UBt
<1>2. LDBt(u)'
  <2>. SUFFICES ASSUME NEW p \in Paths, w'[u].local[p] # Nil, w'[u].baseline[p] # Nil
                PROVE  Derives(w[u].local[p], w[u].baseline[p])' BY DEF LDBt
  <2>0. PICK l \in Opt(Handles), b \in minted : l = w[u].local[p] /\ b = w[u].baseline[p] BY <1>y DEF Opt
  <2>1. Derives(l, b) BY <2>0, <1>c DEF TC, LDBt
  <2>2. Derives(l, b)' BY <2>1, DerivesStep DEF IndC
  <2>. QED BY <2>0, <2>2
<1>3. LDSt(u)'
  <2>. SUFFICES ASSUME w'[u].pc \in Bar, NEW p \in w'[u].uploads, w'[u].local[p] # Nil
                PROVE  Derives(w[u].local[p], w[u].snap[p])' BY DEF LDSt
  <2>0. PICK l \in Opt(Handles), b \in minted : l = w[u].local[p] /\ b = w[u].snap[p] BY <1>y DEF Opt
  <2>1. Derives(l, b) BY <2>0, <1>c DEF TC, LDSt
  <2>2. Derives(l, b)' BY <2>1, DerivesStep DEF IndC
  <2>. QED BY <2>0, <2>2
<1>4. SDBt(u)'
  <2>. SUFFICES ASSUME w'[u].pc \in Bar, NEW p \in w'[u].uploads, w'[u].baseline[p] # Nil
                PROVE  Derives(w[u].snap[p], w[u].baseline[p])' BY DEF SDBt
  <2>0. PICK l \in Opt(Handles), b \in minted : l = w[u].snap[p] /\ b = w[u].baseline[p] BY <1>y DEF Opt
  <2>1. Derives(l, b) BY <2>0, <1>c DEF TC, SDBt
  <2>2. Derives(l, b)' BY <2>1, DerivesStep DEF IndC
  <2>. QED BY <2>0, <2>2
<1>5. PC1t(u)'
  <2>. SUFFICES ASSUME NEW p \in Paths, w'[u].baseline[p] # Nil, w'[u].local[p] = w'[u].baseline[p]
                PROVE  DorN(p, w[u].baseline[p])' BY DEF PC1t
  <2>0. PICK b \in minted : b = w[u].baseline[p] BY <1>y DEF Opt
  <2>1. DorN(p, b) BY <2>0, <1>c DEF TC, PC1t
  <2>2. DorN(p, b)' BY <2>1
  <2>. QED BY <2>0, <2>2
<1>6. PC2t(u)'
  <2>. SUFFICES ASSUME w'[u].pc = "cased", NEW p \in w'[u].uploads \cap w'[u].upDone
                PROVE  DorN(p, w[u].snap[p])' BY DEF PC2t
  <2>0. PICK b \in minted : b = w[u].snap[p] BY <1>y
  <2>p. p \in Paths BY <1>y
  <2>1. DorN(p, b) BY <2>0, <1>c DEF TC, PC2t
  <2>2. DorN(p, b)' BY <2>1, <2>p
  <2>. QED BY <2>0, <2>2
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF TC

LEMMA TCKeepOne ==
  ASSUME IndC, IndM5', HStep, NEW u \in Writers, w'[u] = w[u],
         \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)'
  PROVE  TC(u)'
<1>1. w[u].pc \notin Bar => w[u].uploads = {} BY DEF IndC, TreeC, TC, UBt
<1>. QED BY <1>1, TCFields

\* A writer step on s: every other tree, and the conjunction, from s's.
LEMMA TCWriter ==
  ASSUME IndC, IndM5', HStep, NEW s \in Writers, \A u \in Writers \ {s} : w'[u] = w[u],
         \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)',
         TC(s)'
  PROVE  TreeC'
<1>1. \A u \in Writers \ {s} : TC(u)' BY TCKeepOne
<1>. QED BY <1>1 DEF TreeC

\* A step out of the barrier: each path taken from the document (or
\* dropped), adopted, cleared, unlinked, or left.
TakeShape(s, p) ==
  \/ w'[s].local[p] = w'[s].baseline[p] /\ w'[s].baseline[p] \in {doc[p], Nil}
  \/ w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
  \/ w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = Nil
  \/ w'[s].local[p] = Nil /\ w'[s].baseline[p] = w[s].baseline[p]
  \/ /\ w'[s].baseline[p] = doc[p] /\ doc[p] # Nil /\ w'[s].local[p] = w[s].local[p]
     /\ w[s].local[p] # Nil /\ Content(w[s].local[p]) = Content(doc[p])

LEMMA TCTake ==
  ASSUME IndC, IndM5', HStep, NEW s \in Writers,
         \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)',
         doc' = doc, w'[s].pc \notin Bar, w'[s].uploads = {},
         \A p \in Paths : TakeShape(s, p)
  PROVE  TC(s)'
<1>y. /\ \A p \in Paths : /\ w[s].local[p] \in Opt(minted) /\ w[s].baseline[p] \in Opt(minted)
      /\ minted \subseteq Handles /\ \A p \in Paths : doc[p] \in Opt(minted)
  BY TreeTy DEF IndC
<1>c. TC(s) BY DEF IndC, TreeC
<1>1. UBt(s)' BY DEF UBt
<1>2. LDBt(s)'
  <2>. SUFFICES ASSUME NEW p \in Paths, w'[s].local[p] # Nil, w'[s].baseline[p] # Nil
                PROVE  Derives(w[s].local[p], w[s].baseline[p])' BY DEF LDBt
  <2>1. CASE w'[s].local[p] = w'[s].baseline[p] BY <2>1 DEF Derives
  <2>2. CASE w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[p] /\ b = w[s].baseline[p] BY <2>2, <1>y DEF Opt
    <3>1. Derives(l, b) BY <3>0, <2>2, <1>c DEF TC, LDBt
    <3>2. Derives(l, b)' BY <3>1, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>2, <2>2
  <2>3. CASE /\ w'[s].baseline[p] = doc[p] /\ doc[p] # Nil /\ w'[s].local[p] = w[s].local[p]
             /\ w[s].local[p] # Nil /\ Content(w[s].local[p]) = Content(doc[p])
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[p] /\ b = doc[p] BY <2>3, <1>y DEF Opt
    <3>1. Derives(l, b) BY <3>0, <2>3 DEF Derives
    <3>2. Derives(l, b)' BY <3>1, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>2, <2>3
  <2>. QED BY <2>1, <2>2, <2>3 DEF TakeShape
<1>3. LDSt(s)' BY DEF LDSt, Bar
<1>4. SDBt(s)' BY DEF SDBt, Bar
<1>5. PC1t(s)'
  <2>. SUFFICES ASSUME NEW p \in Paths, w'[s].baseline[p] # Nil, w'[s].local[p] = w'[s].baseline[p]
                PROVE  DorN(p, w[s].baseline[p])' BY DEF PC1t
  <2>1. CASE w'[s].baseline[p] = doc[p]
    <3>1. doc'[p] # Nil /\ doc'[p] = w'[s].baseline[p] BY <2>1
    <3>. QED BY <3>1 DEF DorN, Derives
  <2>2. CASE w'[s].local[p] = w[s].local[p] /\ w'[s].baseline[p] = w[s].baseline[p]
    <3>0. PICK b \in minted : b = w[s].baseline[p] BY <2>2, <1>y DEF Opt
    <3>1. DorN(p, b) BY <3>0, <2>2, <1>c DEF TC, PC1t
    <3>2. DorN(p, b)' BY <3>1
    <3>. QED BY <3>0, <3>2, <2>2
  <2>. QED BY <2>1, <2>2 DEF TakeShape
<1>6. PC2t(s)' BY DEF PC2t, Bar
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF TC

------------------------------------------------------------------------------
(* Part C: the writer steps that move no tree field the conjuncts read, or  *)
(* leave the barrier taking, clearing or keeping each path.                 *)

LEMMA Claim_C ==
  ASSUME IndC, NEW s \in Writers, Claim(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Claim_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = ClaimW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Claim, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN ClaimW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = ClaimW(s) BY <1>w
  <2>. QED BY <2>1, <1>m, <1>hs, <1>d, TCFields DEF ClaimW, Claim, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Verify_C ==
  ASSUME IndC, NEW s \in Writers, Verify(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Verify_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = VerifyW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Verify, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN VerifyW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = VerifyW(s) BY <1>w
  <2>. QED BY <2>1, <1>m, <1>hs, <1>d, TCFields DEF VerifyW, Verify, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Collect_C ==
  ASSUME IndC, NEW s \in Writers, Collect(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Collect_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = CollectW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Collect, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN CollectW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = CollectW(s) BY <1>w
  <2>. QED BY <2>1, <1>m, <1>hs, <1>d, TCFields DEF CollectW, Collect, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Skip_C ==
  ASSUME IndC, NEW s \in Writers, Skip(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Skip_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = SkipW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Skip, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN SkipW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = SkipW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF SkipW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF SkipW, Skip, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA PullOnly_C ==
  ASSUME IndC, NEW s \in Writers, PullOnly(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY PullOnly_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = PullOnlyW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF PullOnly, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN PullOnlyW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = PullOnlyW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF PullOnlyW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF PullOnlyW, PullOnly, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Restart_C ==
  ASSUME IndC, NEW s \in Writers, Restart(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Restart_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = RestartW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Restart, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RestartW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = RestartW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF RestartW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF RestartW, Restart, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA RPullRead_C ==
  ASSUME IndC, NEW s \in Writers, RPullRead(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY RPullRead_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = RPullReadW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF RPullRead, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RPullReadW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = RPullReadW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF RPullReadW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF RPullReadW, RPullRead, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA RescopeFirst_C ==
  ASSUME IndC, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY RescopeFirst_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = RescopeFirstW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF RescopeFirst, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RescopeFirstW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = RescopeFirstW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1, ShippedShape DEF RescopeFirstW, Shipped, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF RescopeFirstW, RescopeFirst, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA RescopeBegin_C ==
  ASSUME IndC, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY RescopeBegin_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>p. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>1. /\ w' = [w EXCEPT ![s] = RescopeBeginW(s, T)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY <1>p DEF RescopeBegin, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RescopeBeginW(s, T) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = RescopeBeginW(s, T) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF RescopeBeginW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF RescopeBeginW, RescopeBegin, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Sync_C ==
  ASSUME IndC, NEW s \in Writers, Sync(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Sync_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>p. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>1. /\ w' = [w EXCEPT ![s] = SyncW(s, fail)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY <1>p DEF Sync, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN SyncW(s, fail) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = SyncW(s, fail) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF SyncW, SyncBl, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF SyncW, Sync, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA RPullSync_C ==
  ASSUME IndC, NEW s \in Writers, RPullSync(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY RPullSync_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>p. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>1. /\ w' = [w EXCEPT ![s] = RPullSyncW(s, fail)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY <1>p DEF RPullSync, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RPullSyncW(s, fail) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = RPullSyncW(s, fail) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF RPullSyncW, SyncBl, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF RPullSyncW, RPullSync, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Checkout_C ==
  ASSUME IndC, NEW s \in Writers, Checkout(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Checkout_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>p. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>1. /\ w' = [w EXCEPT ![s] = CheckoutW(s, T)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY <1>p DEF Checkout, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN CheckoutW(s, T) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>off. w[s] = WriterInit BY DEF Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
<1>s. TC(s)'
  <2>1. w'[s] = CheckoutW(s, T) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>1 DEF CheckoutW, CheckoutHeld, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2, <1>off DEF CheckoutW, Checkout, Bar, WriterInit
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Consume_C ==
  ASSUME IndC, NEW s \in Writers, Consume(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Consume_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>k. UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>> BY DEF Consume, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>k, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>k, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>k, <1>hs, DorNKeep DEF IndC
<1>i. w[s].pc = "idle" /\ w[s].uploads = {} BY DEF Consume, IndC, TreeC, TC, UBt, Bar
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>ck
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN ConsumeCheapW(s, ck) ELSE w[t] BY <1>0, <2>1, WriteAny
  <2>2. w'[s] = ConsumeCheapW(s, ck) BY <2>w
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>2 DEF ConsumeCheapW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>2, <1>i DEF ConsumeCheapW, Bar
  <2>s. TC(s)' BY <2>3, <2>4, <1>k, <1>m, <1>hs, <1>d, TCTake
  <2>. QED BY <1>a, <2>w, <2>s, <1>m, <1>hs, <1>d, TCWriter
<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>ck
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN ConsumeW(s, fail, ck) ELSE w[t] BY <1>0, <2>0, WriteAny
  <2>2. w'[s] = ConsumeW(s, fail, ck) BY <2>w
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>2 DEF ConsumeW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>2, <1>i DEF ConsumeW, Bar
  <2>s. TC(s)' BY <2>3, <2>4, <1>k, <1>m, <1>hs, <1>d, TCTake
  <2>. QED BY <1>a, <2>w, <2>s, <1>m, <1>hs, <1>d, TCWriter
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* Part C: the writer steps that mint or move a tree field the conjuncts    *)
(* read.                                                                    *)

\* A handle minted this step on e's chain derives from what e derives from.
LEMMA DerivesNew ==
  ASSUME IndM5, HStep, NEW hh \in Handles, NEW e \in minted, NEW x \in minted,
         anc'[hh] = {e} \cup anc[e], Derives(e, x)
  PROVE  Derives(hh, x)'
<1>0. hh # Nil BY NilHandle
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. PICK u \in DS(e) : Content(u) = Content(x) BY <1>t, DerivesDS
<1>2. u \in minted BY ContentMinted
<1>3. Content(u)' = Content(u) /\ Content(x)' = Content(x) BY <1>2 DEF HStep, HistEv, Content
<1>4. u \in anc'[hh] BY <1>1 DEF DS
<1>5. x # Nil BY <1>t, NilHandle
<1>. QED BY <1>0, <1>3, <1>4, <1>5, <1>1 DEF Derives

\* A copy of h0 (h0's bytes on h0's chain) derives from what h0 derives from.
LEMMA DerivesCopyFrom ==
  ASSUME IndM5, HStep, NEW c \in Handles, NEW h0 \in minted, NEW x \in minted,
         anc'[c] = anc[h0], Content(c)' = Content(h0), Derives(h0, x)
  PROVE  Derives(c, x)'
<1>0. c # Nil BY NilHandle
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. PICK u \in DS(h0) : Content(u) = Content(x) BY <1>t, DerivesDS
<1>2. u \in minted BY ContentMinted
<1>3. Content(x)' = Content(x) /\ Content(u)' = Content(u) BY <1>2 DEF HStep, HistEv, Content
<1>5. x # Nil BY <1>t, NilHandle
<1>6. CASE u = h0 BY <1>0, <1>1, <1>3, <1>5, <1>6 DEF Derives
<1>7. CASE u \in anc[h0] BY <1>0, <1>1, <1>3, <1>5, <1>7 DEF Derives
<1>. QED BY <1>6, <1>7 DEF DS

\* ...and what derives from h0 derives from its copy.
LEMMA DerivesToCopy ==
  ASSUME IndM5, HStep, NEW c \in Handles, NEW h0 \in minted, NEW k \in minted,
         Content(c)' = Content(h0), Derives(k, h0)
  PROVE  Derives(k, c)'
<1>0. c # Nil /\ k # Nil BY NilHandle, MintedHandles
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. PICK u \in DS(k) : Content(u) = Content(h0) BY <1>t, DerivesDS
<1>2. u \in minted BY ContentMinted
<1>3. Content(u)' = Content(u) /\ Content(k)' = Content(k) /\ anc'[k] = anc[k]
  BY <1>2, <1>t DEF HStep, HistEv, Content, AncUpdate
<1>6. CASE u = k BY <1>0, <1>1, <1>3, <1>6 DEF Derives
<1>7. CASE u \in anc[k] BY <1>0, <1>1, <1>3, <1>7 DEF Derives
<1>. QED BY <1>6, <1>7 DEF DS

\* A step that writes no tree field the conjuncts read except at one path p,
\* where the new bytes, if any, are fresh or derive as the lemma's caller
\* shows; the other paths' parts are TCFields'.  Stated per conjunct below.

LEMMA Delete_C ==
  ASSUME IndC, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Delete_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = DeleteW(s, p)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Delete, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN DeleteW(s, p) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles /\ w[s] \in Writer
  BY TreeTy DEF IndC
<1>c. TC(s) BY DEF IndC, TreeC
<1>r. /\ w'[s].local = [w[s].local EXCEPT ![p] = Nil]
      /\ w'[s].baseline = w[s].baseline /\ w'[s].snap = w[s].snap /\ w'[s].uploads = w[s].uploads
      /\ w'[s].upDone = w[s].upDone /\ w'[s].pc = w[s].pc
  BY <1>w DEF DeleteW
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
<1>l. \A q \in Paths : w'[s].local[q] = IF q = p THEN Nil ELSE w[s].local[q] BY <1>r, <1>lt
<1>s. TC(s)'
  <2>1. UBt(s)' BY <1>r, <1>c DEF TC, UBt
  <2>2. LDBt(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # Nil, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].baseline[q])' BY DEF LDBt
    <3>1. q # p /\ w'[s].local[q] = w[s].local[q] BY <1>l
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].baseline[q] BY <1>y, <1>r, <3>1 DEF Opt
    <3>2. Derives(l, b) BY <3>0, <3>1, <1>r, <1>c DEF TC, LDBt
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>1, <3>3, <1>r
  <2>3. LDSt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].local[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].snap[q])' BY DEF LDSt
    <3>p. q \in Paths BY <1>r, <1>y
    <3>1. q # p /\ w'[s].local[q] = w[s].local[q] BY <1>l, <3>p
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].snap[q] BY <1>y, <1>r, <3>1 DEF Opt
    <3>2. Derives(l, b) BY <3>0, <3>1, <1>r, <1>c DEF TC, LDSt
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>1, <3>3, <1>r
  <2>4. SDBt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].snap[q], w[s].baseline[q])' BY DEF SDBt
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].snap[q] /\ b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. Derives(l, b) BY <3>0, <1>r, <1>c DEF TC, SDBt
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>3, <1>r
  <2>5. PC1t(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].baseline[q] # Nil, w'[s].local[q] = w'[s].baseline[q]
                  PROVE  DorN(q, w[s].baseline[q])' BY DEF PC1t
    <3>1. q # p /\ w'[s].local[q] = w[s].local[q] BY <1>l
    <3>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. DorN(q, b) BY <3>0, <3>1, <1>r, <1>c DEF TC, PC1t
    <3>3. DorN(q, b)' BY <3>2, <1>d
    <3>. QED BY <3>0, <3>3, <1>r
  <2>6. PC2t(s)'
    <3>. SUFFICES ASSUME w'[s].pc = "cased", NEW q \in w'[s].uploads \cap w'[s].upDone
                  PROVE  DorN(q, w[s].snap[q])' BY DEF PC2t
    <3>0. PICK b \in minted : b = w[s].snap[q] BY <1>y, <1>r
    <3>p. q \in Paths BY <1>r, <1>y
    <3>2. DorN(q, b) BY <3>0, <1>r, <1>c DEF TC, PC2t
    <3>3. DorN(q, b)' BY <3>2, <3>p, <1>d
    <3>. QED BY <3>0, <3>3, <1>r
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF TC
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Edit_C ==
  ASSUME IndC, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Edit_M5 DEF IndC
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
<1>a. HistC' BY <1>m, <1>u, <1>f, <1>1, <1>7, <1>h, HCOrig DEF IndC, Gen
<1>an. anc'[hh] = AncOf(e) BY <1>u, <1>f, <1>7, MintAnc DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN EditW(s, p) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>c. TC(s) BY DEF IndC, TreeC
<1>r. /\ w'[s].local = [w[s].local EXCEPT ![p] = hh]
      /\ w'[s].baseline = w[s].baseline /\ w'[s].snap = w[s].snap /\ w'[s].uploads = w[s].uploads
      /\ w'[s].upDone = w[s].upDone /\ w'[s].pc = w[s].pc
  BY <1>w, <1>h DEF EditW
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
<1>l. \A q \in Paths : w'[s].local[q] = IF q = p THEN hh ELSE w[s].local[q] BY <1>r, <1>lt
\* What the new handle's base derives from.
<1>ne. e # Nil => anc'[hh] = {e} \cup anc[e] BY <1>an DEF AncOf
<1>s. TC(s)'
  <2>1. UBt(s)' BY <1>r, <1>c DEF TC, UBt
  <2>2. LDBt(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # Nil, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].baseline[q])' BY DEF LDBt
    <3>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>1. CASE q = p
      <4>1. Derives(e, b)
        <5>1. CASE w[s].local[p] # Nil BY <5>1, <3>0, <3>1, <1>e, <1>r, <1>c DEF TC, LDBt
        <5>2. CASE w[s].local[p] = Nil /\ p \in w[s].uploads
          <6>1. w[s].pc \in Bar BY <5>2, <1>c DEF TC, UBt
          <6>. QED BY <5>2, <6>1, <3>0, <3>1, <1>e, <1>r, <1>c DEF TC, SDBt
        <5>3. CASE w[s].local[p] = Nil /\ p \notin w[s].uploads BY <5>3, <3>0, <3>1, <1>e DEF Derives
        <5>. QED BY <5>1, <5>2, <5>3
      <4>2. e # Nil /\ e \in minted BY <4>1, <3>0, <1>y, NilHandle DEF Derives, Opt
      <4>3. Derives(hh, b)' BY <4>1, <4>2, <1>ne, <1>hs, DerivesNew DEF IndC
      <4>. QED BY <4>3, <3>0, <3>1, <1>l, <1>r
    <3>2. CASE q # p
      <4>0. PICK l \in Opt(Handles) : l = w[s].local[q] BY <1>y DEF Opt
      <4>1. Derives(l, b) BY <4>0, <3>0, <3>2, <1>l, <1>r, <1>c DEF TC, LDBt
      <4>2. Derives(l, b)' BY <4>1, <1>hs, DerivesStep DEF IndC
      <4>. QED BY <4>0, <4>2, <3>0, <3>2, <1>l, <1>r
    <3>. QED BY <3>1, <3>2
  <2>3. LDSt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].local[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].snap[q])' BY DEF LDSt
    <3>p. q \in Paths /\ q \in w[s].uploads /\ w[s].pc \in Bar BY <1>r, <1>y
    <3>0. PICK b \in minted : b = w[s].snap[q] BY <1>y, <3>p
    <3>1. CASE q = p
      <4>1. Derives(e, b)
        <5>1. CASE w[s].local[p] # Nil BY <5>1, <3>0, <3>1, <3>p, <1>e, <1>c DEF TC, LDSt
        <5>2. CASE w[s].local[p] = Nil BY <5>2, <3>0, <3>1, <3>p, <1>e DEF Derives
        <5>. QED BY <5>1, <5>2
      <4>2. e # Nil /\ e \in minted BY <4>1, <3>0, <1>y, NilHandle DEF Derives, Opt
      <4>3. Derives(hh, b)' BY <4>1, <4>2, <1>ne, <1>hs, DerivesNew DEF IndC
      <4>. QED BY <4>3, <3>0, <3>1, <1>l, <1>r
    <3>2. CASE q # p
      <4>0. PICK l \in Opt(Handles) : l = w[s].local[q] BY <1>y, <3>p DEF Opt
      <4>1. Derives(l, b) BY <4>0, <3>0, <3>2, <3>p, <1>l, <1>r, <1>c DEF TC, LDSt
      <4>2. Derives(l, b)' BY <4>1, <1>hs, DerivesStep DEF IndC
      <4>. QED BY <4>0, <4>2, <3>0, <3>2, <3>p, <1>l, <1>r
    <3>. QED BY <3>1, <3>2
  <2>4. SDBt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].snap[q], w[s].baseline[q])' BY DEF SDBt
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].snap[q] /\ b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. Derives(l, b) BY <3>0, <1>r, <1>c DEF TC, SDBt
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>3, <1>r
  <2>5. PC1t(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].baseline[q] # Nil, w'[s].local[q] = w'[s].baseline[q]
                  PROVE  DorN(q, w[s].baseline[q])' BY DEF PC1t
    <3>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>1. q # p BY <3>0, <1>l, <1>r, <1>4
    <3>2. DorN(q, b) BY <3>0, <3>1, <1>l, <1>r, <1>c DEF TC, PC1t
    <3>3. DorN(q, b)' BY <3>2, <1>d
    <3>. QED BY <3>0, <3>3, <1>r
  <2>6. PC2t(s)'
    <3>. SUFFICES ASSUME w'[s].pc = "cased", NEW q \in w'[s].uploads \cap w'[s].upDone
                  PROVE  DorN(q, w[s].snap[q])' BY DEF PC2t
    <3>0. PICK b \in minted : b = w[s].snap[q] BY <1>y, <1>r
    <3>p. q \in Paths BY <1>r, <1>y
    <3>2. DorN(q, b) BY <3>0, <1>r, <1>c DEF TC, PC2t
    <3>3. DorN(q, b)' BY <3>2, <3>p, <1>d
    <3>. QED BY <3>0, <3>3, <1>r
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF TC
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Scan_C ==
  ASSUME IndC, NEW s \in Writers, Scan(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Scan_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>p. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>1. /\ w' = [w EXCEPT ![s] = ScanW(s, dels)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY <1>p DEF Scan, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN ScanW(s, dels) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
      /\ minted \subseteq Handles
  BY TreeTy DEF IndC
<1>c. TC(s) BY DEF IndC, TreeC
<1>r. /\ w'[s].local = w[s].local /\ w'[s].baseline = w[s].baseline /\ w'[s].snap = w[s].local
      /\ w'[s].uploads = ScanUps(s) /\ w'[s].pc = "scanned"
  BY <1>w DEF ScanW
<1>s. TC(s)'
  <2>1. UBt(s)' BY <1>r DEF UBt, Bar
  <2>2. LDBt(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # Nil, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].baseline[q])' BY DEF LDBt
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. Derives(l, b) BY <3>0, <1>r, <1>c DEF TC, LDBt
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>3, <1>r
  <2>3. LDSt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].local[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].snap[q])' BY DEF LDSt
    <3>0. PICK l \in Opt(Handles) : l = w[s].local[q] BY <1>y, <1>r DEF Opt, ScanUps, ScanDirty
    <3>1. Derives(l, l)' BY DEF Derives
    <3>. QED BY <3>0, <3>1, <1>r
  <2>4. SDBt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].snap[q], w[s].baseline[q])' BY DEF SDBt
    <3>p. q \in Paths /\ w[s].local[q] # Nil BY <1>r DEF ScanUps, ScanDirty
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].baseline[q] BY <1>y, <1>r, <3>p DEF Opt
    <3>2. Derives(l, b) BY <3>0, <3>p, <1>r, <1>c DEF TC, LDBt
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>3, <1>r
  <2>5. PC1t(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].baseline[q] # Nil, w'[s].local[q] = w'[s].baseline[q]
                  PROVE  DorN(q, w[s].baseline[q])' BY DEF PC1t
    <3>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. DorN(q, b) BY <3>0, <1>r, <1>c DEF TC, PC1t
    <3>3. DorN(q, b)' BY <3>2, <1>d
    <3>. QED BY <3>0, <3>3, <1>r
  <2>6. PC2t(s)' BY <1>r DEF PC2t
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF TC
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Finish_C ==
  ASSUME IndC, NEW s \in Writers, Finish(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Finish_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = FinishW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Finish, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN FinishW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles
  BY TreeTy DEF IndC
<1>c. TC(s) BY DEF IndC, TreeC
<1>k. w[s].pc = "cased" BY DEF Finish
<1>r. /\ w'[s].local = w[s].local /\ w'[s].pc = "idle" /\ w'[s].uploads = {}
      /\ w'[s].baseline = [q \in Paths |-> IF q \in w[s].uploads \cap w[s].upDone THEN w[s].snap[q]
                                         ELSE IF q \in w[s].deletes /\ w[s].inst[q] = Nil THEN Nil
                                         ELSE w[s].baseline[q]]
  BY <1>w DEF FinishW
<1>s. TC(s)'
  <2>1. UBt(s)' BY <1>r DEF UBt
  <2>2. LDBt(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # Nil, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].baseline[q])' BY DEF LDBt
    <3>1. CASE q \in w[s].uploads \cap w[s].upDone
      <4>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].snap[q] BY <1>y, <3>1 DEF Opt
      <4>1. Derives(l, b) BY <4>0, <3>1, <1>k, <1>r, <1>c DEF TC, LDSt, Bar
      <4>2. Derives(l, b)' BY <4>1, <1>hs, DerivesStep DEF IndC
      <4>. QED BY <4>0, <4>2, <3>1, <1>r
    <3>2. CASE q \notin w[s].uploads \cap w[s].upDone /\ q \in w[s].deletes /\ w[s].inst[q] = Nil BY <3>2, <1>r
    <3>3. CASE q \notin w[s].uploads \cap w[s].upDone /\ ~(q \in w[s].deletes /\ w[s].inst[q] = Nil)
      <4>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].baseline[q] BY <1>y, <3>3, <1>r DEF Opt
      <4>1. Derives(l, b) BY <4>0, <3>3, <1>r, <1>c DEF TC, LDBt
      <4>2. Derives(l, b)' BY <4>1, <1>hs, DerivesStep DEF IndC
      <4>. QED BY <4>0, <4>2, <3>3, <1>r
    <3>. QED BY <3>1, <3>2, <3>3
  <2>3. LDSt(s)' BY <1>r DEF LDSt, Bar
  <2>4. SDBt(s)' BY <1>r DEF SDBt, Bar
  <2>5. PC1t(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].baseline[q] # Nil, w'[s].local[q] = w'[s].baseline[q]
                  PROVE  DorN(q, w[s].baseline[q])' BY DEF PC1t
    <3>1. CASE q \in w[s].uploads \cap w[s].upDone
      <4>0. PICK b \in minted : b = w[s].snap[q] BY <1>y, <3>1
      <4>1. DorN(q, b) BY <4>0, <3>1, <1>k, <1>c DEF TC, PC2t
      <4>2. DorN(q, b)' BY <4>1, <1>d
      <4>. QED BY <4>0, <4>2, <3>1, <1>r
    <3>2. CASE q \notin w[s].uploads \cap w[s].upDone /\ q \in w[s].deletes /\ w[s].inst[q] = Nil BY <3>2, <1>r
    <3>3. CASE q \notin w[s].uploads \cap w[s].upDone /\ ~(q \in w[s].deletes /\ w[s].inst[q] = Nil)
      <4>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <3>3, <1>r DEF Opt
      <4>1. DorN(q, b) BY <4>0, <3>3, <1>r, <1>c DEF TC, PC1t
      <4>2. DorN(q, b)' BY <4>1, <1>d
      <4>. QED BY <4>0, <4>2, <3>3, <1>r
    <3>. QED BY <3>1, <3>2, <3>3
  <2>6. PC2t(s)' BY <1>r DEF PC2t
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF TC
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

\* A commit keeps every DorN.  Over the version it held, its snapshot
\* derives from that version (SDB); over one it did not, it records a
\* conflict naming it (CommitSurfacesForeign); where it deletes, the
\* acknowledged delete names it.
LEMMA InstallDorN ==
  ASSUME IndC, NEW s \in Writers, Install(s), HStep
  PROVE  \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)'
<1>. SUFFICES ASSUME NEW p \in Paths, NEW x \in minted, DorN(p, x) PROVE DorN(p, x)' OBVIOUS
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ minted \subseteq Handles /\ doc \in [Paths -> Opt(Handles)] /\ \A q \in Paths : doc[q] \in Opt(minted)
  BY TreeTy DEF IndC
<1>sh. CommitSurfacesForeign /\ DeleteWinsPreserved BY ShippedShape DEF Shipped
<1>0. /\ doc' = InstallInst(s)
      /\ conflicts \subseteq conflicts' /\ udel \subseteq udel' /\ acked' = acked
      /\ \A q \in InstallContested(s) : <<q, doc[q]>> \in conflicts'
      /\ \A q \in Paths : (doc[q] # Nil /\ InstallInst(s)[q] = Nil) => <<q, doc[q]>> \in udel'
  BY DEF Install
<1>k. w[s].pc = "claimed" BY DEF Install
<1>c. TC(s) BY DEF IndC, TreeC
<1>i. InstallInst(s)[p] = IF p \in w[s].gone THEN doc[p]
                          ELSE IF p \in InstallMine(s) THEN w[s].snap[p]
                          ELSE IF p \in w[s].deletes THEN Nil
                          ELSE doc[p]
  BY <1>sh DEF InstallInst
<1>1. CASE NamedAt(p, x) BY <1>1, <1>0, NamedKeep DEF IndC, DorN
<1>2. CASE ~NamedAt(p, x)
  <2>0. doc[p] # Nil /\ Derives(doc[p], x) BY <1>2 DEF DorN
  <2>d. PICK d \in minted : d = doc[p] BY <2>0, <1>y, DerivesFromMinted DEF IndC
  <2>x. Derives(d, x)' BY <2>0, <2>d, <1>y, DerivesStep DEF IndC, Opt
  <2>1. CASE p \in w[s].gone \/ (p \notin InstallMine(s) /\ p \notin w[s].deletes)
    <3>1. doc'[p] = d BY <2>1, <2>d, <1>0, <1>i
    <3>. QED BY <3>1, <2>x, <2>d, <2>0 DEF DorN
  <2>2. CASE p \notin w[s].gone /\ p \in InstallMine(s)
    <3>0. doc'[p] = w[s].snap[p] BY <2>2, <1>0, <1>i
    <3>1. CASE Foreign(s, p)
      <4>1. p \in InstallContested(s) BY <2>2, <3>1, <2>0, <1>sh DEF InstallContested
      <4>2. <<p, d>> \in conflicts' BY <4>1, <1>0, <2>d
      <4>. QED BY <4>2, <2>x DEF DorN, NamedAt
    <3>2. CASE ~Foreign(s, p)
      <4>1. d = w[s].baseline[p] /\ p \in w[s].uploads BY <3>2, <2>d, <2>2 DEF Foreign, InstallMine
      <4>2. PICK sn \in minted : sn = w[s].snap[p] BY <4>1, <1>y
      <4>3. Derives(sn, d) BY <4>1, <4>2, <2>0, <2>d, <1>k, <1>c DEF TC, SDBt, Bar
      <4>4. Derives(sn, x) BY <4>3, <2>0, <2>d, DerivesTrans DEF IndC
      <4>5. Derives(sn, x)' BY <4>4, <1>y, DerivesStep DEF IndC, Opt
      <4>6. sn # Nil BY <1>y, NilHandle
      <4>. QED BY <3>0, <4>2, <4>5, <4>6 DEF DorN
    <3>. QED BY <3>1, <3>2
  <2>3. CASE p \notin w[s].gone /\ p \notin InstallMine(s) /\ p \in w[s].deletes
    <3>1. InstallInst(s)[p] = Nil BY <2>3, <1>i
    <3>2. <<p, d>> \in udel' BY <3>1, <2>0, <2>d, <1>0
    <3>. QED BY <3>2, <2>x DEF DorN, NamedAt
  <2>. QED BY <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2

LEMMA Install_C ==
  ASSUME IndC, NEW s \in Writers, Install(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Install_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = InstallW(s)] /\ UNCHANGED <<minted, base, orig, nextGen>> /\ doc' = InstallInst(s)
  BY DEF Install, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN InstallW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>hs, InstallDorN
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles
  BY TreeTy DEF IndC
<1>c. TC(s) BY DEF IndC, TreeC
<1>k. w[s].pc = "claimed" BY DEF Install
<1>r. /\ w'[s].local = w[s].local /\ w'[s].baseline = w[s].baseline /\ w'[s].snap = w[s].snap
      /\ w'[s].uploads = w[s].uploads /\ w'[s].upDone = w[s].upDone \ w[s].gone /\ w'[s].pc = "cased"
  BY <1>w DEF InstallW
<1>s. TC(s)'
  <2>1. UBt(s)' BY <1>r DEF UBt, Bar
  <2>2. LDBt(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # Nil, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].baseline[q])' BY DEF LDBt
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. Derives(l, b) BY <3>0, <1>r, <1>c DEF TC, LDBt
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>3, <1>r
  <2>3. LDSt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].local[q] # Nil
                  PROVE  Derives(w[s].local[q], w[s].snap[q])' BY DEF LDSt
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].snap[q] BY <1>y, <1>r DEF Opt
    <3>2. Derives(l, b) BY <3>0, <1>r, <1>k, <1>c DEF TC, LDSt, Bar
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>3, <1>r
  <2>4. SDBt(s)'
    <3>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].baseline[q] # Nil
                  PROVE  Derives(w[s].snap[q], w[s].baseline[q])' BY DEF SDBt
    <3>0. PICK l \in Opt(Handles), b \in minted : l = w[s].snap[q] /\ b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. Derives(l, b) BY <3>0, <1>r, <1>k, <1>c DEF TC, SDBt, Bar
    <3>3. Derives(l, b)' BY <3>2, <1>hs, DerivesStep DEF IndC
    <3>. QED BY <3>0, <3>3, <1>r
  <2>5. PC1t(s)'
    <3>. SUFFICES ASSUME NEW q \in Paths, w'[s].baseline[q] # Nil, w'[s].local[q] = w'[s].baseline[q]
                  PROVE  DorN(q, w[s].baseline[q])' BY DEF PC1t
    <3>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <1>r DEF Opt
    <3>2. DorN(q, b) BY <3>0, <1>r, <1>c DEF TC, PC1t
    <3>3. DorN(q, b)' BY <3>2, <1>d
    <3>. QED BY <3>0, <3>3, <1>r
  <2>6. PC2t(s)'
    <3>. SUFFICES ASSUME NEW q \in w'[s].uploads \cap w'[s].upDone
                  PROVE  DorN(q, w[s].snap[q])' BY DEF PC2t
    <3>p. q \in Paths /\ q \in InstallMine(s) /\ q \notin w[s].gone BY <1>r, <1>y DEF InstallMine
    <3>0. PICK sn \in minted : sn = w[s].snap[q] BY <1>y, <1>r, <3>p
    <3>1. doc'[q] = sn BY <3>p, <3>0, <1>1 DEF InstallInst
    <3>2. Derives(sn, sn)' /\ sn # Nil BY <1>y, NilHandle DEF Derives
    <3>. QED BY <3>0, <3>1, <3>2, <1>r DEF DorN
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF TC
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Upload_C ==
  ASSUME IndC, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Upload_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>y. /\ \A q \in Paths : /\ w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted)
                          /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles /\ w[s] \in Writer
  BY TreeTy DEF IndC
<1>c. TC(s) BY DEF IndC, TreeC
<1>k. w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
<1>e. UNCHANGED <<doc, conflicts, udel, acked, nextGen>> BY DEF Upload
<1>h0. PICK h0 \in minted : h0 = w[s].snap[p] BY <1>k, <1>y
<1>1. CASE h0 \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ UNCHANGED <<minted, base, orig>> BY <1>1, <1>h0 DEF Upload
  <2>hs. HStep BY <1>m, <1>u, <2>1, HStepSame DEF IndC
  <2>n. nextGen \in Nat BY <1>0 DEF TypeOK
  <2>a. HistC' BY <2>1, <1>e, <1>u, <2>n, AncSame, HCSame, MintedHandles DEF IndC
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN UploadW(s, p) ELSE w[t] BY <1>0, <2>1, WriteAny
  <2>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>e, <2>hs, DorNKeep DEF IndC
  <2>r. /\ w'[s].local = w[s].local /\ w'[s].baseline = w[s].baseline /\ w'[s].snap = w[s].snap
        /\ w'[s].uploads = w[s].uploads /\ w'[s].pc = "scanned"
    BY <2>w, <1>k DEF UploadW
  <2>s. TC(s)'
    <3>1. UBt(s)' BY <2>r DEF UBt, Bar
    <3>2. LDBt(s)'
      <4>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # Nil, w'[s].baseline[q] # Nil
                    PROVE  Derives(w[s].local[q], w[s].baseline[q])' BY DEF LDBt
      <4>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].baseline[q] BY <1>y, <2>r DEF Opt
      <4>2. Derives(l, b) BY <4>0, <2>r, <1>c DEF TC, LDBt
      <4>3. Derives(l, b)' BY <4>2, <2>hs, DerivesStep DEF IndC
      <4>. QED BY <4>0, <4>3, <2>r
    <3>3. LDSt(s)'
      <4>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].local[q] # Nil
                    PROVE  Derives(w[s].local[q], w[s].snap[q])' BY DEF LDSt
      <4>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].snap[q] BY <1>y, <2>r DEF Opt
      <4>2. Derives(l, b) BY <4>0, <2>r, <1>k, <1>c DEF TC, LDSt, Bar
      <4>3. Derives(l, b)' BY <4>2, <2>hs, DerivesStep DEF IndC
      <4>. QED BY <4>0, <4>3, <2>r
    <3>4. SDBt(s)'
      <4>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].baseline[q] # Nil
                    PROVE  Derives(w[s].snap[q], w[s].baseline[q])' BY DEF SDBt
      <4>0. PICK l \in Opt(Handles), b \in minted : l = w[s].snap[q] /\ b = w[s].baseline[q] BY <1>y, <2>r DEF Opt
      <4>2. Derives(l, b) BY <4>0, <2>r, <1>k, <1>c DEF TC, SDBt, Bar
      <4>3. Derives(l, b)' BY <4>2, <2>hs, DerivesStep DEF IndC
      <4>. QED BY <4>0, <4>3, <2>r
    <3>5. PC1t(s)'
      <4>. SUFFICES ASSUME NEW q \in Paths, w'[s].baseline[q] # Nil, w'[s].local[q] = w'[s].baseline[q]
                    PROVE  DorN(q, w[s].baseline[q])' BY DEF PC1t
      <4>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <2>r DEF Opt
      <4>2. DorN(q, b) BY <4>0, <2>r, <1>c DEF TC, PC1t
      <4>3. DorN(q, b)' BY <4>2, <2>d
      <4>. QED BY <4>0, <4>3, <2>r
    <3>6. PC2t(s)' BY <2>r DEF PC2t
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6 DEF TC
  <2>b. TreeC' BY <1>m, <2>hs, <2>w, <2>d, <2>s, TCWriter
  <2>. QED BY <2>a, <2>b
<1>2. CASE h0 \in upped
  <2>g. copies \in Nat /\ copies < MaxCopies /\ nextGen \in Nat
    BY <1>2, <1>h0 DEF Upload, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts, TypeOK
  <2>c. PICK c \in Handles : c = <<p, MaxMint + copies + 1>>
    BY <2>g, MaxMintNat, MaxCopiesNat DEF Handles, Gens, Seed
  <2>1. /\ minted' = minted \cup {c} /\ base' = [base EXCEPT ![c] = base[h0]]
        /\ orig' = [orig EXCEPT ![c] = Content(h0)] /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
    BY <1>2, <1>h0, <2>c DEF Upload
  <2>4. c \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
      BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>2. nextGen <= MaxMint + 1 BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>. QED BY <3>1, <3>2, <2>c, <2>g, MaxMintNat DEF Gen
  <2>ty. base \in [Handles -> Opt(Handles)] /\ orig \in [Handles -> Opt(Handles)]
    BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
  <2>f. MintFacts(c) BY <2>1, <2>4, <2>ty, <1>y DEF MintFacts
  <2>hm. base[h0] \in Opt(minted) /\ Content(h0) \in minted /\ orig[Content(h0)] = Nil
    BY ContentMinted DEF IndC, IndM5, M5, Hist, HistMinted
  <2>7. base'[c] = base[h0] /\ orig'[c] = Content(h0) BY <2>1, <2>ty
  <2>8. \A z \in Handles : z # c => base'[z] = base[z] /\ orig'[z] = orig[z] BY <2>1, <2>ty
  <2>hs. HStep BY <1>m, <1>u, <2>f, <2>7, <2>8, <2>hm, HStepMint DEF IndC, Opt
  <2>a. HistC' BY <1>m, <1>u, <2>f, <2>7, <1>e, HCCopy DEF IndC
  <2>an. anc'[c] = anc[h0]
    <3>1. anc'[c] = AncOf(base[h0]) BY <1>u, <2>f, <2>7, MintAnc DEF IndC
    <3>2. anc[h0] = AncOf(base[h0]) BY <1>y DEF IndC, IndM5, M5, Hist, HistAnc
    <3>. QED BY <3>1, <3>2
  <2>cc. Content(c)' = Content(h0) BY <2>7, <2>hm, <1>y, NilHandle DEF Content
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN UploadCopyW(s, p, c) ELSE w[t] BY <1>0, <2>1, WriteAny
  <2>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>e, <2>hs, DorNKeep DEF IndC
  <2>r. /\ w'[s].local = [w[s].local EXCEPT ![p] = IF w[s].local[p] = h0 THEN c ELSE w[s].local[p]]
        /\ w'[s].baseline = w[s].baseline /\ w'[s].snap = [w[s].snap EXCEPT ![p] = c]
        /\ w'[s].uploads = w[s].uploads /\ w'[s].pc = "scanned"
    BY <2>w, <1>k, <1>h0 DEF UploadCopyW
  <2>lt. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
  <2>l. \A q \in Paths : /\ w'[s].local[q] = IF q = p /\ w[s].local[p] = h0 THEN c ELSE w[s].local[q]
                         /\ w'[s].snap[q] = IF q = p THEN c ELSE w[s].snap[q]
    BY <2>r, <2>lt
  <2>s. TC(s)'
    <3>1. UBt(s)' BY <2>r DEF UBt, Bar
    <3>2. LDBt(s)'
      <4>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # Nil, w'[s].baseline[q] # Nil
                    PROVE  Derives(w[s].local[q], w[s].baseline[q])' BY DEF LDBt
      <4>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <2>r DEF Opt
      <4>1. CASE q = p /\ w[s].local[p] = h0
        <5>0. h0 # Nil /\ b # Nil BY <1>y, NilHandle
        <5>1. Derives(h0, b) BY <5>0, <4>0, <4>1, <2>r, <1>h0, <1>c DEF TC, LDBt
        <5>2. Derives(c, b)' BY <5>1, <2>an, <2>cc, <2>hs, DerivesCopyFrom DEF IndC
        <5>. QED BY <5>2, <4>0, <4>1, <2>l, <2>r
      <4>2. CASE ~(q = p /\ w[s].local[p] = h0)
        <5>0. PICK l \in Opt(Handles) : l = w[s].local[q] BY <1>y DEF Opt
        <5>1. Derives(l, b) BY <5>0, <4>0, <4>2, <2>l, <2>r, <1>c DEF TC, LDBt
        <5>2. Derives(l, b)' BY <5>1, <2>hs, DerivesStep DEF IndC
        <5>. QED BY <5>0, <5>2, <4>0, <4>2, <2>l, <2>r
      <4>. QED BY <4>1, <4>2
    <3>3. LDSt(s)'
      <4>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].local[q] # Nil
                    PROVE  Derives(w[s].local[q], w[s].snap[q])' BY DEF LDSt
      <4>p. q \in Paths /\ q \in w[s].uploads BY <2>r, <1>y
      <4>1. CASE q = p /\ w[s].local[p] = h0
        <5>1. Derives(c, c)' BY DEF Derives
        <5>. QED BY <5>1, <4>1, <2>l
      <4>2. CASE q = p /\ w[s].local[p] # h0
        <5>0. PICK l \in minted : l = w[s].local[p] BY <4>2, <4>p, <2>l, <1>y DEF Opt
        <5>n. l # Nil BY <5>0, <1>y, NilHandle
        <5>1. Derives(l, h0) BY <5>n, <5>0, <4>2, <4>p, <1>h0, <1>k, <1>c DEF TC, LDSt, Bar
        <5>2. Derives(l, c)' BY <5>1, <2>cc, <2>hs, DerivesToCopy DEF IndC
        <5>. QED BY <5>0, <5>2, <4>2, <2>l
      <4>3. CASE q # p
        <5>0. PICK l \in Opt(Handles), b \in minted : l = w[s].local[q] /\ b = w[s].snap[q] BY <4>p, <1>y DEF Opt
        <5>1. Derives(l, b) BY <5>0, <4>3, <4>p, <2>l, <1>k, <1>c DEF TC, LDSt, Bar
        <5>2. Derives(l, b)' BY <5>1, <2>hs, DerivesStep DEF IndC
        <5>. QED BY <5>0, <5>2, <4>3, <4>p, <2>l
      <4>. QED BY <4>1, <4>2, <4>3
    <3>4. SDBt(s)'
      <4>. SUFFICES ASSUME w'[s].pc \in Bar, NEW q \in w'[s].uploads, w'[s].baseline[q] # Nil
                    PROVE  Derives(w[s].snap[q], w[s].baseline[q])' BY DEF SDBt
      <4>p. q \in Paths /\ q \in w[s].uploads BY <2>r, <1>y
      <4>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <2>r, <4>p DEF Opt
      <4>1. CASE q = p
        <5>1. Derives(h0, b) BY <4>0, <4>1, <4>p, <1>h0, <1>k, <2>r, <1>c DEF TC, SDBt, Bar
        <5>2. Derives(c, b)' BY <5>1, <2>an, <2>cc, <2>hs, DerivesCopyFrom DEF IndC
        <5>. QED BY <5>2, <4>0, <4>1, <2>l, <2>r
      <4>2. CASE q # p
        <5>0. PICK l \in Opt(Handles) : l = w[s].snap[q] BY <4>p, <1>y DEF Opt
        <5>1. Derives(l, b) BY <5>0, <4>0, <4>2, <4>p, <2>r, <1>k, <1>c DEF TC, SDBt, Bar
        <5>2. Derives(l, b)' BY <5>1, <2>hs, DerivesStep DEF IndC
        <5>. QED BY <5>0, <5>2, <4>0, <4>2, <4>p, <2>l, <2>r
      <4>. QED BY <4>1, <4>2
    <3>5. PC1t(s)'
      <4>. SUFFICES ASSUME NEW q \in Paths, w'[s].baseline[q] # Nil, w'[s].local[q] = w'[s].baseline[q]
                    PROVE  DorN(q, w[s].baseline[q])' BY DEF PC1t
      <4>0. PICK b \in minted : b = w[s].baseline[q] BY <1>y, <2>r DEF Opt
      <4>1. ~(q = p /\ w[s].local[p] = h0) BY <4>0, <2>l, <2>r, <2>4
      <4>2. DorN(q, b) BY <4>0, <4>1, <2>l, <2>r, <1>c DEF TC, PC1t
      <4>3. DorN(q, b)' BY <4>2, <2>d
      <4>. QED BY <4>0, <4>3, <2>r
    <3>6. PC2t(s)' BY <2>r DEF PC2t
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6 DEF TC
  <2>b. TreeC' BY <1>m, <2>hs, <2>w, <2>d, <2>s, TCWriter
  <2>. QED BY <2>a, <2>b
<1>. QED BY <1>1, <1>2

LEMMA RescopeSecond_C ==
  ASSUME IndC, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY RescopeSecond_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>p. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>1. /\ w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY <1>p DEF RescopeSecond, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RescopeSecondW(s, wfail) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>sh. RescopeUnciteFirst /\ WidenKeepsLocal BY ShippedShape DEF Shipped
<1>s. TC(s)'
  <2>1. w'[s] = RescopeSecondW(s, wfail) BY <1>w
  <2>2. w[s].pc = "idle" /\ w[s].uploads = {} BY DEF RescopeSecond, IndC, TreeC, TC, UBt, Bar
  <2>3. \A q \in Paths : TakeShape(s, q)
    <3>. SUFFICES ASSUME NEW q \in Paths PROVE TakeShape(s, q) OBVIOUS
    <3>l. w'[s].local[q] = IF q \in RescopeFetch(s, wfail) /\ RescopeLocal1(s)[q] = Nil
                           THEN doc[q] ELSE RescopeLocal1(s)[q]
      BY <2>1, <1>sh DEF RescopeSecondW
    <3>b. w'[s].baseline[q] = IF q \in RescopeFetch(s, wfail) THEN doc[q] ELSE w[s].baseline[q]
      BY <2>1, <1>sh DEF RescopeSecondW, RescopeBase1
    <3>1. RescopeLocal1(s)[q] = IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q] BY <1>sh DEF RescopeLocal1
    <3>2. CASE q \in RescopeFetch(s, wfail) /\ RescopeLocal1(s)[q] = Nil BY <3>2, <3>l, <3>b DEF TakeShape
    <3>3. CASE q \in RescopeFetch(s, wfail) /\ RescopeLocal1(s)[q] # Nil
      <4>1. RescopeLocal1(s)[q] = w[s].local[q] /\ w[s].local[q] # Nil BY <3>3, <3>1
      <4>2. q \in RescopeAdd(s) /\ q \notin RescopeKept(s) BY <3>3 DEF RescopeFetch, RescopeFetch0
      <4>3. doc[q] # Nil BY <4>2 DEF RescopeAdd
      <4>4. Content(w[s].local[q]) = Content(doc[q]) BY <4>1, <4>2, <1>sh DEF RescopeKept
      <4>. QED BY <3>3, <3>l, <3>b, <4>1, <4>3, <4>4 DEF TakeShape
    <3>4. CASE q \notin RescopeFetch(s, wfail) BY <3>4, <3>l, <3>b, <3>1 DEF TakeShape
    <3>. QED BY <3>2, <3>3, <3>4
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>1, <2>2 DEF RescopeSecondW, Bar
  <2>. QED BY <2>3, <2>4, <1>1, <1>m, <1>hs, <1>d, TCTake
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

------------------------------------------------------------------------------
(* Part C: rows 15-17 -- the answer's steps, deposal and the fence.         *)

LEMMA Touch_C ==
  ASSUME IndC, NEW s \in Writers, Touch(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Touch_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = TouchW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Touch, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN TouchW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = TouchW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>. QED BY <2>1, <2>2, <1>m, <1>hs, <1>d, TCFields DEF TouchW, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA Honor_C ==
  ASSUME IndC, NEW s \in Writers, Honor(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Honor_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = HonorW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Honor, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN HonorW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = HonorW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>. QED BY <2>1, <2>2, <1>m, <1>hs, <1>d, TCFields DEF HonorW, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA HonorReader_C ==
  ASSUME IndC, NEW s \in Writers, HonorReader(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY HonorReader_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = HonorReaderW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF HonorReader, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN HonorReaderW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = HonorReaderW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>. QED BY <2>1, <2>2, <1>m, <1>hs, <1>d, TCFields DEF HonorReaderW, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

LEMMA RetireByAck_C ==
  ASSUME IndC, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY RetireByAck_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w' = [w EXCEPT ![s] = RetireByAckW(s)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF RetireByAck, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RetireByAckW(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = RetireByAckW(s) BY <1>w
  <2>2. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>. QED BY <2>1, <2>2, <1>m, <1>hs, <1>d, TCFields DEF RetireByAckW, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

\* Two trees move: the taker scanned -> claimed (both in the barrier), the
\* deposed holder only marked.
LEMMA Depose_C ==
  ASSUME IndC, NEW t \in Writers, Depose(t), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Depose_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Depose, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>w. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>f. \A u \in Writers : /\ w'[u].local = w[u].local /\ w'[u].baseline = w[u].baseline /\ w'[u].snap = w[u].snap
                         /\ w'[u].uploads = w[u].uploads /\ w'[u].upDone = w[u].upDone
                         /\ w'[u].pc = IF u = t THEN "claimed" ELSE w[u].pc
  BY <1>1, <1>w DEF DeposeW, DeposedW
<1>b. TreeC'
  <2>. SUFFICES ASSUME NEW u \in Writers PROVE TC(u)' BY DEF TreeC
  <2>1. w[u].pc \notin Bar => w[u].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>. QED BY <2>1, <1>1, <1>f, <1>m, <1>hs, <1>d, TCFields DEF Bar
<1>. QED BY <1>a, <1>b

\* Before the CAS the barrier is abandoned (the tree and baseline kept);
\* after it, the retire set dropped.
LEMMA Fence_C ==
  ASSUME IndC, NEW s \in Writers, Fence(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Fence_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>k. UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>> BY DEF Fence, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>k, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>k, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>k, <1>hs, DorNKeep DEF IndC
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN FenceBeforeW(s) ELSE w[t] BY <1>0, <2>1, WriteAny
  <2>2. w'[s] = FenceBeforeW(s) BY <2>w
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>2 DEF FenceBeforeW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>2 DEF FenceBeforeW, Bar
  <2>s. TC(s)' BY <2>3, <2>4, <1>k, <1>m, <1>hs, <1>d, TCTake
  <2>. QED BY <1>a, <2>w, <2>s, <1>m, <1>hs, <1>d, TCWriter
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN FenceAfterW(s) ELSE w[t] BY <1>0, <2>1, WriteAny
  <2>2. w'[s] = FenceAfterW(s) BY <2>w
  <2>3. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>s. TC(s)' BY <2>2, <2>3, <1>m, <1>hs, <1>d, TCFields DEF FenceAfterW, Bar
  <2>. QED BY <1>a, <2>w, <2>s, <1>m, <1>hs, <1>d, TCWriter
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* Part C: G elapses, a reader loads, a writer reaps -- no history, no tree. *)

LEMMA AncUpdateSame ==
  ASSUME IndM5, UNCHANGED <<anc, base>> PROVE AncUpdate
BY DEF AncUpdate, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK

LEMMA Quiet_C ==
  ASSUME IndC, IndM5', UNCHANGED <<anc, minted, base, orig, nextGen, doc, conflicts, udel, acked, w>>
  PROVE  HistC' /\ TreeC'
<1>u. AncUpdate BY AncUpdateSame DEF IndC
<1>hs. HStep BY <1>u, HStepSame DEF IndC
<1>n. nextGen \in Nat BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>a. HistC' BY <1>n, HCSame, MintedHandles DEF IndC
<1>d. \A q \in Paths, x \in minted : DorN(q, x) => DorN(q, x)' BY <1>hs, DorNKeep DEF IndC
<1>b. TreeC' BY <1>hs, <1>d, TCKeepAll
<1>. QED BY <1>a, <1>b

LEMMA Age_C == ASSUME IndC, Age, UNCHANGED anc PROVE HistC' /\ TreeC'
<1>m. IndM5' BY Age_M5 DEF IndC
<1>. QED BY <1>m, Quiet_C DEF Age, aux
LEMMA RLoad_C == ASSUME IndC, RLoad, UNCHANGED anc PROVE HistC' /\ TreeC'
<1>m. IndM5' BY RLoad_M5 DEF IndC
<1>. QED BY <1>m, Quiet_C DEF RLoad, aux
LEMMA Reap_C == ASSUME IndC, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc PROVE HistC' /\ TreeC'
<1>m. IndM5' BY Reap_M5 DEF IndC
<1>. QED BY <1>m, Quiet_C DEF Reap, aux

------------------------------------------------------------------------------
(* Part C: the invariant.                                                   *)

LEMMA Init_C == Init => IndC
<1>. SUFFICES ASSUME Init PROVE IndC OBVIOUS
<1>1. IndM5 BY Init_M5
<1>2. anc = [h \in Handles |-> {}] /\ orig = [h \in Handles |-> Nil] /\ nextGen = Seed + 1
      /\ minted = {<<p, Seed>> : p \in Paths \ Free}
  BY DEF Init
<1>t. minted \subseteq Handles BY <1>1, MintedHandles
<1>3. HistC
  <2>1. RankC BY <1>2, <1>t DEF RankC
  <2>2. OrigC BY <1>2 DEF OrigC, Gen, Seed
  <2>3. SameC BY <1>2, <1>t DEF SameC
  <2>4. ClosedC BY <1>2, <1>t DEF ClosedC
  <2>. QED BY <2>1, <2>2, <2>3, <2>4 DEF HistC
<1>4. TreeC
  <2>1. \A u \in Writers : w[u] = WriterInit BY DEF Init
  <2>. QED BY <2>1 DEF TreeC, TC, UBt, LDBt, LDSt, SDBt, PC1t, PC2t, WriterInit, Bar
<1>. QED BY <1>1, <1>3, <1>4 DEF IndC

LEMMA Next_C == IndC /\ Next => IndC'
<1>. SUFFICES ASSUME IndC, Next PROVE IndC' OBVIOUS
<1>m. IndM5' BY Next_M5 DEF IndC
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE HistC' /\ TreeC' BY <3>1, GPut_C
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE HistC' /\ TreeC' BY <3>2, GCas_C
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE HistC' /\ TreeC' BY <3>3, GDelete_C
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE HistC' /\ TreeC' BY <3>4, GRename_C
    <3>5. ASSUME GRenameFinish PROVE HistC' /\ TreeC' BY <3>5, GRenameFinish_C
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5, <1>m DEF GatewayStep, IndC
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE HistC' /\ TreeC' BY <3>1, Checkout_C
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE HistC' /\ TreeC' BY <3>2, Consume_C
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE HistC' /\ TreeC' BY <3>3, Scan_C
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE HistC' /\ TreeC' BY <3>4, Skip_C
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE HistC' /\ TreeC' BY <3>5, PullOnly_C
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE HistC' /\ TreeC' BY <3>6, Claim_C
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE HistC' /\ TreeC' BY <3>7, Verify_C
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE HistC' /\ TreeC' BY <3>8, Install_C
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE HistC' /\ TreeC' BY <3>9, Collect_C
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE HistC' /\ TreeC' BY <3>10, Finish_C
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE HistC' /\ TreeC' BY <3>11, Restart_C
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE HistC' /\ TreeC' BY <3>12, Sync_C
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE HistC' /\ TreeC' BY <3>13, RescopeBegin_C
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE HistC' /\ TreeC' BY <3>14, RescopeFirst_C
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE HistC' /\ TreeC' BY <3>15, RescopeSecond_C
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE HistC' /\ TreeC' BY <3>16, RPullRead_C
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE HistC' /\ TreeC' BY <3>17, RPullSync_C
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE HistC' /\ TreeC' BY <3>18, Edit_C
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE HistC' /\ TreeC' BY <3>19, Delete_C
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE HistC' /\ TreeC' BY <3>20, Upload_C
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE HistC' /\ TreeC' BY <3>21, Sweep_C
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE HistC' /\ TreeC' BY <3>22, Touch_C
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE HistC' /\ TreeC' BY <3>23, Honor_C
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE HistC' /\ TreeC' BY <3>24, HonorReader_C
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE HistC' /\ TreeC' BY <3>25, RetireByAck_C
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE HistC' /\ TreeC' BY <3>26, Depose_C
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE HistC' /\ TreeC' BY <3>27, Fence_C
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27, <1>m
         DEF WriterStep, IndC
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_C, <1>m DEF IndC
  <2>2. CASE RLoad BY <2>2, RLoad_C, <1>m DEF IndC
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndC' BY <2>3, Reap_C, <1>m DEF IndC
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

HCT == HistC /\ TreeC

LEMMA CStep == IndM5 /\ IndM5' /\ HCT /\ [Next]_vars => HCT'
<1>. SUFFICES ASSUME IndM5, IndM5', HCT, [Next]_vars PROVE HCT' OBVIOUS
<1>i. IndC BY DEF IndC, HCT
<1>1. CASE Next BY <1>1, <1>i, Next_C DEF IndC, HCT
<1>2. CASE UNCHANGED vars
  <2>u. UNCHANGED <<anc, minted, base, orig, nextGen, doc, conflicts, udel, acked, w>> BY <1>2 DEF vars, bucket, aux
  <2>. QED BY <2>u, <1>i, Quiet_C DEF HCT
<1>. QED BY <1>1, <1>2

LEMMA CInvariant == Spec => []IndC
<1>1. Init => HCT BY Init_C DEF IndC, HCT
<1>2. Spec => []HCT BY <1>1, CStep, M5Invariant, PTL DEF Spec
<1>. QED BY <1>2, M5Invariant, PTL DEF IndC, HCT

------------------------------------------------------------------------------
(* Part C: NR5.  A step moves a tree's baseline onto doc[p] from a clean    *)
(* baseline (an owed take), from bytes holding the document's (a            *)
(* convergence), or from the snapshot it installed (a finish); none steps   *)
(* back unnamed.                                                            *)

TakeWhy(s, p) ==
  \/ w[s].local[p] = w[s].baseline[p]
  \/ w[s].local[p] # Nil /\ Content(w[s].local[p]) = Content(doc[p])
  \/ w[s].pc = "cased" /\ p \in w[s].uploads /\ w[s].snap[p] = doc[p]
NRPre == \A u \in Writers, p \in Paths :
           (/\ w'[u].baseline[p] = doc[p] /\ doc[p] # Nil /\ w[u].baseline[p] # Nil
            /\ w'[u].baseline[p] # w[u].baseline[p])
             => TakeWhy(u, p)
NRAct == \A s \in Writers, p \in Paths : ~(BackR5(s, p) /\ Takes(s, p))

\* Bytes with a version's content derive as that version does.
LEMMA DerivesSameBytes ==
  ASSUME IndM5, HistC, NEW k \in minted, NEW k2 \in minted, NEW x \in minted,
         Content(k) = Content(k2), Derives(k, x)
  PROVE  Derives(k2, x)
<1>t. minted \subseteq Handles BY MintedHandles
<1>1. PICK u \in DS(k) : Content(u) = Content(x) BY <1>t, DerivesDS
<1>2. anc[k] = anc[k2] BY DEF HistC, SameC
<1>3. CASE u = k
  <2>1. Content(k2) = Content(x) BY <1>1, <1>3
  <2>. QED BY <2>1, <1>t, DerivesDS DEF DS
<1>4. CASE u \in anc[k]
  <2>1. u \in DS(k2) BY <1>2, <1>4 DEF DS
  <2>. QED BY <2>1, <1>1, <1>t, DerivesDS
<1>. QED BY <1>3, <1>4 DEF DS

LEMMA WhyNotBack ==
  ASSUME IndC, NEW s \in Writers, NEW p \in Paths, TakeWhy(s, p)
  PROVE  ~BackR5(s, p)
<1>. SUFFICES ASSUME BackR5(s, p) PROVE FALSE OBVIOUS
<1>0. /\ doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[p])
      /\ Content(doc[p]) # Content(w[s].baseline[p]) /\ ~NamedAt(p, w[s].baseline[p])
  BY DEF BackR5
<1>y. /\ w[s].local[p] \in Opt(minted) /\ w[s].baseline[p] \in Opt(minted) /\ w[s].snap[p] \in Opt(minted)
      /\ doc[p] \in Opt(minted) /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
  BY TreeTy DEF IndC
<1>b. PICK b \in minted : b = w[s].baseline[p] BY <1>0, <1>y DEF Opt
<1>d. PICK d \in minted : d = doc[p] BY <1>0, <1>y DEF Opt
<1>c. TC(s) BY DEF IndC, TreeC
<1>1. Derives(d, b)
  <2>1. CASE w[s].local[p] = w[s].baseline[p]
    <3>1. DorN(p, b) BY <2>1, <1>0, <1>b, <1>c DEF TC, PC1t
    <3>. QED BY <3>1, <1>0, <1>b, <1>d DEF DorN
  <2>2. CASE w[s].local[p] # Nil /\ Content(w[s].local[p]) = Content(doc[p])
    <3>0. PICK l \in minted : l = w[s].local[p] BY <2>2, <1>y DEF Opt
    <3>1. Derives(l, b) BY <3>0, <2>2, <1>0, <1>b, <1>c DEF TC, LDBt
    <3>. QED BY <3>0, <3>1, <2>2, <1>d, DerivesSameBytes DEF IndC
  <2>3. CASE w[s].pc = "cased" /\ p \in w[s].uploads /\ w[s].snap[p] = doc[p]
    <3>1. Derives(w[s].snap[p], w[s].baseline[p]) BY <2>3, <1>0, <1>c DEF TC, SDBt, Bar
    <3>. QED BY <3>1, <2>3, <1>b, <1>d
  <2>. QED BY <2>1, <2>2, <2>3 DEF TakeWhy
<1>. QED BY <1>1, <1>0, <1>b, <1>d, NotOlder DEF IndC

LEMMA NRFrom ==
  ASSUME IndC, NRPre PROVE NRAct
<1>. SUFFICES ASSUME NEW s \in Writers, NEW p \in Paths, BackR5(s, p), Takes(s, p) PROVE FALSE BY DEF NRAct
<1>0. doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Content(doc[p]) # Content(w[s].baseline[p]) BY DEF BackR5
<1>1. w'[s].baseline[p] = doc[p] BY DEF Takes
<1>2. w'[s].baseline[p] # w[s].baseline[p] BY <1>0, <1>1
<1>3. TakeWhy(s, p) BY <1>0, <1>1, <1>2 DEF NRPre
<1>. QED BY <1>3, WhyNotBack

\* A step that moves no baseline.
LEMMA NRSame ==
  ASSUME \A u \in Writers : w'[u].baseline = w[u].baseline PROVE NRPre
BY DEF NRPre

\* A writer step on s: only s's baseline can move.
LEMMA NRWrite ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         \A p \in Paths : (/\ R.baseline[p] = doc[p] /\ doc[p] # Nil /\ w[s].baseline[p] # Nil
                           /\ R.baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
  PROVE  NRPre
<1>1. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY WriteAny
<1>. QED BY <1>1 DEF NRPre


LEMMA Next_NR == IndC /\ Next => NRPre
<1>. SUFFICES ASSUME IndC, Next PROVE NRPre OBVIOUS
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>sh. Shipped BY ShippedShape
<1>g. CASE GatewayStep
  <2>1. w' = w
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE w' = w BY <3>1 DEF GPut
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE w' = w BY <3>2 DEF GCas
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE w' = w BY <3>3 DEF GDelete
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE w' = w BY <3>4 DEF GRename
    <3>5. CASE GRenameFinish BY <3>5 DEF GRenameFinish
    <3>. QED BY <1>g, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>. QED BY <2>1, NRSame
<1>q. CASE Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h) \/ Sweep(s, h)
  <2>1. w' = w BY <1>q DEF Age, RLoad, Reap, Sweep
  <2>. QED BY <2>1, NRSame
<1>s1. ASSUME NEW s \in Writers, Scan(s) PROVE NRPre
  <2>0. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY <1>s1 DEF Scan
  <2>1. w' = [w EXCEPT ![s] = ScanW(s, dels)] BY <2>0
  <2>2. ScanW(s, dels).baseline = w[s].baseline BY DEF ScanW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s2. ASSUME NEW s \in Writers, Skip(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = SkipW(s)] BY <1>s2 DEF Skip
  <2>2. SkipW(s).baseline = w[s].baseline BY DEF SkipW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s3. ASSUME NEW s \in Writers, PullOnly(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY <1>s3 DEF PullOnly
  <2>2. PullOnlyW(s).baseline = w[s].baseline BY DEF PullOnlyW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s4. ASSUME NEW s \in Writers, Claim(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = ClaimW(s)] BY <1>s4 DEF Claim
  <2>2. ClaimW(s).baseline = w[s].baseline BY DEF ClaimW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s5. ASSUME NEW s \in Writers, Verify(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = VerifyW(s)] BY <1>s5 DEF Verify
  <2>2. VerifyW(s).baseline = w[s].baseline BY DEF VerifyW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s6. ASSUME NEW s \in Writers, Install(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = InstallW(s)] BY <1>s6 DEF Install
  <2>2. InstallW(s).baseline = w[s].baseline BY DEF InstallW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s7. ASSUME NEW s \in Writers, Collect(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = CollectW(s)] BY <1>s7 DEF Collect
  <2>2. CollectW(s).baseline = w[s].baseline BY DEF CollectW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s8. ASSUME NEW s \in Writers, Restart(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = RestartW(s)] BY <1>s8 DEF Restart
  <2>2. RestartW(s).baseline = w[s].baseline BY DEF RestartW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s9. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE NRPre
  <2>0. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY <1>s9 DEF RescopeBegin
  <2>1. w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY <2>0
  <2>2. RescopeBeginW(s, T).baseline = w[s].baseline BY DEF RescopeBeginW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s10. ASSUME NEW s \in Writers, RPullRead(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = RPullReadW(s)] BY <1>s10 DEF RPullRead
  <2>2. RPullReadW(s).baseline = w[s].baseline BY DEF RPullReadW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s11. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY <1>s11 DEF Edit
  <2>2. EditW(s, p).baseline = w[s].baseline BY DEF EditW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>s12. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY <1>s12 DEF Delete
  <2>2. DeleteW(s, p).baseline = w[s].baseline BY DEF DeleteW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>up. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE NRPre
  <2>1. CASE w[s].snap[p] \notin upped
    <3>1. w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>up, <2>1 DEF Upload
    <3>2. UploadW(s, p).baseline = w[s].baseline BY DEF UploadW
    <3>. QED BY <3>1, <3>2, <1>0, NRWrite
  <2>2. CASE w[s].snap[p] \in upped
    <3>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, <<p, MaxMint + copies + 1>>)] BY <1>up, <2>2 DEF Upload
    <3>2. UploadCopyW(s, p, <<p, MaxMint + copies + 1>>).baseline = w[s].baseline BY DEF UploadCopyW
    <3>. QED BY <3>1, <3>2, <1>0, NRWrite
  <2>. QED BY <2>1, <2>2
<1>co. ASSUME NEW s \in Writers, Consume(s) PROVE NRPre
  <2>ck. PICK ck \in CarryClocks(s) :
            IF CheapPath(s)
            THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
            ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
    BY <1>co DEF Consume
  <2>1. CASE CheapPath(s)
    <3>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <2>ck, <2>1
    <3>2. ConsumeCheapW(s, ck).baseline = w[s].baseline BY DEF ConsumeCheapW
    <3>. QED BY <3>1, <3>2, <1>0, NRWrite
  <2>2. CASE ~CheapPath(s)
    <3>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <2>ck, <2>2
    <3>1. \A p \in Paths : (/\ ConsumeW(s, fail, ck).baseline[p] = doc[p] /\ doc[p] # Nil /\ w[s].baseline[p] # Nil
                            /\ ConsumeW(s, fail, ck).baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
      <4>. SUFFICES ASSUME NEW p \in Paths, ConsumeW(s, fail, ck).baseline[p] # w[s].baseline[p], doc[p] # Nil
                    PROVE  TakeWhy(s, p) OBVIOUS
      <4>1. p \in ConsumeTaken(s, fail) BY DEF ConsumeW
      <4>2. CASE p \in ConsumeOwed(s) BY <4>2 DEF ConsumeOwed, Owed, TakeWhy
      <4>3. CASE p \in ConsumeConv(s) BY <4>3 DEF ConsumeConv, Converges, TakeWhy
      <4>. QED BY <4>1, <4>2, <4>3 DEF ConsumeTaken
    <3>. QED BY <3>0, <3>1, <1>0, NRWrite
  <2>. QED BY <2>1, <2>2
<1>sy. ASSUME NEW s \in Writers, Sync(s) PROVE NRPre
  <2>0. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY <1>sy DEF Sync
  <2>1. \A p \in Paths : (/\ SyncW(s, fail).baseline[p] = doc[p] /\ doc[p] # Nil /\ w[s].baseline[p] # Nil
                          /\ SyncW(s, fail).baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
    BY DEF SyncW, SyncBl, SyncOwed, SyncAll, Owed, TakeWhy
  <2>. QED BY <2>0, <2>1, <1>0, NRWrite
<1>rs. ASSUME NEW s \in Writers, RPullSync(s) PROVE NRPre
  <2>0. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY <1>rs DEF RPullSync
  <2>1. \A p \in Paths : (/\ RPullSyncW(s, fail).baseline[p] = doc[p] /\ doc[p] # Nil /\ w[s].baseline[p] # Nil
                          /\ RPullSyncW(s, fail).baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
    BY DEF RPullSyncW, SyncBl, SyncOwed, SyncAll, Owed, TakeWhy
  <2>. QED BY <2>0, <2>1, <1>0, NRWrite
<1>fi. ASSUME NEW s \in Writers, Finish(s) PROVE NRPre
  <2>0. w' = [w EXCEPT ![s] = FinishW(s)] /\ w[s].pc = "cased" BY <1>fi DEF Finish
  <2>1. \A p \in Paths : (/\ FinishW(s).baseline[p] = doc[p] /\ doc[p] # Nil /\ w[s].baseline[p] # Nil
                          /\ FinishW(s).baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
    BY <2>0 DEF FinishW, TakeWhy
  <2>. QED BY <2>0, <2>1, <1>0, NRWrite
<1>ck. ASSUME NEW s \in Writers, Checkout(s) PROVE NRPre
  <2>0. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY <1>ck DEF Checkout
  <2>1. w[s] = WriterInit BY <1>ck DEF Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
  <2>2. \A p \in Paths : w[s].baseline[p] = Nil BY <2>1 DEF WriterInit
  <2>. QED BY <2>0, <2>2, <1>0, NRWrite
<1>r1. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE NRPre
  <2>0. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY <1>r1 DEF RescopeFirst
  <2>1. \A p \in Paths : RescopeFirstW(s).baseline[p] \in {w[s].baseline[p], Nil}
    BY <1>sh DEF RescopeFirstW, Shipped
  <2>2. \A p \in Paths : (/\ RescopeFirstW(s).baseline[p] = doc[p] /\ doc[p] # Nil /\ w[s].baseline[p] # Nil
                          /\ RescopeFirstW(s).baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
    BY <2>1
  <2>. QED BY <2>0, <2>2, <1>0, NRWrite
<1>r2. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE NRPre
  <2>0. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
          w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
    BY <1>r2 DEF RescopeSecond
  <2>1. \A p \in Paths : RescopeSecondW(s, wfail).baseline[p] # w[s].baseline[p] => w[s].baseline[p] = Nil
    BY <1>sh DEF RescopeSecondW, RescopeBase1, RescopeFetch, RescopeFetch0, RescopeAdd, Shipped
  <2>. QED BY <2>0, <2>1, <1>0, NRWrite
<1>n1. ASSUME NEW s \in Writers, Touch(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = TouchW(s)] BY <1>n1 DEF Touch
  <2>2. TouchW(s).baseline = w[s].baseline BY DEF TouchW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>n2. ASSUME NEW s \in Writers, Honor(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = HonorW(s)] BY <1>n2 DEF Honor
  <2>2. HonorW(s).baseline = w[s].baseline BY DEF HonorW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>n3. ASSUME NEW s \in Writers, HonorReader(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY <1>n3 DEF HonorReader
  <2>2. HonorReaderW(s).baseline = w[s].baseline BY DEF HonorReaderW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>n4. ASSUME NEW s \in Writers, RetireByAck(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY <1>n4 DEF RetireByAck
  <2>2. RetireByAckW(s).baseline = w[s].baseline BY DEF RetireByAckW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>n5. ASSUME NEW t \in Writers, Depose(t) PROVE NRPre
  <2>1. holder \in Writers /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)] BY <1>n5 DEF Depose
  <2>2. \A u \in Writers : w'[u].baseline = w[u].baseline BY <2>1, <1>0 DEF TypeOK, DeposeW, DeposedW
  <2>. QED BY <2>2 DEF NRPre
<1>n6. ASSUME NEW s \in Writers, Fence(s) PROVE NRPre
  <2>1. \/ w' = [w EXCEPT ![s] = FenceBeforeW(s)]
        \/ w' = [w EXCEPT ![s] = FenceAfterW(s)]
    BY <1>n6 DEF Fence
  <2>2. FenceBeforeW(s).baseline = w[s].baseline /\ FenceAfterW(s).baseline = w[s].baseline BY DEF FenceBeforeW, FenceAfterW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
<1>w. CASE WriterStep /\ ~(\E s \in Writers, h \in Handles : Sweep(s, h))
  BY <1>w, <1>s1, <1>s2, <1>s3, <1>s4, <1>s5, <1>s6, <1>s7, <1>s8, <1>s9, <1>s10, <1>s11, <1>s12, <1>up, <1>co, <1>sy, <1>rs, <1>fi, <1>ck, <1>r1, <1>r2,
     <1>n1, <1>n2, <1>n3, <1>n4, <1>n5, <1>n6
     DEF WriterStep
<1>. QED BY <1>g, <1>q, <1>w DEF Next

THEOREM NoRegress == Spec => NR5
<1>1. IndC /\ [Next]_vars => [NRAct]_vars
  <2>. SUFFICES ASSUME IndC, [Next]_vars PROVE [NRAct]_vars OBVIOUS
  <2>1. CASE Next BY <2>1, Next_NR, NRFrom
  <2>2. CASE UNCHANGED vars BY <2>2
  <2>. QED BY <2>1, <2>2
<1>. QED BY CInvariant, <1>1, PTL DEF Spec, NR5, NRAct

==============================================================================
