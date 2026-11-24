---------------------------- MODULE FlintCatchupSInd ----------------------------
(***************************************************************************)
(* Candidate conjuncts of the inductive invariant for the stamp proof      *)
(* (FlintCatchupSProof-PLAN.md), each checked as an invariant on the      *)
(* c28/c27 arms before it is used.  v0: the stamp layer.                   *)
(***************************************************************************)
EXTENDS FlintCatchupS

J_DueHold   == due => hold
J_HoldAtt   == hold => attached
J_Rel       == attached /\ ~hold => rgenA = agen /\ rmemA = members
J_Cur       == attached /\ ~due => \A q \in members : Core(stamp[q]) = Core(CurStamp)
J_ZxOut     == zx # 0 => zx \notin members
J_Aseq      == aseq = nseq
J_SeqSync   == \A q \in InSyncR : stamp[q].seq = nseq /\ stamp[q].size = asize
J_MemRel    == attached => members \subseteq rmemA
J_RelHold   == \A q \in rmemA \cap InSyncR : \A x \in Blocks : ~Pending(x) => head[q][x] >= acked[x]
J_NoAckBad  == ~badAck

SInd0 == J_DueHold /\ J_HoldAtt /\ J_Rel /\ J_Cur /\ J_ZxOut /\ J_Aseq /\ J_SeqSync /\ J_MemRel /\ J_RelHold /\ J_NoAckBad

