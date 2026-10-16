------------------------------ MODULE LeanP1ProofM7 ------------------------------
(* M7, part 1 (row 15): AK3 and AK4 (S1 rows 4 and 5).  Generated:
   results/2026-10-15-tlaps-rows15-17/gen-m7.py.  A per-tree predicate AKW
   holds of every tree; each step writes at most two trees and AKW of each
   follows from AKW of what it read (the touch keeps every nonce it does not
   cap; the honour answers the record's nonces with the bound's seq, clock
   and install bit; the retire waits for an ack that names them; the install
   stamps the seq it installed; the finish moves that into the bound).    *)
EXTENDS LeanP1ProofM0

IdleSet == {"idle", "consumed", "scanned", "claimed", "pulling"}
AKW(W) ==
  /\ W.cN \ W.capped \subseteq W.pend.nonces \cup W.aN
  /\ W.pend.id = 0 => W.pend.nonces = {}
  /\ W.ack.nonces \subseteq W.aN
  /\ (W.ack.st \in {"ok", "partial"} /\ W.ack.inst) => <<W.ack.seq, W.ack.src>> \in W.stamps
  /\ W.pc \in IdleSet => ~W.instd
  /\ W.instd => <<W.iseq, W.clk>> \in W.stamps
  /\ W.bound.inst => <<W.bound.seq, W.bound.clk>> \in W.stamps
AKInv == \A s \in Writers : AKW(w[s])
IndAK == IndTypeOK /\ AKInv

LEMMA AKWrite ==
  ASSUME AKInv, TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R], AKW(R)
  PROVE  AKInv'
<1>1. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY DEF TypeOK
<1>. QED BY <1>1 DEF AKInv

LEMMA AKSame == AKInv /\ w' = w => AKInv'
BY DEF AKInv

LEMMA Init_AK == Init => IndAK
<1>. SUFFICES ASSUME Init PROVE IndAK OBVIOUS
<1>1. IndTypeOK BY Init_TypeOK
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>3. AKW(WriterInit) BY DEF AKW, WriterInit, NoPend, NoAck, NoCar, IdleSet
<1>. QED BY <1>1, <1>2, <1>3 DEF IndAK, AKInv

------------------------------------------------------------------------------
(* The steps that write no tree.                                           *)

LEMMA GPut_AK ==
  ASSUME IndAK, NEW p \in Paths, GPut(p)
  PROVE  AKInv'
<1>1. w' = w BY DEF GPut
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA GCas_AK ==
  ASSUME IndAK, NEW p \in Paths, GCas(p)
  PROVE  AKInv'
<1>1. w' = w BY DEF GCas
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA GRename_AK ==
  ASSUME IndAK, NEW p \in Paths, NEW q \in Paths, GRename(p, q)
  PROVE  AKInv'
<1>1. w' = w BY DEF GRename
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA GRenameFinish_AK ==
  ASSUME IndAK, GRenameFinish
  PROVE  AKInv'
<1>1. w' = w BY DEF GRenameFinish
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA GDelete_AK ==
  ASSUME IndAK, NEW p \in Paths, GDelete(p)
  PROVE  AKInv'
<1>1. w' = w BY DEF GDelete
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA Sweep_AK ==
  ASSUME IndAK, NEW s \in Writers, NEW h \in Handles, Sweep(s, h)
  PROVE  AKInv'
<1>1. w' = w BY DEF Sweep
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA Age_AK ==
  ASSUME IndAK, Age
  PROVE  AKInv'
<1>1. w' = w BY DEF Age
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA RLoad_AK ==
  ASSUME IndAK, RLoad
  PROVE  AKInv'
<1>1. w' = w BY DEF RLoad
<1>. QED BY <1>1, AKSame DEF IndAK

LEMMA Reap_AK ==
  ASSUME IndAK, NEW s \in Writers, NEW h \in Handles, Reap(s, h)
  PROVE  AKInv'
<1>1. w' = w BY DEF Reap
<1>. QED BY <1>1, AKSame DEF IndAK

------------------------------------------------------------------------------
(* The writer steps.                                                       *)

LEMMA Edit_AK ==
  ASSUME IndAK, NEW s \in Writers, NEW p \in Paths, Edit(s, p)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>2. AKW(EditW(s, p))
  BY <1>w DEF AKW, EditW, Edit
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Delete_AK ==
  ASSUME IndAK, NEW s \in Writers, NEW p \in Paths, Delete(s, p)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete
