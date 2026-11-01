---------------------------- MODULE FlintCatchupTInd ----------------------------
(***************************************************************************)
(* The inductive invariant of the trust rule's proof (FlintCatchupTProof), *)
(* stated where a model checker can check it part by part.  The clean     *)
(* setting: the capture, no crash, no zombie move, a raid that fails a    *)
(* base at its first dropped write, one landing acks, one leg stages, the *)
(* base-inclusive copy, CutStampsAll = FALSE, PinRetention.               *)
(*                                                                         *)
(* A snapshot chain is a present set P, parents Par, ghost ancestors An,   *)
(* allocations Al and contents Con: parents are older and present, An is  *)
(* the parent walk, and a block a snapshot did not allocate reads through *)
(* to its parent.  Cut skew is bounded: a snapshot holds nothing acked    *)
(* after a newer snapshot's cut (cutAck).  A leg catching up holds        *)
(* everything older than its head's epoch (HeadLo) and nothing acked after *)
(* any newer cut (HeadHi).                                                 *)
(***************************************************************************)
EXTENDS FlintCatchupT

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

IndInv ==
  /\ TypeOK /\ I_Ranges /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_Hold
  /\ I_Legal /\ I_PendSeen /\ I_AckedEver /\ I_Vers /\ I_WHi /\ I_HeadHi /\ I_HeadLo /\ I_Reverted
  /\ I_HeadPend /\ I_Session
  /\ Inv_TrustComplete /\ I_Fresh /\ ~badLoss

\* what the theorem claims
Goal == Inv_Catchup /\ Inv_MembersHold
=============================================================================
