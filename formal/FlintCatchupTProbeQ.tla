--------------------------- MODULE FlintCatchupTProbeQ ---------------------------
(***************************************************************************)
(* The candidate inductive invariant for the TLAPS proof of the trust     *)
(* rule, clean setting (capture, no crash, no zombie move, RaidFailsAtDrop,*)
(* WriteQuorum = AssemblyQuorum = 1, base-inclusive), stated part by part *)
(* so a model checker names the part that fails.  TLC/tla-mc only.        *)
(***************************************************************************)
EXTENDS FlintCatchupT

PendVal(x) == {w.v : w \in {u \in pend : u.b = x}}

\* --- ranges ---------------------------------------------------------------
Q_Ranges ==
  /\ cutK # 0 => cutK = nextSeq
  /\ ~attached => pend = {} /\ cutK = 0
  /\ zx = 0
  /\ \A q \in R : /\ sPres[q] \subseteq 1..nextSeq
                  /\ tr[q] \subseteq sPres[q] \cap 1..(nextSeq - 1)
                  /\ hPar[q] \in sPres[q] \cup {0}
  /\ \A e \in Epochs : ~conf[e] => attached /\ e < nextSeq
  /\ recorded \subseteq 1..(nextSeq - 1)

\* --- the lineage: descends, parents present, reads through, ghost anc ---
Q_Lineage ==
  \A q \in R :
    /\ \A e \in sPres[q] : sPar[q][e] < e /\ (sPar[q][e] # 0 => sPar[q][e] \in sPres[q])
    /\ \A e \in sPres[q] : anc[q][e] = {e} \cup AncOf(q, sPar[q][e])
    /\ \A e \in Epochs \ sPres[q] : anc[q][e] = {}
Q_Chain ==
  \A q \in R : \A x \in Blocks :
    /\ x \notin hAl[q] => head[q][x] = IF hPar[q] = 0 THEN 0 ELSE sCon[q][hPar[q]][x]
    /\ \A e \in sPres[q] : x \notin sAl[q][e] =>
         sCon[q][e][x] = IF sPar[q][e] = 0 THEN 0 ELSE sCon[q][sPar[q][e]][x]

\* --- the data plane ------------------------------------------------------
Q_Pend ==
  /\ \A w1, w2 \in pend : w1.b = w2.b => w1 = w2
  /\ \A w \in pend : /\ w.v > acked[w.b] /\ w.v <= ver
                     /\ w.landed \cap w.dropped = {}
                     /\ w.landed \cup w.dropped \subseteq w.to
                     /\ members \subseteq w.to \ w.dropped
                     /\ \A q \in w.landed \cap members : head[q][w.b] = w.v
                     /\ w.cutAt <= nextSeq
Q_Members == attached => members \subseteq InSyncR /\ Healthy # {}
Q_Hold ==
  /\ attached => \A r \in members : \A x \in Blocks : head[r][x] >= acked[x]
  /\ ~attached => InSyncR # {} /\ \A r \in InSyncR : \A x \in Blocks : head[r][x] >= acked[x]
\* every value on a leg is legal, and at most acked unless it is a write in flight
Q_Legal ==
  \A q \in R : \A x \in Blocks :
    /\ head[q][x] \in ackedEver[x] \cup PendVal(x)
    /\ head[q][x] <= acked[x] \/ head[q][x] \in PendVal(x)
    /\ \A e \in sPres[q] : /\ sCon[q][e][x] \in ackedEver[x] \cup PendVal(x)
                           /\ \/ sCon[q][e][x] <= acked[x]
                              \/ \E w \in pend : w.b = x /\ w.v = sCon[q][e][x] /\ w.cutAt >= e
Q_AckedEver == \A x \in Blocks : acked[x] \in ackedEver[x] /\ \A v \in ackedEver[x] : v <= acked[x]

\* --- cut skew is bounded (cutAck is acked at the snapshot's cut) --------
Q_WHi ==
  \A q1, q2 \in R : \A e1 \in sPres[q1] : \A e2 \in sPres[q2] :
    e1 < e2 => \A x \in Blocks : sCon[q1][e1][x] <= cutAck[q2][e2][x]

\* --- a leg catching up: its head's epoch (Hi) and what it holds (Lo) ---
CatchingUp(r) == (dState[r] = "stale" /\ reverted[r] # -1) \/ dState[r] = "standby"
HiOf(r) == IF dState[r] = "standby" THEN mark[r] ELSE reverted[r]
Q_HeadHi ==
  \A r \in R : CatchingUp(r) =>
    \A q2 \in R : \A e2 \in sPres[q2] : e2 > HiOf(r) =>
      \A x \in Blocks : head[r][x] <= cutAck[q2][e2][x]
Q_HeadLo ==
  \A r \in R : CatchingUp(r) =>
    \A q \in R : \A p \in sPres[q] : p < HiOf(r) =>
      \A x \in Blocks : sCon[q][p][x] <= head[r][x]
\* a reverted head IS its base, nothing newer kept
Q_Reverted ==
  \A r \in R : dState[r] = "stale" /\ reverted[r] # -1 =>
    /\ sb[r] = reverted[r] /\ hAl[r] = {} /\ hPar[r] = reverted[r]
    /\ head[r] = IF reverted[r] = 0 THEN Zero ELSE sCon[r][reverted[r]]
    /\ \A e \in sPres[r] : e <= reverted[r] \/ e >= nextSeq \/ (reverted[r] = 0 /\ e \notin recorded)
Q_Standby ==
  \A r \in R : dState[r] = "standby" => mark[r] >= 1 /\ r \notin members

\* --- trust ----------------------------------------------------------------
Q_TrustComplete == Inv_TrustComplete
Bases == IF attached THEN members ELSE InSyncR
Q_Fresh ==
  \A q \in Bases : \A e \in sPres[q] : (e >= nextSeq \/ ~conf[e]) =>
    \A x \in Blocks : sCon[q][e][x] >= cutAck[q][e][x]
Q_CfgAt == \A e \in Epochs : ~conf[e] => cfgAt[e] \subseteq R

\* --- the base never sits above the copy's target --------------------------
Q_BaseLeTgt ==
  \A r \in R : (dState[r] = "stale" /\ sb[r] > 0 /\ reverted[r] = sb[r]) => sb[r] <= Tgt

Q_Goal == Inv_Catchup /\ Inv_MembersHold
=============================================================================
