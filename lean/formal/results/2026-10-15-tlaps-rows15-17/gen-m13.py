#!/usr/bin/env python3
"""Emit LeanP1ProofM13.tla: the narrow's side facts M11 assumes (A1KeepCited,
A1HeldSame), proved inductive with one helper (A3HeldMinted: a held entry is a
minted handle).  M11 reads them in Restart (the restart arm of A1LeftSet takes
no kept path in) and RescopeSecond (an unlinked file is the leftover by handle).

  KeepCited  at "mid" a kept path is cited: RescopeFirst keeps only cited dirty
             paths and uncites the rest; nothing at "mid" moves a baseline (a
             barrier and Sync need another stage; RPullSync too, R5).
  HeldSame   a leftover candidate (dropped, not kept at "mid", uncited at
             "saved", a file and a held entry) holding the held bytes holds the
             held handle: the uncite records the handle the clean file holds;
             after it the file changes only by the agent (Edit: a fresh mint,
             bytes no handle had; Delete: no file) or not at all (Sync and
             RPullSync take owed paths only, and a candidate is dirty).
  HeldMinted the uncite records a baseline (minted); minted only grows.
TLC: m10c KeepCited, HeldSame (worlds N1X, N3X, N6X).
Usage: gen-m13.py OUTDIR"""
import sys, os
OUT = sys.argv[1]

t = []
w = t.append

w(r'''------------------------------ MODULE LeanP1ProofM13 ------------------------------
(* M13 (row 15): the narrow's side facts M11 assumes, proved inductive.
   Generated: results/2026-10-15-tlaps-rows15-17/gen-m13.py (its docstring has
   the argument).                                                           *)
EXTENDS LeanP1ProofM12

A3HeldMinted == \A s \in Writers, p \in Paths : w[s].sHeld[p] \in Opt(minted)
A3Side == A1KeepCited /\ A1HeldSame /\ A3HeldMinted
\* The fields A3Side reads in a tree.
A3F(R, W) == /\ R.sStage = W.sStage /\ R.sKeep = W.sKeep /\ R.sDrop = W.sDrop
             /\ R.sHeld = W.sHeld /\ R.local = W.local /\ R.baseline = W.baseline
\* Content is kept on every minted handle (orig changes only at a fresh copy).
A3CK == \A h \in Opt(minted) : Content(h)' = Content(h)

LEMMA A3CF == ASSUME orig' = orig PROVE A3CK
BY DEF A3CK, Content

LEMMA A3Ty ==
  ASSUME IndC, NEW u \in Writers
  PROVE  /\ w[u] \in Writer
         /\ \A p \in Paths : w[u].local[p] \in Opt(minted) /\ w[u].baseline[p] \in Opt(minted)
         /\ w[u].sDrop \subseteq Paths /\ w[u].sKeep \subseteq Paths
         /\ w[u].sStage \in {"none", "saved", "mid"}
<1>1. w[u] \in Writer /\ \A p \in Paths : w[u].local[p] \in Opt(minted) /\ w[u].baseline[p] \in Opt(minted)
  BY TreeTy DEF IndC
<1>2. w[u].sDrop \subseteq Paths /\ w[u].sKeep \subseteq Paths /\ w[u].sStage \in {"none", "saved", "mid"}
  BY <1>1, WriterFields
<1>. QED BY <1>1, <1>2

\* Every tree keeps the fields A3Side reads.
LEMMA A3Keep ==
  ASSUME IndC, A3Side, minted \subseteq minted', A3CK, \A u \in Writers : A3F(w'[u], w[u])
  PROVE  A3Side'
<1>1. A1KeepCited' BY DEF A3Side, A1KeepCited, A3F
<1>2. A3HeldMinted' BY DEF A3Side, A3HeldMinted, A3F, Opt
<1>3. A1HeldSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths,
                       (A1LeftCand(w[u], p) /\ Content(w[u].local[p]) = Content(w[u].sHeld[p]))'
                PROVE  (w[u].local[p] = w[u].sHeld[p])'
    BY DEF A1HeldSame
  <2>0. A3F(w'[u], w[u]) OBVIOUS
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[p] BY A3Ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].sHeld[p] BY DEF A3Side, A3HeldMinted
  <2>3. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <2>1, <2>2 DEF A3CK
  <2>4. w'[u].local[p] = h1 /\ w'[u].sHeld[p] = h2 BY <2>0, <2>1, <2>2 DEF A3F
  <2>5. A1LeftCand(w[u], p) BY <2>0 DEF A1LeftCand, A3F
  <2>6. Content(w[u].local[p]) = Content(w[u].sHeld[p]) BY <2>1, <2>2, <2>3, <2>4
  <2>. QED BY <2>4, <2>5, <2>6, <2>1, <2>2 DEF A3Side, A1HeldSame
<1>. QED BY <1>1, <1>2, <1>3 DEF A3Side

\* One tree written, A3F.
LEMMA A3Keep1 ==
  ASSUME IndC, A3Side, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R], A3F(R, w[s]),
         minted \subseteq minted', A3CK
  PROVE  A3Side'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. \A u \in Writers : A3F(w'[u], w[u]) BY <1>0 DEF TypeOK, A3F
<1>. QED BY <1>1, A3Keep

\* No tree written.
LEMMA A3Quiet ==
  ASSUME IndC, A3Side, w' = w, minted \subseteq minted', A3CK
  PROVE  A3Side'
<1>1. \A u \in Writers : A3F(w'[u], w[u]) BY DEF A3F
<1>. QED BY <1>1, A3Keep

\* One tree written that ends (or stays out of) a narrow: stage "none" after,
\* its held entries kept.
LEMMA A3None ==
  ASSUME IndC, A3Side, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         R.sStage = "none", R.sHeld = w[s].sHeld, minted \subseteq minted', A3CK
  PROVE  A3Side'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>1. A1KeepCited' BY <1>w DEF A3Side, A1KeepCited
<1>2. A3HeldMinted' BY <1>w DEF A3Side, A3HeldMinted, Opt
<1>3. A1HeldSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths,
                       (A1LeftCand(w[u], p) /\ Content(w[u].local[p]) = Content(w[u].sHeld[p]))'
                PROVE  (w[u].local[p] = w[u].sHeld[p])'
    BY DEF A1HeldSame
  <2>0. u # s BY <1>w DEF A1LeftCand
  <2>a. w'[u] = w[u] BY <1>w, <2>0
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[p] BY A3Ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].sHeld[p] BY DEF A3Side, A3HeldMinted
  <2>3. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <2>1, <2>2 DEF A3CK
  <2>5. A1LeftCand(w[u], p) BY <2>a
  <2>6. Content(w[u].local[p]) = Content(w[u].sHeld[p]) BY <2>a, <2>1, <2>2, <2>3
  <2>. QED BY <2>a, <2>5, <2>6, <2>1, <2>2 DEF A3Side, A1HeldSame
<1>. QED BY <1>1, <1>2, <1>3 DEF A3Side

------------------------------------------------------------------------------
(* The gateway and the quiet steps write no tree.                           *)

LEMMA GPut_A3 ==
  ASSUME IndC, IndC', A3Side, NEW p \in Paths, GPut(p), Frame
  PROVE  A3Side'
<1>1. w' = w /\ minted \subseteq minted' /\ orig' = orig BY DEF GPut, aux
<1>. QED BY <1>1, A3CF, A3Quiet
''')

