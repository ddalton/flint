#!/usr/bin/env python3
"""Emit LeanP1ProofM10.tla: AK1 (row 15, S1 rows 1 and 2), part 1.

AK1 is an action property: the step that writes an `ok`/`partial` ack names
every declared path the bound's document does not carry.  Part 1 reduces it
to a state invariant, BoundOK ("were the honour to fire now, its `dropped`
would name every broken declared path"):
  - no step but Honor writes an `ok`/`partial` ack (HonorReader writes
    `refused-read-only`; every other step keeps the ack);
  - Honor's guard is Honorable(s) and its `dropped` is HDropped(s), so
    BoundOK gives AK1 at that step;
  - UncitedCatches, the heart of L-137: a declared path in carrier form
    (the carrier published the declared bytes or the declared delete, or
    names it dropped) is broken in a document `d` only if it is dropped or
    Uncited(car, d) names it.  Part 2 (BoundOK inductive) uses it at every
    barrier end, where the bound's document is fixed and `uncited` computed.
Part 2 is written after MCLeanP1M7's worlds check BoundOK, DTree and CPub.
Generated: results/2026-10-15-tlaps-rows15-17/gen-m10.py.  EXTENDS M0.
Usage: gen-m10.py OUTDIR"""
import sys, os
OUT = sys.argv[1]

# (name, signature, binders, PICK restatement or None, record)
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
 ('RetireByAck', 'RetireByAck(s)', '', None, 'RetireByAckW(s)'),
]
NOTREE = [('GPut', 'GPut(p)', ', NEW p \\in Paths'), ('GCas', 'GCas(p)', ', NEW p \\in Paths'),
          ('GRename', 'GRename(p, q)', ', NEW p \\in Paths, NEW q \\in Paths'), ('GRenameFinish', 'GRenameFinish', ''),
          ('GDelete', 'GDelete(p)', ', NEW p \\in Paths'), ('Sweep', 'Sweep(s, h)', ', NEW s \\in Writers, NEW h \\in Handles'),
          ('Age', 'Age', ''), ('RLoad', 'RLoad', ''), ('Reap', 'Reap(s, h)', ', NEW s \\in Writers, NEW h \\in Handles')]

H = r'''------------------------------ MODULE LeanP1ProofM10 ------------------------------
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

'''

def lemma(n, sig, b, pick, rec):
    restate = f"{pick} w' = [w EXCEPT ![s] = {rec}]" if pick else f"w' = [w EXCEPT ![s] = {rec}]"
    return f'''LEMMA {n}_AK1 ==
  ASSUME TypeOK, NEW s \\in Writers{b}, {sig}
  PROVE  AK1Step
<1>1. {restate} BY DEF {n}
<1>2. {rec}.ack = w[s].ack BY DEF {rec.split('(')[0]}
<1>. QED BY <1>1, <1>2, AckKeep

'''

t = H
t += '------------------------------------------------------------------------------\n(* The steps that write no tree.                                           *)\n\n'
for n, sig, b in NOTREE:
    t += f'''LEMMA {n}_AK1 ==
  ASSUME TypeOK{b}, {sig}
  PROVE  AK1Step
<1>1. w' = w BY DEF {sig.split('(')[0]}
<1>. QED BY <1>1, AckSame

'''
t += '------------------------------------------------------------------------------\n(* The writer steps that keep the ack.                                    *)\n\n'
for st in STEPS:
    t += lemma(*st)
t += r'''LEMMA Consume_AK1 ==
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
'''
CASES = [('Edit', 'NEW p \\in Paths, Edit(s, p)'), ('Delete', 'NEW p \\in Paths, Delete(s, p)'),
         ('Checkout', 'Checkout(s)'), ('Scan', 'Scan(s)'), ('Skip', 'Skip(s)'), ('PullOnly', 'PullOnly(s)'),
         ('Claim', 'Claim(s)'), ('Verify', 'Verify(s)'), ('Install', 'Install(s)'), ('Collect', 'Collect(s)'),
         ('Finish', 'Finish(s)'), ('Restart', 'Restart(s)'), ('Sync', 'Sync(s)'),
         ('RescopeBegin', 'RescopeBegin(s)'), ('RescopeFirst', 'RescopeFirst(s)'), ('RescopeSecond', 'RescopeSecond(s)'),
         ('RPullRead', 'RPullRead(s)'), ('RPullSync', 'RPullSync(s)'), ('Touch', 'Touch(s)'), ('Honor', 'Honor(s)'),
         ('HonorReader', 'HonorReader(s)'), ('RetireByAck', 'RetireByAck(s)'), ('Consume', 'Consume(s)'),
         ('Upload', 'NEW p \\in Paths, Upload(s, p)'), ('Depose', 'Depose(s)'), ('Fence', 'Fence(s)'),
         ('Sweep', 'NEW h \\in Handles, Sweep(s, h)')]
for i, (n, a) in enumerate(CASES, 1):
    t += f'  <2>{i}. ASSUME NEW s \\in Writers, {a} PROVE AK1Step BY <2>{i}, {n}_AK1\n'
t += '  <2>. QED BY <1>2, ' + ', '.join(f'<2>{i}' for i in range(1, len(CASES) + 1)) + ' DEF WriterStep\n'
t += r'''<1>3. CASE Age BY <1>3, Age_AK1
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
'''
open(os.path.join(OUT, 'LeanP1ProofM10.tla'), 'w').write(t)
print('written', OUT)
