#!/usr/bin/env python3
"""Emit LeanP1ProofM7.tla: AK3 and AK4 (row 15), EXTENDS LeanP1ProofM0.
One per-writer predicate on a tree, AKW, carries both claims and what makes
them inductive (m7/MCLeanP1M7.tla, TLC-checked first):
  AK3  every consumed nonce is pending or answered (but the capped ones);
  AK3a a record with no id has no nonces;  AK3b an ack's nonces were answered;
  AK4  an installed ack names a stamp;  AK4a nothing is installed outside the
  install-to-finish span;  AK4b the in-flight install's stamp, and AK4c the
  bound's, are stamps the CAS wrote.
Every step lemma proves AKW of the tree it writes from AKW of the tree it
read.  (AK1 is not here: the delete+rename trace, ackx/out/AckRename.out.)
Usage: gen-m7.py OUTDIR"""
import sys, os
OUT = sys.argv[1]
STEPS = [
 ('Edit', 'Edit(s, p)', ', NEW p \\in Paths', None, 'EditW(s, p)'),
 ('Delete', 'Delete(s, p)', ', NEW p \\in Paths', None, 'DeleteW(s, p)'),
 ('Checkout', 'Checkout(s)', '', 'PICK T \\in Scopes :', 'CheckoutW(s, T)'),
 ('Scan', 'Scan(s)', '', 'PICK dels \\in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) :', 'ScanW(s, dels)'),
 ('Skip', 'Skip(s)', '', None, 'SkipW(s)'),
 ('PullOnly', 'PullOnly(s)', '', None, 'PullOnlyW(s)'),
 ('Claim', 'Claim(s)', '', None, 'ClaimW(s)'),
 ('Verify', 'Verify(s)', '', None, 'VerifyW(s)'),
 ('Install', 'Install(s)', '', None, 'InstallW(s)'),
 ('Collect', 'Collect(s)', '', None, 'CollectW(s)'),
 ('Finish', 'Finish(s)', '', None, 'FinishW(s)'),
 ('Restart', 'Restart(s)', '', None, 'RestartW(s)'),
 ('Sync', 'Sync(s)', '', 'PICK fail \\in SUBSET SyncAll(s) :', 'SyncW(s, fail)'),
 ('RescopeBegin', 'RescopeBegin(s)', '', 'PICK T \\in Scopes \\ {w[s].scope} :', 'RescopeBeginW(s, T)'),
 ('RescopeFirst', 'RescopeFirst(s)', '', None, 'RescopeFirstW(s)'),
 ('RescopeSecond', 'RescopeSecond(s)', '', 'PICK wfail \\in SUBSET {q \\in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \\/ ~WidenKeepsLocal} :', 'RescopeSecondW(s, wfail)'),
 ('RPullRead', 'RPullRead(s)', '', None, 'RPullReadW(s)'),
 ('RPullSync', 'RPullSync(s)', '', 'PICK fail \\in SUBSET SyncAll(s) :', 'RPullSyncW(s, fail)'),
 ('Touch', 'Touch(s)', '', None, 'TouchW(s)'),
 ('Honor', 'Honor(s)', '', None, 'HonorW(s)'),
 ('HonorReader', 'HonorReader(s)', '', None, 'HonorReaderW(s)'),
 ('RetireByAck', 'RetireByAck(s)', '', None, 'RetireByAckW(s)'),
]
BY = {
 'Touch': '<1>w, <1>t DEF AKW, TouchW, TouchN, TouchFresh, TouchNs, TouchKept',
 'Honor': '<1>w DEF AKW, HonorW, Honor',
 'HonorReader': '<1>w DEF AKW, HonorReaderW, NoAck',
 'RetireByAck': '<1>w DEF AKW, RetireByAckW, RetireByAck, Covered, NoPend',
 'Install': '<1>w DEF AKW, InstallW, Install, Carries',
 'Finish': '<1>w DEF AKW, FinishW',
}
NOTREE = [('GPut', 'GPut(p)', ', NEW p \\in Paths'), ('GCas', 'GCas(p)', ', NEW p \\in Paths'),
          ('GRename', 'GRename(p, q)', ', NEW p \\in Paths, NEW q \\in Paths'), ('GRenameFinish', 'GRenameFinish', ''),
          ('GDelete', 'GDelete(p)', ', NEW p \\in Paths'), ('Sweep', 'Sweep(s, h)', ', NEW s \\in Writers, NEW h \\in Handles'),
          ('Age', 'Age', ''), ('RLoad', 'RLoad', ''), ('Reap', 'Reap(s, h)', ', NEW s \\in Writers, NEW h \\in Handles')]