for g, sig, bind in [('GCas', 'GCas(p)', 'NEW p \\in Paths'), ('GDelete', 'GDelete(p)', 'NEW p \\in Paths'),
                     ('GRename', 'GRename(p, q)', 'NEW p \\in Paths, NEW q \\in Paths'),
                     ('GRenameFinish', 'GRenameFinish', None),
                     ('Sweep', 'Sweep(s, h)', 'NEW s \\in Writers, NEW h \\in Handles')]:
    b = f'{bind}, ' if bind else ''
    w(f'''
LEMMA {g}_A3 ==
  ASSUME IndC, IndC', A3Side, {b}{sig}, Frame
  PROVE  A3Side'
<1>1. w' = w /\\ minted \\subseteq minted' /\\ orig' = orig BY DEF {g}, aux, bucket
<1>. QED BY <1>1, A3CF, A3Quiet
''')
for g, sig, bind in [('Age', 'Age', None), ('RLoad', 'RLoad', None),
                     ('Reap', 'Reap(s, h)', 'NEW s \\in Writers, NEW h \\in Handles')]:
    b = f'{bind}, ' if bind else ''
    w(f'''
LEMMA {g}_A3 == ASSUME IndC, A3Side, {b}{sig} PROVE A3Side'
<1>1. w' = w /\\ minted \\subseteq minted' /\\ orig' = orig BY DEF {g}, aux
<1>. QED BY <1>1, A3CF, A3Quiet
''')

w(r'''
------------------------------------------------------------------------------
(* Writer steps that keep the narrow's fields and the tree.                  *)
''')

def keep(name, act, rec, params='NEW s \\in Writers', actargs='(s)', recargs='(s)', unch='bucket, aux', pick=None, pre=''):
    hdr = f'''
LEMMA {name}_A3 ==
  ASSUME IndC, IndC', A3Side, {params}, {act}{actargs}, Frame
  PROVE  A3Side'
{pre}'''
    if pick:
        hdr += pick
        wline = f"<1>1. minted \\subseteq minted' /\\ orig' = orig BY DEF {act}, {unch}\n"
    else:
        wline = f"<1>1. w' = [w EXCEPT ![s] = {rec}{recargs}] /\\ minted \\subseteq minted' /\\ orig' = orig BY DEF {act}, {unch}\n"
    return hdr + wline + f'''<1>2. A3F({rec}{recargs}, w[s]) BY DEF {rec}, A3F
<1>. QED BY <1>1, <1>2, {'<1>w, ' if pick else ''}A3CF, A3Keep1
'''

for n in ['Touch', 'Honor', 'HonorReader', 'RetireByAck', 'Skip', 'PullOnly', 'RPullRead']:
    w(keep(n, n, n + 'W'))
w(keep('Claim', 'Claim', 'ClaimW'))
w(keep('Verify', 'Verify', 'VerifyW'))
w(keep('Install', 'Install', 'InstallW', unch='aux'))
w(keep('Collect', 'Collect', 'CollectW', unch='aux', pre='<1>r. RetireAge BY ShippedShape DEF Shipped\n').replace(
    "BY DEF Collect, aux\n", "BY <1>r DEF Collect, aux\n"))
w(keep('Scan', 'Scan', 'ScanW', recargs='(s, dels)',
       pick='<1>w. PICK dels \\in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) :\n'
            '        w\' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan\n'))

