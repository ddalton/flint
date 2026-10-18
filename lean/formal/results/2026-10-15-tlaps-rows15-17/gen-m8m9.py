#!/usr/bin/env python3
"""Emit LeanP1ProofM8.tla (NR1, row 16) and LeanP1ProofM9.tla (DP1, DP2, row 17),
each EXTENDS LeanP1ProofM2 (the split chain's M2 module: IndM2 and M2Invariant).
Both are per-step facts about one written tree: NR1 reads its bytes (a path the
tree deleted, still published, comes back only by the agent's own write -- a
mint); DP1 and DP2 its pc and collected bit (an install, and a collect that
moves `live`, are the lease holder's).  Usage: gen-m8m9.py OUTDIR"""
import sys, os
OUT = sys.argv[1]

# (name, sig, binders, pick or None, record)
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
NOTREE = [('GPut', 'GPut(p)', ', NEW p \\in Paths'), ('GCas', 'GCas(p)', ', NEW p \\in Paths'),
          ('GRename', 'GRename(p, q)', ', NEW p \\in Paths, NEW q \\in Paths'), ('GRenameFinish', 'GRenameFinish', ''),
          ('GDelete', 'GDelete(p)', ', NEW p \\in Paths'), ('Sweep', 'Sweep(s, h)', ', NEW s \\in Writers, NEW h \\in Handles'),
          ('Age', 'Age', ''), ('RLoad', 'RLoad', ''), ('Reap', 'Reap(s, h)', ', NEW s \\in Writers, NEW h \\in Handles')]

def rule(title):
    return '------------------------------------------------------------------------------\n(* ' + title.ljust(73) + '*)\n\n'

