---------------------------- MODULE LeanP1ProofM3 ----------------------------
EXTENDS LeanP1ProofM2

------------------------------------------------------------------------------
(* M3: Inv_ShortcutSound, Inv_ReaderSound.                                  *)

\* seq starts at 1, so a record of 0 ("nothing derived") never matches it.
M3Seq == seq >= 1
\* I10 and I9's bounds, for every tree (a non-reader's memo stays 0).
Bounds == \A s \in Writers :
            /\ w[s].derived <= seq /\ w[s].memo <= seq /\ w[s].rnow <= seq
            /\ (w[s].derived # 0 => w[s].memo <= w[s].derived)
\* I8: the cheap path's record.
Record == \A s \in Writers : (w[s].derived = seq /\ w[s].sStage = "none") =>
            \A p \in Paths : (Held(s, p) /\ doc[p] # w[s].baseline[p]) => p \in w[s].skipped
\* Between the CAS and the finish: the install's own paths hold the snapshot;
\* with `adv`, every other held path where the INSTALLED document differs
\* from the baseline is skipped; a record still current means the install
\* moved nothing.
CasedMine == \A s \in Writers : w[s].pc = "cased" =>
               \A p \in w[s].uploads \cap w[s].upDone : w[s].inst[p] = w[s].snap[p]
CasedAdv == \A s \in Writers : (w[s].pc = "cased" /\ w[s].adv) =>
              \A p \in Paths \ ((w[s].uploads \cap w[s].upDone) \cup w[s].deletes) :
                (Held(s, p) /\ w[s].inst[p] # w[s].baseline[p]) => p \in w[s].skipped
CasedSame == \A s \in Writers : (w[s].pc = "cased" /\ w[s].derived = seq) =>
               w[s].adv /\ w[s].inst = doc
M3 == M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame
IndM3 == IndM2 /\ M3

\* The step's event: the pointer never goes back, and it moves when the
\* document does.
SeqEv == seq' \in Nat /\ seq <= seq' /\ (seq' = seq => doc' = doc)
\* What the written tree R must satisfy, read off R (the primed pointer and
\* document are the step's).
HeldR(R, p) == R.baseline[p] # Nil \/ p \in R.scope
RB(R) == /\ R.derived <= seq' /\ R.memo <= seq' /\ R.rnow <= seq'
         /\ (R.derived # 0 => R.memo <= R.derived)
RRec(R) == (R.derived = seq' /\ R.sStage = "none") =>
             \A p \in Paths : (HeldR(R, p) /\ doc'[p] # R.baseline[p]) => p \in R.skipped
RMine(R) == R.pc = "cased" => \A p \in R.uploads \cap R.upDone : R.inst[p] = R.snap[p]
RAdv(R) == (R.pc = "cased" /\ R.adv) =>
             \A p \in Paths \ ((R.uploads \cap R.upDone) \cup R.deletes) :
               (HeldR(R, p) /\ R.inst[p] # R.baseline[p]) => p \in R.skipped
RSame(R) == (R.pc = "cased" /\ R.derived = seq') => R.adv /\ R.inst = doc'

------------------------------------------------------------------------------
(* What the event and the tree give M3.                                     *)

LEMMA M3Write ==
  ASSUME M3, seq \in Nat, SeqEv, w \in [Writers -> Writer], w' \in [Writers -> Writer],
         NEW t, NEW R, Wr(t, R),
         t \in Writers => RB(R) /\ RRec(R) /\ RMine(R) /\ RAdv(R) /\ RSame(R)
  PROVE  M3'
<1>0. seq' \in Nat /\ seq <= seq' /\ (seq' = seq => doc' = doc) BY DEF SeqEv
<1>1. M3Seq' BY <1>0 DEF M3, M3Seq
<1>n. \A u \in Writers : w[u].derived \in Nat /\ w[u].memo \in Nat /\ w[u].rnow \in Nat
  <2>. SUFFICES ASSUME NEW u \in Writers PROVE w[u].derived \in Nat /\ w[u].memo \in Nat /\ w[u].rnow \in Nat
    OBVIOUS
  <2>1. w[u] \in Writer OBVIOUS
  <2>. QED BY <2>1, WriterFields
<1>k. \A u \in Writers : u # t => w'[u] = w[u] BY DEF Wr
<1>t. t \in Writers => w'[t] = R BY DEF Wr
<1>2. Bounds'
  <2>. SUFFICES ASSUME NEW u \in Writers
                PROVE  /\ w'[u].derived <= seq' /\ w'[u].memo <= seq' /\ w'[u].rnow <= seq'
                       /\ (w'[u].derived # 0 => w'[u].memo <= w'[u].derived)
    BY DEF Bounds
  <2>1. CASE u = t BY <2>1, <1>t DEF RB
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. /\ w[u].derived <= seq /\ w[u].memo <= seq /\ w[u].rnow <= seq
          /\ (w[u].derived # 0 => w[u].memo <= w[u].derived)
      BY DEF M3, Bounds
    <3>. QED BY <3>1, <3>2, <1>0, <1>n
  <2>. QED BY <2>1, <2>2
<1>3. Record'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].derived = seq', w'[u].sStage = "none",
                       NEW p \in Paths, w'[u].baseline[p] # Nil \/ p \in w'[u].scope,
                       doc'[p] # w'[u].baseline[p]
                PROVE  p \in w'[u].skipped
    BY DEF Record, Held
  <2>1. CASE u = t BY <2>1, <1>t DEF RRec, HeldR
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. w[u].derived <= seq BY DEF M3, Bounds
    <3>3. seq' = seq /\ doc' = doc BY <3>1, <3>2, <1>0, <1>n
    <3>. QED BY <3>1, <3>3 DEF M3, Record, Held
  <2>. QED BY <2>1, <2>2
<1>4. CasedMine'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased"
                PROVE  \A p \in w'[u].uploads \cap w'[u].upDone : w'[u].inst[p] = w'[u].snap[p]
    BY DEF CasedMine
  <2>1. CASE u = t BY <2>1, <1>t DEF RMine
  <2>2. CASE u # t BY <2>2, <1>k DEF M3, CasedMine
  <2>. QED BY <2>1, <2>2
<1>5. CasedAdv'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased", w'[u].adv,
                       NEW p \in Paths \ ((w'[u].uploads \cap w'[u].upDone) \cup w'[u].deletes),
                       w'[u].baseline[p] # Nil \/ p \in w'[u].scope,
                       w'[u].inst[p] # w'[u].baseline[p]
                PROVE  p \in w'[u].skipped
    BY DEF CasedAdv, Held
  <2>1. CASE u = t BY <2>1, <1>t DEF RAdv, HeldR
  <2>2. CASE u # t BY <2>2, <1>k DEF M3, CasedAdv, Held
  <2>. QED BY <2>1, <2>2
<1>6. CasedSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased", w'[u].derived = seq'
                PROVE  w'[u].adv /\ w'[u].inst = doc'
    BY DEF CasedSame
  <2>1. CASE u = t BY <2>1, <1>t DEF RSame
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. w[u].derived <= seq BY DEF M3, Bounds
    <3>3. seq' = seq /\ doc' = doc BY <3>1, <3>2, <1>0, <1>n
    <3>. QED BY <3>1, <3>3 DEF M3, CasedSame
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF M3

\* M3 after a step that moves nothing M3 reads.
LEMMA M3Same ==
  ASSUME M3, UNCHANGED <<doc, seq, w>>
  PROVE  M3'
BY DEF M3, M3Seq, Bounds, Record, CasedMine, CasedAdv, CasedSame, Held

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M3 == Init => IndM3
<1>. SUFFICES ASSUME Init PROVE IndM3 OBVIOUS
<1>1. IndM2 BY Init_M2
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>3. \A s \in Writers : /\ w[s].pc = "idle" /\ w[s].derived = 0 /\ w[s].memo = 0 /\ w[s].rnow = 0
  BY <1>2 DEF WriterInit
<1>4. seq = 1 BY DEF Init
<1>5. M3Seq /\ Bounds BY <1>3, <1>4 DEF M3Seq, Bounds
<1>6. Record BY <1>3, <1>4 DEF Record
<1>7. CasedMine /\ CasedAdv /\ CasedSame BY <1>3 DEF CasedMine, CasedAdv, CasedSame
<1>. QED BY <1>1, <1>5, <1>6, <1>7 DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the gateway and the steps that write no tree.                        *)

LEMMA GPut_M3 ==
  ASSUME IndM3, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GPut_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF GPut
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GCas_M3 ==
  ASSUME IndM3, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GCas_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ (seq' = seq + 1 \/ (seq' = seq /\ doc' = doc)) BY DEF GCas
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GRename_M3 ==
  ASSUME IndM3, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GRename_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ seq' = seq + 1 BY DEF GRename
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GRenameFinish_M3 ==
  ASSUME IndM3, GRenameFinish, Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GRenameFinish_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ seq' = seq + 1 BY DEF GRenameFinish
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GDelete_M3 ==
  ASSUME IndM3, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GDelete_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ seq' = seq + 1 BY DEF GDelete
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA Sweep_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Sweep_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF Sweep
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA Reap_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Reap_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF Reap
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA Age_M3 ==
  ASSUME IndM3, Age, UNCHANGED anc
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Age_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF Age
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA RLoad_M3 ==
  ASSUME IndM3, RLoad, UNCHANGED anc
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RLoad_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF RLoad
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the agent and the commit section.                                    *)

LEMMA Edit_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Edit_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] /\ UNCHANGED <<doc, seq>> BY DEF Edit
<1>g. On(s) BY DEF Edit
<1>2. Wr(s, EditW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ EditW(s, p).pc = w[s].pc
      /\ EditW(s, p).derived = w[s].derived
      /\ EditW(s, p).memo = w[s].memo
      /\ EditW(s, p).rnow = w[s].rnow
      /\ EditW(s, p).sStage = w[s].sStage
      /\ EditW(s, p).scope = w[s].scope
      /\ EditW(s, p).skipped = w[s].skipped
  BY DEF EditW
<1>3b. /\ EditW(s, p).baseline = w[s].baseline
      /\ EditW(s, p).uploads = w[s].uploads
      /\ EditW(s, p).upDone = w[s].upDone
      /\ EditW(s, p).deletes = w[s].deletes
      /\ EditW(s, p).snap = w[s].snap
      /\ EditW(s, p).inst = w[s].inst
      /\ EditW(s, p).adv = w[s].adv
  BY DEF EditW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(EditW(s, p)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(EditW(s, p)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(EditW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(EditW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(EditW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Delete_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Delete_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<doc, seq>> BY DEF Delete, bucket
<1>g. On(s) BY DEF Delete
<1>2. Wr(s, DeleteW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ DeleteW(s, p).pc = w[s].pc
      /\ DeleteW(s, p).derived = w[s].derived
      /\ DeleteW(s, p).memo = w[s].memo
      /\ DeleteW(s, p).rnow = w[s].rnow
      /\ DeleteW(s, p).sStage = w[s].sStage
      /\ DeleteW(s, p).scope = w[s].scope
      /\ DeleteW(s, p).skipped = w[s].skipped
  BY DEF DeleteW
<1>3b. /\ DeleteW(s, p).baseline = w[s].baseline
      /\ DeleteW(s, p).uploads = w[s].uploads
      /\ DeleteW(s, p).upDone = w[s].upDone
      /\ DeleteW(s, p).deletes = w[s].deletes
      /\ DeleteW(s, p).snap = w[s].snap
      /\ DeleteW(s, p).inst = w[s].inst
      /\ DeleteW(s, p).adv = w[s].adv
  BY DEF DeleteW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(DeleteW(s, p)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(DeleteW(s, p)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(DeleteW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(DeleteW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(DeleteW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Checkout_M3 ==
  ASSUME IndM3, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Checkout_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\ UNCHANGED <<doc, seq>> BY DEF Checkout, bucket
<1>g. w[s].st = "off" BY DEF Checkout
<1>2. Wr(s, CheckoutW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ CheckoutW(s, T).pc = w[s].pc
      /\ CheckoutW(s, T).derived = seq
      /\ CheckoutW(s, T).memo = w[s].memo
      /\ CheckoutW(s, T).rnow = w[s].rnow
      /\ CheckoutW(s, T).sStage = w[s].sStage
      /\ CheckoutW(s, T).scope = T
      /\ CheckoutW(s, T).skipped = {}
  BY DEF CheckoutW
<1>3b. /\ CheckoutW(s, T).baseline = CheckoutHeld(s, T)
      /\ CheckoutW(s, T).uploads = w[s].uploads
      /\ CheckoutW(s, T).upDone = w[s].upDone
      /\ CheckoutW(s, T).deletes = w[s].deletes
      /\ CheckoutW(s, T).snap = w[s].snap
      /\ CheckoutW(s, T).inst = doc
      /\ CheckoutW(s, T).adv = w[s].adv
  BY DEF CheckoutW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(CheckoutW(s, T)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(CheckoutW(s, T)) BY <1>1, <1>3, <1>3b DEF RRec, HeldR, CheckoutHeld
<1>6. RMine(CheckoutW(s, T)) BY <1>3, <1>g, <1>m DEF RMine, Off, WriterInit
<1>7. RAdv(CheckoutW(s, T)) BY <1>3, <1>g, <1>m DEF RAdv, Off, WriterInit
<1>8. RSame(CheckoutW(s, T)) BY <1>3, <1>g, <1>m DEF RSame, Off, WriterInit
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Consume_M3 ==
  ASSUME IndM3, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Consume_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. ConsumeKeepsLeft /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] /\ UNCHANGED <<doc, seq>> BY <1>1, <1>ck DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeCheapW(s, ck)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeCheapW(s, ck).pc = "consumed"
        /\ ConsumeCheapW(s, ck).derived = w[s].derived
        /\ ConsumeCheapW(s, ck).memo = w[s].memo
        /\ ConsumeCheapW(s, ck).rnow = w[s].rnow
        /\ ConsumeCheapW(s, ck).sStage = w[s].sStage
        /\ ConsumeCheapW(s, ck).scope = w[s].scope
        /\ ConsumeCheapW(s, ck).skipped = w[s].skipped
    BY DEF ConsumeCheapW
  <2>3b. /\ ConsumeCheapW(s, ck).baseline = w[s].baseline
        /\ ConsumeCheapW(s, ck).uploads = w[s].uploads
        /\ ConsumeCheapW(s, ck).upDone = w[s].upDone
        /\ ConsumeCheapW(s, ck).deletes = w[s].deletes
        /\ ConsumeCheapW(s, ck).snap = w[s].snap
        /\ ConsumeCheapW(s, ck).inst = w[s].inst
        /\ ConsumeCheapW(s, ck).adv = w[s].adv
    BY DEF ConsumeCheapW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(ConsumeCheapW(s, ck)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(ConsumeCheapW(s, ck)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(ConsumeCheapW(s, ck)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(ConsumeCheapW(s, ck)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(ConsumeCheapW(s, ck)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] /\ UNCHANGED <<doc, seq>> BY <1>2, <1>ck DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeW(s, fail, ck)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeW(s, fail, ck).pc = "consumed"
        /\ ConsumeW(s, fail, ck).derived = IF fail # {} /\ ConsumeKeepsLeft THEN 0 ELSE seq
        /\ ConsumeW(s, fail, ck).memo = w[s].memo
        /\ ConsumeW(s, fail, ck).rnow = w[s].rnow
        /\ ConsumeW(s, fail, ck).sStage = w[s].sStage
        /\ ConsumeW(s, fail, ck).scope = w[s].scope
        /\ ConsumeW(s, fail, ck).skipped = {q \in Paths \ ConsumeTaken(s, fail) : doc[q] # w[s].baseline[q] /\ Held(s, q)
                                              /\ w[s].local[q] # w[s].baseline[q]}
    BY DEF ConsumeW
  <2>3b. /\ ConsumeW(s, fail, ck).baseline = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]]
        /\ ConsumeW(s, fail, ck).uploads = w[s].uploads
        /\ ConsumeW(s, fail, ck).upDone = w[s].upDone
        /\ ConsumeW(s, fail, ck).deletes = w[s].deletes
        /\ ConsumeW(s, fail, ck).snap = w[s].snap
        /\ ConsumeW(s, fail, ck).inst = w[s].inst
        /\ ConsumeW(s, fail, ck).adv = w[s].adv
    BY DEF ConsumeW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(ConsumeW(s, fail, ck)) BY <2>1, <2>3, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(ConsumeW(s, fail, ck)) 
    <3>1. ASSUME ConsumeW(s, fail, ck).derived = seq' PROVE fail = {}
      BY <3>1, <2>1, <2>3, <1>s, <1>c DEF M3Seq
    <3>2. ASSUME NEW p \in Paths, fail = {},
                 HeldR(ConsumeW(s, fail, ck), p), doc'[p] # ConsumeW(s, fail, ck).baseline[p]
          PROVE  p \in ConsumeW(s, fail, ck).skipped
      <4>1. p \notin ConsumeTaken(s, fail) BY <3>2, <2>1, <2>3b
      <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF ConsumeTaken, ConsumeOwed
      <4>3. ConsumeW(s, fail, ck).baseline[p] = w[s].baseline[p] BY <4>1, <2>3b
      <4>4. Held(s, p) BY <3>2, <4>3, <2>3 DEF HeldR, Held
      <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <2>1, <1>s DEF Owed
      <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <2>1, <2>3
    <3>. QED BY <3>1, <3>2 DEF RRec
  <2>6. RMine(ConsumeW(s, fail, ck)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(ConsumeW(s, fail, ck)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(ConsumeW(s, fail, ck)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>. QED BY <1>1, <1>2

LEMMA Scan_M3 ==
  ASSUME IndM3, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Scan_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\ UNCHANGED <<doc, seq>> BY DEF Scan, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Scan
<1>2. Wr(s, ScanW(s, dels)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ ScanW(s, dels).pc = "scanned"
      /\ ScanW(s, dels).derived = w[s].derived
      /\ ScanW(s, dels).memo = w[s].memo
      /\ ScanW(s, dels).rnow = w[s].rnow
      /\ ScanW(s, dels).sStage = w[s].sStage
      /\ ScanW(s, dels).scope = w[s].scope
      /\ ScanW(s, dels).skipped = w[s].skipped
  BY DEF ScanW
<1>3b. /\ ScanW(s, dels).baseline = w[s].baseline
      /\ ScanW(s, dels).inst = w[s].inst
      /\ ScanW(s, dels).adv = w[s].adv
  BY DEF ScanW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(ScanW(s, dels)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(ScanW(s, dels)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(ScanW(s, dels)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(ScanW(s, dels)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(ScanW(s, dels)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Skip_M3 ==
  ASSUME IndM3, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Skip_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Skip, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Skip
<1>2. Wr(s, SkipW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ SkipW(s).pc = "idle"
      /\ SkipW(s).derived = w[s].derived
      /\ SkipW(s).memo = w[s].memo
      /\ SkipW(s).rnow = w[s].rnow
      /\ SkipW(s).sStage = w[s].sStage
      /\ SkipW(s).scope = w[s].scope
      /\ SkipW(s).skipped = w[s].skipped
  BY DEF SkipW
<1>3b. /\ SkipW(s).baseline = w[s].baseline
      /\ SkipW(s).uploads = w[s].uploads
      /\ SkipW(s).upDone = w[s].upDone
      /\ SkipW(s).deletes = w[s].deletes
      /\ SkipW(s).snap = w[s].snap
      /\ SkipW(s).inst = w[s].inst
      /\ SkipW(s).adv = w[s].adv
  BY DEF SkipW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(SkipW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(SkipW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(SkipW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(SkipW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(SkipW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Upload_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Upload_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ UNCHANGED <<doc, seq>> BY <1>1 DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" BY DEF Upload
  <2>2. Wr(s, UploadW(s, p)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ UploadW(s, p).pc = w[s].pc
        /\ UploadW(s, p).derived = w[s].derived
        /\ UploadW(s, p).memo = w[s].memo
        /\ UploadW(s, p).rnow = w[s].rnow
        /\ UploadW(s, p).sStage = w[s].sStage
        /\ UploadW(s, p).scope = w[s].scope
        /\ UploadW(s, p).skipped = w[s].skipped
    BY DEF UploadW
  <2>3b. /\ UploadW(s, p).baseline = w[s].baseline
        /\ UploadW(s, p).uploads = w[s].uploads
        /\ UploadW(s, p).deletes = w[s].deletes
        /\ UploadW(s, p).snap = w[s].snap
        /\ UploadW(s, p).inst = w[s].inst
        /\ UploadW(s, p).adv = w[s].adv
    BY DEF UploadW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(UploadW(s, p)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(UploadW(s, p)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(UploadW(s, p)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(UploadW(s, p)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(UploadW(s, p)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\ UNCHANGED <<doc, seq>> BY <1>2 DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" BY DEF Upload
  <2>2. Wr(s, UploadCopyW(s, p, c)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ UploadCopyW(s, p, c).pc = w[s].pc
        /\ UploadCopyW(s, p, c).derived = w[s].derived
        /\ UploadCopyW(s, p, c).memo = w[s].memo
        /\ UploadCopyW(s, p, c).rnow = w[s].rnow
        /\ UploadCopyW(s, p, c).sStage = w[s].sStage
        /\ UploadCopyW(s, p, c).scope = w[s].scope
        /\ UploadCopyW(s, p, c).skipped = w[s].skipped
    BY DEF UploadCopyW
  <2>3b. /\ UploadCopyW(s, p, c).baseline = w[s].baseline
        /\ UploadCopyW(s, p, c).uploads = w[s].uploads
        /\ UploadCopyW(s, p, c).deletes = w[s].deletes
        /\ UploadCopyW(s, p, c).inst = w[s].inst
        /\ UploadCopyW(s, p, c).adv = w[s].adv
    BY DEF UploadCopyW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(UploadCopyW(s, p, c)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(UploadCopyW(s, p, c)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(UploadCopyW(s, p, c)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(UploadCopyW(s, p, c)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(UploadCopyW(s, p, c)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>. QED BY <1>1, <1>2

LEMMA PullOnly_M3 ==
  ASSUME IndM3, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, PullOnly_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] /\ UNCHANGED <<doc, seq>> BY DEF PullOnly, bucket
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF PullOnly
<1>2. Wr(s, PullOnlyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ PullOnlyW(s).pc = "idle"
      /\ PullOnlyW(s).derived = w[s].derived
      /\ PullOnlyW(s).memo = w[s].memo
      /\ PullOnlyW(s).rnow = w[s].rnow
      /\ PullOnlyW(s).sStage = w[s].sStage
      /\ PullOnlyW(s).scope = w[s].scope
      /\ PullOnlyW(s).skipped = w[s].skipped
  BY DEF PullOnlyW
<1>3b. /\ PullOnlyW(s).baseline = w[s].baseline
      /\ PullOnlyW(s).uploads = w[s].uploads
      /\ PullOnlyW(s).deletes = w[s].deletes
      /\ PullOnlyW(s).adv = w[s].adv
  BY DEF PullOnlyW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(PullOnlyW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(PullOnlyW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(PullOnlyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(PullOnlyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(PullOnlyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Claim_M3 ==
  ASSUME IndM3, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Claim_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Claim
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF Claim
<1>2. Wr(s, ClaimW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ ClaimW(s).pc = "claimed"
      /\ ClaimW(s).derived = w[s].derived
      /\ ClaimW(s).memo = w[s].memo
      /\ ClaimW(s).rnow = w[s].rnow
      /\ ClaimW(s).sStage = w[s].sStage
      /\ ClaimW(s).scope = w[s].scope
      /\ ClaimW(s).skipped = w[s].skipped
  BY DEF ClaimW
<1>3b. /\ ClaimW(s).baseline = w[s].baseline
      /\ ClaimW(s).uploads = w[s].uploads
      /\ ClaimW(s).upDone = w[s].upDone
      /\ ClaimW(s).deletes = w[s].deletes
      /\ ClaimW(s).snap = w[s].snap
      /\ ClaimW(s).inst = w[s].inst
      /\ ClaimW(s).adv = w[s].adv
  BY DEF ClaimW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(ClaimW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(ClaimW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(ClaimW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(ClaimW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(ClaimW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Verify_M3 ==
  ASSUME IndM3, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Verify_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Verify, bucket
<1>g. On(s) /\ w[s].pc = "claimed" BY DEF Verify
<1>2. Wr(s, VerifyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ VerifyW(s).pc = w[s].pc
      /\ VerifyW(s).derived = w[s].derived
      /\ VerifyW(s).memo = w[s].memo
      /\ VerifyW(s).rnow = w[s].rnow
      /\ VerifyW(s).sStage = w[s].sStage
      /\ VerifyW(s).scope = w[s].scope
      /\ VerifyW(s).skipped = w[s].skipped
  BY DEF VerifyW
<1>3b. /\ VerifyW(s).baseline = w[s].baseline
      /\ VerifyW(s).uploads = w[s].uploads
      /\ VerifyW(s).upDone = w[s].upDone
      /\ VerifyW(s).deletes = w[s].deletes
      /\ VerifyW(s).snap = w[s].snap
      /\ VerifyW(s).inst = w[s].inst
      /\ VerifyW(s).adv = w[s].adv
  BY DEF VerifyW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(VerifyW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(VerifyW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(VerifyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(VerifyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(VerifyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Install_M3 ==
  ASSUME IndM3, NEW s \in Writers, Install(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Install_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. CommitAdvanceGuarded BY ShippedShape DEF Shipped
<1>g0. w[s].pc = "claimed" BY DEF Install
<1>r1. w[s].sStage = "none" BY <1>m, <1>g0 DEF R1
<1>ii. \A k \in Paths : k \notin (w[s].uploads \cap (w[s].upDone \ w[s].gone)) \cup w[s].deletes
                       => InstallInst(s)[k] = doc[k]
  BY DEF InstallInst, InstallMine
<1>1. /\ doc' = InstallInst(s) /\ w' = [w EXCEPT ![s] = InstallW(s)]
      /\ seq' = IF InstallInst(s) = doc THEN seq ELSE seq + 1
  BY DEF Install
<1>g. On(s) /\ w[s].pc = "claimed" BY DEF Install
<1>2. Wr(s, InstallW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ InstallW(s).pc = "cased"
      /\ InstallW(s).derived = w[s].derived
      /\ InstallW(s).memo = w[s].memo
      /\ InstallW(s).rnow = w[s].rnow
      /\ InstallW(s).sStage = w[s].sStage
      /\ InstallW(s).scope = w[s].scope
      /\ InstallW(s).skipped = w[s].skipped
  BY DEF InstallW
<1>3b. /\ InstallW(s).baseline = w[s].baseline
      /\ InstallW(s).uploads = w[s].uploads
      /\ InstallW(s).upDone = w[s].upDone \ w[s].gone
      /\ InstallW(s).deletes = w[s].deletes
      /\ InstallW(s).snap = w[s].snap
      /\ InstallW(s).inst = InstallInst(s)
      /\ InstallW(s).adv = IF CommitAdvanceGuarded THEN seq = w[s].derived ELSE TRUE
  BY DEF InstallW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(InstallW(s)) BY <1>1, <1>3, <1>e, <1>c, <1>f, <1>y DEF RB, Bounds, SeqEv
<1>5. RRec(InstallW(s)) 
  <2>1. ASSUME InstallW(s).derived = seq' PROVE seq' = seq /\ doc' = doc
    BY <2>1, <1>3, <1>1, <1>e, <1>c, <1>f, <1>y DEF Bounds, SeqEv
  <2>. QED BY <2>1, <1>3, <1>3b, <1>r1, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(InstallW(s)) 
  <2>. SUFFICES ASSUME NEW p \in w[s].uploads \cap (w[s].upDone \ w[s].gone) PROVE InstallInst(s)[p] = w[s].snap[p]
    BY <1>3, <1>3b DEF RMine
  <2>1. p \in Paths /\ p \notin w[s].gone /\ p \in InstallMine(s) BY <1>f DEF InstallMine
  <2>. QED BY <2>1 DEF InstallInst
<1>7. RAdv(InstallW(s)) 
  <2>1. ASSUME InstallW(s).adv PROVE w[s].derived = seq BY <2>1, <1>3b, <1>s
  <2>2. ASSUME InstallW(s).adv,
               NEW p \in Paths \ ((InstallW(s).uploads \cap InstallW(s).upDone) \cup InstallW(s).deletes),
               HeldR(InstallW(s), p), InstallW(s).inst[p] # InstallW(s).baseline[p]
        PROVE  p \in InstallW(s).skipped
    <3>1. InstallInst(s)[p] = doc[p] BY <2>2, <1>3b, <1>ii
    <3>2. Held(s, p) /\ doc[p] # w[s].baseline[p] BY <2>2, <3>1, <1>3, <1>3b DEF HeldR, Held
    <3>. QED BY <2>1, <2>2, <3>2, <1>3, <1>r1, <1>c DEF Record
  <2>. QED BY <2>1, <2>2 DEF RAdv
<1>8. RSame(InstallW(s)) 
  <2>1. ASSUME InstallW(s).derived = seq' PROVE seq' = seq /\ w[s].derived = seq
    BY <2>1, <1>3, <1>1, <1>e, <1>c, <1>f, <1>y DEF Bounds, SeqEv
  <2>. QED BY <2>1, <1>1, <1>3, <1>3b, <1>s DEF RSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Collect_M3 ==
  ASSUME IndM3, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Collect_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Collect
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Collect
<1>2. Wr(s, CollectW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ CollectW(s).pc = w[s].pc
      /\ CollectW(s).derived = w[s].derived
      /\ CollectW(s).memo = w[s].memo
      /\ CollectW(s).rnow = w[s].rnow
      /\ CollectW(s).sStage = w[s].sStage
      /\ CollectW(s).scope = w[s].scope
      /\ CollectW(s).skipped = w[s].skipped
  BY DEF CollectW
<1>3b. /\ CollectW(s).baseline = w[s].baseline
      /\ CollectW(s).uploads = w[s].uploads
      /\ CollectW(s).upDone = w[s].upDone
      /\ CollectW(s).deletes = w[s].deletes
      /\ CollectW(s).snap = w[s].snap
      /\ CollectW(s).inst = w[s].inst
      /\ CollectW(s).adv = w[s].adv
  BY DEF CollectW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(CollectW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(CollectW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(CollectW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(CollectW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(CollectW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Finish_M3 ==
  ASSUME IndM3, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Finish_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Finish
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Finish
<1>2. Wr(s, FinishW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ FinishW(s).pc = "idle"
      /\ FinishW(s).derived = IF w[s].adv /\ w[s].inst = doc THEN seq ELSE w[s].derived
      /\ FinishW(s).memo = w[s].memo
      /\ FinishW(s).rnow = w[s].rnow
      /\ FinishW(s).sStage = w[s].sStage
      /\ FinishW(s).scope = w[s].scope
      /\ FinishW(s).skipped = IF w[s].adv /\ w[s].inst = doc
                 THEN w[s].skipped \ ((w[s].uploads \cap w[s].upDone) \cup {q \in w[s].deletes : w[s].inst[q] = Nil})
                 ELSE w[s].skipped
  BY DEF FinishW
<1>3b. /\ FinishW(s).baseline = [q \in Paths |-> IF q \in w[s].uploads \cap w[s].upDone THEN w[s].snap[q]
                                ELSE IF q \in w[s].deletes /\ w[s].inst[q] = Nil THEN Nil
                                ELSE w[s].baseline[q]]
      /\ FinishW(s).inst = w[s].inst
  BY DEF FinishW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(FinishW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(FinishW(s)) 
  <2>1. w[s].sStage = "none" /\ w[s].pc = "cased" BY <1>g, <1>m DEF R1
  <2>2. CASE w[s].adv /\ w[s].inst = doc
    <3>. SUFFICES ASSUME NEW p \in Paths, HeldR(FinishW(s), p), doc[p] # FinishW(s).baseline[p]
                  PROVE  p \in FinishW(s).skipped
      BY <1>1 DEF RRec
    <3>1. p \notin w[s].uploads \cap w[s].upDone
      BY <2>1, <2>2, <1>3b, <1>c DEF CasedMine
    <3>2. p \notin w[s].deletes
      BY <2>1, <2>2, <3>1, <1>3b, <1>m DEF Cased
    <3>3. FinishW(s).baseline[p] = w[s].baseline[p] BY <3>1, <3>2, <1>3b
    <3>4. Held(s, p) /\ w[s].inst[p] # w[s].baseline[p] BY <2>2, <3>3, <1>3 DEF HeldR, Held
    <3>5. p \in w[s].skipped BY <2>1, <2>2, <3>1, <3>2, <3>4, <1>c DEF CasedAdv
    <3>. QED BY <2>2, <3>1, <3>2, <3>5, <1>3
  <2>3. CASE ~(w[s].adv /\ w[s].inst = doc)
    <3>1. w[s].derived # seq BY <2>1, <2>3, <1>c DEF CasedSame
    <3>. QED BY <2>3, <3>1, <1>1, <1>3 DEF RRec
  <2>. QED BY <2>2, <2>3
<1>6. RMine(FinishW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(FinishW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(FinishW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the restart and the sync.                                            *)

LEMMA Restart_M3 ==
  ASSUME IndM3, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Restart_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Restart
<1>g. On(s) BY DEF Restart
<1>2. Wr(s, RestartW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RestartW(s).pc = "idle"
      /\ RestartW(s).derived = w[s].derived
      /\ RestartW(s).memo = w[s].memo
      /\ RestartW(s).rnow = w[s].rnow
      /\ RestartW(s).sStage = IF w[s].sStage = "none" THEN "none" ELSE "saved"
      /\ RestartW(s).scope = w[s].scope
      /\ RestartW(s).skipped = w[s].skipped
  BY DEF RestartW
<1>3b. /\ RestartW(s).baseline = w[s].baseline
  BY DEF RestartW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RestartW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RestartW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RestartW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RestartW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RestartW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Sync_M3 ==
  ASSUME IndM3, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Sync_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. SyncKeepsLeft /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\ UNCHANGED <<doc, seq>> BY DEF Sync, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Sync
<1>2. Wr(s, SyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ SyncW(s, fail).pc = w[s].pc
      /\ SyncW(s, fail).derived = IF fail # {} /\ SyncKeepsLeft THEN 0 ELSE seq
      /\ SyncW(s, fail).memo = w[s].memo
      /\ SyncW(s, fail).rnow = w[s].rnow
      /\ SyncW(s, fail).sStage = w[s].sStage
      /\ SyncW(s, fail).scope = w[s].scope
      /\ SyncW(s, fail).skipped = IF fail # {} /\ SyncKeepsLeft THEN {}
              ELSE {q \in Paths : doc[q] # SyncBl(s, fail)[q] /\ w[s].local[q] # SyncBl(s, fail)[q] /\ Held(s, q)}
  BY DEF SyncW
<1>3b. /\ SyncW(s, fail).baseline = SyncBl(s, fail)
      /\ SyncW(s, fail).uploads = w[s].uploads
      /\ SyncW(s, fail).upDone = w[s].upDone
      /\ SyncW(s, fail).deletes = w[s].deletes
      /\ SyncW(s, fail).snap = w[s].snap
      /\ SyncW(s, fail).inst = w[s].inst
      /\ SyncW(s, fail).adv = w[s].adv
  BY DEF SyncW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(SyncW(s, fail)) BY <1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(SyncW(s, fail)) 
  <3>1. ASSUME SyncW(s, fail).derived = seq' PROVE fail = {}
    BY <3>1, <1>1, <1>3, <1>s, <1>c DEF M3Seq
  <3>2. ASSUME NEW p \in Paths, fail = {},
               HeldR(SyncW(s, fail), p), doc'[p] # SyncW(s, fail).baseline[p]
        PROVE  p \in SyncW(s, fail).skipped
    <4>1. p \notin SyncOwed(s, fail) BY <3>2, <1>1, <1>3b DEF SyncBl
    <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF SyncOwed, SyncAll
    <4>3. SyncW(s, fail).baseline[p] = w[s].baseline[p] BY <4>1, <1>3b DEF SyncBl
    <4>4. Held(s, p) BY <3>2, <4>3, <1>3 DEF HeldR, Held
    <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <1>1, <1>s DEF Owed
    <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <1>1, <1>3 DEF SyncBl
  <3>. QED BY <3>1, <3>2 DEF RRec
<1>6. RMine(SyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(SyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(SyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the narrow / widen verb.                                             *)

LEMMA RescopeBegin_M3 ==
  ASSUME IndM3, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RescopeBegin_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\ UNCHANGED <<doc, seq>> BY DEF RescopeBegin, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeBegin
<1>2. Wr(s, RescopeBeginW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeBeginW(s, T).pc = w[s].pc
      /\ RescopeBeginW(s, T).derived = w[s].derived
      /\ RescopeBeginW(s, T).memo = w[s].memo
      /\ RescopeBeginW(s, T).rnow = w[s].rnow
      /\ RescopeBeginW(s, T).sStage = "saved"
      /\ RescopeBeginW(s, T).scope = w[s].scope
      /\ RescopeBeginW(s, T).skipped = w[s].skipped
  BY DEF RescopeBeginW
<1>3b. /\ RescopeBeginW(s, T).baseline = w[s].baseline
      /\ RescopeBeginW(s, T).uploads = w[s].uploads
      /\ RescopeBeginW(s, T).upDone = w[s].upDone
      /\ RescopeBeginW(s, T).deletes = w[s].deletes
      /\ RescopeBeginW(s, T).snap = w[s].snap
      /\ RescopeBeginW(s, T).inst = w[s].inst
      /\ RescopeBeginW(s, T).adv = w[s].adv
  BY DEF RescopeBeginW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RescopeBeginW(s, T)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RescopeBeginW(s, T)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RescopeBeginW(s, T)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RescopeBeginW(s, T)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RescopeBeginW(s, T)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA RescopeFirst_M3 ==
  ASSUME IndM3, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RescopeFirst_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\ UNCHANGED <<doc, seq>> BY DEF RescopeFirst, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeFirst
<1>2. Wr(s, RescopeFirstW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeFirstW(s).pc = w[s].pc
      /\ RescopeFirstW(s).derived = w[s].derived
      /\ RescopeFirstW(s).memo = w[s].memo
      /\ RescopeFirstW(s).rnow = w[s].rnow
      /\ RescopeFirstW(s).sStage = "mid"
      /\ RescopeFirstW(s).scope = w[s].scope
      /\ RescopeFirstW(s).skipped = w[s].skipped
  BY DEF RescopeFirstW
<1>3b. /\ RescopeFirstW(s).uploads = w[s].uploads
      /\ RescopeFirstW(s).upDone = w[s].upDone
      /\ RescopeFirstW(s).deletes = w[s].deletes
      /\ RescopeFirstW(s).snap = w[s].snap
      /\ RescopeFirstW(s).inst = w[s].inst
      /\ RescopeFirstW(s).adv = w[s].adv
  BY DEF RescopeFirstW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RescopeFirstW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RescopeFirstW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RescopeFirstW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RescopeFirstW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RescopeFirstW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA RescopeSecond_M3 ==
  ASSUME IndM3, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RescopeSecond_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\ UNCHANGED <<doc, seq>>
  BY DEF RescopeSecond, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeSecond
<1>2. Wr(s, RescopeSecondW(s, wfail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeSecondW(s, wfail).pc = w[s].pc
      /\ RescopeSecondW(s, wfail).derived = 0
      /\ RescopeSecondW(s, wfail).memo = w[s].memo
      /\ RescopeSecondW(s, wfail).rnow = w[s].rnow
      /\ RescopeSecondW(s, wfail).sStage = "none"
      /\ RescopeSecondW(s, wfail).skipped = {}
  BY DEF RescopeSecondW
<1>3b. /\ RescopeSecondW(s, wfail).uploads = w[s].uploads
      /\ RescopeSecondW(s, wfail).upDone = w[s].upDone
      /\ RescopeSecondW(s, wfail).deletes = w[s].deletes
      /\ RescopeSecondW(s, wfail).snap = w[s].snap
      /\ RescopeSecondW(s, wfail).inst = w[s].inst
      /\ RescopeSecondW(s, wfail).adv = w[s].adv
  BY DEF RescopeSecondW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RescopeSecondW(s, wfail)) BY <1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RescopeSecondW(s, wfail)) BY <1>1, <1>3, <1>c DEF RRec, M3Seq
<1>6. RMine(RescopeSecondW(s, wfail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RescopeSecondW(s, wfail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RescopeSecondW(s, wfail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: a reader's tick.                                                     *)

LEMMA RPullRead_M3 ==
  ASSUME IndM3, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RPullRead_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<doc, seq>> BY DEF RPullRead, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RPullRead
<1>2. Wr(s, RPullReadW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RPullReadW(s).pc = "pulling"
      /\ RPullReadW(s).derived = w[s].derived
      /\ RPullReadW(s).memo = w[s].memo
      /\ RPullReadW(s).rnow = seq
      /\ RPullReadW(s).sStage = w[s].sStage
      /\ RPullReadW(s).scope = w[s].scope
      /\ RPullReadW(s).skipped = w[s].skipped
  BY DEF RPullReadW
<1>3b. /\ RPullReadW(s).baseline = w[s].baseline
      /\ RPullReadW(s).uploads = w[s].uploads
      /\ RPullReadW(s).upDone = w[s].upDone
      /\ RPullReadW(s).deletes = w[s].deletes
      /\ RPullReadW(s).snap = w[s].snap
      /\ RPullReadW(s).inst = w[s].inst
      /\ RPullReadW(s).adv = w[s].adv
  BY DEF RPullReadW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RPullReadW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RPullReadW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RPullReadW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RPullReadW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RPullReadW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA RPullSync_M3 ==
  ASSUME IndM3, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RPullSync_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. SyncKeepsLeft /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\ UNCHANGED <<doc, seq>> BY DEF RPullSync, bucket
<1>g. On(s) /\ w[s].pc = "pulling" BY DEF RPullSync
<1>2. Wr(s, RPullSyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RPullSyncW(s, fail).pc = "idle"
      /\ RPullSyncW(s, fail).derived = IF fail # {} /\ SyncKeepsLeft THEN 0 ELSE seq
      /\ RPullSyncW(s, fail).memo = w[s].rnow
      /\ RPullSyncW(s, fail).rnow = w[s].rnow
      /\ RPullSyncW(s, fail).sStage = w[s].sStage
      /\ RPullSyncW(s, fail).scope = w[s].scope
      /\ RPullSyncW(s, fail).skipped = IF fail # {} /\ SyncKeepsLeft THEN {}
              ELSE {q \in Paths : doc[q] # SyncBl(s, fail)[q] /\ w[s].local[q] # SyncBl(s, fail)[q] /\ Held(s, q)}
  BY DEF RPullSyncW
<1>3b. /\ RPullSyncW(s, fail).baseline = SyncBl(s, fail)
      /\ RPullSyncW(s, fail).uploads = w[s].uploads
      /\ RPullSyncW(s, fail).upDone = w[s].upDone
      /\ RPullSyncW(s, fail).deletes = w[s].deletes
      /\ RPullSyncW(s, fail).snap = w[s].snap
      /\ RPullSyncW(s, fail).inst = w[s].inst
      /\ RPullSyncW(s, fail).adv = w[s].adv
  BY DEF RPullSyncW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RPullSyncW(s, fail)) BY <1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RPullSyncW(s, fail)) 
  <3>1. ASSUME RPullSyncW(s, fail).derived = seq' PROVE fail = {}
    BY <3>1, <1>1, <1>3, <1>s, <1>c DEF M3Seq
  <3>2. ASSUME NEW p \in Paths, fail = {},
               HeldR(RPullSyncW(s, fail), p), doc'[p] # RPullSyncW(s, fail).baseline[p]
        PROVE  p \in RPullSyncW(s, fail).skipped
    <4>1. p \notin SyncOwed(s, fail) BY <3>2, <1>1, <1>3b DEF SyncBl
    <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF SyncOwed, SyncAll
    <4>3. RPullSyncW(s, fail).baseline[p] = w[s].baseline[p] BY <4>1, <1>3b DEF SyncBl
    <4>4. Held(s, p) BY <3>2, <4>3, <1>3 DEF HeldR, Held
    <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <1>1, <1>s DEF Owed
    <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <1>1, <1>3 DEF SyncBl
  <3>. QED BY <3>1, <3>2 DEF RRec
<1>6. RMine(RPullSyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RPullSyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RPullSyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the answer (row 15).                                                 *)

LEMMA Touch_M3 ==
  ASSUME IndM3, NEW s \in Writers, Touch(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Touch_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Touch, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Touch
<1>2. Wr(s, TouchW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ TouchW(s).pc = w[s].pc
      /\ TouchW(s).derived = w[s].derived
      /\ TouchW(s).memo = w[s].memo
      /\ TouchW(s).rnow = w[s].rnow
      /\ TouchW(s).sStage = w[s].sStage
      /\ TouchW(s).scope = w[s].scope
      /\ TouchW(s).skipped = w[s].skipped
  BY DEF TouchW
<1>3b. /\ TouchW(s).baseline = w[s].baseline
      /\ TouchW(s).uploads = w[s].uploads
      /\ TouchW(s).upDone = w[s].upDone
      /\ TouchW(s).deletes = w[s].deletes
      /\ TouchW(s).snap = w[s].snap
      /\ TouchW(s).inst = w[s].inst
      /\ TouchW(s).adv = w[s].adv
  BY DEF TouchW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(TouchW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(TouchW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(TouchW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(TouchW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(TouchW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Honor_M3 ==
  ASSUME IndM3, NEW s \in Writers, Honor(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Honor_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Honor, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Honor
<1>2. Wr(s, HonorW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ HonorW(s).pc = w[s].pc
      /\ HonorW(s).derived = w[s].derived
      /\ HonorW(s).memo = w[s].memo
      /\ HonorW(s).rnow = w[s].rnow
      /\ HonorW(s).sStage = w[s].sStage
      /\ HonorW(s).scope = w[s].scope
      /\ HonorW(s).skipped = w[s].skipped
  BY DEF HonorW
<1>3b. /\ HonorW(s).baseline = w[s].baseline
      /\ HonorW(s).uploads = w[s].uploads
      /\ HonorW(s).upDone = w[s].upDone
      /\ HonorW(s).deletes = w[s].deletes
      /\ HonorW(s).snap = w[s].snap
      /\ HonorW(s).inst = w[s].inst
      /\ HonorW(s).adv = w[s].adv
  BY DEF HonorW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(HonorW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(HonorW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(HonorW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(HonorW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(HonorW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA HonorReader_M3 ==
  ASSUME IndM3, NEW s \in Writers, HonorReader(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, HonorReader_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] /\ UNCHANGED <<doc, seq>> BY DEF HonorReader, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF HonorReader
<1>2. Wr(s, HonorReaderW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ HonorReaderW(s).pc = w[s].pc
      /\ HonorReaderW(s).derived = w[s].derived
      /\ HonorReaderW(s).memo = w[s].memo
      /\ HonorReaderW(s).rnow = w[s].rnow
      /\ HonorReaderW(s).sStage = w[s].sStage
      /\ HonorReaderW(s).scope = w[s].scope
      /\ HonorReaderW(s).skipped = w[s].skipped
  BY DEF HonorReaderW
<1>3b. /\ HonorReaderW(s).baseline = w[s].baseline
      /\ HonorReaderW(s).uploads = w[s].uploads
      /\ HonorReaderW(s).upDone = w[s].upDone
      /\ HonorReaderW(s).deletes = w[s].deletes
      /\ HonorReaderW(s).snap = w[s].snap
      /\ HonorReaderW(s).inst = w[s].inst
      /\ HonorReaderW(s).adv = w[s].adv
  BY DEF HonorReaderW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(HonorReaderW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(HonorReaderW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(HonorReaderW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(HonorReaderW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(HonorReaderW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA RetireByAck_M3 ==
  ASSUME IndM3, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RetireByAck_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] /\ UNCHANGED <<doc, seq>> BY DEF RetireByAck, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RetireByAck
<1>2. Wr(s, RetireByAckW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RetireByAckW(s).pc = w[s].pc
      /\ RetireByAckW(s).derived = w[s].derived
      /\ RetireByAckW(s).memo = w[s].memo
      /\ RetireByAckW(s).rnow = w[s].rnow
      /\ RetireByAckW(s).sStage = w[s].sStage
      /\ RetireByAckW(s).scope = w[s].scope
      /\ RetireByAckW(s).skipped = w[s].skipped
  BY DEF RetireByAckW
<1>3b. /\ RetireByAckW(s).baseline = w[s].baseline
      /\ RetireByAckW(s).uploads = w[s].uploads
      /\ RetireByAckW(s).upDone = w[s].upDone
      /\ RetireByAckW(s).deletes = w[s].deletes
      /\ RetireByAckW(s).snap = w[s].snap
      /\ RetireByAckW(s).inst = w[s].inst
      /\ RetireByAckW(s).adv = w[s].adv
  BY DEF RetireByAckW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RetireByAckW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RetireByAckW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RetireByAckW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RetireByAckW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RetireByAckW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: deposal (row 17).                                                    *)

\* The rotation moves the pointer and not the document: no record matches the new seq.
LEMMA Depose_M3 ==
  ASSUME IndM3, NEW t \in Writers, Depose(t), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Depose_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ seq' = seq + 1 /\ doc' = doc
  BY DEF Depose
<1>2. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>y, <1>1
<1>3. \A u \in Writers : /\ w'[u].derived = w[u].derived
                         /\ w'[u].memo = w[u].memo
                         /\ w'[u].rnow = w[u].rnow
                         /\ w'[u].sStage = w[u].sStage
                         /\ w'[u].scope = w[u].scope
                         /\ w'[u].skipped = w[u].skipped
                         /\ w'[u].baseline = w[u].baseline
                         /\ w'[u].uploads = w[u].uploads
                         /\ w'[u].upDone = w[u].upDone
                         /\ w'[u].deletes = w[u].deletes
                         /\ w'[u].snap = w[u].snap
                         /\ w'[u].inst = w[u].inst
                         /\ w'[u].adv = w[u].adv
                         /\ w'[u].pc = IF u = t THEN "claimed" ELSE w[u].pc
  BY <1>1, <1>2 DEF DeposeW, DeposedW
<1>n. \A u \in Writers : w[u].derived \in Nat /\ w[u].memo \in Nat /\ w[u].rnow \in Nat
  BY <1>y, WriterFields
<1>4. M3Seq' /\ Bounds' BY <1>1, <1>3, <1>c, <1>n, <1>y DEF M3Seq, Bounds
<1>5. Record' BY <1>1, <1>3, <1>c, <1>n, <1>y DEF Record, Bounds
<1>6. CasedMine' /\ CasedAdv' BY <1>1, <1>3, <1>c DEF CasedMine, CasedAdv, Held
<1>7. CasedSame' BY <1>1, <1>3, <1>c, <1>n, <1>y DEF CasedSame, Bounds
<1>. QED BY <1>a, <1>4, <1>5, <1>6, <1>7 DEF IndM3, M3

LEMMA Fence_M3 ==
  ASSUME IndM3, NEW s \in Writers, Fence(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Fence_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] /\ UNCHANGED <<doc, seq>> BY <1>1 DEF Fence, bucket
  <2>g. On(s) BY DEF Fence
  <2>2. Wr(s, FenceBeforeW(s)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ FenceBeforeW(s).pc = "idle"
        /\ FenceBeforeW(s).derived = w[s].derived
        /\ FenceBeforeW(s).memo = w[s].memo
        /\ FenceBeforeW(s).rnow = w[s].rnow
        /\ FenceBeforeW(s).sStage = w[s].sStage
        /\ FenceBeforeW(s).scope = w[s].scope
        /\ FenceBeforeW(s).skipped = w[s].skipped
    BY DEF FenceBeforeW
  <2>3b. /\ FenceBeforeW(s).baseline = w[s].baseline
    BY DEF FenceBeforeW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(FenceBeforeW(s)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(FenceBeforeW(s)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(FenceBeforeW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(FenceBeforeW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(FenceBeforeW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] /\ UNCHANGED <<doc, seq>> BY <1>2 DEF Fence, bucket
  <2>g. On(s) BY DEF Fence
  <2>2. Wr(s, FenceAfterW(s)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ FenceAfterW(s).pc = w[s].pc
        /\ FenceAfterW(s).derived = w[s].derived
        /\ FenceAfterW(s).memo = w[s].memo
        /\ FenceAfterW(s).rnow = w[s].rnow
        /\ FenceAfterW(s).sStage = w[s].sStage
        /\ FenceAfterW(s).scope = w[s].scope
        /\ FenceAfterW(s).skipped = w[s].skipped
    BY DEF FenceAfterW
  <2>3b. /\ FenceAfterW(s).baseline = w[s].baseline
        /\ FenceAfterW(s).uploads = w[s].uploads
        /\ FenceAfterW(s).upDone = w[s].upDone
        /\ FenceAfterW(s).deletes = w[s].deletes
        /\ FenceAfterW(s).snap = w[s].snap
        /\ FenceAfterW(s).inst = w[s].inst
        /\ FenceAfterW(s).adv = w[s].adv
    BY DEF FenceAfterW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(FenceAfterW(s)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(FenceAfterW(s)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(FenceAfterW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(FenceAfterW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(FenceAfterW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* M3: the step, and the theorems.                                          *)

LEMMA Next_M3 == IndM3 /\ Next => IndM3'
<1>. SUFFICES ASSUME IndM3, Next PROVE IndM3' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM3' BY <3>1, GPut_M3
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM3' BY <3>2, GCas_M3
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM3' BY <3>3, GDelete_M3
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM3' BY <3>4, GRename_M3
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M3
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM3' BY <3>1, Checkout_M3
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM3' BY <3>2, Consume_M3
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM3' BY <3>3, Scan_M3
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM3' BY <3>4, Skip_M3
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM3' BY <3>5, PullOnly_M3
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM3' BY <3>6, Claim_M3
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM3' BY <3>7, Verify_M3
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM3' BY <3>8, Install_M3
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM3' BY <3>9, Collect_M3
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM3' BY <3>10, Finish_M3
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM3' BY <3>11, Restart_M3
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM3' BY <3>12, Sync_M3
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM3' BY <3>13, RescopeBegin_M3
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM3' BY <3>14, RescopeFirst_M3
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM3' BY <3>15, RescopeSecond_M3
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM3' BY <3>16, RPullRead_M3
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM3' BY <3>17, RPullSync_M3
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM3' BY <3>18, Edit_M3
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM3' BY <3>19, Delete_M3
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM3' BY <3>20, Upload_M3
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM3' BY <3>21, Sweep_M3
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE IndM3' BY <3>22, Touch_M3
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE IndM3' BY <3>23, Honor_M3
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE IndM3' BY <3>24, HonorReader_M3
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE IndM3' BY <3>25, RetireByAck_M3
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE IndM3' BY <3>26, Depose_M3
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE IndM3' BY <3>27, Fence_M3
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M3
  <2>2. CASE RLoad BY <2>2, RLoad_M3
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM3' BY <2>3, Reap_M3
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M3Invariant == Spec => []IndM3
<1>1. Init => IndM3 BY Init_M3
<1>2. IndM3 /\ [Next]_vars => IndM3'
  <2>1. IndM3 /\ Next => IndM3' BY Next_M3
  <2>2. IndM3 /\ UNCHANGED vars => IndM3'
    <3>1. IndM3 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM3 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM3, IndM2, IndM1, vars
    <3>3. IndM3 /\ UNCHANGED vars => M2' BY M2Same DEF IndM3, IndM2, vars, aux, ret
    <3>4. IndM3 /\ UNCHANGED vars => M3' BY M3Same DEF IndM3, vars, bucket
    <3>. QED BY <3>1, <3>2, <3>3, <3>4 DEF IndM3, IndM2, IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

\* The cheap path skips only a writer owed nothing: an owed path is held and
\* differs from the baseline, so the record has it skipped, and the cheap
\* path re-checks every skipped path as dirty.
LEMMA ShortcutFromRecord == IndM3 => Inv_ShortcutSound
<1>. SUFFICES ASSUME IndM3, NEW s \in Writers, On(s), w[s].pc = "idle", w[s].sStage = "none", CheapPath(s),
                     NEW p \in Paths, Owed(s, p)
              PROVE  FALSE
  BY DEF Inv_ShortcutSound
<1>s. RecheckSkipped /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. w[s].derived = seq /\ \A q \in w[s].skipped : w[s].local[q] # w[s].baseline[q] BY <1>s DEF CheapPath
<1>2. Held(s, p) /\ doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p] BY <1>s DEF Owed
<1>3. p \in w[s].skipped BY <1>1, <1>2 DEF IndM3, M3, Record
<1>. QED BY <1>1, <1>2, <1>3

\* A reader that skips a tick has `memo = seq` and something derived, so by
\* the bounds its record is current; it re-checks every skipped path.
LEMMA ReaderFromRecord == IndM3 => Inv_ReaderSound
<1>. SUFFICES ASSUME IndM3, NEW s \in Readers, On(s), w[s].pc = "idle", w[s].sStage = "none", ReaderSkips(s),
                     NEW p \in Paths, Owed(s, p)
              PROVE  FALSE
  BY DEF Inv_ReaderSound
<1>0. s \in Writers BY ReadersWriters
<1>s. ReaderRechecksOwed /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>y. w[s] \in Writer /\ seq \in Nat BY <1>0 DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>n. w[s].derived \in Nat /\ w[s].memo \in Nat BY <1>y, WriterFields
<1>1. w[s].memo = seq /\ w[s].derived # 0 /\ \A q \in w[s].skipped : w[s].local[q] # w[s].baseline[q]
  BY <1>s DEF ReaderSkips, StillOwed
<1>2. w[s].derived = seq BY <1>0, <1>1, <1>y, <1>n DEF IndM3, M3, Bounds
<1>3. Held(s, p) /\ doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p] BY <1>s DEF Owed
<1>4. p \in w[s].skipped BY <1>0, <1>2, <1>3 DEF IndM3, M3, Record
<1>. QED BY <1>1, <1>3, <1>4

THEOREM ShortcutSound == Spec => []Inv_ShortcutSound
BY M3Invariant, ShortcutFromRecord, PTL

THEOREM ReaderSound == Spec => []Inv_ReaderSound
BY M3Invariant, ReaderFromRecord, PTL

==============================================================================