w(r'''
LEMMA Fence_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, Fence(s), Frame
  PROVE  A3Side'
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF Fence, bucket, aux
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
  <2>2. A3F(FenceBeforeW(s), w[s]) BY DEF FenceBeforeW, A3F
  <2>. QED BY <2>1, <2>2, <1>u, A3CF, A3Keep1
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
  <2>2. A3F(FenceAfterW(s), w[s]) BY DEF FenceAfterW, A3F
  <2>. QED BY <2>1, <2>2, <1>u, A3CF, A3Keep1
<1>. QED BY <1>1, <1>2

\* Depose writes two trees, the deposed one and the holder's; both keep the
\* narrow's fields and the tree.
LEMMA Depose_A3 ==
  ASSUME IndC, IndC', A3Side, NEW t \in Writers, Depose(t), Frame
  PROVE  A3Side'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ minted \subseteq minted' /\ orig' = orig
  BY DEF Depose, bucket, aux
<1>2. A3F(DeposeW(t), w[t]) /\ A3F(DeposedW(holder), w[holder]) BY <1>1 DEF DeposeW, DeposedW, A3F
<1>3. \A u \in Writers : A3F(w'[u], w[u]) BY <1>0, <1>1, <1>2 DEF TypeOK, A3F
<1>. QED BY <1>1, <1>3, A3CF, A3Keep

\* Upload: the first arm keeps the tree; the copy arm moves local and snap to a
\* fresh copy c (orig[c] = the bytes'), outside any narrow (R1: pc scanned).
LEMMA Upload_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  A3Side'
<1>k. w[s].pc = "scanned" BY DEF Upload
<1>st. w[s].sStage = "none" BY <1>k DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ minted \subseteq minted' /\ orig' = orig BY <1>1 DEF Upload
  <2>2. A3F(UploadW(s, p), w[s]) BY DEF UploadW, A3F
  <2>. QED BY <2>1, <2>2, A3CF, A3Keep1
<1>2. CASE w[s].snap[p] \in upped
  <2>y. w[s].snap[p] \in minted /\ copies \in Nat /\ copies < MaxCopies
    BY <1>2, TreeTy DEF Upload, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
  <2>c. PICK c \in Handles : c = <<p, MaxMint + copies + 1>>
    BY <2>y, MaxMintNat, MaxCopiesNat DEF Handles, Gens, Seed
  <2>1. /\ minted' = minted \cup {c} /\ orig' = [orig EXCEPT ![c] = Content(w[s].snap[p])]
        /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
    BY <1>2, <2>c DEF Upload
  <2>4. c \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
      BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>2. nextGen <= MaxMint + 1 BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>3. nextGen \in Nat BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
    <3>. QED BY <3>1, <3>2, <3>3, <2>c, <2>y, MaxMintNat DEF Gen
  <2>ty. orig \in [Handles -> Opt(Handles)] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
  <2>ck. A3CK
    <3>. SUFFICES ASSUME NEW h \in Opt(minted) PROVE Content(h)' = Content(h) BY DEF A3CK
    <3>1. h # c BY <2>4, <2>c, NilHandle DEF Opt
    <3>2. h \in Handles \cup {Nil} BY TreeTy DEF IndC, Opt
    <3>3. CASE h = Nil BY <3>3 DEF Content
    <3>4. CASE h # Nil
      <4>1. h \in Handles BY <3>2, <3>4
      <4>2. orig'[h] = orig[h] BY <4>1, <3>1, <2>1, <2>ty
      <4>. QED BY <4>2, <3>4 DEF Content
    <3>. QED BY <3>3, <3>4
  <2>2. UploadCopyW(s, p, c).sStage = "none" /\ UploadCopyW(s, p, c).sHeld = w[s].sHeld BY <1>st DEF UploadCopyW
  <2>. QED BY <2>1, <2>2, <2>ck, A3None
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* Writer steps outside a narrow (stage "none" after, held entries kept).    *)
''')

def none_step(name, act, rec, recargs, wpick, guard, unch, extra=''):
    return f'''
LEMMA {name}_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \\in Writers, {act}(s), Frame
  PROVE  A3Side'
{wpick}<1>u. minted \\subseteq minted' /\\ orig' = orig BY DEF {act}, {unch}
{guard}<1>2. {rec}{recargs}.sStage = "none" /\\ {rec}{recargs}.sHeld = w[s].sHeld BY <1>st DEF {rec}
<1>. QED BY <1>w, <1>u, <1>2, A3CF, A3None
'''
R1 = 'DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1'
w(r'''
LEMMA Consume_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, Consume(s), Frame
  PROVE  A3Side'
<1>c. PICK ck \in CarryClocks(s) :
        IF CheapPath(s) THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
        ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF Consume
<1>st. w[s].sStage = "none" BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>c
  <2>2. ConsumeCheapW(s, ck).sStage = "none" /\ ConsumeCheapW(s, ck).sHeld = w[s].sHeld BY <1>st DEF ConsumeCheapW
  <2>. QED BY <2>1, <2>2, <1>u, A3CF, A3None
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>c
  <2>2. ConsumeW(s, fail, ck).sStage = "none" /\ ConsumeW(s, fail, ck).sHeld = w[s].sHeld BY <1>st DEF ConsumeW
  <2>. QED BY <2>1, <2>2, <1>u, A3CF, A3None
<1>. QED BY <1>1, <1>2
''')
w(none_step('Finish', 'Finish', 'FinishW', '(s)', "<1>w. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish\n",
    f'<1>st. w[s].sStage = "none" BY {R1}, Finish\n', 'aux'))
