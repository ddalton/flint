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

\* --- v4: T's I_Fresh with the detached bases restricted to the in_sync legs of the last released membership
\* (at three replicas a majority release leaves a lost leg in_sync whose unconfirmed epochs are incomplete:
\* T's I_Fresh fails there; the stage never assembles it)
BasesS      == IF attached THEN members ELSE InSyncR \cap rmemA
I_FreshS    ==
  \A q \in BasesS : \A e \in sPres[q] : (e >= nextSeq \/ ~conf[e]) =>
    \A x \in Blocks : sCon[q][e][x] >= cutAck[q][e][x]

\* --- v5 (the type layer, 10-31): the generations stay inside StampRec's range.  agen counts the survivors' stamps
\* since the assembly; each lands on a loss (due), and a loss takes a member in the same step; the zombie's
\* generation is a copy of agen at its move
J_GenBound  == attached => agen + Cardinality(members) + (IF due THEN 1 ELSE 0) <= asize
J_ZGen      == zgen <= NR

\* the candidate inductive invariant (v4): the stamp layer, the surviving FlintCatchupT conjuncts, the restatements
SIndAll ==
  /\ SInd1
  /\ TypeOK /\ I_RangesS /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_Members /\ I_HoldAtt /\ I_PendSeen /\ I_Vers
  /\ I_Reverted /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_NoBadLoss
  /\ I_AckedEverS /\ I_LegalS /\ I_WHiS /\ I_AckMono /\ I_HeadLoS /\ I_FreshS
\* --- v6 (rev 16, the in-write rule, 10-31): with LastFails every member may begin to fail, so T's "Healthy # {}" half
\* of I_Members goes (pk1: VIOL d2); the members stay record-in_sync
I_MembersS  == attached => members \subseteq InSyncR

\* --- v7 candidates (the invariant agents' NEEDS-INV, each peeled before IndInv takes it) ---------------------
\* (inv-sess, RevertD's I_HeadPend: T's I_Legal said a snapshot's value in flight was written at that epoch; S kept
\* only membership in LegalV) a snapshot holds a value in flight only from its own epoch or older
I_SnapPend  == \A q \in R : \A e \in sPres[q] : \A w \in pend : sCon[q][e][w.b] = w.v => w.cutAt <= e

\* (inv-zombie) the zombie's leg is not in a catch-up session: it was a member (in_sync, sb = -1) at the move, and
\* SelectBase never picks it (LocalLegGuard)
J_ZxSb      == zx # 0 => sb[zx] = -1

\* (inv-admit) the zombie's leg is idle: not standby, not in a session (StageAdmit admits a standby r; it must not be zx)
J_ZxIdle    == zx # 0 => dState[zx] # "standby" /\ sb[zx] = -1 /\ reverted[zx] = -1 /\ stamp[zx].seq = zseq
\* (inv-zombie C) an in_sync zombie leg is the detached release's own: its registers are the live ones (set at the
\* move; every new assembly marks zx stale; Release, RecordConfirm and StampLand need attached)
J_ZLive     == zx # 0 /\ dState[zx] = "insync" =>
                 /\ ~attached /\ zx \in rmemA /\ zseq = nseq /\ zsize = asize /\ zgen = agen
                 /\ zrgen = rgenA /\ zrmem = rmemA
J_RelLe     == rgenA <= agen
\* (inv-zombie A) LegalV without zjv: under ZombiePreStampLegal every zombie write joins ackedEver (pre-stamp) or zpost
LegalV2(x)  == ackedEver[x] \cup PendVal(x) \cup zpost
I_LegalS2   == \A q \in R : \A x \in Blocks :
                 /\ head[q][x] \in LegalV2(x)
                 /\ \A e \in sPres[q] : sCon[q][e][x] \in LegalV2(x)
\* (inv-stage N1) J_RelHold without the in-flight exemption: a released in_sync leg holds every acked value (a step that
\* empties pend while attached, e.g. CrashUnstage, HeldFail, StampLand with no survivor, needs it)
J_RelHoldS  == \A q \in rmemA \cap InSyncR : \A x \in Blocks : head[q][x] >= acked[x]
\* (inv-stage N2) I_FreshS with the detached bases at every time (attached: members \subseteq InSyncR \cap rmemA)
I_FreshS2   ==
  \A q \in InSyncR \cap rmemA : \A e \in sPres[q] : (e >= nextSeq \/ ~conf[e]) =>
    \A x \in Blocks : sCon[q][e][x] >= cutAck[q][e][x]
\* (inv-stage N3) no stamp, confirmation or zombie copy is ahead of the live seq
J_SeqLe     == (\A q \in R : stamp[q].seq <= nseq) /\ (\A c \in sole : c[1] <= nseq) /\ zseq <= nseq
\* (inv-stage N4, inv-stamp N4: the stronger form, which gives ~Majority(RelStamp)) a confirmation of the live seq
\* holds the last released membership, and is a minority of the assembly (RecordConfirm; Release cannot follow it)
J_SoleRel   == \A c \in sole : c[1] <= nseq /\ (c[1] = nseq => rmemA \subseteq c[3] /\ 2 * Cardinality(c[3]) <= asize)
\* (inv-stamp N3) no in_sync leg reads a generation past agen, and a leg that left the members unconfirmed (no loss
\* due) reads an older one (Release: rgenA' = agen, rmemA' = members, J_ViewOut')
J_GenTop    == attached => \A q \in InSyncR : LET V == View(stamp[q]) IN
                 V.gen <= agen /\ (~due /\ q \notin members => V.gen < agen)

\* (inv-copy G1) the acked-value forms of T's I_WHi and I_HeadLo: an acked value a snapshot holds is held by every later
\* cut's acked map, and by a leg catching up past it (the copy's untouched blocks, Inv_TrustComplete at the target)
AckW(x)     == {acked[x]} \cup UNION {{cutAck[q][e][x] : e \in sPres[q]} : q \in R}
I_WHiC      == \A q, q2 \in R : \A p \in sPres[q] : \A e2 \in sPres[q2] : p < e2 =>
                 \A x \in Blocks : \A v \in AckW(x) : v <= sCon[q][p][x] => v <= cutAck[q2][e2][x]
I_HeadLoC   == \A r \in R : CatchingUp(r) => \A q \in R : \A p \in sPres[q] : p < HiOf(r) =>
                 \A x \in Blocks : \A v \in AckW(x) : v <= sCon[q][p][x] => v <= head[r][x]

\* (inv-admit A) the weaker form of I_HeadLoC that StageAdmit's untouched blocks need
I_HeadLoA   == \A r \in R : CatchingUp(r) => \A q \in R : \A p \in sPres[q] : p < HiOf(r) =>
                 \A x \in Blocks : acked[x] <= sCon[q][p][x] => acked[x] <= head[r][x]
\* (inv-admit B) a standby's head holds only acked or in-flight values (no zombie value: StageAdmit's badLoss)
I_StandbyLegal == \A q \in R : dState[q] = "standby" => \A x \in Blocks : head[q][x] \in ackedEver[x] \cup PendVal(x)

