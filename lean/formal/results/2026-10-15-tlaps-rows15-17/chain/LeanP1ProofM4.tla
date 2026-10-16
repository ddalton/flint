---------------------------- MODULE LeanP1ProofM4 ----------------------------
EXTENDS LeanP1ProofM3

------------------------------------------------------------------------------
(* M4: Inv_ReaderFetches.                                                   *)

\* The never-re-cited lemma: a step cites a fresh handle or moves a cited
\* one, and what stops being cited is logged retiring (`RetUpdate`).
NeverReCited == \A p \in Paths : doc[p] # Nil => doc[p] \notin retiring \cup aged
\* I11: a reader that loaded the document less than G ago holds handles each
\* live, not aged, and still cited or retiring.
ReaderLive == ~rlag => \A p \in Paths : rdoc[p] # Nil =>
                /\ rdoc[p] \in live /\ rdoc[p] \notin aged
                /\ (Cited(rdoc[p]) \/ rdoc[p] \in retiring)
M4 == NeverReCited /\ ReaderLive
IndM4 == IndM3 /\ M4

\* What a step may newly cite is cited already or neither retiring nor aged.
DocNew == \A k \in Paths : doc'[k] # Nil => doc'[k] \in CitedSet \/ doc'[k] \notin retiring \cup aged
\* What `live` may lose is uncited and not retiring.
LiveLoss == \A h \in live : h \notin live' => ~Cited(h) /\ h \notin retiring

------------------------------------------------------------------------------
(* What the events give M4.                                                 *)

\* Under the retire age every frame step logs exactly what stops being cited.
LEMMA RetExact ==
  ASSUME RetireAge, RetUpdate
  PROVE  retiring' = retiring \cup (CitedSet \ CitedSet')
BY DEF RetUpdate, CitedSet

LEMMA DocSameNew == doc' = doc => DocNew
BY DEF DocNew, CitedSet

LEMMA LiveGrows == live \subseteq live' => LiveLoss
BY DEF LiveLoss

LEMMA M4Write ==
  ASSUME M4, DocNew, LiveLoss,
         retiring' = retiring \cup (CitedSet \ CitedSet'),
         aged' = aged, rdoc' = rdoc, rlag' = rlag
  PROVE  M4'
<1>1. NeverReCited'
  <2>. SUFFICES ASSUME NEW p \in Paths, doc'[p] # Nil PROVE doc'[p] \notin retiring' \cup aged'
    BY DEF NeverReCited
  <2>1. doc'[p] \in CitedSet' BY DEF CitedSet
  <2>2. CASE doc'[p] \in CitedSet
    <3>1. doc'[p] \notin retiring \cup aged BY <2>2 DEF M4, NeverReCited, CitedSet
    <3>. QED BY <2>1, <3>1
  <2>3. CASE doc'[p] \notin retiring \cup aged BY <2>1, <2>3
  <2>. QED BY <2>2, <2>3 DEF DocNew
