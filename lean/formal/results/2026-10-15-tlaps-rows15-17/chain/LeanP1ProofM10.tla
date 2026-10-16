------------------------------ MODULE LeanP1ProofM10 ------------------------------
(* M10, part 1 (row 15): AK1 (S1 rows 1 and 2) from BoundOK.  Generated:
   results/2026-10-15-tlaps-rows15-17/gen-m10.py (its docstring has the
   argument).  Part 2 proves BoundOK an invariant.                         *)
EXTENDS LeanP1ProofM0

\* The honour's guard and its `dropped` (LeanP1.tla `Honor`).
Honorable(s) == w[s].pend.id # 0 /\ w[s].bound.id = w[s].pend.id /\ w[s].bound.fold = w[s].pend.fold
HDropped(s) == w[s].bound.wd \cup (IF w[s].car.id = w[s].pend.id THEN w[s].car.dropped \cup w[s].car.uncited ELSE {})
\* AK1's state form.
BoundOK == \A s \in Writers : Honorable(s) =>
             \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s)
\* AK1's step form (AK1 == [][AK1Step]_vars).
AK1Step == \A s \in Writers :
             AckWrite(s) => {p \in Paths : Broken(s, p, w[s].bound.doc)} \subseteq w'[s].ack.dropped

\* A declared path in carrier form in `c`: `c` published the declared bytes
\* or the declared delete, or names the path dropped (MCLeanP1M7's CPub).
CarForm(s, p, c) ==
  /\ p \in c.paths
  /\ \/ p \in c.dropped
     \/ w[s].decl[p] = Nil /\ p \in c.deletes
     \/ /\ w[s].decl[p] # Nil /\ p \notin c.deletes
        /\ c.pub[p] # Nil /\ Content(c.pub[p]) = Content(w[s].decl[p])

------------------------------------------------------------------------------
(* L-137: Uncited catches every broken path in carrier form.                *)

LEMMA UncitedCatches ==
  ASSUME NEW s \in Writers, NEW p \in Paths, NEW c, NEW d,
         CarForm(s, p, c), Broken(s, p, d)
  PROVE  p \in c.dropped \cup Uncited(c, d)
<1>0. p \in c.paths BY DEF CarForm
<1>1. CASE p \in c.dropped BY <1>1
<1>2. CASE w[s].decl[p] = Nil /\ p \in c.deletes
  \* A declared delete: an absent path carries it, a present one is later.
  <2>1. CASE d[p] = Nil BY <1>2, <2>1 DEF Broken
  <2>2. CASE d[p] # Nil BY <1>2, <2>2 DEF Broken, LaterVersion
  <2>. QED BY <2>1, <2>2
<1>3. CASE /\ w[s].decl[p] # Nil /\ p \notin c.deletes
           /\ c.pub[p] # Nil /\ Content(c.pub[p]) = Content(w[s].decl[p])
  <2>1. Content(d[p]) # Content(c.pub[p]) BY <1>3 DEF Broken
  <2>2. d[p] = Nil \/ (c.pub[p] # Nil /\ Content(d[p]) # Content(c.pub[p])) BY <1>3, <2>1
  <2>. QED BY <1>0, <1>3, <2>2 DEF Uncited
<1>. QED BY <1>1, <1>2, <1>3 DEF CarForm

------------------------------------------------------------------------------
(* No step but the honour writes an `ok`/`partial` ack.                    *)

LEMMA AckKeep ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         R.ack = w[s].ack \/ R.ack.st \notin {"ok", "partial"}
  PROVE  AK1Step
<1>1. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY DEF TypeOK
<1>. QED BY <1>1 DEF AK1Step, AckWrite

LEMMA AckSame == w' = w => AK1Step
BY DEF AK1Step, AckWrite

------------------------------------------------------------------------------
(* The steps that write no tree.                                           *)

LEMMA GPut_AK1 ==
  ASSUME TypeOK, NEW p \in Paths, GPut(p)
  PROVE  AK1Step
<1>1. w' = w BY DEF GPut
<1>. QED BY <1>1, AckSame

LEMMA GCas_AK1 ==
  ASSUME TypeOK, NEW p \in Paths, GCas(p)
  PROVE  AK1Step
<1>1. w' = w BY DEF GCas
<1>. QED BY <1>1, AckSame

LEMMA GRename_AK1 ==
  ASSUME TypeOK, NEW p \in Paths, NEW q \in Paths, GRename(p, q)
  PROVE  AK1Step
<1>1. w' = w BY DEF GRename
<1>. QED BY <1>1, AckSame

LEMMA GRenameFinish_AK1 ==
  ASSUME TypeOK, GRenameFinish
  PROVE  AK1Step
<1>1. w' = w BY DEF GRenameFinish
<1>. QED BY <1>1, AckSame

LEMMA GDelete_AK1 ==
  ASSUME TypeOK, NEW p \in Paths, GDelete(p)
  PROVE  AK1Step
<1>1. w' = w BY DEF GDelete
<1>. QED BY <1>1, AckSame

LEMMA Sweep_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, NEW h \in Handles, Sweep(s, h)
  PROVE  AK1Step
<1>1. w' = w BY DEF Sweep
<1>. QED BY <1>1, AckSame

LEMMA Age_AK1 ==
  ASSUME TypeOK, Age
  PROVE  AK1Step
<1>1. w' = w BY DEF Age
<1>. QED BY <1>1, AckSame

LEMMA RLoad_AK1 ==
  ASSUME TypeOK, RLoad
  PROVE  AK1Step
<1>1. w' = w BY DEF RLoad
<1>. QED BY <1>1, AckSame

LEMMA Reap_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, NEW h \in Handles, Reap(s, h)
  PROVE  AK1Step
<1>1. w' = w BY DEF Reap
<1>. QED BY <1>1, AckSame

------------------------------------------------------------------------------
(* The writer steps that keep the ack.                                    *)

LEMMA Edit_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, NEW p \in Paths, Edit(s, p)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>2. EditW(s, p).ack = w[s].ack BY DEF EditW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Delete_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, NEW p \in Paths, Delete(s, p)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete
<1>2. DeleteW(s, p).ack = w[s].ack BY DEF DeleteW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Checkout_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Checkout(s)
  PROVE  AK1Step
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>2. CheckoutW(s, T).ack = w[s].ack BY DEF CheckoutW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Scan_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Scan(s)
  PROVE  AK1Step
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>2. ScanW(s, dels).ack = w[s].ack BY DEF ScanW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Skip_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Skip(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>2. SkipW(s).ack = w[s].ack BY DEF SkipW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA PullOnly_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, PullOnly(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>2. PullOnlyW(s).ack = w[s].ack BY DEF PullOnlyW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Claim_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Claim(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim
<1>2. ClaimW(s).ack = w[s].ack BY DEF ClaimW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Verify_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Verify(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify
<1>2. VerifyW(s).ack = w[s].ack BY DEF VerifyW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Install_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Install(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] BY DEF Install
<1>2. InstallW(s).ack = w[s].ack BY DEF InstallW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Collect_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Collect(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] BY DEF Collect
<1>2. CollectW(s).ack = w[s].ack BY DEF CollectW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Finish_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Finish(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>2. FinishW(s).ack = w[s].ack BY DEF FinishW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Restart_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Restart(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>2. RestartW(s).ack = w[s].ack BY DEF RestartW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Sync_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Sync(s)
  PROVE  AK1Step
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>2. SyncW(s, fail).ack = w[s].ack BY DEF SyncW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA RescopeBegin_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, RescopeBegin(s)
  PROVE  AK1Step
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>2. RescopeBeginW(s, T).ack = w[s].ack BY DEF RescopeBeginW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA RescopeFirst_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, RescopeFirst(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>2. RescopeFirstW(s).ack = w[s].ack BY DEF RescopeFirstW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA RescopeSecond_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, RescopeSecond(s)
  PROVE  AK1Step
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} : w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] BY DEF RescopeSecond
<1>2. RescopeSecondW(s, wfail).ack = w[s].ack BY DEF RescopeSecondW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA RPullRead_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, RPullRead(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>2. RPullReadW(s).ack = w[s].ack BY DEF RPullReadW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA RPullSync_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, RPullSync(s)
  PROVE  AK1Step
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>2. RPullSyncW(s, fail).ack = w[s].ack BY DEF RPullSyncW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Touch_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Touch(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] BY DEF Touch
<1>2. TouchW(s).ack = w[s].ack BY DEF TouchW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA RetireByAck_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, RetireByAck(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY DEF RetireByAck
<1>2. RetireByAckW(s).ack = w[s].ack BY DEF RetireByAckW
<1>. QED BY <1>1, <1>2, AckKeep

LEMMA Consume_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Consume(s)
  PROVE  AK1Step
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>ck
  <2>2. ConsumeCheapW(s, ck).ack = w[s].ack BY DEF ConsumeCheapW
  <2>. QED BY <2>1, <2>2, AckKeep
<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>ck
  <2>2. ConsumeW(s, fail, ck).ack = w[s].ack BY DEF ConsumeW
  <2>. QED BY <2>0, <2>2, AckKeep
<1>. QED BY <1>1, <1>2

LEMMA Upload_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, NEW p \in Paths, Upload(s, p)
  PROVE  AK1Step
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>1 DEF Upload
  <2>2. UploadW(s, p).ack = w[s].ack BY DEF UploadW
  <2>. QED BY <2>1, <2>2, AckKeep
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] BY <1>2 DEF Upload
  <2>2. UploadCopyW(s, p, c).ack = w[s].ack BY DEF UploadCopyW
  <2>. QED BY <2>1, <2>2, AckKeep
<1>. QED BY <1>1, <1>2

\* Two trees, neither's ack written.
LEMMA Depose_AK1 ==
  ASSUME TypeOK, NEW t \in Writers, Depose(t)
  PROVE  AK1Step
<1>1. /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
  BY DEF Depose
<1>2. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>1 DEF TypeOK
<1>3. DeposeW(t).ack = w[t].ack /\ DeposedW(holder).ack = w[holder].ack BY DEF DeposeW, DeposedW
<1>. QED BY <1>1, <1>2, <1>3 DEF AK1Step, AckWrite

LEMMA Fence_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, Fence(s)
  PROVE  AK1Step
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>2. FenceBeforeW(s).ack = w[s].ack BY DEF FenceBeforeW
  <2>. QED BY <2>1, <2>2, AckKeep
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>2. FenceAfterW(s).ack = w[s].ack BY DEF FenceAfterW
  <2>. QED BY <2>1, <2>2, AckKeep
<1>. QED BY <1>1, <1>2

\* The reader's answer is `refused-read-only`.
LEMMA HonorReader_AK1 ==
  ASSUME TypeOK, NEW s \in Writers, HonorReader(s)
  PROVE  AK1Step
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY DEF HonorReader
<1>2. HonorReaderW(s).ack.st = "refused-read-only" BY DEF HonorReaderW, NoAck
<1>. QED BY <1>1, <1>2, AckKeep

------------------------------------------------------------------------------
(* The honour: its guard is Honorable, its `dropped` HDropped.             *)

LEMMA Honor_AK1 ==
  ASSUME TypeOK, BoundOK, NEW s \in Writers, Honor(s)
  PROVE  AK1Step
<1>h. Honorable(s) BY DEF Honor, Honorable
<1>0. w' = [w EXCEPT ![s] = HonorW(s)] BY DEF Honor
<1>d. HonorW(s).ack.dropped = HDropped(s) BY DEF HonorW, HonorDr, HonorCarried, HDropped
<1>1. /\ \A t \in Writers \ {s} : w'[t] = w[t]
      /\ w'[s].ack.dropped = HDropped(s)
  BY <1>0, <1>d DEF TypeOK
<1>2. \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s) BY <1>h DEF BoundOK
<1>. QED BY <1>1, <1>2 DEF AK1Step, AckWrite

------------------------------------------------------------------------------
(* The step, and the theorem.                                              *)

LEMMA Next_AK1 == TypeOK /\ BoundOK /\ Next => AK1Step
<1>. SUFFICES ASSUME TypeOK, BoundOK, Next PROVE AK1Step OBVIOUS
<1>1. CASE GatewayStep
  <2>1. ASSUME NEW p \in Paths, GPut(p) PROVE AK1Step BY <2>1, GPut_AK1
  <2>2. ASSUME NEW p \in Paths, GCas(p) PROVE AK1Step BY <2>2, GCas_AK1
  <2>3. ASSUME NEW p \in Paths, GDelete(p) PROVE AK1Step BY <2>3, GDelete_AK1
  <2>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE AK1Step BY <2>4, GRename_AK1
  <2>5. CASE GRenameFinish BY <2>5, GRenameFinish_AK1
  <2>. QED BY <1>1, <2>1, <2>2, <2>3, <2>4, <2>5 DEF GatewayStep
<1>2. CASE WriterStep
  <2>1. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE AK1Step BY <2>1, Edit_AK1
  <2>2. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE AK1Step BY <2>2, Delete_AK1
  <2>3. ASSUME NEW s \in Writers, Checkout(s) PROVE AK1Step BY <2>3, Checkout_AK1
  <2>4. ASSUME NEW s \in Writers, Scan(s) PROVE AK1Step BY <2>4, Scan_AK1
  <2>5. ASSUME NEW s \in Writers, Skip(s) PROVE AK1Step BY <2>5, Skip_AK1
  <2>6. ASSUME NEW s \in Writers, PullOnly(s) PROVE AK1Step BY <2>6, PullOnly_AK1
  <2>7. ASSUME NEW s \in Writers, Claim(s) PROVE AK1Step BY <2>7, Claim_AK1
  <2>8. ASSUME NEW s \in Writers, Verify(s) PROVE AK1Step BY <2>8, Verify_AK1
  <2>9. ASSUME NEW s \in Writers, Install(s) PROVE AK1Step BY <2>9, Install_AK1
  <2>10. ASSUME NEW s \in Writers, Collect(s) PROVE AK1Step BY <2>10, Collect_AK1
  <2>11. ASSUME NEW s \in Writers, Finish(s) PROVE AK1Step BY <2>11, Finish_AK1
  <2>12. ASSUME NEW s \in Writers, Restart(s) PROVE AK1Step BY <2>12, Restart_AK1
  <2>13. ASSUME NEW s \in Writers, Sync(s) PROVE AK1Step BY <2>13, Sync_AK1
  <2>14. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE AK1Step BY <2>14, RescopeBegin_AK1
  <2>15. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE AK1Step BY <2>15, RescopeFirst_AK1
  <2>16. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE AK1Step BY <2>16, RescopeSecond_AK1
  <2>17. ASSUME NEW s \in Writers, RPullRead(s) PROVE AK1Step BY <2>17, RPullRead_AK1
  <2>18. ASSUME NEW s \in Writers, RPullSync(s) PROVE AK1Step BY <2>18, RPullSync_AK1
  <2>19. ASSUME NEW s \in Writers, Touch(s) PROVE AK1Step BY <2>19, Touch_AK1
  <2>20. ASSUME NEW s \in Writers, Honor(s) PROVE AK1Step BY <2>20, Honor_AK1
  <2>21. ASSUME NEW s \in Writers, HonorReader(s) PROVE AK1Step BY <2>21, HonorReader_AK1
  <2>22. ASSUME NEW s \in Writers, RetireByAck(s) PROVE AK1Step BY <2>22, RetireByAck_AK1
  <2>23. ASSUME NEW s \in Writers, Consume(s) PROVE AK1Step BY <2>23, Consume_AK1
  <2>24. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE AK1Step BY <2>24, Upload_AK1
  <2>25. ASSUME NEW s \in Writers, Depose(s) PROVE AK1Step BY <2>25, Depose_AK1
  <2>26. ASSUME NEW s \in Writers, Fence(s) PROVE AK1Step BY <2>26, Fence_AK1
  <2>27. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE AK1Step BY <2>27, Sweep_AK1
  <2>. QED BY <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>18, <2>19, <2>20, <2>21, <2>22, <2>23, <2>24, <2>25, <2>26, <2>27 DEF WriterStep
<1>3. CASE Age BY <1>3, Age_AK1
<1>4. CASE RLoad BY <1>4, RLoad_AK1
<1>5. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE AK1Step BY <1>5, Reap_AK1
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Next

\* AK1, given BoundOK an invariant (part 2).
THEOREM AK1FromBoundOK == Spec /\ []BoundOK => AK1
<1>1. Spec => []TypeOK /\ [][Next]_vars BY TypeInvariant, PTL DEF Spec
<1>2. TypeOK /\ BoundOK /\ [Next]_vars => [AK1Step]_vars
  <2>1. TypeOK /\ BoundOK /\ Next => AK1Step BY Next_AK1
  <2>2. UNCHANGED vars => AK1Step BY AckSame DEF vars
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF AK1, AK1Step

==============================================================================
