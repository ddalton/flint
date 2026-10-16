---------------------------- MODULE LeanP1ProofM8 ----------------------------
(* M8 (row 16): NR1 -- a path the tree deleted and has not published comes
   back only by the agent's own write: no step but Edit puts bytes where the
   tree holds none over a baseline it holds (the consume and the syncs take
   only owed paths -- clean ones -- or converge on the document's bytes, and
   a converged delete stays one; the widen fetches only unheld paths; the
   copy upload re-points only a path still holding its snapshot, which a
   pending upload has: M2's Pending; a checkout starts from no baseline: M1's
   Off).  Generated: results/2026-10-15-tlaps-rows15-17/gen-m8m9.py.       *)
EXTENDS LeanP1ProofM2

NR1Act == \A s \in Writers, p \in Paths :
            w[s].local[p] = Nil /\ w[s].baseline[p] # Nil /\ w'[s].local[p] # Nil
              => w'[s].local[p] = <<p, nextGen>> /\ nextGen' = nextGen + 1

\* A step writing one tree R: NR1 from R's bytes alone.
LEMMA NR1ActWrite ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ R.local[q] # Nil)
                            => R.local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  PROVE  NR1Act
<1>1. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY WriteAny
<1>. QED BY <1>1 DEF NR1Act

LEMMA NR1ActSame == w' = w => NR1Act
BY DEF NR1Act

------------------------------------------------------------------------------
(* The steps that write no tree.                                            *)

LEMMA GPut_NR1Act ==
  ASSUME IndM2, NEW p \in Paths, GPut(p)
  PROVE  NR1Act
<1>1. w' = w BY DEF GPut
<1>. QED BY <1>1, NR1ActSame

LEMMA GCas_NR1Act ==
  ASSUME IndM2, NEW p \in Paths, GCas(p)
  PROVE  NR1Act
<1>1. w' = w BY DEF GCas
<1>. QED BY <1>1, NR1ActSame

LEMMA GRename_NR1Act ==
  ASSUME IndM2, NEW p \in Paths, NEW q \in Paths, GRename(p, q)
  PROVE  NR1Act
<1>1. w' = w BY DEF GRename
<1>. QED BY <1>1, NR1ActSame

LEMMA GRenameFinish_NR1Act ==
  ASSUME IndM2, GRenameFinish
  PROVE  NR1Act
<1>1. w' = w BY DEF GRenameFinish
<1>. QED BY <1>1, NR1ActSame

LEMMA GDelete_NR1Act ==
  ASSUME IndM2, NEW p \in Paths, GDelete(p)
  PROVE  NR1Act
<1>1. w' = w BY DEF GDelete
<1>. QED BY <1>1, NR1ActSame

LEMMA Sweep_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, NEW h \in Handles, Sweep(s, h)
  PROVE  NR1Act
<1>1. w' = w BY DEF Sweep
<1>. QED BY <1>1, NR1ActSame

LEMMA Age_NR1Act ==
  ASSUME IndM2, Age
  PROVE  NR1Act
<1>1. w' = w BY DEF Age
<1>. QED BY <1>1, NR1ActSame

LEMMA RLoad_NR1Act ==
  ASSUME IndM2, RLoad
  PROVE  NR1Act
<1>1. w' = w BY DEF RLoad
<1>. QED BY <1>1, NR1ActSame

LEMMA Reap_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, NEW h \in Handles, Reap(s, h)
  PROVE  NR1Act
<1>1. w' = w BY DEF Reap
<1>. QED BY <1>1, NR1ActSame

------------------------------------------------------------------------------
(* The writer steps: one tree.                                              *)

