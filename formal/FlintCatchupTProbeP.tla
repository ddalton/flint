--------------------------- MODULE FlintCatchupTProbeP ---------------------------
(***************************************************************************)
(* Candidate strengthening facts for the TLAPS proof of the trust rule     *)
(* (clean setting: capture, no crash, no zombie move, RaidFailsAtDrop),    *)
(* checked before they are proved.  TLC/tla-mc only.                       *)
(***************************************************************************)
EXTENDS FlintCatchupT

\* a snapshot's parent is older (the lineage descends)
Probe_LinDec == \A q \in R : \A e \in sPres[q] : sPar[q][e] < e

\* a parent named is present (or 0), so the chain can be walked
Probe_ParPresent ==
  \A q \in R : /\ hPar[q] # 0 => hPar[q] \in sPres[q]
               /\ \A e \in sPres[q] : sPar[q][e] # 0 => sPar[q][e] \in sPres[q]

\* the chain: a block a snapshot (or the head) did not allocate reads through
\* to its parent
Probe_Chain ==
  \A q \in R : \A x \in Blocks :
    /\ x \notin hAl[q] => head[q][x] = IF hPar[q] = 0 THEN 0 ELSE sCon[q][hPar[q]][x]
    /\ \A e \in sPres[q] : x \notin sAl[q][e] =>
         sCon[q][e][x] = IF sPar[q][e] = 0 THEN 0 ELSE sCon[q][sPar[q][e]][x]

\* cut skew is bounded: every snapshot of an older epoch, on any leg, holds
\* nothing acked after a newer trusted epoch's cut
Probe_WHi ==
  \A q1, q2 \in R : \A e1 \in sPres[q1] : \A e2 \in tr[q2] :
    e1 < e2 => \A x \in Blocks : sCon[q1][e1][x] <= cutAck[q2][e2][x]

\* a standby's head holds its trusted mark's acked map
Probe_Standby ==
  \A r \in R : dState[r] = "standby" /\ mark[r] \in tr[r] =>
    \A x \in Blocks : head[r][x] >= cutAck[r][mark[r]][x]

\* the bases: members hold every acked write; detached, every in-sync leg does
Probe_MembersInSync == attached => members \subseteq InSyncR
Probe_HoldAttached == attached => \A r \in members : \A x \in Blocks : head[r][x] >= acked[x]
Probe_HoldDetached == ~attached => pend = {} /\ \A r \in InSyncR : \A x \in Blocks : head[r][x] >= acked[x]

\* a base newer than the copy's target (the model allows it; the code does not)
Probe_BaseLeTgt ==
  \A r \in R : (dState[r] = "stale" /\ sb[r] > 0 /\ reverted[r] = sb[r]) => sb[r] <= Tgt

Probe_All == /\ Probe_LinDec /\ Probe_ParPresent /\ Probe_Chain /\ Probe_WHi
             /\ Probe_Standby /\ Probe_MembersInSync /\ Probe_HoldAttached
             /\ Probe_HoldDetached
=============================================================================