H = r'''------------------------------ MODULE LeanP1ProofM7 ------------------------------
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

'''
# AKW's seven conjuncts, one obligation each: the steps whose record is large
# timed out proving AKW whole (EC2 2026-10-15: Scan, Claim, Install, RPullRead,
# RPullSync at 103-138% of the 60 s limit).
AKW_CONJ = [
 r'R.cN \ R.capped \subseteq R.pend.nonces \cup R.aN',
 r'R.pend.id = 0 => R.pend.nonces = {}',
 r'R.ack.nonces \subseteq R.aN',
 r'(R.ack.st \in {"ok", "partial"} /\ R.ack.inst) => <<R.ack.seq, R.ack.src>> \in R.stamps',
 r'R.pc \in IdleSet => ~R.instd',
 r'R.instd => <<R.iseq, R.clk>> \in R.stamps',
 r'R.bound.inst => <<R.bound.seq, R.bound.clk>> \in R.stamps',
]
SPLIT = {'Scan', 'Claim', 'Install', 'RPullRead', 'RPullSync'}
def akw_proof(n, rec):
    by = BY.get(n, '<1>w DEF AKW, ' + rec.split('(')[0] + ', ' + n)
    if n not in SPLIT:
        return f'  BY {by}'
    s = f'  <2> DEFINE R == {rec}\n'
    for i, c in enumerate(AKW_CONJ, 1):
        s += f'  <2>{i}. {c} BY {by}, IdleSet\n'
    s += '  <2> QED BY ' + ', '.join(f'<2>{i}' for i in range(1, 8)) + ' DEF AKW'
    return s

def lemma(n, sig, b, pick, rec):
    restate = f"{pick} w' = [w EXCEPT ![s] = {rec}]" if pick else f"w' = [w EXCEPT ![s] = {rec}]"
    return f'''LEMMA {n}_AK ==
  ASSUME IndAK, NEW s \\in Writers{b}, {sig}
  PROVE  AKInv'
<1>0. TypeOK BY DEF IndAK, IndTypeOK
<1>w. AKW(w[s]) BY DEF IndAK, AKInv
<1>t. w[s] \\in Writer /\\ w[s].nt \\in Nat BY <1>0, WriterFields DEF TypeOK
<1>1. {restate} BY DEF {n}
<1>2. AKW({rec})
{akw_proof(n, rec)}
<1>. QED BY <1>0, <1>w, <1>1, <1>2, AKWrite DEF IndAK

'''
t = H
t += '------------------------------------------------------------------------------\n(* The steps that write no tree.                                           *)\n\n'
for n, sig, b in NOTREE:
    t += f'''LEMMA {n}_AK ==
  ASSUME IndAK{b}, {sig}
  PROVE  AKInv'
<1>1. w' = w BY DEF {sig.split('(')[0]}
<1>. QED BY <1>1, AKSame DEF IndAK

'''
t += '------------------------------------------------------------------------------\n(* The writer steps.                                                       *)\n\n'
for st in STEPS:
    t += lemma(*st)
t += r'''LEMMA Consume_AK ==
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
'''
k = 1; labels = []
for n, sig, b, pick, rec in STEPS + [('Consume', 'Consume(s)', '', None, None), ('Upload', 'Upload(s, p)', ', NEW p \\in Paths', None, None),
                                      ('Depose', 'Depose(s)', '', None, None), ('Fence', 'Fence(s)', '', None, None)]:
    t += f"  <2>{k}. ASSUME NEW s \\in Writers{b}, {sig} PROVE AKInv' BY <2>{k}, {n}_AK\n"; labels.append(f'<2>{k}'); k += 1
t += f"  <2>{k}. ASSUME NEW s \\in Writers, NEW h \\in Handles, Sweep(s, h) PROVE AKInv' BY <2>{k}, Sweep_AK\n"; labels.append(f'<2>{k}')
t += f"  <2>. QED BY <1>2, {', '.join(labels)} DEF WriterStep\n"
t += r'''<1>3. CASE Age BY <1>3, Age_AK
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

''' + '=' * 78 + '\n'
os.makedirs(OUT, exist_ok=True)
open(os.path.join(OUT, 'LeanP1ProofM7.tla'), 'w').write(t)
print('written', OUT)