<1>2. ReaderLive'
  <2>. SUFFICES ASSUME ~rlag, NEW p \in Paths, rdoc[p] # Nil
                PROVE  /\ rdoc[p] \in live' /\ rdoc[p] \notin aged'
                       /\ ((\E k \in Paths : doc'[k] = rdoc[p]) \/ rdoc[p] \in retiring')
    BY DEF ReaderLive, Cited
  <2>0. /\ rdoc[p] \in live /\ rdoc[p] \notin aged
        /\ ((\E k \in Paths : doc[k] = rdoc[p]) \/ rdoc[p] \in retiring)
    BY DEF M4, ReaderLive, Cited
  <2>1. rdoc[p] \in live' BY <2>0 DEF LiveLoss, Cited
  <2>2. CASE \E k \in Paths : doc[k] = rdoc[p]
    <3>1. rdoc[p] \in CitedSet BY <2>2 DEF CitedSet
    <3>. QED BY <2>0, <2>1, <3>1 DEF CitedSet
  <2>. QED BY <2>0, <2>1, <2>2
<1>. QED BY <1>1, <1>2 DEF M4

\* M4 after a step that moves nothing M4 reads.
LEMMA M4Same ==
  ASSUME M4, UNCHANGED <<doc, live, retiring, aged, rdoc, rlag>>
  PROVE  M4'
BY DEF M4, NeverReCited, ReaderLive, Cited

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M4 == Init => IndM4
<1>. SUFFICES ASSUME Init PROVE IndM4 OBVIOUS
<1>1. IndM3 BY Init_M3
<1>2. NeverReCited /\ ReaderLive BY DEF Init, NeverReCited, ReaderLive
<1>. QED BY <1>1, <1>2 DEF IndM4, M4

------------------------------------------------------------------------------
(* M4: the gateway.                                                         *)

LEMMA GPut_M4 ==
  ASSUME IndM4, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GPut_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF GPut, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF GPut, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA GCas_M4 ==
  ASSUME IndM4, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GCas_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d. DocNew 
  <2>1. gw[p] \notin retiring \cup aged /\ gw[p] # Nil BY DEF GCas, IndM4, IndM3, IndM2, M2, Flight
  <2>2. \A k \in Paths : doc'[k] = doc[k] \/ doc'[k] = gw[p]
    <3>1. doc \in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
    <3>. QED BY <3>1 DEF GCas
  <2>. QED BY <2>1, <2>2 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF GCas, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA GRename_M4 ==
  ASSUME IndM4, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GRename_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>sr. RenameAtomic BY ShippedShape DEF Shipped
<1>d. DocNew 
  <2>1. doc \in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. doc[p] # Nil /\ \A k \in Paths : doc'[k] = doc[k] \/ doc'[k] = Nil \/ doc'[k] = doc[p]
    BY <1>sr, <2>1 DEF GRename
  <2>. QED BY <2>2 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF GRename, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA GRenameFinish_M4 ==
  ASSUME IndM4, GRenameFinish, Frame
  PROVE  IndM4'
<1>1. mv = Nil BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

LEMMA GDelete_M4 ==
  ASSUME IndM4, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GDelete_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d. DocNew 
  <2>1. doc \in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. \A k \in Paths : doc'[k] = doc[k] \/ doc'[k] = Nil BY <2>1 DEF GDelete
  <2>. QED BY <2>2 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF GDelete, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Sweep_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Sweep_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Sweep, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l. LiveLoss 
  <2>1. live' = live \ {h} /\ ~Cited(h) /\ h \notin retiring BY <1>s DEF Sweep
  <2>. QED BY <2>1 DEF LiveLoss
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

------------------------------------------------------------------------------
(* M4: the agent, the commit section, the sync, the rescope, the reader.    *)

LEMMA Edit_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Edit_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Edit, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Edit, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Delete_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Delete_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Delete, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Delete, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Checkout_M4 ==
  ASSUME IndM4, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Checkout_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Checkout, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Checkout, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Consume_M4 ==
  ASSUME IndM4, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Consume_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Consume, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Consume, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Scan_M4 ==
  ASSUME IndM4, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Scan_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Scan, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Scan, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Skip_M4 ==
  ASSUME IndM4, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Skip_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Skip, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Skip, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Upload_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Upload_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Upload, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Upload, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA PullOnly_M4 ==
  ASSUME IndM4, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, PullOnly_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF PullOnly, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF PullOnly, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Claim_M4 ==
  ASSUME IndM4, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Claim_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Claim, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Claim, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Verify_M4 ==
  ASSUME IndM4, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Verify_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Verify, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Verify, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Install_M4 ==
  ASSUME IndM4, NEW s \in Writers, Install(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Install_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d. DocNew 
  <2>1. \A k \in Paths : InstallInst(s)[k] = Nil \/ InstallInst(s)[k] = doc[k]
                         \/ (k \in InstallMine(s) \ w[s].gone /\ InstallInst(s)[k] = w[s].snap[k])
    BY DEF InstallInst
  <2>2. \A k \in InstallMine(s) \ w[s].gone : w[s].snap[k] \notin retiring \cup aged
    BY DEF IndM4, IndM3, IndM2, M2, Uploaded, Up, InstallMine, Install
  <2>3. doc' = InstallInst(s) BY DEF Install
  <2>. QED BY <2>1, <2>2, <2>3 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF Install, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Collect_M4 ==
  ASSUME IndM4, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Collect_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Collect, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l. LiveLoss 
  <2>1. live' = live BY <1>s DEF Collect
  <2>. QED BY <2>1, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Finish_M4 ==
  ASSUME IndM4, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Finish_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Finish, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Finish, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Restart_M4 ==
  ASSUME IndM4, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Restart_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Restart, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Restart, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Sync_M4 ==
  ASSUME IndM4, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Sync_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Sync, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Sync, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RescopeBegin_M4 ==
  ASSUME IndM4, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RescopeBegin_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RescopeBegin, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RescopeBegin, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RescopeFirst_M4 ==
  ASSUME IndM4, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RescopeFirst_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RescopeFirst, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RescopeFirst, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RescopeSecond_M4 ==
  ASSUME IndM4, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RescopeSecond_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RescopeSecond, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RescopeSecond, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RPullRead_M4 ==
  ASSUME IndM4, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RPullRead_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RPullRead, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RPullRead, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RPullSync_M4 ==
  ASSUME IndM4, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RPullSync_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RPullSync, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RPullSync, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Touch_M4 ==
  ASSUME IndM4, NEW s \in Writers, Touch(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Touch_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Touch, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Touch, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Honor_M4 ==
  ASSUME IndM4, NEW s \in Writers, Honor(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Honor_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Honor, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Honor, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA HonorReader_M4 ==
  ASSUME IndM4, NEW s \in Writers, HonorReader(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, HonorReader_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF HonorReader, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF HonorReader, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RetireByAck_M4 ==
  ASSUME IndM4, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RetireByAck_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RetireByAck, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RetireByAck, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Depose_M4 ==
  ASSUME IndM4, NEW t \in Writers, Depose(t), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Depose_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Depose, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Depose, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Fence_M4 ==
  ASSUME IndM4, NEW s \in Writers, Fence(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Fence_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Fence, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Fence, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

------------------------------------------------------------------------------
(* M4: the retire age and the reader's load.                                *)

LEMMA Age_M4 ==
  ASSUME IndM4, Age, UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Age_M3
<1>c. NeverReCited BY DEF IndM4, M4
<1>1. aged' = aged \cup retiring /\ retiring' = {} /\ rlag' = TRUE /\ doc' = doc BY DEF Age
<1>2. NeverReCited' BY <1>1, <1>c DEF NeverReCited
<1>3. ReaderLive' BY <1>1 DEF ReaderLive
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

LEMMA Reap_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Reap_M3
<1>1. /\ h \in aged /\ live' = live \ {h} /\ aged' = aged \ {h}
      /\ UNCHANGED <<doc, retiring, rdoc, rlag>>
  BY DEF Reap
<1>2. NeverReCited' BY <1>1 DEF IndM4, M4, NeverReCited
<1>3. ReaderLive'
  <2>. SUFFICES ASSUME ~rlag, NEW p \in Paths, rdoc[p] # Nil
                PROVE  /\ rdoc[p] \in live' /\ rdoc[p] \notin aged'
                       /\ ((\E k \in Paths : doc'[k] = rdoc[p]) \/ rdoc[p] \in retiring')
    BY <1>1 DEF ReaderLive, Cited
  <2>1. rdoc[p] \in live /\ rdoc[p] \notin aged /\ ((\E k \in Paths : doc[k] = rdoc[p]) \/ rdoc[p] \in retiring)
    BY DEF IndM4, M4, ReaderLive, Cited
  <2>. QED BY <1>1, <2>1
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

LEMMA RLoad_M4 ==
  ASSUME IndM4, RLoad, UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RLoad_M3
<1>1. rdoc' = doc /\ rlag' = FALSE /\ UNCHANGED <<live, doc, retiring, aged>> BY DEF RLoad
<1>2. NeverReCited' BY <1>1 DEF IndM4, M4, NeverReCited
<1>3. ReaderLive'
  <2>. SUFFICES ASSUME NEW p \in Paths, doc[p] # Nil
                PROVE  /\ doc[p] \in live /\ doc[p] \notin aged
                       /\ ((\E k \in Paths : doc[k] = doc[p]) \/ doc[p] \in retiring)
    BY <1>1 DEF ReaderLive, Cited
  <2>1. doc[p] \in live BY DEF IndM4, IndM3, IndM2, M2, Inv_CitationsLive
  <2>2. doc[p] \notin aged BY DEF IndM4, M4, NeverReCited
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

------------------------------------------------------------------------------
(* M4: the step, and the theorem.                                           *)

LEMMA Next_M4 == IndM4 /\ Next => IndM4'
<1>. SUFFICES ASSUME IndM4, Next PROVE IndM4' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM4' BY <3>1, GPut_M4
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM4' BY <3>2, GCas_M4
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM4' BY <3>3, GDelete_M4
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM4' BY <3>4, GRename_M4
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M4
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM4' BY <3>1, Checkout_M4
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM4' BY <3>2, Consume_M4
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM4' BY <3>3, Scan_M4
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM4' BY <3>4, Skip_M4
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM4' BY <3>5, PullOnly_M4
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM4' BY <3>6, Claim_M4
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM4' BY <3>7, Verify_M4
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM4' BY <3>8, Install_M4
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM4' BY <3>9, Collect_M4
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM4' BY <3>10, Finish_M4
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM4' BY <3>11, Restart_M4
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM4' BY <3>12, Sync_M4
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM4' BY <3>13, RescopeBegin_M4
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM4' BY <3>14, RescopeFirst_M4
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM4' BY <3>15, RescopeSecond_M4
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM4' BY <3>16, RPullRead_M4
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM4' BY <3>17, RPullSync_M4
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM4' BY <3>18, Edit_M4
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM4' BY <3>19, Delete_M4
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM4' BY <3>20, Upload_M4
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM4' BY <3>21, Sweep_M4
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE IndM4' BY <3>22, Touch_M4
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE IndM4' BY <3>23, Honor_M4
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE IndM4' BY <3>24, HonorReader_M4
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE IndM4' BY <3>25, RetireByAck_M4
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE IndM4' BY <3>26, Depose_M4
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE IndM4' BY <3>27, Fence_M4
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M4
  <2>2. CASE RLoad BY <2>2, RLoad_M4
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM4' BY <2>3, Reap_M4
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M4Invariant == Spec => []IndM4
<1>1. Init => IndM4 BY Init_M4
<1>2. IndM4 /\ [Next]_vars => IndM4'
  <2>1. IndM4 /\ Next => IndM4' BY Next_M4
  <2>2. IndM4 /\ UNCHANGED vars => IndM4'
    <3>1. IndM4 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM4 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM4, IndM3, IndM2, IndM1, vars
    <3>3. IndM4 /\ UNCHANGED vars => M2' BY M2Same DEF IndM4, IndM3, IndM2, vars, aux, ret
    <3>4. IndM4 /\ UNCHANGED vars => M3' BY M3Same DEF IndM4, IndM3, vars, bucket
    <3>5. IndM4 /\ UNCHANGED vars => M4' BY M4Same DEF IndM4, vars, ret
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5 DEF IndM4, IndM3, IndM2, IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

THEOREM ReaderFetches == Spec => []Inv_ReaderFetches
<1>1. IndM4 => Inv_ReaderFetches BY DEF IndM4, M4, ReaderLive, Inv_ReaderFetches
<1>. QED BY M4Invariant, <1>1, PTL

==============================================================================