def module(name, head, act, write_lemma, same_lemma, fact, by, extra, theorems, actname, two_tree, fence):
    t = '-' * ((78 - len(f' MODULE {name} ')) // 2) + f' MODULE {name} '
    t += '-' * (78 - len(t)) + '\n' + head + 'EXTENDS LeanP1ProofM2\n\n'
    t += act + '\n' + write_lemma + '\n' + same_lemma + '\n'
    t += rule('The steps that write no tree.')
    for n, sig, b in NOTREE:
        t += f'''LEMMA {n}_{actname} ==
  ASSUME IndM2{b}, {sig}
  PROVE  {actname}
<1>1. w' = w BY DEF {sig.split('(')[0]}
<1>. QED BY <1>1, {actname}Same

'''
    t += rule('The writer steps: one tree.')
    for n, sig, b, pick, rec in STEPS:
        restate = f"{pick} w' = [w EXCEPT ![s] = {rec}]" if pick else f"w' = [w EXCEPT ![s] = {rec}]"
        t += f'''LEMMA {n}_{actname} ==
  ASSUME IndM2, NEW s \\in Writers{b}, {sig}
  PROVE  {actname}
<1>0. TypeOK /\\ Pending /\\ Off /\\ w[s] \\in Writer BY DEF IndM2, IndM1, IndTypeOK, M2, M1, TypeOK
<1>f. w[s].local \\in [Paths -> Opt(Handles)] /\\ w[s].baseline \\in [Paths -> Opt(Handles)] BY <1>0, WriterFields
<1>1. {restate} BY DEF {n}
<1>2. {fact(rec)}
  BY {by.get(n, 'DEF ' + rec.split('(')[0])}
<1>. QED BY <1>0, <1>1, <1>2, {actname}Write

'''
    t += extra
    t += two_tree + fence
    # Next
    t += rule('The step, and the theorems.')
    cases = []
    i = 1
    for n, sig, b in NOTREE[:5]:
        pass
    t += f'''LEMMA Next_{actname} == IndM2 /\\ Next => {actname}
<1>. SUFFICES ASSUME IndM2, Next PROVE {actname} OBVIOUS
<1>1. CASE GatewayStep
  <2>1. ASSUME NEW p \\in Paths, GPut(p) PROVE {actname} BY <2>1, GPut_{actname}
  <2>2. ASSUME NEW p \\in Paths, GCas(p) PROVE {actname} BY <2>2, GCas_{actname}
  <2>3. ASSUME NEW p \\in Paths, GDelete(p) PROVE {actname} BY <2>3, GDelete_{actname}
  <2>4. ASSUME NEW p \\in Paths, NEW q \\in Paths, GRename(p, q) PROVE {actname} BY <2>4, GRename_{actname}
  <2>5. CASE GRenameFinish BY <2>5, GRenameFinish_{actname}
  <2>. QED BY <1>1, <2>1, <2>2, <2>3, <2>4, <2>5 DEF GatewayStep
<1>2. CASE WriterStep
'''
    k = 1
    labels = []
    for n, sig, b, pick, rec in STEPS + [('Consume', 'Consume(s)', '', None, None), ('Upload', 'Upload(s, p)', ', NEW p \\in Paths', None, None),
                                          ('Depose', 'Depose(s)', '', None, None), ('Fence', 'Fence(s)', '', None, None)]:
        t += f'  <2>{k}. ASSUME NEW s \\in Writers{b}, {sig} PROVE {actname} BY <2>{k}, {n}_{actname}\n'
        labels.append(f'<2>{k}'); k += 1
    t += f'  <2>{k}. ASSUME NEW s \\in Writers, NEW h \\in Handles, Sweep(s, h) PROVE {actname} BY <2>{k}, Sweep_{actname}\n'
    labels.append(f'<2>{k}')
    t += f'  <2>. QED BY <1>2, {", ".join(labels)} DEF WriterStep\n'
    t += f'''<1>3. CASE Age BY <1>3, Age_{actname}
<1>4. CASE RLoad BY <1>4, RLoad_{actname}
<1>5. ASSUME NEW s \\in Writers, NEW h \\in Handles, Reap(s, h) PROVE {actname} BY <1>5, Reap_{actname}
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Next

''' + theorems + '=' * 78 + '\n'
    return t

# ------------------------------------------------------------------ M8: NR1
NR_HEAD = '''(* M8 (row 16): NR1 -- a path the tree deleted and has not published comes
   back only by the agent's own write: no step but Edit puts bytes where the
   tree holds none over a baseline it holds (the consume and the syncs take
   only owed paths -- clean ones -- or converge on the document's bytes, and
   a converged delete stays one; the widen fetches only unheld paths; the
   copy upload re-points only a path still holding its snapshot, which a
   pending upload has: M2's Pending; a checkout starts from no baseline: M1's
   Off).  Generated: results/2026-10-15-tlaps-rows15-17/gen-m8m9.py.       *)
'''
NR_ACT = r'''NR1Act == \A s \in Writers, p \in Paths :
            w[s].local[p] = Nil /\ w[s].baseline[p] # Nil /\ w'[s].local[p] # Nil
              => w'[s].local[p] = <<p, nextGen>> /\ nextGen' = nextGen + 1
'''
NR_WRITE = r'''\* A step writing one tree R: NR1 from R's bytes alone.
LEMMA NR1ActWrite ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         \A q \in Paths : (w[s].local[q] = Nil /\ w[s].baseline[q] # Nil /\ R.local[q] # Nil)
                            => R.local[q] = <<q, nextGen>> /\ nextGen' = nextGen + 1
  PROVE  NR1Act
<1>1. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY WriteAny
<1>. QED BY <1>1 DEF NR1Act
'''
NR_SAME = '''LEMMA NR1ActSame == w' = w => NR1Act
BY DEF NR1Act
'''
def nr_fact(rec):
    return (f"\\A q \\in Paths : (w[s].local[q] = Nil /\\ w[s].baseline[q] # Nil /\\ {rec}.local[q] # Nil)\n"
            f"                       => {rec}.local[q] = <<q, nextGen>> /\\ nextGen' = nextGen + 1")
NR_BY = {
 'Edit': '<1>f DEF EditW, Edit',
 'Delete': '<1>f DEF DeleteW',
 'Checkout': '<1>0 DEF Checkout, Off, WriterInit, CheckoutW, CheckoutHeld',
 'Sync': '<1>f DEF SyncW, SyncBl, SyncTaken, SyncOwed, SyncConv, SyncAll, Owed, Converges',
 'RPullSync': '<1>f DEF RPullSyncW, SyncBl, SyncTaken, SyncOwed, SyncConv, SyncAll, Owed, Converges',
 'RescopeFirst': 'ShippedShape DEF RescopeFirstW, Shipped',
 'RescopeSecond': '<1>f, ShippedShape DEF RescopeSecondW, RescopeFetch, RescopeFetch0, RescopeAdd, RescopeBase1, RescopeLocal1, Shipped',
}
NR_EXTRA = r'''LEMMA Consume_NR1Act ==
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

'''
NR_TWO = r'''LEMMA Depose_NR1Act ==
  ASSUME IndM2, NEW t \in Writers, Depose(t)
  PROVE  NR1Act
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>1. holder \in Writers /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)] BY DEF Depose
<1>2. \A u \in Writers : w'[u].local = w[u].local BY <1>0, <1>1 DEF TypeOK, DeposeW, DeposedW
<1>. QED BY <1>2 DEF NR1Act

'''
NR_FENCE = r'''LEMMA Fence_NR1Act ==
  ASSUME IndM2, NEW s \in Writers, Fence(s)
  PROVE  NR1Act
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>1. \/ w' = [w EXCEPT ![s] = FenceBeforeW(s)]
      \/ w' = [w EXCEPT ![s] = FenceAfterW(s)]
  BY DEF Fence
<1>2. FenceBeforeW(s).local = w[s].local /\ FenceAfterW(s).local = w[s].local BY DEF FenceBeforeW, FenceAfterW
<1>. QED BY <1>0, <1>1, <1>2, NR1ActWrite

'''
NR_THM = r'''THEOREM DeletedStaysDeleted == Spec => NR1
<1>1. IndM2 /\ [Next]_vars => [NR1Act]_vars
  <2>. SUFFICES ASSUME IndM2, [Next]_vars PROVE [NR1Act]_vars OBVIOUS
  <2>1. CASE Next BY <2>1, Next_NR1Act
  <2>2. CASE UNCHANGED vars BY <2>2 DEF vars
  <2>. QED BY <2>1, <2>2
<1>. QED BY M2Invariant, <1>1, PTL DEF Spec, NR1, NR1Act

'''
def gen_nr():
    def fact(rec): return nr_fact(rec)
    t = module('LeanP1ProofM8', NR_HEAD, NR_ACT, NR_WRITE, NR_SAME, fact,
               {k: v for k, v in NR_BY.items()}, NR_EXTRA, NR_THM, 'NR1Act', NR_TWO, NR_FENCE)
    # the per-step default: the record keeps the tree's bytes
    return t.replace('NR1ActWrite\n\n', 'NR1ActWrite\n\n')

# ------------------------------------------------------------------ M9: DP1, DP2
DP_HEAD = '''(* M9 (row 17): DP1 and DP2 -- an install is the lease holder's, and so is
   a collect that moves `live`.  The only step from claimed to cased is
   Install, fenced (InstallFenced: the CAS reads the lease); the only steps
   that set `collected` are Install and the fence after the CAS (neither
   moves `live`) and Collect (DeletesUnderLease; under the retire age it
   moves nothing either).  Inv_OneHolder itself, restated for deposal, is
   M1's.  Generated: results/2026-10-15-tlaps-rows15-17/gen-m8m9.py.      *)
'''
DP_ACT = r'''DPAct ==
  /\ \A s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased" => holder = s
  /\ \A s \in Writers : ~w[s].collected /\ w'[s].collected /\ live' # live => holder = s
'''
DP_WRITE = r'''LEMMA DPActWrite ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         /\ (R.pc = "cased" /\ w[s].pc = "claimed") => holder = s
         /\ (~w[s].collected /\ R.collected /\ live' # live) => holder = s
  PROVE  DPAct
<1>1. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY WriteAny
<1>. QED BY <1>1 DEF DPAct
'''
DP_SAME = '''LEMMA DPActSame == w' = w => DPAct
BY DEF DPAct
'''
def dp_fact(rec):
    return (f"/\\ ({rec}.pc = \"cased\" /\\ w[s].pc = \"claimed\") => holder = s\n"
            f"      /\\ (~w[s].collected /\\ {rec}.collected /\\ live' # live) => holder = s")
DP_BY = {
 'Install': 'ShippedShape DEF Install, InstallW, Shipped',
 'Collect': 'ShippedShape DEF Collect, CollectW, Shipped',
}
DP_EXTRA = r'''LEMMA Consume_DPAct ==
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

'''
DP_TWO = r'''\* The taker goes scanned -> claimed; the deposed keeps its pc and bit.
LEMMA Depose_DPAct ==
  ASSUME IndM2, NEW t \in Writers, Depose(t)
  PROVE  DPAct
<1>0. TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>1. holder \in Writers /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)] BY DEF Depose
<1>2. \A u \in Writers : /\ w'[u].pc \in {w[u].pc, "claimed"}
                         /\ w'[u].collected = w[u].collected
  BY <1>0, <1>1 DEF TypeOK, DeposeW, DeposedW
<1>. QED BY <1>2 DEF DPAct

'''
DP_FENCE = r'''\* Before the CAS: back to idle; after it, `collected` set with `live` kept.
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

'''
DP_THM = r'''THEOREM InstallUnderLease == Spec => DP1
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

'''
def gen_dp():
    def fact(rec): return dp_fact(rec)
    by = dict(DP_BY)
    for n, sig, b, pick, rec in STEPS:
        by.setdefault(n, f'DEF {sig.split("(")[0]}, {rec.split("(")[0]}, bucket, aux')
    return module('LeanP1ProofM9', DP_HEAD, DP_ACT, DP_WRITE, DP_SAME, fact, by, DP_EXTRA, DP_THM, 'DPAct', DP_TWO, DP_FENCE)

os.makedirs(OUT, exist_ok=True)
nr = gen_nr()
# NR1's defaults: the record keeps the bytes (DEF the record)
open(os.path.join(OUT, 'LeanP1ProofM8.tla'), 'w').write(nr)
open(os.path.join(OUT, 'LeanP1ProofM9.tla'), 'w').write(gen_dp())
print('written', OUT)