\* (inv-copy 1) the acked-value form of T's I_HeadHi: an acked value a catching-up head holds is in every later cut's map
I_HeadHiC   == \A r \in R : CatchingUp(r) => \A q2 \in R : \A e2 \in sPres[q2] : e2 > HiOf(r) =>
                 \A x \in Blocks : \A v \in AckW(x) : v <= head[r][x] => v <= cutAck[q2][e2][x]
\* (inv-copy 2) a snapshot holds only acked or in-flight values (zombie writes stay on zx's head)
I_SnapLegal == \A q \in R : \A e \in sPres[q] : \A x \in Blocks : sCon[q][e][x] \in ackedEver[x] \cup PendVal(x)

\* (inv-data a) no leg holds a value above a write in flight on its block (Submit takes ver + 1; Zombie needs ~Pending)
I_PendTop   == \A w \in pend : \A q \in R : head[q][w.b] <= w.v /\ \A e \in sPres[q] : sCon[q][e][w.b] <= w.v
\* (inv-data b) a snapshot at nextSeq (a cut's) means every write in flight belongs to nextSeq
I_PendNext  == \A w \in pend : \A q \in R : nextSeq \in sPres[q] => w.cutAt = nextSeq

\* (inv-zombie, under the triple) a zombie that may acknowledge is the only in_sync leg
J_ZRel      == ZombieReleases => InSyncR = {zx}

\* (inv-cut) a cut's targets and an attached raid's in_sync legs hold only acked or in-flight values (CutOne snapshots
\* the head: I_SnapLegal')
I_CutLegal    == cutK # 0 => \A q \in cutTgt : \A x \in Blocks : head[q][x] \in ackedEver[x] \cup PendVal(x)
I_InSyncLegal == attached => \A q \in InSyncR : \A x \in Blocks : head[q][x] \in ackedEver[x] \cup PendVal(x)

\* (inv-admit C) a record-in_sync leg with a valid stamp holds only acked or in-flight values (the stage and the admission
\* assemble valid-stamp legs and cut them: I_SnapLegal', I_InSyncLegal'; a post-stamp zombie leg's stamp is a minority)
I_ValidLegal == \A q \in InSyncR : Valid(stamp[q]) => \A x \in Blocks : head[q][x] \in ackedEver[x] \cup PendVal(x)

\* (inv-zombie, for I_ValidLegal at Zombie) a zombie leg that stamped itself carries an invalid stamp while in_sync; its
\* move members include it and fit its size; a confirmation of the live seq is no newer than agen
J_ZNoValid  == zx # 0 /\ zst /\ dState[zx] = "insync" => ~Valid(stamp[zx])
J_ZM0       == zx # 0 => zx \in zm0 /\ Cardinality(zm0) <= zsize
J_SoleGen   == \A c \in sole : c[1] = nseq => c[2] <= agen

\* (inv-zombie, for I_CutLegal at Zombie) a cut in flight never targets the zombie's leg
J_ZCut      == cutK # 0 => zx \notin cutTgt

\* v5: v4 and the two generation bounds
SIndAll5 == SIndAll /\ J_GenBound /\ J_ZGen
\* v6: v5 with I_MembersS in place of I_Members (the proof's IndInv on rev 16)
SIndAll6 ==
  /\ SInd1 /\ J_GenBound /\ J_ZGen
  /\ TypeOK /\ I_RangesS /\ I_Lineage /\ I_Chain /\ I_Pend /\ I_MembersS /\ I_HoldAtt /\ I_PendSeen /\ I_Vers
  /\ I_Reverted /\ I_HeadPend /\ I_Session /\ Inv_TrustComplete /\ I_NoBadLoss
  /\ I_AckedEverS /\ I_LegalS /\ I_WHiS /\ I_AckMono /\ I_HeadLoS /\ I_FreshS
=============================================================================
