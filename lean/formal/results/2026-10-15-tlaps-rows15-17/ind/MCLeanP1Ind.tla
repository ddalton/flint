---------------------------- MODULE MCLeanP1Ind ----------------------------
(* The old proof's inductive conjuncts (M0-M5B, the 2026-10-14 full record:
   results/2026-10-07-tlaps-m5/gen-proof.py chain), as TLC invariants on the
   rows 15-17 model: which of them deposal, the ack and the restart break.
   Generated: the definitions are the proof's text, byte for byte.        *)
EXTENDS LeanP1Anc
Ghosts == upped \subseteq Handles /\ copies \in Nat /\ orig \in [Handles -> Opt(Handles)]
Minted ==
  /\ \A p \in Paths : doc[p] \in Opt(minted)
  /\ \A p \in Paths : gw[p] \in Opt(minted)
  /\ \A s \in Writers, p \in Paths : w[s].local[p] \in Opt(minted)
  /\ \A s \in Writers : \A p \in w[s].uploads : w[s].snap[p] \in minted
IndTypeOK == TypeOK /\ Ghosts /\ mv = Nil /\ nextGen > Seed /\ Minted
CC == {"claimed", "cased"}
Holder == holder # "none" => w[holder].pc \in CC /\ w[holder].st = "on"
Cased == \A s \in Writers : w[s].pc = "cased" => \A p \in w[s].deletes : w[s].inst[p] = Nil
Mine == \A s \in Writers : w[s].deletes \cap w[s].uploads = {} /\ w[s].gone \subseteq w[s].uploads
Ups == \A s \in Writers : w[s].pc \in {"scanned", "claimed", "cased"} =>
         (w[s].uploads \cup w[s].deletes) \cap w[s].unlinked = {}
Off == \A s \in Writers : w[s].st = "off" => w[s] = WriterInit
R1 == \A s \in Writers : w[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w[s].sStage = "none"
R2 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage = "none") => w[s].local[p] = w[s].baseline[p]
R3 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage \in {"saved", "mid"}) =>
          \/ w[s].local[p] = w[s].baseline[p]
          \/ /\ p \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[p] = Nil
             /\ w[s].local[p] # Nil /\ w[s].sHeld[p] = w[s].local[p]
R4 == \A s \in Writers : w[s].sStage = "mid" => \A p \in w[s].sDrop \ w[s].sKeep : w[s].baseline[p] = Nil
R5 == \A s \in Writers : w[s].pc = "pulling" => w[s].sStage \in {"none", "saved"}
Rescope == R1 /\ R2 /\ R3 /\ R4 /\ R5
M1 == Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope
IndM1 == IndTypeOK /\ M1
CitedSet == {doc[p] : p \in {q \in Paths : doc[q] # Nil}}
Priv(s, h) == \A t \in Writers \ {s}, q \in Paths : w[t].local[q] # h /\ w[t].snap[q] # h
Fresh ==
  /\ live \subseteq upped /\ upped \subseteq minted
  /\ (retiring \cup aged) \subseteq upped
  /\ nextGen <= MaxMint + 1
  /\ \A h \in minted : Gen(h) < nextGen \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies)
SnapMinted == \A s \in Writers, p \in Paths : w[s].snap[p] \in Opt(minted)
Flight == \A p \in Paths : gw[p] # Nil =>
            /\ gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p] \notin retiring \cup aged
            /\ gw[p][1] = p
            /\ \A s \in Writers, q \in Paths : w[s].local[q] # gw[p] /\ w[s].snap[q] # gw[p]