\* --- v1: the detached volume (the stage's read) ----------------------------------------
\* the legs carrying the live seq: the gen-0 assembly; the released generation and every
\* record-in_sync leg lie inside it
SeqLegs     == {q \in R : stamp[q].seq = nseq}
RelStamp    == [seq |-> nseq, gen |-> rgenA, size |-> asize, mem |-> rmemA, rgen |-> rgenA, rmem |-> rmemA]
J_M0        == Cardinality(SeqLegs) = asize /\ rmemA \subseteq SeqLegs /\ InSyncR \subseteq SeqLegs
J_RelValid  == Valid(RelStamp)
\* a minority release is a record confirm: every in_sync leg is in it
J_MinorRel  == ~Majority(RelStamp) => InSyncR \subseteq rmemA
\* the last healthy member never begins to fail (FailBegin): the release keeps an in_sync leg
J_RelInSync == rmemA \cap InSyncR # {}
\* an in_sync leg of the released generation reads it or a newer valid view inside it; any
\* other in_sync leg reads an older generation
J_ViewIn    == \A q \in InSyncR \cap rmemA :
                 LET V == View(stamp[q]) IN
                   /\ Valid(V) /\ V.seq = nseq /\ V.gen >= rgenA /\ V.mem \subseteq rmemA
                   /\ (V.gen = rgenA => V.mem = rmemA)
J_ViewOut   == \A q \in InSyncR \ rmemA : View(stamp[q]).seq = nseq /\ View(stamp[q]).gen < rgenA
\* a confirmed continuation of the live seq bounds the in_sync legs
J_SoleSeq   == \A c \in sole : c[1] = nseq => InSyncR \subseteq c[3]

SInd1 == SInd0 /\ J_M0 /\ J_RelValid /\ J_MinorRel /\ J_RelInSync /\ J_ViewIn /\ J_ViewOut /\ J_SoleSeq

\* --- v2: FlintCatchupT's inductive invariant (FlintCatchupTInd.tla d5039bf8), copied verbatim, to learn which
\* conjuncts survive the crash, the zombie and the stamp (each checked alone; the peel runner drops a violated one)
Up(Par, Con, p, x) == IF p = 0 THEN 0 ELSE Con[p][x]

ChainShape(P, Par, An) ==
  /\ P \subseteq Epochs
  /\ \A e \in P : Par[e] \in 0..MaxEpoch /\ Par[e] < e /\ (Par[e] # 0 => Par[e] \in P)
  /\ \A e \in P : An[e] = {e} \cup (IF Par[e] = 0 THEN {} ELSE An[Par[e]])

ChainReads(P, Par, Al, Con) ==
  \A e \in P : \A x \in Blocks : x \notin Al[e] => Con[e][x] = Up(Par, Con, Par[e], x)

PendVal(x) == {w.v : w \in {u \in pend : u.b = x}}

\* --- ranges and bookkeeping ------------------------------------------------
I_Ranges ==
  /\ cutK # 0 => cutK = nextSeq
  /\ ~attached => pend = {} /\ cutK = 0 /\ members = {}
  /\ zx = 0
  /\ recorded \subseteq 1..(nextSeq - 1)
  /\ \A e \in Epochs : ~conf[e] => attached /\ e < nextSeq
  /\ \A q \in R : /\ sPres[q] \subseteq 1..nextSeq
                  /\ tr[q] \subseteq sPres[q] \cap 1..(nextSeq - 1)
                  /\ hPar[q] \in sPres[q] \cup {0}

\* --- the lineage ---------------------------------------------------------------
I_Lineage ==
  \A q \in R : /\ ChainShape(sPres[q], sPar[q], anc[q])
               /\ \A e \in Epochs \ sPres[q] : anc[q][e] = {}
I_Chain ==
  \A q \in R : /\ ChainReads(sPres[q], sPar[q], sAl[q], sCon[q])
               /\ \A x \in Blocks : x \notin hAl[q] => head[q][x] = Up(sPar[q], sCon[q], hPar[q], x)

\* --- the data plane ----------------------------------------------------------
I_Pend ==
  /\ \A w1, w2 \in pend : w1.b = w2.b => w1 = w2
  /\ \A w \in pend : /\ w.v > acked[w.b] /\ w.v <= ver
                     /\ w.landed \cap w.dropped = {}
                     /\ w.landed \cup w.dropped \subseteq w.to
                     /\ members \subseteq w.to \ w.dropped
                     /\ \A q \in w.landed \cap members : head[q][w.b] = w.v
                     /\ w.cutAt <= nextSeq /\ w.cutAt >= nextSeq - 1
                     /\ cutK # 0 => w.cutAt >= cutK
I_Members == attached => members \subseteq InSyncR /\ Healthy # {}
I_Hold ==
  /\ attached => \A r \in members : \A x \in Blocks : head[r][x] >= acked[x]
  /\ ~attached => InSyncR # {} /\ \A r \in InSyncR : \A x \in Blocks : head[r][x] >= acked[x]
I_Legal ==
  \A q \in R : \A x \in Blocks :
    /\ head[q][x] \in ackedEver[x] \cup PendVal(x)
    /\ head[q][x] <= acked[x] \/ head[q][x] \in PendVal(x)
    /\ \A e \in sPres[q] : /\ sCon[q][e][x] \in ackedEver[x] \cup PendVal(x)
                           /\ \/ sCon[q][e][x] <= acked[x]
                              \/ \E w \in pend : w.b = x /\ w.v = sCon[q][e][x] /\ w.cutAt = e
\* a value in flight shows on a leg only once the write landed somewhere
I_PendSeen ==
  \A w \in pend : (\E q \in R : head[q][w.b] = w.v \/ \E e \in sPres[q] : sCon[q][e][w.b] = w.v)
                   => w.landed # {}
I_AckedEver == \A x \in Blocks : acked[x] \in ackedEver[x] /\ 0 \in ackedEver[x] /\ \A v \in ackedEver[x] : v <= acked[x]
\* no version on a leg is newer than the last one handed out
I_Vers ==
  /\ \A x \in Blocks : acked[x] <= ver
  /\ \A q \in R : \A x \in Blocks : head[q][x] <= ver /\ \A e \in sPres[q] : sCon[q][e][x] <= ver

\* --- cut skew is bounded ----------------------------------------------------------
I_WHi ==
  \A q1, q2 \in R : \A e1 \in sPres[q1] : \A e2 \in sPres[q2] :
    e1 < e2 => \A x \in Blocks : sCon[q1][e1][x] <= cutAck[q2][e2][x]

\* --- a leg catching up ----------------------------------------------------------------
CatchingUp(r) == (dState[r] = "stale" /\ reverted[r] # -1) \/ dState[r] = "standby"
HiOf(r) == IF dState[r] = "standby" THEN mark[r] ELSE reverted[r]
I_HeadHi ==
  \A r \in R : CatchingUp(r) =>
    \A q2 \in R : \A e2 \in sPres[q2] : e2 > HiOf(r) =>
      \A x \in Blocks : head[r][x] <= cutAck[q2][e2][x]
I_HeadLo ==
  \A r \in R : CatchingUp(r) =>
    \A q \in R : \A p \in sPres[q] : p < HiOf(r) =>
      \A x \in Blocks : sCon[q][p][x] <= head[r][x]
RevOK(r) ==
  dState[r] = "stale" /\ reverted[r] # -1 =>
    /\ sb[r] = reverted[r] /\ hAl[r] = {} /\ hPar[r] = reverted[r]
    /\ head[r] = IF reverted[r] = 0 THEN Zero ELSE sCon[r][reverted[r]]
    /\ reverted[r] > 0 => reverted[r] \in sPres[r]
    /\ \A e \in sPres[r] : e <= reverted[r] \/ (reverted[r] = 0 /\ \A f \in recorded : e < f)
I_Reverted == \A r \in R : RevOK(r)
\* a leg catching up holds a value in flight only from its own epoch or older
I_HeadPend ==
  \A r \in R : CatchingUp(r) => \A w \in pend : head[r][w.b] = w.v => w.cutAt <= HiOf(r)
SessionOK(r) ==
    /\ dState[r] = "insync" => reverted[r] = -1 /\ sb[r] = -1
    /\ dState[r] = "standby" => /\ sb[r] = -1 /\ mark[r] >= 1 /\ r \notin members
                                /\ hPar[r] <= mark[r] /\ mark[r] < nextSeq
                                /\ pin[r] = mark[r] /\ mark[r] \in recorded
                                /\ \A e \in sPres[r] : e <= mark[r] \/ \A f \in recorded : e < f
    /\ sb[r] # -1 => dState[r] = "stale" /\ sb[r] < nextSeq /\ recorded # {}
    /\ dState[r] = "stale" /\ sb[r] = -1 => reverted[r] = -1
    /\ dState[r] = "stale" /\ sb[r] > 0 => pin[r] = sb[r] /\ sb[r] \in recorded
    /\ dState[r] = "stale" /\ sb[r] > 0 /\ reverted[r] = -1 /\ sb[r] \in sPres[r] => sb[r] \in tr[r]
    /\ CatchingUp(r) => path[r] = "back" /\ nextSeq \notin sPres[r]
    /\ cutK # 0 /\ CatchingUp(r) /\ r \in cutTgt => r \in cutDone \cup cutGone
I_Session == \A r \in R : SessionOK(r)

\* --- trust -----------------------------------------------------------------------------
Bases == IF attached THEN members ELSE InSyncR
I_Fresh ==
  \A q \in Bases : \A e \in sPres[q] : (e >= nextSeq \/ ~conf[e]) =>
    \A x \in Blocks : sCon[q][e][x] >= cutAck[q][e][x]

\* T's ranges without "zx = 0" (the zombie is ON here); T's hold split: the attached half alone
I_RangesS ==
  /\ cutK # 0 => cutK = nextSeq
  /\ ~attached => pend = {} /\ cutK = 0 /\ members = {}
  /\ recorded \subseteq 1..(nextSeq - 1)
  /\ \A e \in Epochs : ~conf[e] => attached /\ e < nextSeq
  /\ \A q \in R : /\ sPres[q] \subseteq 1..nextSeq
                  /\ tr[q] \subseteq sPres[q] \cap 1..(nextSeq - 1)
                  /\ hPar[q] \in sPres[q] \cup {0}
I_HoldAtt == attached => \A r \in members : \A x \in Blocks : head[r][x] >= acked[x]
I_NoBadLoss == ~badLoss

\* --- v3: T's five ordering conjuncts restated over ACKED values.  In S a held write that times out
\* (HeldFail, StampLand with no survivor), a crash-torn write and the zombie's pre-stamp write are LEGAL
\* but never acked, so a value on a leg may exceed every acked map; the cuts' acked maps (cutAck) carry
\* only acked values, so the lower bounds the goal needs are stated on them.
LegalV(x)   == ackedEver[x] \cup PendVal(x) \cup zjv \cup zpost
I_AckedEverS == \A x \in Blocks : acked[x] \in ackedEver[x] /\ 0 \in ackedEver[x]
I_LegalS    == \A q \in R : \A x \in Blocks :
                 /\ head[q][x] \in LegalV(x)
                 /\ \A e \in sPres[q] : sCon[q][e][x] \in LegalV(x)
\* the acked maps the cuts recorded grow with the epoch, and never pass the acked map
I_WHiS      == \A q1, q2 \in R : \A e1 \in sPres[q1] : \A e2 \in sPres[q2] :
                 e1 < e2 => \A x \in Blocks : cutAck[q1][e1][x] <= cutAck[q2][e2][x]
I_AckMono   == \A q \in R : \A e \in sPres[q] : \A x \in Blocks : cutAck[q][e][x] <= acked[x]
\* a leg catching up holds what was acked by any cut older than its epoch
I_HeadLoS   == \A r \in R : CatchingUp(r) =>
                 \A q \in R : \A p \in sPres[q] : p < HiOf(r) =>
                   \A x \in Blocks : cutAck[q][p][x] <= head[r][x]
=============================================================================