LEMMA Edit_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Edit(s, p)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ EditW(s, p).local[q] # Nil)
                       => EditW(s, p).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY <1>f DEF EditW, Edit
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Delete_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Delete(s, p)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ DeleteW(s, p).local[q] # Nil)
                       => DeleteW(s, p).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY <1>f DEF DeleteW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Checkout_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Checkout(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ CheckoutW(s, T).local[q] # Nil)
                       => CheckoutW(s, T).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY <1>0 DEF Checkout, Off, WriterInit, CheckoutW, CheckoutHeld
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Scan_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Scan(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ ScanW(s, dels).local[q] # Nil)
                       => ScanW(s, dels).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF ScanW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Skip_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Skip(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ SkipW(s).local[q] # Nil)
                       => SkipW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF SkipW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA PullOnly_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, PullOnly(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ PullOnlyW(s).local[q] # Nil)
                       => PullOnlyW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF PullOnlyW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Claim_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Claim(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ ClaimW(s).local[q] # Nil)
                       => ClaimW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF ClaimW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Verify_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Verify(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ VerifyW(s).local[q] # Nil)
                       => VerifyW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF VerifyW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Install_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Install(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] BY DEF Install
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ InstallW(s).local[q] # Nil)
                       => InstallW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF InstallW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Collect_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Collect(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] BY DEF Collect
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ CollectW(s).local[q] # Nil)
                       => CollectW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF CollectW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Finish_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Finish(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ FinishW(s).local[q] # Nil)
                       => FinishW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF FinishW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Restart_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Restart(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ RestartW(s).local[q] # Nil)
                       => RestartW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF RestartW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Sync_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Sync(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ SyncW(s, fail).local[q] # Nil)
                       => SyncW(s, fail).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY <1>f DEF SyncW, SyncOwed, SyncAll, Owed
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA RescopeBegin_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, RescopeBegin(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ RescopeBeginW(s, T).local[q] # Nil)
                       => RescopeBeginW(s, T).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF RescopeBeginW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA RescopeFirst_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, RescopeFirst(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ RescopeFirstW(s).local[q] # Nil)
                       => RescopeFirstW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY ShippedShape DEF RescopeFirstW, Shipped
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA RescopeSecond_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, RescopeSecond(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} : w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] BY DEF RescopeSecond
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ RescopeSecondW(s, wfail).local[q] # Nil)
                       => RescopeSecondW(s, wfail).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY <1>f, ShippedShape DEF RescopeSecondW, RescopeFetch, RescopeFetch0, RescopeAdd, RescopeBase1, RescopeLocal1, Shipped
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA RPullRead_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, RPullRead(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ RPullReadW(s).local[q] # Nil)
                       => RPullReadW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF RPullReadW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA RPullSync_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, RPullSync(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ RPullSyncW(s, fail).local[q] # Nil)
                       => RPullSyncW(s, fail).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY <1>f DEF RPullSyncW, SyncOwed, SyncAll, Owed
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Touch_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Touch(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] BY DEF Touch
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ TouchW(s).local[q] # Nil)
                       => TouchW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF TouchW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Honor_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Honor(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] BY DEF Honor
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ HonorW(s).local[q] # Nil)
                       => HonorW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF HonorW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA HonorReader_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, HonorReader(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY DEF HonorReader
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ HonorReaderW(s).local[q] # Nil)
                       => HonorReaderW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF HonorReaderW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA RetireByAck_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, RetireByAck(s)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ Off /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY DEF RetireByAck
<1>2. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ RetireByAckW(s).local[q] # Nil)
                       => RetireByAckW(s).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  BY DEF RetireByAckW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

LEMMA Consume_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Consume(s)
  PROVE  NR1Act
<1>0. TypeOK /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>ck
  <2>2. ConsumeCheapW(s, ck).local = w[s].local BY DEF ConsumeCheapW
  <2>. QED BY <1>0, <2>1, <2>2, NR1ActWrite
<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>ck
  \* A taken path was owed (clean: local = baseline) or converged (its bytes
  \* are the document's, or both are a delete).
  <2>1. \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ ConsumeW(s, fail, ck).local[q] # Nil)
                            => ConsumeW(s, fail, ck).local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
    BY <1>f DEF ConsumeW, ConsumeTaken, ConsumeOwed, ConsumeConv, Owed, Converges
  <2>. QED BY <1>0, <2>0, <2>1, NR1ActWrite
<1>. QED BY <1>1, <1>2

LEMMA Upload_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Upload(s, p)
  PROVE  NR1Act
<1>0. TypeOK /\ Pending /\ w[s] \in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, TypeOK
<1>f. w[s].local \in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>g. w[s].pc = "scanned" /\ p \in w[s].uploads \ w[s].upDone BY DEF Upload
<1>h. w[s].snap[p] # Nil BY <1>0, <1>g DEF Pending
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>1 DEF Upload
  <2>2. UploadW(s, p).local = w[s].local BY DEF UploadW
  <2>. QED BY <1>0, <2>1, <2>2, NR1ActWrite
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] BY <1>2 DEF Upload
  \* The copy re-points p only where the tree still holds the snapshot (not Nil).
  <2>2. \A q \in Paths : UploadCopyW(s, p, c).local[q] = w[s].local[q] \/ w[s].local[q] # Nil
    BY <1>f, <1>h DEF UploadCopyW
  <2>. QED BY <1>0, <2>1, <2>2, NR1ActWrite
<1>. QED BY <1>1, <1>2

LEMMA Depose_NR1Act ==
  ASSUME IndM2, NEW t \in Writers, Depose(t)
  PROVE  NR1Act
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>1. holder \in Writers /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)] BY DEF Depose
<1>2. \A u \in Writers : w'[u].local = w[u].local BY <1>0, <1>1 DEF TypeOK, DeposeW, DeposedW
<1>. QED BY <1>2 DEF NR1Act

LEMMA Fence_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Fence(s)
  PROVE  NR1Act
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>1. \/ w' = [w EXCEPT ![s] = FenceBeforeW(s)]
      \/ w' = [w EXCEPT ![s] = FenceAfterW(s)]
  BY DEF Fence
<1>2. FenceBeforeW(s).local = w[s].local /\ FenceAfterW(s).local = w[s].local BY DEF FenceBeforeW, FenceAfterW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

------------------------------------------------------------------------------
(* The step, and the theorems.                                              *)

LEMMA Next_NR1Act == IndM2 /\ Next => NR1Act
<1>. SUFFICES ASSUME IndM2, Next PROVE NR1Act OBVIOUS
<1>1. CASE GatewayStep
  <2>1. ASSUME NEW p \in Paths, GPut(p) PROVE NR1Act BY <2>1, GPut_NR1Act
  <2>2. ASSUME NEW p \in Paths, GCas(p) PROVE NR1Act BY <2>2, GCas_NR1Act
  <2>3. ASSUME NEW p \in Paths, GDelete(p) PROVE NR1Act BY <2>3, GDelete_NR1Act
  <2>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE NR1Act BY <2>4, GRename_NR1Act
  <2>5. CASE GRenameFinish BY <2>5, GRenameFinish_NR1Act
  <2>. QED BY <1>1, <2>1, <2>2, <2>3, <2>4, <2>5 DEF GatewayStep
<1>2. CASE WriterStep
  <2>1. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE NR1Act BY <2>1, Edit_NR1Act
  <2>2. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE NR1Act BY <2>2, Delete_NR1Act
  <2>3. ASSUME NEW s \in Writers, Checkout(s) PROVE NR1Act BY <2>3, Checkout_NR1Act
  <2>4. ASSUME NEW s \in Writers, Scan(s) PROVE NR1Act BY <2>4, Scan_NR1Act
  <2>5. ASSUME NEW s \in Writers, Skip(s) PROVE NR1Act BY <2>5, Skip_NR1Act
  <2>6. ASSUME NEW s \in Writers, PullOnly(s) PROVE NR1Act BY <2>6, PullOnly_NR1Act
  <2>7. ASSUME NEW s \in Writers, Claim(s) PROVE NR1Act BY <2>7, Claim_NR1Act
  <2>8. ASSUME NEW s \in Writers, Verify(s) PROVE NR1Act BY <2>8, Verify_NR1Act
  <2>9. ASSUME NEW s \in Writers, Install(s) PROVE NR1Act BY <2>9, Install_NR1Act
  <2>10. ASSUME NEW s \in Writers, Collect(s) PROVE NR1Act BY <2>10, Collect_NR1Act
  <2>11. ASSUME NEW s \in Writers, Finish(s) PROVE NR1Act BY <2>11, Finish_NR1Act
  <2>12. ASSUME NEW s \in Writers, Restart(s) PROVE NR1Act BY <2>12, Restart_NR1Act
  <2>13. ASSUME NEW s \in Writers, Sync(s) PROVE NR1Act BY <2>13, Sync_NR1Act
  <2>14. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE NR1Act BY <2>14, RescopeBegin_NR1Act
  <2>15. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE NR1Act BY <2>15, RescopeFirst_NR1Act
  <2>16. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE NR1Act BY <2>16, RescopeSecond_NR1Act
  <2>17. ASSUME NEW s \in Writers, RPullRead(s) PROVE NR1Act BY <2>17, RPullRead_NR1Act
  <2>18. ASSUME NEW s \in Writers, RPullSync(s) PROVE NR1Act BY <2>18, RPullSync_NR1Act
  <2>19. ASSUME NEW s \in Writers, Touch(s) PROVE NR1Act BY <2>19, Touch_NR1Act
  <2>20. ASSUME NEW s \in Writers, Honor(s) PROVE NR1Act BY <2>20, Honor_NR1Act
  <2>21. ASSUME NEW s \in Writers, HonorReader(s) PROVE NR1Act BY <2>21, HonorReader_NR1Act
  <2>22. ASSUME NEW s \in Writers, RetireByAck(s) PROVE NR1Act BY <2>22, RetireByAck_NR1Act
  <2>23. ASSUME NEW s \in Writers, Consume(s) PROVE NR1Act BY <2>23, Consume_NR1Act
  <2>24. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE NR1Act BY <2>24, Upload_NR1Act
  <2>25. ASSUME NEW s \in Writers, Depose(s) PROVE NR1Act BY <2>25, Depose_NR1Act
  <2>26. ASSUME NEW s \in Writers, Fence(s) PROVE NR1Act BY <2>26, Fence_NR1Act
  <2>27. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE NR1Act BY <2>27, Sweep_NR1Act
  <2>. QED BY <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>18, <2>19, <2>20, <2>21, <2>22, <2>23, <2>24, <2>25, <2>26, <2>27 DEF WriterStep
<1>3. CASE Age BY <1>3, Age_NR1Act
<1>4. CASE RLoad BY <1>4, RLoad_NR1Act
<1>5. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE NR1Act BY <1>5, Reap_NR1Act
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Next

THEOREM DeletedStaysDeleted == Spec => NR1
<1>1. IndM2 /\ [Next]_vars => [NR1Act]_vars
  <2>. SUFFICES ASSUME IndM2, [Next]_vars PROVE [NR1Act]_vars OBVIOUS
  <2>1. CASE Next BY <2>1, Next_NR1Act
  <2>2. CASE UNCHANGED vars BY <2>2 DEF vars
  <2>. QED BY <2>1, <2>2
<1>. QED BY M2Invariant, <1>1, PTL DEF Spec, NR1, NR1Act

==============================================================================
