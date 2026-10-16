#!/usr/bin/env python3
"""Rows 15-17 in M5C: the six new steps' Part C lemmas, their Next_C and
Next_NR cases.  None writes the history, the document, a record or a field the
tree conjuncts read but pc (Depose: scanned -> claimed, both in Bar; Fence:
claimed -> idle with the barrier dropped, or cased kept)."""
p = 'm5c/LeanP1ProofM5C.tla'; t = open(p).read()
def R(old, new, cnt=1):
    global t
    assert t.count(old) == cnt, (old[:90], t.count(old)); t = t.replace(old, new)
def simple(n):
    return f'''LEMMA {n}_C ==
  ASSUME IndC, NEW s \\in Writers, {n}(s), Frame
  PROVE  HistC' /\\ TreeC'
<1>m. IndM5' BY {n}_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\\ w' = [w EXCEPT ![s] = {n}W(s)]
      /\\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF {n}, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \\in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>w. \\A t \\in Writers : w'[t] = IF t = s THEN {n}W(s) ELSE w[t] BY <1>0, <1>1, WriteAny
<1>d. \\A p \\in Paths, x \\in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>s. TC(s)'
  <2>1. w'[s] = {n}W(s) BY <1>w
  <2>2. w[s].pc \\notin Bar => w[s].uploads = {{}} BY DEF IndC, TreeC, TC, UBt
  <2>. QED BY <2>1, <2>2, <1>m, <1>hs, <1>d, TCFields DEF {n}W, Bar
<1>b. TreeC' BY <1>m, <1>hs, <1>w, <1>d, <1>s, TCWriter
<1>. QED BY <1>a, <1>b

'''
NEW = '''------------------------------------------------------------------------------
(* Part C: rows 15-17 -- the answer's steps, deposal and the fence.         *)

''' + ''.join(simple(n) for n in ['Touch', 'Honor', 'HonorReader', 'RetireByAck']) + r'''\* Two trees move: the taker scanned -> claimed (both in the barrier), the
\* deposed holder only marked.
LEMMA Depose_C ==
  ASSUME IndC, NEW t \in Writers, Depose(t), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Depose_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>>
  BY DEF Depose, aux
<1>hs. HStep BY <1>m, <1>u, <1>1, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>1, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>1, <1>hs, DorNKeep DEF IndC
<1>w. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>f. \A u \in Writers : /\ w'[u].local = w[u].local /\ w'[u].baseline = w[u].baseline /\ w'[u].snap = w[u].snap
                         /\ w'[u].uploads = w[u].uploads /\ w'[u].upDone = w[u].upDone
                         /\ w'[u].pc = IF u = t THEN "claimed" ELSE w[u].pc
  BY <1>1, <1>w DEF DeposeW, DeposedW
<1>b. TreeC'
  <2>. SUFFICES ASSUME NEW u \in Writers PROVE TC(u)' BY DEF TreeC
  <2>1. w[u].pc \notin Bar => w[u].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>. QED BY <2>1, <1>1, <1>f, <1>m, <1>hs, <1>d, TCFields DEF Bar
<1>. QED BY <1>a, <1>b

\* Before the CAS the barrier is abandoned (the tree and baseline kept);
\* after it, the retire set dropped.
LEMMA Fence_C ==
  ASSUME IndC, NEW s \in Writers, Fence(s), Frame
  PROVE  HistC' /\ TreeC'
<1>m. IndM5' BY Fence_M5 DEF IndC
<1>u. AncUpdate BY DEF Frame
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>k. UNCHANGED <<minted, base, orig, nextGen, doc, conflicts, udel, acked>> BY DEF Fence, bucket, aux
<1>hs. HStep BY <1>m, <1>u, <1>k, HStepSame DEF IndC
<1>n. nextGen \in Nat BY <1>0 DEF TypeOK
<1>a. HistC' BY <1>k, <1>u, <1>n, AncSame, HCSame, MintedHandles DEF IndC
<1>d. \A p \in Paths, x \in minted : DorN(p, x) => DorN(p, x)' BY <1>k, <1>hs, DorNKeep DEF IndC
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN FenceBeforeW(s) ELSE w[t] BY <1>0, <2>1, WriteAny
  <2>2. w'[s] = FenceBeforeW(s) BY <2>w
  <2>3. \A p \in Paths : TakeShape(s, p) BY <2>2 DEF FenceBeforeW, TakeShape
  <2>4. w'[s].pc \notin Bar /\ w'[s].uploads = {} BY <2>2 DEF FenceBeforeW, Bar
  <2>s. TC(s)' BY <2>3, <2>4, <1>k, <1>m, <1>hs, <1>d, TCTake
  <2>. QED BY <1>a, <2>w, <2>s, <1>m, <1>hs, <1>d, TCWriter
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN FenceAfterW(s) ELSE w[t] BY <1>0, <2>1, WriteAny
  <2>2. w'[s] = FenceAfterW(s) BY <2>w
  <2>3. w[s].pc \notin Bar => w[s].uploads = {} BY DEF IndC, TreeC, TC, UBt
  <2>s. TC(s)' BY <2>2, <2>3, <1>m, <1>hs, <1>d, TCFields DEF FenceAfterW, Bar
  <2>. QED BY <1>a, <2>w, <2>s, <1>m, <1>hs, <1>d, TCWriter
<1>. QED BY <1>1, <1>2

'''
R('''------------------------------------------------------------------------------
(* Part C: G elapses, a reader loads, a writer reaps -- no history, no tree. *)''',
  NEW + '''------------------------------------------------------------------------------
(* Part C: G elapses, a reader loads, a writer reaps -- no history, no tree. *)''')
R('''    <3>21. ASSUME NEW s \\in Writers, NEW h \\in Handles, Sweep(s, h) PROVE HistC' /\\ TreeC' BY <3>21, Sweep_C
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21, <1>m''',
'''    <3>21. ASSUME NEW s \\in Writers, NEW h \\in Handles, Sweep(s, h) PROVE HistC' /\\ TreeC' BY <3>21, Sweep_C
    <3>22. ASSUME NEW s \\in Writers, Touch(s) PROVE HistC' /\\ TreeC' BY <3>22, Touch_C
    <3>23. ASSUME NEW s \\in Writers, Honor(s) PROVE HistC' /\\ TreeC' BY <3>23, Honor_C
    <3>24. ASSUME NEW s \\in Writers, HonorReader(s) PROVE HistC' /\\ TreeC' BY <3>24, HonorReader_C
    <3>25. ASSUME NEW s \\in Writers, RetireByAck(s) PROVE HistC' /\\ TreeC' BY <3>25, RetireByAck_C
    <3>26. ASSUME NEW s \\in Writers, Depose(s) PROVE HistC' /\\ TreeC' BY <3>26, Depose_C
    <3>27. ASSUME NEW s \\in Writers, Fence(s) PROVE HistC' /\\ TreeC' BY <3>27, Fence_C
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27, <1>m''')
nr = ''
for i, n in enumerate(['Touch', 'Honor', 'HonorReader', 'RetireByAck'], 1):
    nr += f'''<1>n{i}. ASSUME NEW s \\in Writers, {n}(s) PROVE NRPre
  <2>1. w' = [w EXCEPT ![s] = {n}W(s)] BY <1>n{i} DEF {n}
  <2>2. {n}W(s).baseline = w[s].baseline BY DEF {n}W
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
'''
nr += r'''<1>n5. ASSUME NEW t \in Writers, Depose(t) PROVE NRPre
  <2>1. holder \in Writers /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)] BY <1>n5 DEF Depose
  <2>2. \A u \in Writers : w'[u].baseline = w[u].baseline BY <2>1, <1>0 DEF TypeOK, DeposeW, DeposedW
  <2>. QED BY <2>2 DEF NRPre
<1>n6. ASSUME NEW s \in Writers, Fence(s) PROVE NRPre
  <2>1. \/ w' = [w EXCEPT ![s] = FenceBeforeW(s)]
        \/ w' = [w EXCEPT ![s] = FenceAfterW(s)]
    BY <1>n6 DEF Fence
  <2>2. FenceBeforeW(s).baseline = w[s].baseline /\ FenceAfterW(s).baseline = w[s].baseline BY DEF FenceBeforeW, FenceAfterW
  <2>. QED BY <2>1, <2>2, <1>0, NRWrite
'''
R('''<1>w. CASE WriterStep /\\ ~(\\E s \\in Writers, h \\in Handles : Sweep(s, h))
  BY <1>w, <1>s1, <1>s2, <1>s3, <1>s4, <1>s5, <1>s6, <1>s7, <1>s8, <1>s9, <1>s10, <1>s11, <1>s12, <1>up, <1>co, <1>sy, <1>rs, <1>fi, <1>ck, <1>r1, <1>r2
     DEF WriterStep''', nr + '''<1>w. CASE WriterStep /\\ ~(\\E s \\in Writers, h \\in Handles : Sweep(s, h))
  BY <1>w, <1>s1, <1>s2, <1>s3, <1>s4, <1>s5, <1>s6, <1>s7, <1>s8, <1>s9, <1>s10, <1>s11, <1>s12, <1>up, <1>co, <1>sy, <1>rs, <1>fi, <1>ck, <1>r1, <1>r2,
     <1>n1, <1>n2, <1>n3, <1>n4, <1>n5, <1>n6
     DEF WriterStep''')
open(p, 'w').write(t); print('ok')
