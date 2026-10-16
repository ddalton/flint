#!/usr/bin/env python3
"""Rows 15-17 in M5D: the six new steps' Part D lemmas and their Next_D cases.
None writes a tree's bytes, a record or the document."""
p = 'm5d/LeanP1ProofM5D.tla'; t = open(p).read()
def R(old, new, cnt=1):
    global t
    assert t.count(old) == cnt, (old[:90], t.count(old)); t = t.replace(old, new)
def simple(n):
    return f'''LEMMA {n}_D ==
  ASSUME IndC, IndC', DInv, NEW s \\in Writers, {n}(s), Frame PROVE DInv'
<1>1. WStep(s, {n}W(s)) BY DEF {n}, WStep, bucket, aux
<1>2. UDR({n}W(s)) BY UDSame DEF {n}W
<1>. QED BY <1>1, <1>2, DWriter DEF {n}W

'''
NEW = '''------------------------------------------------------------------------------
(* Part D: rows 15-17 -- the answer's steps, deposal and the fence.         *)

''' + ''.join(simple(n) for n in ['Touch', 'Honor', 'HonorReader', 'RetireByAck']) + r'''LEMMA Depose_D ==
  ASSUME IndC, IndC', DInv, NEW t \in Writers, Depose(t), Frame PROVE DInv'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<minted, base, orig, doc, acked, conflicts, udel>>
  BY DEF Depose, aux
<1>w. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>f. \A u \in Writers : /\ w'[u].local = w[u].local /\ w'[u].baseline = w[u].baseline
                         /\ w'[u].uploads = w[u].uploads
                         /\ (w'[u].pc \in Bar => w[u].pc \in Bar)
  BY <1>1, <1>w DEF DeposeW, DeposedW, Bar
<1>hs. HStep BY <1>1, HStepQ DEF Frame
<1>tk. TookOK BY <1>0, TookUpd DEF Frame
<1>l. LocalOK BY <1>f DEF LocalOK
<1>u. UD' BY <1>f DEF DInv, UD, UDR
<1>. QED BY <1>1, <1>hs, <1>tk, <1>l, <1>u, DCore

LEMMA Fence_D ==
  ASSUME IndC, IndC', DInv, NEW s \in Writers, Fence(s), Frame PROVE DInv'
<1>1. CASE w[s].pc = "claimed"
  <2>1. WStep(s, FenceBeforeW(s)) BY <1>1 DEF Fence, WStep, bucket, aux
  <2>2. UDR(FenceBeforeW(s)) BY DEF UDR, FenceBeforeW, Bar
  <2>. QED BY <2>1, <2>2, DWriter DEF FenceBeforeW
<1>2. CASE w[s].pc # "claimed"
  <2>1. WStep(s, FenceAfterW(s)) BY <1>2 DEF Fence, WStep, bucket, aux
  <2>2. UDR(FenceAfterW(s)) BY UDSame DEF FenceAfterW
  <2>. QED BY <2>1, <2>2, DWriter DEF FenceAfterW
<1>. QED BY <1>1, <1>2

'''
R('''LEMMA Quiet_D ==''', NEW + '''LEMMA Quiet_D ==''')
R('''    <3>21. ASSUME NEW s \\in Writers, NEW h \\in Handles, Sweep(s, h) PROVE DInv' BY <3>21, Sweep_D
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21''',
'''    <3>21. ASSUME NEW s \\in Writers, NEW h \\in Handles, Sweep(s, h) PROVE DInv' BY <3>21, Sweep_D
    <3>22. ASSUME NEW s \\in Writers, Touch(s) PROVE DInv' BY <3>22, Touch_D
    <3>23. ASSUME NEW s \\in Writers, Honor(s) PROVE DInv' BY <3>23, Honor_D
    <3>24. ASSUME NEW s \\in Writers, HonorReader(s) PROVE DInv' BY <3>24, HonorReader_D
    <3>25. ASSUME NEW s \\in Writers, RetireByAck(s) PROVE DInv' BY <3>25, RetireByAck_D
    <3>26. ASSUME NEW s \\in Writers, Depose(s) PROVE DInv' BY <3>26, Depose_D
    <3>27. ASSUME NEW s \\in Writers, Fence(s) PROVE DInv' BY <3>27, Fence_D
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27''')
open(p, 'w').write(t); print('ok')