w(r'''
LEMMA Checkout_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, Checkout(s), Frame
  PROVE  A3Side'
<1>i. w[s] = WriterInit BY DEF Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
<1>T. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF Checkout, bucket, aux
<1>2. CheckoutW(s, T).sStage = "none" /\ CheckoutW(s, T).sHeld = w[s].sHeld BY <1>i DEF CheckoutW, WriterInit
<1>. QED BY <1>T, <1>u, <1>2, A3CF, A3None

LEMMA RescopeSecond_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  A3Side'
<1>w. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF RescopeSecond, bucket
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ww. \A u \in Writers : w'[u] = IF u = s THEN RescopeSecondW(s, wfail) ELSE w[u] BY <1>w, <1>0 DEF TypeOK
<1>2. RescopeSecondW(s, wfail).sStage = "none" BY DEF RescopeSecondW
<1>h. \A q \in Paths : RescopeSecondW(s, wfail).sHeld[q] \in Opt(minted) BY DEF RescopeSecondW, Opt
\* stage "none" after: s's conjuncts are vacuous but HeldMinted (sHeld cleared)
<1>1. A1KeepCited' BY <1>ww, <1>2 DEF A3Side, A1KeepCited
<1>3. A3HeldMinted' BY <1>ww, <1>h, <1>u DEF A3Side, A3HeldMinted, Opt
<1>4. A1HeldSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths,
                       (A1LeftCand(w[u], p) /\ Content(w[u].local[p]) = Content(w[u].sHeld[p]))'
                PROVE  (w[u].local[p] = w[u].sHeld[p])'
    BY DEF A1HeldSame
  <2>0. u # s BY <1>ww, <1>2 DEF A1LeftCand
  <2>a. w'[u] = w[u] BY <1>ww, <2>0
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[p] BY A3Ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].sHeld[p] BY DEF A3Side, A3HeldMinted
  <2>3. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <2>1, <2>2, <1>u, A3CF DEF A3CK
  <2>. QED BY <2>a, <2>1, <2>2, <2>3 DEF A3Side, A1HeldSame
<1>. QED BY <1>1, <1>3, <1>4 DEF A3Side

------------------------------------------------------------------------------
(* The narrow's own steps, and the steps that move a tree inside one.       *)

\* RescopeBegin: "saved", nothing held yet.
LEMMA RescopeBegin_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  A3Side'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>T. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF RescopeBegin, bucket
<1>ww. \A u \in Writers : w'[u] = IF u = s THEN RescopeBeginW(s, T) ELSE w[u] BY <1>T, <1>0 DEF TypeOK
<1>2. RescopeBeginW(s, T).sStage = "saved" /\ RescopeBeginW(s, T).sHeld = [q \in Paths |-> Nil]
  BY DEF RescopeBeginW
<1>1. A1KeepCited' BY <1>ww, <1>2 DEF A3Side, A1KeepCited
<1>3. A3HeldMinted' BY <1>ww, <1>2, <1>u DEF A3Side, A3HeldMinted, Opt
<1>4. A1HeldSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths,
                       (A1LeftCand(w[u], p) /\ Content(w[u].local[p]) = Content(w[u].sHeld[p]))'
                PROVE  (w[u].local[p] = w[u].sHeld[p])'
    BY DEF A1HeldSame
  <2>0. u # s BY <1>ww, <1>2 DEF A1LeftCand
  <2>a. w'[u] = w[u] BY <1>ww, <2>0
  <2>1. PICK h1 \in Opt(minted) : h1 = w[u].local[p] BY A3Ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[u].sHeld[p] BY DEF A3Side, A3HeldMinted
  <2>3. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <2>1, <2>2, <1>u, A3CF DEF A3CK
  <2>. QED BY <2>a, <2>1, <2>2, <2>3 DEF A3Side, A1HeldSame
<1>. QED BY <1>1, <1>3, <1>4 DEF A3Side

\* The rest write one tree s; every other tree is kept.  What each proves for s.
A3For(s) ==
  /\ (\A p \in Paths : w[s].sStage = "mid" /\ p \in w[s].sKeep => w[s].baseline[p] # Nil)'
  /\ (\A p \in Paths : w[s].sHeld[p] \in Opt(minted))'
  /\ (\A p \in Paths : A1LeftCand(w[s], p) /\ Content(w[s].local[p]) = Content(w[s].sHeld[p]) => w[s].local[p] = w[s].sHeld[p])'
LEMMA A3One ==
  ASSUME IndC, A3Side, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R], minted \subseteq minted', A3CK,
         A3For(s)
  PROVE  A3Side'
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>1. A1KeepCited' BY <1>w DEF A3For, A3Side, A1KeepCited
<1>2. A3HeldMinted' BY <1>w DEF A3For, A3Side, A3HeldMinted, Opt
<1>3. A1HeldSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths,
                       (A1LeftCand(w[u], p) /\ Content(w[u].local[p]) = Content(w[u].sHeld[p]))'
                PROVE  (w[u].local[p] = w[u].sHeld[p])'
    BY DEF A1HeldSame
  <2>1. CASE u = s BY <2>1 DEF A3For
  <2>2. CASE u # s
    <3>a. w'[u] = w[u] BY <1>w, <2>2
    <3>1. PICK h1 \in Opt(minted) : h1 = w[u].local[p] BY A3Ty
    <3>2. PICK h2 \in Opt(minted) : h2 = w[u].sHeld[p] BY DEF A3Side, A3HeldMinted
    <3>3. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <3>1, <3>2 DEF A3CK
    <3>. QED BY <3>a, <3>1, <3>2, <3>3 DEF A3Side, A1HeldSame
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, <1>3 DEF A3Side

\* Restart: "mid" -> "saved", sKeep cleared.  A saved candidate was a mid one:
\* uncited, so not kept (KeepCited).
LEMMA Restart_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, Restart(s), Frame
  PROVE  A3Side'
<1>w. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF Restart
<1>ck. A3CK BY <1>u, A3CF
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>s. w'[s] = RestartW(s) BY <1>w, <1>0 DEF TypeOK
<1>f. /\ RestartW(s).sDrop = w[s].sDrop /\ RestartW(s).sHeld = w[s].sHeld /\ RestartW(s).local = w[s].local
      /\ RestartW(s).baseline = w[s].baseline /\ RestartW(s).sKeep = {}
      /\ RestartW(s).sStage = IF w[s].sStage = "none" THEN "none" ELSE "saved"
  BY DEF RestartW
<1>t. w[s].sStage \in {"none", "saved", "mid"} BY A3Ty
<1>1. A3For(s)
  <2>1. (\A p \in Paths : w[s].sStage = "mid" /\ p \in w[s].sKeep => w[s].baseline[p] # Nil)' BY <1>s, <1>f
  <2>2. (\A p \in Paths : w[s].sHeld[p] \in Opt(minted))' BY <1>s, <1>f, <1>u DEF A3Side, A3HeldMinted, Opt
  <2>3. (\A p \in Paths : A1LeftCand(w[s], p) /\ Content(w[s].local[p]) = Content(w[s].sHeld[p]) => w[s].local[p] = w[s].sHeld[p])'
    <3>. SUFFICES ASSUME NEW p \in Paths,
                         (A1LeftCand(w[s], p) /\ Content(w[s].local[p]) = Content(w[s].sHeld[p]))'
                  PROVE  (w[s].local[p] = w[s].sHeld[p])'
      OBVIOUS
    <3>1. A1LeftCand(w[s], p)
      <4>1. w[s].sStage = "mid" => p \notin w[s].sKeep BY <1>s, <1>f DEF A1LeftCand, A3Side, A1KeepCited
      <4>. QED BY <4>1, <1>s, <1>f, <1>t DEF A1LeftCand
    <3>2. PICK h1 \in Opt(minted) : h1 = w[s].local[p] BY A3Ty
    <3>3. PICK h2 \in Opt(minted) : h2 = w[s].sHeld[p] BY DEF A3Side, A3HeldMinted
    <3>4. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <3>2, <3>3, <1>ck DEF A3CK
    <3>5. Content(w[s].local[p]) = Content(w[s].sHeld[p]) BY <1>s, <1>f, <3>2, <3>3, <3>4
    <3>. QED BY <3>1, <3>5, <1>s, <1>f DEF A3Side, A1HeldSame
  <2>. QED BY <2>1, <2>2, <2>3 DEF A3For
<1>. QED BY <1>w, <1>u, <1>ck, <1>1, A3One

\* RescopeFirst: "saved" -> "mid".  Kept: cited (KeepSet).  Uncited now: the
\* held entry is the clean file's handle, or (a replay) the one recorded before.
LEMMA RescopeFirst_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  A3Side'
<1>sh. RescopeUnciteFirst /\ RescopeKeepsDirty BY ShippedShape DEF Shipped
<1>w. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF RescopeFirst, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>s. w'[s] = RescopeFirstW(s) BY <1>w, <1>0 DEF TypeOK
<1>gs. w[s].sStage = "saved" BY DEF RescopeFirst
<1>f. /\ RescopeFirstW(s).sStage = "mid" /\ RescopeFirstW(s).sDrop = w[s].sDrop
      /\ RescopeFirstW(s).sKeep = KeepSet(s) /\ RescopeFirstW(s).local = w[s].local
      /\ RescopeFirstW(s).baseline = [q \in Paths |-> IF q \in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[q]]
      /\ RescopeFirstW(s).sHeld = [q \in Paths |-> IF q \in RescopeFirstDd(s) /\ w[s].baseline[q] # Nil
                                                 THEN w[s].baseline[q] ELSE w[s].sHeld[q]]
  BY <1>sh DEF RescopeFirstW
<1>ks. KeepSet(s) = {q \in w[s].sDrop : w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q]} BY <1>sh DEF KeepSet
<1>ty. \A q \in Paths : w[s].local[q] \in Opt(minted) /\ w[s].baseline[q] \in Opt(minted) BY A3Ty
<1>1. A3For(s)
  <2>1. (\A p \in Paths : w[s].sStage = "mid" /\ p \in w[s].sKeep => w[s].baseline[p] # Nil)'
    BY <1>s, <1>f, <1>ks DEF RescopeFirstDd
  <2>2. (\A p \in Paths : w[s].sHeld[p] \in Opt(minted))' BY <1>s, <1>f, <1>ty, <1>u DEF A3Side, A3HeldMinted, Opt
  <2>3. (\A p \in Paths : A1LeftCand(w[s], p) /\ Content(w[s].local[p]) = Content(w[s].sHeld[p]) => w[s].local[p] = w[s].sHeld[p])'
    <3>. SUFFICES ASSUME NEW p \in Paths,
                         (A1LeftCand(w[s], p) /\ Content(w[s].local[p]) = Content(w[s].sHeld[p]))'
                  PROVE  (w[s].local[p] = w[s].sHeld[p])'
      OBVIOUS
    <3>0. p \in RescopeFirstDd(s) BY <1>s, <1>f DEF A1LeftCand, RescopeFirstDd
    <3>1. CASE w[s].baseline[p] # Nil
      \* not kept, so clean: the file is the baseline the uncite records
      <4>1. w[s].local[p] = w[s].baseline[p] BY <3>0, <3>1, <1>ks DEF RescopeFirstDd
      <4>. QED BY <4>1, <3>0, <3>1, <1>s, <1>f
    <3>2. CASE w[s].baseline[p] = Nil
      \* a replay: the record and the file are kept, and it was a saved candidate
      <4>1. w'[s].sHeld[p] = w[s].sHeld[p] /\ w'[s].local[p] = w[s].local[p] BY <3>2, <1>s, <1>f
      <4>2. A1LeftCand(w[s], p) BY <3>0, <3>2, <4>1, <1>s, <1>f, <1>gs DEF A1LeftCand, RescopeFirstDd
      <4>3. PICK h1 \in Opt(minted) : h1 = w[s].local[p] BY <1>ty
      <4>4. PICK h2 \in Opt(minted) : h2 = w[s].sHeld[p] BY DEF A3Side, A3HeldMinted
      <4>5. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <4>3, <4>4, <1>ck DEF A3CK
      <4>6. Content(w[s].local[p]) = Content(w[s].sHeld[p]) BY <4>1, <4>3, <4>4, <4>5
      <4>. QED BY <4>1, <4>2, <4>6 DEF A3Side, A1HeldSame
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2, <2>3 DEF A3For
<1>. QED BY <1>w, <1>u, <1>ck, <1>1, A3One

\* Sync and RPullSync take owed and converged paths, each clean after (a
\* candidate is an uncited file); the narrow's fields are kept.
''')

