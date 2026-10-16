#!/usr/bin/env python3
"""Rows 15-17 in M6: the six new steps and their Next_M6 cases.  None changes a
tree's bytes, its baseline, its held set, or (in a barrier) its uploads."""
p = 'm6/LeanP1ProofM6.tla'; t = open(p).read()
def R(old, new, cnt=1):
    global t
    assert t.count(old) == cnt, (old[:90], t.count(old)); t = t.replace(old, new)
def simple(n):
    return f'''LEMMA {n}_M6 == ASSUME IndC, UI, NEW s \\in Writers, {n}(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = {n}W(s)] /\\ UNCHANGED <<upped, orig, minted>> BY DEF {n}, bucket, aux
<1>. QED BY <1>1, Simple_M6 DEF {n}W, Bar

'''
NEW = '''------------------------------------------------------------------------------
(* M6: rows 15-17 -- the answer's steps, deposal and the fence.             *)

''' + ''.join(simple(n) for n in ['Touch', 'Honor', 'HonorReader', 'RetireByAck']) + r'''LEMMA Depose_M6 == ASSUME IndC, UI, NEW t \in Writers, Depose(t) PROVE M6Goal
<1>0. TypeOK BY TypeOKC
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<upped, orig, minted>>
  BY DEF Depose, aux
<1>w. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>f. \A u \in Writers : /\ w'[u].local = w[u].local /\ w'[u].baseline = w[u].baseline
                         /\ w'[u].sHeld = w[u].sHeld /\ w'[u].upDone = w[u].upDone /\ w'[u].snap = w[u].snap
                         /\ (w'[u].pc \in Bar => w[u].pc \in Bar)
  BY <1>1, <1>w DEF DeposeW, DeposedW, Bar
<1>a. \A u \in Writers : AWt(u) /\ SRt(u) BY <1>f, M6Same
<1>b. AWA /\ SRA BY <1>a, M6Per
<1>c. UIo' BY <1>1, UIoKeep
<1>d. \A u \in Writers : UIR(w[u])' BY <1>f DEF UI, UIR
<1>. QED BY <1>b, <1>c, <1>d DEF M6Goal, UI

LEMMA Fence_M6 == ASSUME IndC, UI, NEW s \in Writers, Fence(s) PROVE M6Goal
<1>k. UNCHANGED <<upped, orig, minted>> BY DEF Fence, bucket, aux
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>. QED BY <2>1, <1>k, Simple_M6 DEF FenceBeforeW, Bar
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>. QED BY <2>1, <1>k, Simple_M6 DEF FenceAfterW, Bar
<1>. QED BY <1>1, <1>2

'''
R('''LEMMA Next_M6 ==''', NEW + '''LEMMA Next_M6 ==''')
R('''      BY <3>21, Gw_M6 DEF Sweep, aux
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21''',
'''      BY <3>21, Gw_M6 DEF Sweep, aux
    <3>22. ASSUME NEW s \\in Writers, Touch(s) PROVE M6Goal BY <3>22, Touch_M6
    <3>23. ASSUME NEW s \\in Writers, Honor(s) PROVE M6Goal BY <3>23, Honor_M6
    <3>24. ASSUME NEW s \\in Writers, HonorReader(s) PROVE M6Goal BY <3>24, HonorReader_M6
    <3>25. ASSUME NEW s \\in Writers, RetireByAck(s) PROVE M6Goal BY <3>25, RetireByAck_M6
    <3>26. ASSUME NEW s \\in Writers, Depose(s) PROVE M6Goal BY <3>26, Depose_M6
    <3>27. ASSUME NEW s \\in Writers, Fence(s) PROVE M6Goal BY <3>27, Fence_M6
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27''')
open(p, 'w').write(t); print('ok')