Private == \A s \in Writers, p \in Paths :
             (w[s].local[p] # Nil /\ w[s].local[p] \notin upped)
               => w[s].local[p][1] = p /\ Priv(s, w[s].local[p])
Pending == \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
             \A p \in w[s].uploads \ w[s].upDone :
               /\ w[s].snap[p] # Nil
               /\ (w[s].snap[p] \notin upped => w[s].snap[p][1] = p /\ Priv(s, w[s].snap[p]))
Up(s, h) == /\ h # Nil /\ h \in upped /\ ~Cited(h) /\ h \notin retiring \cup aged
            /\ \A q \in Paths : gw[q] # h
            /\ Priv(s, h)
Uploaded == \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
              \A p \in w[s].upDone : w[s].snap[p][1] = p /\ Up(s, w[s].snap[p])
\* (rows 15-17: a DEPOSED writer's uploads may be swept by the taker; it never
\* installs -- InstallFenced -- so the fact is needed only undeposed.)
Verified == \A s \in Writers : (w[s].pc = "claimed" /\ w[s].verified /\ ~w[s].deposed) =>
              \A p \in (w[s].uploads \cap w[s].upDone) \ w[s].gone : w[s].snap[p] \in live
Unverified == \A s \in Writers : w[s].pc = "scanned" => ~w[s].verified
M2 == Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
IndM2 == IndM1 /\ M2
TreeEv(t, R) == t \in Writers =>
  /\ \A q \in Paths : R.local[q] = w[t].local[q] \/ R.local[q] = Nil \/ R.local[q] \notin minted \/ R.local[q] \in CitedSet
  /\ \A q \in Paths : R.snap[q] = w[t].snap[q] \/ R.snap[q] = w[t].local[q] \/ R.snap[q] = Nil \/ R.snap[q] \notin minted
M3Seq == seq >= 1
Bounds == \A s \in Writers :
            /\ w[s].derived <= seq /\ w[s].memo <= seq /\ w[s].rnow <= seq
            /\ (w[s].derived # 0 => w[s].memo <= w[s].derived)
Record == \A s \in Writers : (w[s].derived = seq /\ w[s].sStage = "none") =>
            \A p \in Paths : (Held(s, p) /\ doc[p] # w[s].baseline[p]) => p \in w[s].skipped
CasedMine == \A s \in Writers : w[s].pc = "cased" =>
               \A p \in w[s].uploads \cap w[s].upDone : w[s].inst[p] = w[s].snap[p]
CasedAdv == \A s \in Writers : (w[s].pc = "cased" /\ w[s].adv) =>
              \A p \in Paths \ ((w[s].uploads \cap w[s].upDone) \cup w[s].deletes) :
                (Held(s, p) /\ w[s].inst[p] # w[s].baseline[p]) => p \in w[s].skipped
CasedSame == \A s \in Writers : (w[s].pc = "cased" /\ w[s].derived = seq) =>
               w[s].adv /\ w[s].inst = doc
M3 == M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame
IndM3 == IndM2 /\ M3
NeverReCited == \A p \in Paths : doc[p] # Nil => doc[p] \notin retiring \cup aged
ReaderLive == ~rlag => \A p \in Paths : rdoc[p] # Nil =>
                /\ rdoc[p] \in live /\ rdoc[p] \notin aged
                /\ (Cited(rdoc[p]) \/ rdoc[p] \in retiring)
M4 == NeverReCited /\ ReaderLive
IndM4 == IndM3 /\ M4
HistNew == \A h \in Handles \ minted : base[h] = Nil /\ anc[h] = {} /\ orig[h] = Nil
HistAnc == \A h \in Handles : anc[h] = AncOf(base[h])
HistMinted == \A h \in minted :
                /\ base[h] \in Opt(minted) /\ anc[h] \subseteq minted
                /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
BaselineMinted == \A s \in Writers, p \in Paths : w[s].baseline[p] \in Opt(minted)
Hist == HistNew /\ HistAnc /\ HistMinted /\ BaselineMinted
M5 == Hist
IndM5 == IndM4 /\ M5
NoRev == \A p \in Paths : ~SilentRevert(p)
\* M5C, M5D, M6 (hand-written modules), their state-level definitions.
CR(h) == Gen(Content(h))
DS(k) == {k} \cup anc[k]
RankC   == \A m \in minted : \A a \in anc[m] : CR(a) < CR(m)
OrigC   == \A m \in minted : orig[m] = Nil => Gen(m) < nextGen
SameC   == \A m, n \in minted : Content(m) = Content(n) => anc[m] = anc[n]
ClosedC == \A m \in minted : \A a \in anc[m] : anc[a] \subseteq anc[m]
HistC == RankC /\ OrigC /\ SameC /\ ClosedC
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
HCT == HistC /\ TreeC
TakeWhy(s, p) ==
  \/ w[s].local[p] = w[s].baseline[p]
  \/ w[s].local[p] # Nil /\ Content(w[s].local[p]) = Content(doc[p])
  \/ w[s].pc = "cased" /\ p \in w[s].uploads /\ w[s].snap[p] = doc[p]
ChainH(h) == {h[1]} \cup {q \in Paths : <<q, h>> \in acked}
TookAt(q, h) == \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
KD(h) == \/ Cited(h) \/ Preserved(h) \/ Desc(h)
         \/ \E q \in ChainH(h) : <<q, h>> \in udel \/ TookAt(q, h)
KDall  == \A a \in acked : KD(a[2])
AckMin == \A a \in acked : a[2] \in minted
CHI == \A q \in Paths : doc[q] # Nil => doc[q][1] = q \/ <<q, doc[q]>> \in acked
BT  == \A s \in Writers, p \in Paths : w[s].baseline[p] # Nil => Gen(w[s].baseline[p]) \in took[s][p]
UDR(R) == R.pc \in Bar => \A p \in R.uploads : R.baseline[p] # Nil => R.local[p] # R.baseline[p]
UD  == \A s \in Writers : UDR(w[s])
DInv == AckMin /\ CHI /\ BT /\ UD /\ KDall
UIR(R) == /\ \A p \in Paths : (R.baseline[p] = Nil \/ R.baseline[p] \in upped)
                               /\ (R.sHeld[p] = Nil \/ R.sHeld[p] \in upped)
          /\ R.pc \in Bar => \A p \in R.upDone : R.snap[p] \in upped
UIo == \A m \in minted : orig[m] # Nil => orig[m] \in upped /\ m \in upped
UI == (\A s \in Writers : UIR(w[s])) /\ UIo
==============================================================================