<1>2. AKW(DeleteW(s, p))
  BY <1>w DEF AKW, DeleteW, Delete
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Checkout_AK ==
  ASSUME IndAK, NEW s \in Writers, Checkout(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>2. AKW(CheckoutW(s, T))
  BY <1>w DEF AKW, CheckoutW, Checkout
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Scan_AK ==
  ASSUME IndAK, NEW s \in Writers, Scan(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>2. AKW(ScanW(s, dels))
  <2> DEFINE R == ScanW(s, dels)
  <2>1. R.cN \ R.capped \subseteq R.pend.nonces \cup R.aN BY <1>w DEF AKW, ScanW, Scan, IdleSet
  <2>2. R.pend.id = 0 => R.pend.nonces = {} BY <1>w DEF AKW, ScanW, Scan, IdleSet
  <2>3. R.ack.nonces \subseteq R.aN BY <1>w DEF AKW, ScanW, Scan, IdleSet
  <2>4. (R.ack.st \in {"ok", "partial"} /\ R.ack.inst) => <<R.ack.seq, R.ack.src>> \in R.stamps BY <1>w DEF AKW, ScanW, Scan, IdleSet
  <2>5. R.pc \in IdleSet => ~R.instd BY <1>w DEF AKW, ScanW, Scan, IdleSet
  <2>6. R.instd => <<R.iseq, R.clk>> \in R.stamps BY <1>w DEF AKW, ScanW, Scan, IdleSet
  <2>7. R.bound.inst => <<R.bound.seq, R.bound.clk>> \in R.stamps BY <1>w DEF AKW, ScanW, Scan, IdleSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7 DEF AKW
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Skip_AK ==
  ASSUME IndAK, NEW s \in Writers, Skip(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>2. AKW(SkipW(s))
  BY <1>w DEF AKW, SkipW, Skip
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA PullOnly_AK ==
  ASSUME IndAK, NEW s \in Writers, PullOnly(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>2. AKW(PullOnlyW(s))
  BY <1>w DEF AKW, PullOnlyW, PullOnly
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Claim_AK ==
  ASSUME IndAK, NEW s \in Writers, Claim(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim
<1>2. AKW(ClaimW(s))
  <2> DEFINE R == ClaimW(s)
  <2>1. R.cN \ R.capped \subseteq R.pend.nonces \cup R.aN BY <1>w DEF AKW, ClaimW, Claim, IdleSet
  <2>2. R.pend.id = 0 => R.pend.nonces = {} BY <1>w DEF AKW, ClaimW, Claim, IdleSet
  <2>3. R.ack.nonces \subseteq R.aN BY <1>w DEF AKW, ClaimW, Claim, IdleSet
  <2>4. (R.ack.st \in {"ok", "partial"} /\ R.ack.inst) => <<R.ack.seq, R.ack.src>> \in R.stamps BY <1>w DEF AKW, ClaimW, Claim, IdleSet
  <2>5. R.pc \in IdleSet => ~R.instd BY <1>w DEF AKW, ClaimW, Claim, IdleSet
  <2>6. R.instd => <<R.iseq, R.clk>> \in R.stamps BY <1>w DEF AKW, ClaimW, Claim, IdleSet
  <2>7. R.bound.inst => <<R.bound.seq, R.bound.clk>> \in R.stamps BY <1>w DEF AKW, ClaimW, Claim, IdleSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7 DEF AKW
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Verify_AK ==
  ASSUME IndAK, NEW s \in Writers, Verify(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify
<1>2. AKW(VerifyW(s))
  BY <1>w DEF AKW, VerifyW, Verify
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Install_AK ==
  ASSUME IndAK, NEW s \in Writers, Install(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] BY DEF Install
<1>2. AKW(InstallW(s))
  <2> DEFINE R == InstallW(s)
  <2>1. R.cN \ R.capped \subseteq R.pend.nonces \cup R.aN BY <1>w DEF AKW, InstallW, Install, Carries, IdleSet
  <2>2. R.pend.id = 0 => R.pend.nonces = {} BY <1>w DEF AKW, InstallW, Install, Carries, IdleSet
  <2>3. R.ack.nonces \subseteq R.aN BY <1>w DEF AKW, InstallW, Install, Carries, IdleSet
  <2>4. (R.ack.st \in {"ok", "partial"} /\ R.ack.inst) => <<R.ack.seq, R.ack.src>> \in R.stamps BY <1>w DEF AKW, InstallW, Install, Carries, IdleSet
  <2>5. R.pc \in IdleSet => ~R.instd BY <1>w DEF AKW, InstallW, Install, Carries, IdleSet
  <2>6. R.instd => <<R.iseq, R.clk>> \in R.stamps BY <1>w DEF AKW, InstallW, Install, Carries, IdleSet
  <2>7. R.bound.inst => <<R.bound.seq, R.bound.clk>> \in R.stamps BY <1>w DEF AKW, InstallW, Install, Carries, IdleSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7 DEF AKW
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Collect_AK ==
  ASSUME IndAK, NEW s \in Writers, Collect(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] BY DEF Collect
<1>2. AKW(CollectW(s))
  BY <1>w DEF AKW, CollectW, Collect
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Finish_AK ==
  ASSUME IndAK, NEW s \in Writers, Finish(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>2. AKW(FinishW(s))
  BY <1>w DEF AKW, FinishW
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Restart_AK ==
  ASSUME IndAK, NEW s \in Writers, Restart(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>2. AKW(RestartW(s))
  BY <1>w DEF AKW, RestartW, Restart
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Sync_AK ==
  ASSUME IndAK, NEW s \in Writers, Sync(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>2. AKW(SyncW(s, fail))
  BY <1>w DEF AKW, SyncW, Sync
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA RescopeBegin_AK ==
  ASSUME IndAK, NEW s \in Writers, RescopeBegin(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>2. AKW(RescopeBeginW(s, T))
  BY <1>w DEF AKW, RescopeBeginW, RescopeBegin
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA RescopeFirst_AK ==
  ASSUME IndAK, NEW s \in Writers, RescopeFirst(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>2. AKW(RescopeFirstW(s))
  BY <1>w DEF AKW, RescopeFirstW, RescopeFirst
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA RescopeSecond_AK ==
  ASSUME IndAK, NEW s \in Writers, RescopeSecond(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} : w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] BY DEF RescopeSecond
<1>2. AKW(RescopeSecondW(s, wfail))
  BY <1>w DEF AKW, RescopeSecondW, RescopeSecond
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA RPullRead_AK ==
  ASSUME IndAK, NEW s \in Writers, RPullRead(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>2. AKW(RPullReadW(s))
  <2> DEFINE R == RPullReadW(s)
  <2>1. R.cN \ R.capped \subseteq R.pend.nonces \cup R.aN BY <1>w DEF AKW, RPullReadW, RPullRead, IdleSet
  <2>2. R.pend.id = 0 => R.pend.nonces = {} BY <1>w DEF AKW, RPullReadW, RPullRead, IdleSet
  <2>3. R.ack.nonces \subseteq R.aN BY <1>w DEF AKW, RPullReadW, RPullRead, IdleSet
  <2>4. (R.ack.st \in {"ok", "partial"} /\ R.ack.inst) => <<R.ack.seq, R.ack.src>> \in R.stamps BY <1>w DEF AKW, RPullReadW, RPullRead, IdleSet
  <2>5. R.pc \in IdleSet => ~R.instd BY <1>w DEF AKW, RPullReadW, RPullRead, IdleSet
  <2>6. R.instd => <<R.iseq, R.clk>> \in R.stamps BY <1>w DEF AKW, RPullReadW, RPullRead, IdleSet
  <2>7. R.bound.inst => <<R.bound.seq, R.bound.clk>> \in R.stamps BY <1>w DEF AKW, RPullReadW, RPullRead, IdleSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7 DEF AKW
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA RPullSync_AK ==
  ASSUME IndAK, NEW s \in Writers, RPullSync(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>2. AKW(RPullSyncW(s, fail))
  <2> DEFINE R == RPullSyncW(s, fail)
  <2>1. R.cN \ R.capped \subseteq R.pend.nonces \cup R.aN BY <1>w DEF AKW, RPullSyncW, RPullSync, IdleSet
  <2>2. R.pend.id = 0 => R.pend.nonces = {} BY <1>w DEF AKW, RPullSyncW, RPullSync, IdleSet
  <2>3. R.ack.nonces \subseteq R.aN BY <1>w DEF AKW, RPullSyncW, RPullSync, IdleSet
  <2>4. (R.ack.st \in {"ok", "partial"} /\ R.ack.inst) => <<R.ack.seq, R.ack.src>> \in R.stamps BY <1>w DEF AKW, RPullSyncW, RPullSync, IdleSet
  <2>5. R.pc \in IdleSet => ~R.instd BY <1>w DEF AKW, RPullSyncW, RPullSync, IdleSet
  <2>6. R.instd => <<R.iseq, R.clk>> \in R.stamps BY <1>w DEF AKW, RPullSyncW, RPullSync, IdleSet
  <2>7. R.bound.inst => <<R.bound.seq, R.bound.clk>> \in R.stamps BY <1>w DEF AKW, RPullSyncW, RPullSync, IdleSet
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7 DEF AKW
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Touch_AK ==
  ASSUME IndAK, NEW s \in Writers, Touch(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] BY DEF Touch
<1>2. AKW(TouchW(s))
  BY <1>w, <1>t DEF AKW, TouchW, TouchN, TouchFresh, TouchNs, TouchKept
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Honor_AK ==
  ASSUME IndAK, NEW s \in Writers, Honor(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] BY DEF Honor
<1>2. AKW(HonorW(s))
  BY <1>w DEF AKW, HonorW, Honor
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA HonorReader_AK ==
  ASSUME IndAK, NEW s \in Writers, HonorReader(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY DEF HonorReader
<1>2. AKW(HonorReaderW(s))
  BY <1>w DEF AKW, HonorReaderW, NoAck
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA RetireByAck_AK ==
  ASSUME IndAK, NEW s \in Writers, RetireByAck(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \in Writer /\ w[s].nt \in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY DEF RetireByAck
<1>2. AKW(RetireByAckW(s))
  BY <1>w DEF AKW, RetireByAckW, RetireByAck, Covered, NoPend
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

LEMMA Consume_AK ==
  ASSUME IndAK, NEW s \in Writers, Consume(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>g. w[s].pc = "idle" BY DEF Consume
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>ck
  <2>2. AKW(ConsumeCheapW(s, ck)) BY <1>w, <1>g DEF AKW, ConsumeCheapW, IdleSet
  <2>. QED BY <1>0, <1>w, <2>1, <2>2, AKWrite DEF IndAK
<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>ck
  <2>2. AKW(ConsumeW(s, fail, ck)) BY <1>w, <1>g DEF AKW, ConsumeW, IdleSet
  <2>. QED BY <1>0, <1>w, <2>0, <2>2, AKWrite DEF IndAK
<1>. QED BY <1>1, <1>2

LEMMA Upload_AK ==
  ASSUME IndAK, NEW s \in Writers, NEW p \in Paths, Upload(s, p)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>1 DEF Upload
  <2>2. AKW(UploadW(s, p)) BY <1>w DEF AKW, UploadW
  <2>. QED BY <1>0, <1>w, <2>1, <2>2, AKWrite DEF IndAK
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] BY <1>2 DEF Upload
  <2>2. AKW(UploadCopyW(s, p, c)) BY <1>w DEF AKW, UploadCopyW
  <2>. QED BY <1>0, <1>w, <2>1, <2>2, AKWrite DEF IndAK
<1>. QED BY <1>1, <1>2

\* Two trees: the taker scanned -> claimed, the deposed holder marked.
LEMMA Depose_AK ==
  ASSUME IndAK, NEW t \in Writers, Depose(t)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
  BY DEF Depose
<1>2. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>3. AKW(w[t]) /\ AKW(w[holder]) BY <1>1 DEF IndAK, AKInv
<1>4. AKW(DeposeW(t)) /\ AKW(DeposedW(holder)) BY <1>1, <1>3 DEF AKW, DeposeW, DeposedW, IdleSet
<1>. QED BY <1>2, <1>4 DEF IndAK, AKInv

LEMMA Fence_AK ==
  ASSUME IndAK, NEW s \in Writers, Fence(s)
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>2. AKW(FenceBeforeW(s)) BY <1>w DEF AKW, FenceBeforeW
  <2>. QED BY <1>0, <1>w, <2>1, <2>2, AKWrite DEF IndAK
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>2. AKW(FenceAfterW(s)) BY <1>w DEF AKW, FenceAfterW
  <2>. QED BY <1>0, <1>w, <2>1, <2>2, AKWrite DEF IndAK
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The step, and the theorems.                                              *)

LEMMA Next_AK == IndAK /\ Next => AKInv'
<1>. SUFFICES ASSUME IndAK, Next PROVE AKInv' OBVIOUS
<1>1. CASE GatewayStep
  <2>1. ASSUME NEW p \in Paths, GPut(p) PROVE AKInv' BY <2>1, GPut_AK
  <2>2. ASSUME NEW p \in Paths, GCas(p) PROVE AKInv' BY <2>2, GCas_AK
  <2>3. ASSUME NEW p \in Paths, GDelete(p) PROVE AKInv' BY <2>3, GDelete_AK
  <2>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE AKInv' BY <2>4, GRename_AK
  <2>5. CASE GRenameFinish BY <2>5, GRenameFinish_AK
  <2>. QED BY <1>1, <2>1, <2>2, <2>3, <2>4, <2>5 DEF GatewayStep
<1>2. CASE WriterStep
  <2>1. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE AKInv' BY <2>1, Edit_AK
  <2>2. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE AKInv' BY <2>2, Delete_AK
  <2>3. ASSUME NEW s \in Writers, Checkout(s) PROVE AKInv' BY <2>3, Checkout_AK
  <2>4. ASSUME NEW s \in Writers, Scan(s) PROVE AKInv' BY <2>4, Scan_AK
  <2>5. ASSUME NEW s \in Writers, Skip(s) PROVE AKInv' BY <2>5, Skip_AK
  <2>6. ASSUME NEW s \in Writers, PullOnly(s) PROVE AKInv' BY <2>6, PullOnly_AK
  <2>7. ASSUME NEW s \in Writers, Claim(s) PROVE AKInv' BY <2>7, Claim_AK
  <2>8. ASSUME NEW s \in Writers, Verify(s) PROVE AKInv' BY <2>8, Verify_AK
  <2>9. ASSUME NEW s \in Writers, Install(s) PROVE AKInv' BY <2>9, Install_AK
  <2>10. ASSUME NEW s \in Writers, Collect(s) PROVE AKInv' BY <2>10, Collect_AK
  <2>11. ASSUME NEW s \in Writers, Finish(s) PROVE AKInv' BY <2>11, Finish_AK
  <2>12. ASSUME NEW s \in Writers, Restart(s) PROVE AKInv' BY <2>12, Restart_AK
  <2>13. ASSUME NEW s \in Writers, Sync(s) PROVE AKInv' BY <2>13, Sync_AK
  <2>14. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE AKInv' BY <2>14, RescopeBegin_AK
  <2>15. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE AKInv' BY <2>15, RescopeFirst_AK
  <2>16. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE AKInv' BY <2>16, RescopeSecond_AK
  <2>17. ASSUME NEW s \in Writers, RPullRead(s) PROVE AKInv' BY <2>17, RPullRead_AK
  <2>18. ASSUME NEW s \in Writers, RPullSync(s) PROVE AKInv' BY <2>18, RPullSync_AK
  <2>19. ASSUME NEW s \in Writers, Touch(s) PROVE AKInv' BY <2>19, Touch_AK
  <2>20. ASSUME NEW s \in Writers, Honor(s) PROVE AKInv' BY <2>20, Honor_AK
  <2>21. ASSUME NEW s \in Writers, HonorReader(s) PROVE AKInv' BY <2>21, HonorReader_AK
  <2>22. ASSUME NEW s \in Writers, RetireByAck(s) PROVE AKInv' BY <2>22, RetireByAck_AK
  <2>23. ASSUME NEW s \in Writers, Consume(s) PROVE AKInv' BY <2>23, Consume_AK
  <2>24. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE AKInv' BY <2>24, Upload_AK
  <2>25. ASSUME NEW s \in Writers, Depose(s) PROVE AKInv' BY <2>25, Depose_AK
  <2>26. ASSUME NEW s \in Writers, Fence(s) PROVE AKInv' BY <2>26, Fence_AK
  <2>27. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE AKInv' BY <2>27, Sweep_AK
  <2>. QED BY <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>18, <2>19, <2>20, <2>21, <2>22, <2>23, <2>24, <2>25, <2>26, <2>27 DEF WriterStep
<1>3. CASE Age BY <1>3, Age_AK
<1>4. CASE RLoad BY <1>4, RLoad_AK
<1>5. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE AKInv' BY <1>5, Reap_AK
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Next

LEMMA AKInvariant == Spec => []IndAK
<1>1. Init => IndAK BY Init_AK
<1>2. IndAK /\ [Next]_vars => IndAK'
  <2>1. IndAK /\ Next => IndTypeOK' BY Next_TypeOK DEF IndAK
  <2>2. IndAK /\ Next => AKInv' BY Next_AK
  <2>3. IndAK /\ UNCHANGED vars => IndAK'
    BY DEF IndAK, AKInv, AKW, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
  <2>. QED BY <2>1, <2>2, <2>3 DEF IndAK
<1>. QED BY <1>1, <1>2, PTL DEF Spec

THEOREM NoNonceOrphaned == Spec => []AK3
<1>1. IndAK => AK3 BY DEF IndAK, AKInv, AKW, AK3
<1>. QED BY AKInvariant, <1>1, PTL

THEOREM OneClock == Spec => []AK4
<1>1. IndAK => AK4 BY DEF IndAK, AKInv, AKW, AK4
<1>. QED BY AKInvariant, <1>1, PTL

==============================================================================
