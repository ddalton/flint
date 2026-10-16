---------------------------- MODULE LeanP1ProofM9 ----------------------------
(* M9 (row 17): DP1 and DP2 -- an install is the lease holder's, and so is
   a collect that moves `live`.  The only step from claimed to cased is
   Install, fenced (InstallFenced: the CAS reads the lease); the only steps
   that set `collected` are Install and the fence after the CAS (neither
   moves `live`) and Collect (DeletesUnderLease; under the retire age it
   moves nothing either).  Inv_OneHolder itself, restated for deposal, is
   M1's.  Generated: results/2026-10-15-tlaps-rows15-17/gen-m8m9.py.      *)
EXTENDS LeanP1ProofM2

DPAct ==
  /\ \A s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased" => holder = s
  /\ \A s \in Writers : ~w[s].collected /\ w'[s].collected /\ live' # live => holder = s

LEMMA DPActWrite ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         /\ (R.pc = "cased" /\ w[s].pc = "claimed") => holder = s
         /\ (~w[s].collected /\ R.collected /\ live' # live) => holder = s
  PROVE  DPAct
<1>1. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY WriteAny
<1>. QED BY <1>1 DEF DPAct

LEMMA DPActSame == w' = w => DPAct
BY DEF DPAct

------------------------------------------------------------------------------
(* The steps that write no tree.                                            *)

LEMMA GPut_DPAct ==
  ASSUME IndM2, NEW p \in Paths, GPut(p)
  PROVE  DPAct
<1>1. w' = w BY DEF GPut
<1>. QED BY <1>1, DPActSame

LEMMA GCas_DPAct ==
  ASSUME IndM2, NEW p \in Paths, GCas(p)
  PROVE  DPAct
<1>1. w' = w BY DEF GCas
<1>. QED BY <1>1, DPActSame

LEMMA GRename_DPAct ==
  ASSUME IndM2, NEW p \in Paths, NEW q \in Paths, GRename(p, q)
  PROVE  DPAct
<1>1. w' = w BY DEF GRename
<1>. QED BY <1>1, DPActSame

LEMMA GRenameFinish_DPAct ==
  ASSUME IndM2, GRenameFinish
  PROVE  DPAct
<1>1. w' = w BY DEF GRenameFinish
<1>. QED BY <1>1, DPActSame

LEMMA GDelete_DPAct ==
  ASSUME IndM2, NEW p \in Paths, GDelete(p)
  PROVE  DPAct
<1>1. w' = w BY DEF GDelete
<1>. QED BY <1>1, DPActSame

LEMMA Sweep_DPAct ==
  ASSUME IndM2, NEW s \in Writers, NEW h \in Handles, Sweep(s, h)
  PROVE  DPAct
<1>1. w' = w BY DEF Sweep
<1>. QED BY <1>1, DPActSame

LEMMA Age_DPAct ==
  ASSUME IndM2, Age
  PROVE  DPAct
<1>1. w' = w BY DEF Age
<1>. QED BY <1>1, DPActSame

LEMMA RLoad_DPAct ==
  ASSUME IndM2, RLoad
  PROVE  DPAct
<1>1. w' = w BY DEF RLoad
<1>. QED BY <1>1, DPActSame

LEMMA Reap_DPAct ==
  ASSUME IndM2, NEW s \in Writers, NEW h \in Handles, Reap(s, h)
  PROVE  DPAct
<1>1. w' = w BY DEF Reap
<1>. QED BY <1>1, DPActSame

------------------------------------------------------------------------------
(* The writer steps: one tree.                                              *)

LEMMA Edit_DPAct ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Edit(s, p)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>2. /\ (EditW(s, p).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ EditW(s, p).collected /\ live' # live) => holder = s
  BY DEF Edit, EditW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Delete_DPAct ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Delete(s, p)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete
<1>2. /\ (DeleteW(s, p).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ DeleteW(s, p).collected /\ live' # live) => holder = s
  BY DEF Delete, DeleteW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Checkout_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Checkout(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>2. /\ (CheckoutW(s, T).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ CheckoutW(s, T).collected /\ live' # live) => holder = s
  BY DEF Checkout, CheckoutW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Scan_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Scan(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>2. /\ (ScanW(s, dels).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ ScanW(s, dels).collected /\ live' # live) => holder = s
  BY DEF Scan, ScanW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Skip_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Skip(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>2. /\ (SkipW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ SkipW(s).collected /\ live' # live) => holder = s
  BY DEF Skip, SkipW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA PullOnly_DPAct ==
  ASSUME IndM2, NEW s \in Writers, PullOnly(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>2. /\ (PullOnlyW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ PullOnlyW(s).collected /\ live' # live) => holder = s
  BY DEF PullOnly, PullOnlyW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Claim_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Claim(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim
<1>2. /\ (ClaimW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ ClaimW(s).collected /\ live' # live) => holder = s
  BY DEF Claim, ClaimW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Verify_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Verify(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify
<1>2. /\ (VerifyW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ VerifyW(s).collected /\ live' # live) => holder = s
  BY DEF Verify, VerifyW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Install_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Install(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] BY DEF Install
<1>2. /\ (InstallW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ InstallW(s).collected /\ live' # live) => holder = s
  BY ShippedShape DEF Install, InstallW, Shipped
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Collect_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Collect(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] BY DEF Collect
<1>2. /\ (CollectW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ CollectW(s).collected /\ live' # live) => holder = s
  BY ShippedShape DEF Collect, CollectW, Shipped
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Finish_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Finish(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>2. /\ (FinishW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ FinishW(s).collected /\ live' # live) => holder = s
  BY DEF Finish, FinishW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Restart_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Restart(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>2. /\ (RestartW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ RestartW(s).collected /\ live' # live) => holder = s
  BY DEF Restart, RestartW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Sync_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Sync(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>2. /\ (SyncW(s, fail).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ SyncW(s, fail).collected /\ live' # live) => holder = s
  BY DEF Sync, SyncW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA RescopeBegin_DPAct ==
  ASSUME IndM2, NEW s \in Writers, RescopeBegin(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>2. /\ (RescopeBeginW(s, T).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ RescopeBeginW(s, T).collected /\ live' # live) => holder = s
  BY DEF RescopeBegin, RescopeBeginW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA RescopeFirst_DPAct ==
  ASSUME IndM2, NEW s \in Writers, RescopeFirst(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>2. /\ (RescopeFirstW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ RescopeFirstW(s).collected /\ live' # live) => holder = s
  BY DEF RescopeFirst, RescopeFirstW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA RescopeSecond_DPAct ==
  ASSUME IndM2, NEW s \in Writers, RescopeSecond(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} : w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] BY DEF RescopeSecond
<1>2. /\ (RescopeSecondW(s, wfail).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ RescopeSecondW(s, wfail).collected /\ live' # live) => holder = s
  BY DEF RescopeSecond, RescopeSecondW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA RPullRead_DPAct ==
  ASSUME IndM2, NEW s \in Writers, RPullRead(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>2. /\ (RPullReadW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ RPullReadW(s).collected /\ live' # live) => holder = s
  BY DEF RPullRead, RPullReadW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA RPullSync_DPAct ==
  ASSUME IndM2, NEW s \in Writers, RPullSync(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>2. /\ (RPullSyncW(s, fail).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ RPullSyncW(s, fail).collected /\ live' # live) => holder = s
  BY DEF RPullSync, RPullSyncW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Touch_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Touch(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] BY DEF Touch
<1>2. /\ (TouchW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ TouchW(s).collected /\ live' # live) => holder = s
  BY DEF Touch, TouchW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Honor_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Honor(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] BY DEF Honor
<1>2. /\ (HonorW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ HonorW(s).collected /\ live' # live) => holder = s
  BY DEF Honor, HonorW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA HonorReader_DPAct ==
  ASSUME IndM2, NEW s \in Writers, HonorReader(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY DEF HonorReader
<1>2. /\ (HonorReaderW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ HonorReaderW(s).collected /\ live' # live) => holder = s
  BY DEF HonorReader, HonorReaderW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA RetireByAck_DPAct ==
  ASSUME IndM2, NEW s \in Writers, RetireByAck(s)
  PROVE  DPAct
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY DEF RetireByAck
<1>2. /\ (RetireByAckW(s).pc = "cased" /\ w[s].pc = "claimed") => holder = s
      /\ (~w[s].collected /\ RetireByAckW(s).collected /\ live' # live) => holder = s
  BY DEF RetireByAck, RetireByAckW, bucket, aux
<1>. QED BY <1>0, <1>1, <1>2, DPActWrite

LEMMA Consume_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Consume(s)
  PROVE  DPAct
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>ck
  <2>2. ConsumeCheapW(s, ck).pc = "consumed" /\ ConsumeCheapW(s, ck).collected = w[s].collected BY DEF ConsumeCheapW
  <2>. QED BY <1>0, <2>1, <2>2, DPActWrite
<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>ck
  <2>1. ConsumeW(s, fail, ck).pc = "consumed" /\ ConsumeW(s, fail, ck).collected = w[s].collected BY DEF ConsumeW
  <2>. QED BY <1>0, <2>0, <2>1, DPActWrite
<1>. QED BY <1>1, <1>2

LEMMA Upload_DPAct ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Upload(s, p)
  PROVE  DPAct
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>1 DEF Upload
  <2>2. UploadW(s, p).pc = w[s].pc /\ UploadW(s, p).collected = w[s].collected BY DEF UploadW
  <2>. QED BY <1>0, <2>1, <2>2, DPActWrite
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] BY <1>2 DEF Upload
  <2>2. UploadCopyW(s, p, c).pc = w[s].pc /\ UploadCopyW(s, p, c).collected = w[s].collected BY DEF UploadCopyW
  <2>. QED BY <1>0, <2>1, <2>2, DPActWrite
<1>. QED BY <1>1, <1>2

\* The taker goes scanned -> claimed; the deposed keeps its pc and bit.
LEMMA Depose_DPAct ==
  ASSUME IndM2, NEW t \in Writers, Depose(t)
  PROVE  DPAct
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>1. holder \in Writers /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)] BY DEF Depose
<1>2. \A u \in Writers : /\ w'[u].pc \in {w[u].pc, "claimed"}
                         /\ w'[u].collected = w[u].collected
  BY <1>0, <1>1 DEF TypeOK, DeposeW, DeposedW
<1>. QED BY <1>2 DEF DPAct

\* Before the CAS: back to idle; after it, `collected` set with `live` kept.
LEMMA Fence_DPAct ==
  ASSUME IndM2, NEW s \in Writers, Fence(s)
  PROVE  DPAct
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>u. live' = live BY DEF Fence, bucket
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>2. FenceBeforeW(s).pc = "idle" BY DEF FenceBeforeW
  <2>. QED BY <1>0, <1>u, <2>1, <2>2, DPActWrite
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>2. FenceAfterW(s).pc = w[s].pc BY DEF FenceAfterW
  <2>. QED BY <1>0, <1>u, <1>2, <2>1, <2>2, DPActWrite
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The step, and the theorems.                                              *)

LEMMA Next_DPAct == IndM2 /\ Next => DPAct
<1>. SUFFICES ASSUME IndM2, Next PROVE DPAct OBVIOUS
<1>1. CASE GatewayStep
  <2>1. ASSUME NEW p \in Paths, GPut(p) PROVE DPAct BY <2>1, GPut_DPAct
  <2>2. ASSUME NEW p \in Paths, GCas(p) PROVE DPAct BY <2>2, GCas_DPAct
  <2>3. ASSUME NEW p \in Paths, GDelete(p) PROVE DPAct BY <2>3, GDelete_DPAct
  <2>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE DPAct BY <2>4, GRename_DPAct
  <2>5. CASE GRenameFinish BY <2>5, GRenameFinish_DPAct
  <2>. QED BY <1>1, <2>1, <2>2, <2>3, <2>4, <2>5 DEF GatewayStep
<1>2. CASE WriterStep
  <2>1. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE DPAct BY <2>1, Edit_DPAct
  <2>2. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE DPAct BY <2>2, Delete_DPAct
  <2>3. ASSUME NEW s \in Writers, Checkout(s) PROVE DPAct BY <2>3, Checkout_DPAct
  <2>4. ASSUME NEW s \in Writers, Scan(s) PROVE DPAct BY <2>4, Scan_DPAct
  <2>5. ASSUME NEW s \in Writers, Skip(s) PROVE DPAct BY <2>5, Skip_DPAct
  <2>6. ASSUME NEW s \in Writers, PullOnly(s) PROVE DPAct BY <2>6, PullOnly_DPAct
  <2>7. ASSUME NEW s \in Writers, Claim(s) PROVE DPAct BY <2>7, Claim_DPAct
  <2>8. ASSUME NEW s \in Writers, Verify(s) PROVE DPAct BY <2>8, Verify_DPAct
  <2>9. ASSUME NEW s \in Writers, Install(s) PROVE DPAct BY <2>9, Install_DPAct
  <2>10. ASSUME NEW s \in Writers, Collect(s) PROVE DPAct BY <2>10, Collect_DPAct
  <2>11. ASSUME NEW s \in Writers, Finish(s) PROVE DPAct BY <2>11, Finish_DPAct
  <2>12. ASSUME NEW s \in Writers, Restart(s) PROVE DPAct BY <2>12, Restart_DPAct
  <2>13. ASSUME NEW s \in Writers, Sync(s) PROVE DPAct BY <2>13, Sync_DPAct
  <2>14. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE DPAct BY <2>14, RescopeBegin_DPAct
  <2>15. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE DPAct BY <2>15, RescopeFirst_DPAct
  <2>16. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE DPAct BY <2>16, RescopeSecond_DPAct
  <2>17. ASSUME NEW s \in Writers, RPullRead(s) PROVE DPAct BY <2>17, RPullRead_DPAct
  <2>18. ASSUME NEW s \in Writers, RPullSync(s) PROVE DPAct BY <2>18, RPullSync_DPAct
  <2>19. ASSUME NEW s \in Writers, Touch(s) PROVE DPAct BY <2>19, Touch_DPAct
  <2>20. ASSUME NEW s \in Writers, Honor(s) PROVE DPAct BY <2>20, Honor_DPAct
  <2>21. ASSUME NEW s \in Writers, HonorReader(s) PROVE DPAct BY <2>21, HonorReader_DPAct
  <2>22. ASSUME NEW s \in Writers, RetireByAck(s) PROVE DPAct BY <2>22, RetireByAck_DPAct
  <2>23. ASSUME NEW s \in Writers, Consume(s) PROVE DPAct BY <2>23, Consume_DPAct
  <2>24. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE DPAct BY <2>24, Upload_DPAct
  <2>25. ASSUME NEW s \in Writers, Depose(s) PROVE DPAct BY <2>25, Depose_DPAct
  <2>26. ASSUME NEW s \in Writers, Fence(s) PROVE DPAct BY <2>26, Fence_DPAct
  <2>27. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE DPAct BY <2>27, Sweep_DPAct
  <2>. QED BY <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>18, <2>19, <2>20, <2>21, <2>22, <2>23, <2>24, <2>25, <2>26, <2>27 DEF WriterStep
<1>3. CASE Age BY <1>3, Age_DPAct
<1>4. CASE RLoad BY <1>4, RLoad_DPAct
<1>5. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE DPAct BY <1>5, Reap_DPAct
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Next

THEOREM InstallUnderLease == Spec => DP1
<1>1. IndM2 /\ [Next]_vars => [\A s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased" => holder = s]_vars
  <2>. SUFFICES ASSUME IndM2, [Next]_vars
                PROVE  [\A s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased" => holder = s]_vars OBVIOUS
  <2>1. CASE Next BY <2>1, Next_DPAct DEF DPAct
  <2>2. CASE UNCHANGED vars BY <2>2 DEF vars
  <2>. QED BY <2>1, <2>2
<1>. QED BY M2Invariant, <1>1, PTL DEF Spec, DP1

THEOREM CollectUnderLease == Spec => DP2
<1>1. IndM2 /\ [Next]_vars => [\A s \in Writers : ~w[s].collected /\ w'[s].collected /\ live' # live => holder = s]_vars
  <2>. SUFFICES ASSUME IndM2, [Next]_vars
                PROVE  [\A s \in Writers : ~w[s].collected /\ w'[s].collected /\ live' # live => holder = s]_vars OBVIOUS
  <2>1. CASE Next BY <2>1, Next_DPAct DEF DPAct
  <2>2. CASE UNCHANGED vars BY <2>2 DEF vars
  <2>. QED BY <2>1, <2>2
<1>. QED BY M2Invariant, <1>1, PTL DEF Spec, DP2

==============================================================================