for L, W, act, g in [('Sync', 'SyncW', 'Sync', 'w[s].sStage # "mid" BY DEF Sync'),
                     ('RPullSync', 'RPullSyncW', 'RPullSync',
                      'w[s].sStage # "mid" BY DEF RPullSync, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R5')]:
    w(f'''
LEMMA {L}_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \\in Writers, {act}(s), Frame
  PROVE  A3Side'
<1>w. PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = {W}(s, fail)] BY DEF {act}
<1>u. minted \\subseteq minted' /\\ orig' = orig BY DEF {act}, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>s. w'[s] = {W}(s, fail) BY <1>w, <1>0 DEF TypeOK
<1>g. {g}
<1>f. /\\ {W}(s, fail).sStage = w[s].sStage /\\ {W}(s, fail).sDrop = w[s].sDrop
      /\\ {W}(s, fail).sKeep = w[s].sKeep /\\ {W}(s, fail).sHeld = w[s].sHeld
      /\\ {W}(s, fail).local = [p \\in Paths |-> IF p \\in SyncTaken(s, fail) THEN doc[p] ELSE w[s].local[p]]
      /\\ {W}(s, fail).baseline = SyncBl(s, fail)
  BY DEF {W}
<1>1. A3For(s)
  <2>1. (\\A p \\in Paths : w[s].sStage = "mid" /\\ p \\in w[s].sKeep => w[s].baseline[p] # Nil)' BY <1>s, <1>f, <1>g
  <2>2. (\\A p \\in Paths : w[s].sHeld[p] \\in Opt(minted))' BY <1>s, <1>f, <1>u DEF A3Side, A3HeldMinted, Opt
  <2>3. (\\A p \\in Paths : A1LeftCand(w[s], p) /\\ Content(w[s].local[p]) = Content(w[s].sHeld[p]) => w[s].local[p] = w[s].sHeld[p])'
    <3>. SUFFICES ASSUME NEW p \\in Paths,
                         (A1LeftCand(w[s], p) /\\ Content(w[s].local[p]) = Content(w[s].sHeld[p]))'
                  PROVE  (w[s].local[p] = w[s].sHeld[p])'
      OBVIOUS
    \\* a taken path is clean after; a candidate at "saved" is uncited with a file
    <3>1. p \\notin SyncTaken(s, fail) BY <1>s, <1>f, <1>g DEF A1LeftCand, SyncBl
    <3>2. w'[s].local[p] = w[s].local[p] /\\ w'[s].baseline[p] = w[s].baseline[p] /\\ w'[s].sHeld[p] = w[s].sHeld[p]
      BY <3>1, <1>s, <1>f DEF SyncBl
    <3>3. A1LeftCand(w[s], p) BY <3>2, <1>s, <1>f DEF A1LeftCand
    <3>4. PICK h1 \\in Opt(minted) : h1 = w[s].local[p] BY A3Ty
    <3>5. PICK h2 \\in Opt(minted) : h2 = w[s].sHeld[p] BY DEF A3Side, A3HeldMinted
    <3>6. Content(h1)' = Content(h1) /\\ Content(h2)' = Content(h2) BY <3>4, <3>5, <1>ck DEF A3CK
    <3>7. Content(w[s].local[p]) = Content(w[s].sHeld[p]) BY <3>2, <3>4, <3>5, <3>6
    <3>. QED BY <3>2, <3>3, <3>7 DEF A3Side, A1HeldSame
  <2>. QED BY <2>1, <2>2, <2>3 DEF A3For
<1>. QED BY <1>w, <1>u, <1>ck, <1>1, A3One
''')

w(r'''
\* Delete: no file at p after (not a candidate); the rest is kept.
LEMMA Delete_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  A3Side'
<1>w. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete
<1>u. minted \subseteq minted' /\ orig' = orig BY DEF Delete, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>s. w'[s] = DeleteW(s, p) BY <1>w, <1>0 DEF TypeOK
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY A3Ty, WriterFields
<1>f. /\ DeleteW(s, p).sStage = w[s].sStage /\ DeleteW(s, p).sDrop = w[s].sDrop
      /\ DeleteW(s, p).sKeep = w[s].sKeep /\ DeleteW(s, p).sHeld = w[s].sHeld
      /\ DeleteW(s, p).baseline = w[s].baseline
      /\ DeleteW(s, p).local[p] = Nil /\ \A q \in Paths \ {p} : DeleteW(s, p).local[q] = w[s].local[q]
  BY <1>lt DEF DeleteW
<1>1. A3For(s)
  <2>1. (\A q \in Paths : w[s].sStage = "mid" /\ q \in w[s].sKeep => w[s].baseline[q] # Nil)' BY <1>s, <1>f DEF A3Side, A1KeepCited
  <2>2. (\A q \in Paths : w[s].sHeld[q] \in Opt(minted))' BY <1>s, <1>f, <1>u DEF A3Side, A3HeldMinted, Opt
  <2>3. (\A q \in Paths : A1LeftCand(w[s], q) /\ Content(w[s].local[q]) = Content(w[s].sHeld[q]) => w[s].local[q] = w[s].sHeld[q])'
    <3>. SUFFICES ASSUME NEW q \in Paths,
                         (A1LeftCand(w[s], q) /\ Content(w[s].local[q]) = Content(w[s].sHeld[q]))'
                  PROVE  (w[s].local[q] = w[s].sHeld[q])'
      OBVIOUS
    <3>1. q # p BY <1>s, <1>f DEF A1LeftCand
    <3>2. w'[s].local[q] = w[s].local[q] /\ w'[s].sHeld[q] = w[s].sHeld[q] BY <3>1, <1>s, <1>f
    <3>3. A1LeftCand(w[s], q) BY <3>2, <1>s, <1>f DEF A1LeftCand
    <3>4. PICK h1 \in Opt(minted) : h1 = w[s].local[q] BY A3Ty
    <3>5. PICK h2 \in Opt(minted) : h2 = w[s].sHeld[q] BY DEF A3Side, A3HeldMinted
    <3>6. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) BY <3>4, <3>5, <1>ck DEF A3CK
    <3>7. Content(w[s].local[q]) = Content(w[s].sHeld[q]) BY <3>2, <3>4, <3>5, <3>6
    <3>. QED BY <3>2, <3>3, <3>7 DEF A3Side, A1HeldSame
  <2>. QED BY <2>1, <2>2, <2>3 DEF A3For
<1>. QED BY <1>w, <1>u, <1>ck, <1>1, A3One

\* Edit: a fresh mint at p (no handle's bytes: orig[hh] = Nil, hh not minted,
\* and a held entry's bytes are a minted handle's), so not the held bytes.
LEMMA Edit_A3 ==
  ASSUME IndC, IndC', A3Side, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  A3Side'
<1>g. nextGen \in Nat /\ nextGen > Seed BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>h. PICK hh \in Handles : hh = <<p, nextGen>> BY <1>g, MaxMintNat, MaxCopiesNat DEF Edit, Handles, Gens, Seed
<1>w. /\ w' = [w EXCEPT ![s] = EditW(s, p)] /\ orig' = orig /\ minted' = minted \cup {hh}
  BY <1>h DEF Edit, aux
<1>u. minted \subseteq minted' BY <1>w
<1>ck. A3CK BY <1>w, A3CF
<1>4. hh \notin minted
  <2>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>2. nextGen <= MaxMint BY DEF Edit
  <2>. QED BY <2>1, <2>2, <1>g, <1>h, MaxMintNat DEF Gen
<1>6. orig[hh] = Nil BY <1>4 DEF IndC, IndM5, M5, Hist, HistNew
\* the bytes of a minted handle are a minted handle (an original or its copy's original)
<1>cm. \A m \in minted : Content(m) \in minted BY ContentMinted DEF IndC
<1>0. TypeOK BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>s. w'[s] = EditW(s, p) BY <1>w, <1>0 DEF TypeOK
<1>lt. w[s].local \in [Paths -> Opt(Handles)] BY A3Ty, WriterFields
<1>f. /\ EditW(s, p).sStage = w[s].sStage /\ EditW(s, p).sDrop = w[s].sDrop
      /\ EditW(s, p).sKeep = w[s].sKeep /\ EditW(s, p).sHeld = w[s].sHeld
      /\ EditW(s, p).baseline = w[s].baseline
      /\ EditW(s, p).local[p] = hh /\ \A q \in Paths \ {p} : EditW(s, p).local[q] = w[s].local[q]
  BY <1>lt, <1>h DEF EditW
<1>1. A3For(s)
  <2>1. (\A q \in Paths : w[s].sStage = "mid" /\ q \in w[s].sKeep => w[s].baseline[q] # Nil)' BY <1>s, <1>f DEF A3Side, A1KeepCited
  <2>2. (\A q \in Paths : w[s].sHeld[q] \in Opt(minted))' BY <1>s, <1>f, <1>u DEF A3Side, A3HeldMinted, Opt
  <2>3. (\A q \in Paths : A1LeftCand(w[s], q) /\ Content(w[s].local[q]) = Content(w[s].sHeld[q]) => w[s].local[q] = w[s].sHeld[q])'
    <3>. SUFFICES ASSUME NEW q \in Paths,
                         (A1LeftCand(w[s], q) /\ Content(w[s].local[q]) = Content(w[s].sHeld[q]))'
                  PROVE  (w[s].local[q] = w[s].sHeld[q])'
      OBVIOUS
    <3>5. PICK h2 \in Opt(minted) : h2 = w[s].sHeld[q] BY DEF A3Side, A3HeldMinted
    <3>6. Content(h2)' = Content(h2) BY <3>5, <1>ck DEF A3CK
    <3>1. CASE q = p
      \* hh's bytes are hh (orig[hh] = Nil), no minted handle's
      <4>1. Content(hh)' = hh BY <1>w, <1>6 DEF Content
      <4>2. h2 # Nil BY <3>1, <3>5, <1>s, <1>f DEF A1LeftCand
      <4>3. Content(h2) \in minted BY <4>2, <3>5, <1>cm DEF Opt
      <4>. QED BY <3>1, <4>1, <4>3, <3>5, <3>6, <1>4, <1>s, <1>f
    <3>2. CASE q # p
      <4>2. w'[s].local[q] = w[s].local[q] /\ w'[s].sHeld[q] = w[s].sHeld[q] BY <3>2, <1>s, <1>f
      <4>3. A1LeftCand(w[s], q) BY <4>2, <1>s, <1>f DEF A1LeftCand
      <4>4. PICK h1 \in Opt(minted) : h1 = w[s].local[q] BY A3Ty
      <4>6. Content(h1)' = Content(h1) BY <4>4, <1>ck DEF A3CK
      <4>7. Content(w[s].local[q]) = Content(w[s].sHeld[q]) BY <4>2, <4>4, <3>5, <3>6, <4>6
      <4>. QED BY <4>2, <4>3, <4>7 DEF A3Side, A1HeldSame
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>1, <2>2, <2>3 DEF A3For
<1>. QED BY <1>w, <1>u, <1>ck, <1>1, A3One

------------------------------------------------------------------------------
LEMMA Init_A3 == Init => A3Side
<1>. SUFFICES ASSUME Init PROVE A3Side OBVIOUS
<1>1. \A u \in Writers : w[u] = WriterInit BY DEF Init
<1>2. \A u \in Writers : w[u].sStage = "none" /\ w[u].sHeld = [p \in Paths |-> Nil] BY <1>1 DEF WriterInit
<1>. QED BY <1>2 DEF A3Side, A1KeepCited, A1HeldSame, A1LeftCand, A3HeldMinted, Opt

LEMMA Next_A3 == IndC /\ IndC' /\ A3Side /\ Next => A3Side'
<1>. SUFFICES ASSUME IndC, IndC', A3Side, Next PROVE A3Side' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE A3Side' BY <3>1, GPut_A3
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE A3Side' BY <3>2, GCas_A3
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE A3Side' BY <3>3, GDelete_A3
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE A3Side' BY <3>4, GRename_A3
    <3>5. ASSUME GRenameFinish PROVE A3Side' BY <3>5, GRenameFinish_A3
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE A3Side' BY <3>1, Checkout_A3
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE A3Side' BY <3>2, Consume_A3
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE A3Side' BY <3>3, Scan_A3
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE A3Side' BY <3>4, Skip_A3
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE A3Side' BY <3>5, PullOnly_A3
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE A3Side' BY <3>6, Claim_A3
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE A3Side' BY <3>7, Verify_A3
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE A3Side' BY <3>8, Install_A3
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE A3Side' BY <3>9, Collect_A3
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE A3Side' BY <3>10, Finish_A3
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE A3Side' BY <3>11, Restart_A3
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE A3Side' BY <3>12, Sync_A3
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE A3Side' BY <3>13, RescopeBegin_A3
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE A3Side' BY <3>14, RescopeFirst_A3
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE A3Side' BY <3>15, RescopeSecond_A3
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE A3Side' BY <3>16, RPullRead_A3
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE A3Side' BY <3>17, RPullSync_A3
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE A3Side' BY <3>18, Edit_A3
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE A3Side' BY <3>19, Delete_A3
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE A3Side' BY <3>20, Upload_A3
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE A3Side' BY <3>21, Sweep_A3
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE A3Side' BY <3>22, Touch_A3
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE A3Side' BY <3>23, Honor_A3
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE A3Side' BY <3>24, HonorReader_A3
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE A3Side' BY <3>25, RetireByAck_A3
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE A3Side' BY <3>26, Depose_A3
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE A3Side' BY <3>27, Fence_A3
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>1. CASE Age BY <2>1, Age_A3
  <2>2. CASE RLoad BY <2>2, RLoad_A3
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE A3Side' BY <2>3, Reap_A3
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA A3Step == IndC /\ IndC' /\ A3Side /\ [Next]_vars => A3Side'
<1>. SUFFICES ASSUME IndC, IndC', A3Side, [Next]_vars PROVE A3Side' OBVIOUS
<1>1. CASE Next BY <1>1, Next_A3
<1>2. CASE UNCHANGED vars
  <2>1. w' = w /\ minted \subseteq minted' /\ orig' = orig BY <1>2 DEF vars, bucket, aux
  <2>. QED BY <2>1, A3CF, A3Quiet
<1>. QED BY <1>1, <1>2

THEOREM A3Inv == Spec => []A3Side
BY Init_A3, A3Step, CInvariant, PTL DEF Spec

------------------------------------------------------------------------------
(* AK1: A2Side (M12) and A3Side give seven of A1Side's eight facts; the     *)
(* eighth, A1DeclPriv (a declared file not in carrier form keeps its bytes  *)
(* private: GCas, GRename, Install's other trees), is the premise.           *)
THEOREM AK1FromPriv == Spec /\ []A1DeclPriv => AK1
<1>1. Spec => []A2Side BY A2Inv
<1>2. Spec => []A3Side BY A3Inv
<1>3. A2Side /\ A3Side /\ A1DeclPriv => A1Side BY A2ToA1 DEF A1Side, A3Side
<1>4. Spec /\ []A1DeclPriv => []A1Side BY <1>1, <1>2, <1>3, PTL
<1>. QED BY <1>4, AK1FromSide, PTL
==============================================================================
''')

os.makedirs(OUT, exist_ok=True)
open(os.path.join(OUT, 'LeanP1ProofM13.tla'), 'w').write(''.join(t))
print('written', OUT)
