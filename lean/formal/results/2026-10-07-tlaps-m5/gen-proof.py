#!/usr/bin/env python3
"""Emit lean/formal/LeanP1Proof.tla: M0-M4 (results/2026-10-07-tlaps-m4/gen-proof.py)
followed by M5, part A -- the history conjuncts over IndM5 = IndM4 + Hist
(results/2026-10-07-tlaps-m5/MCLeanP1M5Anc.tla, TLC-checked; NOTES.txt):
`base`, `anc` and `orig` are written only at the handle a step mints, which
was not minted before, so Derives and Content between minted handles never
move (DerivesKeep).  Every later M5 claim reads Derives across a step.
One lemma (HistWrite) turns two facts about the step -- what it does to the
history (HistEv), where a tree's new baseline comes from (BlEv) -- into Hist'.
Part B: Prop_NoSilentRevert, from part A and the typing alone -- only GCas and
Install replace a published version with another, and each does so derived
or recorded (PARTB).
Run from lean/formal:  python3 results/2026-10-07-tlaps-m5/gen-proof.py LeanP1Proof.tla"""
import sys, runpy, os, tempfile
OUT = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
_argv = sys.argv
sys.argv = [_argv[0], os.path.join(tempfile.mkdtemp(), 'm4.tla')]
m4 = runpy.run_path(os.path.join(HERE, '..', '2026-10-07-tlaps-m4', 'gen-proof.py'), run_name='m4')
sys.argv = _argv
END = m4['END']
PRE = m4['PRE'] + m4['M4'] + m4['BODY'] + m4['TAIL4']
_old = "   not aged, and still cited or retiring.                               *)\n"
assert PRE.count(_old) == 1
PRE = PRE.replace(_old,
    "   not aged, and still cited or retiring.\n"
    "   M5 (after M4), part A: the history conjuncts over `IndM5` -- `base`,\n"
    "   `anc` and `orig` are written only at the handle a step mints, so\n"
    "   Derives and Content between minted handles never move\n"
    "   (results/2026-10-07-tlaps-m5/NOTES.txt).  Part B: Prop_NoSilentRevert.\n"
    "                                                                        *)\n")

M5 = r'''
------------------------------------------------------------------------------
(* M5, part A: the history.                                                 *)

\* A handle not yet minted has no history.
HistNew == \A h \in Handles \ minted : base[h] = Nil /\ anc[h] = {} /\ orig[h] = Nil
\* `anc` is what `base` reaches, one level down.
HistAnc == \A h \in Handles : anc[h] = AncOf(base[h])
\* A minted handle's history is minted; an original is no copy.
HistMinted == \A h \in minted :
                /\ base[h] \in Opt(minted) /\ anc[h] \subseteq minted
                /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
\* What a tree's baseline names is minted.
BaselineMinted == \A s \in Writers, p \in Paths : w[s].baseline[p] \in Opt(minted)
Hist == HistNew /\ HistAnc /\ HistMinted /\ BaselineMinted
M5 == Hist
IndM5 == IndM4 /\ M5

\* What a step does to the history: it keeps every minted handle's, and
\* writes a minted history only at what it mints.
HistEv ==
  /\ minted \subseteq minted'
  /\ \A m \in minted : base'[m] = base[m] /\ orig'[m] = orig[m]
  /\ \A h \in minted' \ minted : /\ base'[h] \in Opt(minted) /\ orig'[h] \in Opt(minted)
                                 /\ (orig'[h] # Nil => orig[orig'[h]] = Nil)
  /\ \A h \in Handles \ minted' : base'[h] = Nil /\ orig'[h] = Nil
\* Where a step takes a tree's new baseline from.
BlEv == \A u \in Writers, p \in Paths :
          w'[u].baseline[p] \in {w[u].baseline[p], Nil, doc[p], w[u].snap[p]}
\* The typing HistWrite reads.
HistTy ==
  /\ minted \subseteq Handles /\ minted' \subseteq Handles
  /\ base \in [Handles -> Opt(Handles)] /\ base' \in [Handles -> Opt(Handles)]
  /\ orig \in [Handles -> Opt(Handles)] /\ orig' \in [Handles -> Opt(Handles)]
  /\ anc \in [Handles -> SUBSET Handles]

------------------------------------------------------------------------------
(* What the events give the history.                                        *)

LEMMA HistSame ==
  ASSUME Hist, UNCHANGED <<minted, base, orig>>
  PROVE  HistEv
BY DEF Hist, HistNew, HistEv

LEMMA BlNone == w' = w => BlEv
BY DEF BlEv

LEMMA BlWrite ==
  ASSUME NEW t, NEW R, Wr(t, R),
         t \in Writers => \A p \in Paths : R.baseline[p] \in {w[t].baseline[p], Nil, doc[p], w[t].snap[p]}
  PROVE  BlEv
BY DEF Wr, BlEv

\* A minted handle keeps its `anc` across a step.
LEMMA AncKeep ==
  ASSUME HistTy, HistEv, AncUpdate, NEW m \in minted
  PROVE  anc'[m] = anc[m]
BY DEF HistEv, AncUpdate, HistTy

LEMMA HistWrite ==
  ASSUME Hist, HistEv, HistTy, AncUpdate, BlEv, Minted, SnapMinted
  PROVE  Hist'
<1>k. \A m \in minted : anc'[m] = anc[m] BY AncKeep
<1>1. HistNew'
  <2>. SUFFICES ASSUME NEW h \in Handles \ minted' PROVE base'[h] = Nil /\ anc'[h] = {} /\ orig'[h] = Nil
    BY DEF HistNew
  <2>1. h \notin minted BY DEF HistEv
  <2>2. base[h] = Nil /\ anc[h] = {} BY <2>1 DEF Hist, HistNew
  <2>3. base'[h] = Nil /\ orig'[h] = Nil BY DEF HistEv
  <2>4. anc'[h] = anc[h] BY <2>2, <2>3 DEF AncUpdate, HistTy
  <2>. QED BY <2>2, <2>3, <2>4
<1>2. HistAnc'
  <2>. SUFFICES ASSUME NEW h \in Handles
                PROVE  anc'[h] = IF base'[h] = Nil THEN {} ELSE {base'[h]} \cup anc'[base'[h]]
    BY DEF HistAnc, AncOf
  <2>1. CASE h \in minted
    <3>1. base'[h] = base[h] /\ anc'[h] = anc[h] BY <2>1, <1>k DEF HistEv
    <3>2. anc[h] = IF base[h] = Nil THEN {} ELSE {base[h]} \cup anc[base[h]] BY DEF Hist, HistAnc, AncOf
    <3>3. base[h] \in Opt(minted) BY <2>1 DEF Hist, HistMinted
    <3>4. base[h] # Nil => anc'[base[h]] = anc[base[h]] BY <3>3, <1>k DEF Opt
    <3>. QED BY <3>1, <3>2, <3>4
  <2>2. CASE h \notin minted
    <3>1. base[h] = Nil /\ anc[h] = {} BY <2>2 DEF Hist, HistNew
    <3>2. CASE base'[h] = Nil
      <4>1. anc'[h] = anc[h] BY <3>1, <3>2 DEF AncUpdate, HistTy
      <4>. QED BY <3>1, <3>2, <4>1
    <3>3. CASE base'[h] # Nil
      <4>1. h \in minted' BY <3>3 DEF HistEv
      <4>2. base'[h] \in minted BY <2>2, <3>3, <4>1 DEF HistEv, Opt
      <4>3. anc'[h] = {base'[h]} \cup anc[base'[h]] BY <3>1, <3>3 DEF AncUpdate, AncOf, HistTy
      <4>. QED BY <4>2, <4>3, <3>3, <1>k
    <3>. QED BY <3>2, <3>3
  <2>. QED BY <2>1, <2>2
<1>3. HistMinted'
  <2>. SUFFICES ASSUME NEW h \in minted'
                PROVE  /\ base'[h] \in Opt(minted') /\ anc'[h] \subseteq minted'
                       /\ orig'[h] \in Opt(minted') /\ (orig'[h] # Nil => orig'[orig'[h]] = Nil)
    BY DEF HistMinted
  <2>0. minted \subseteq minted' BY DEF HistEv
  <2>1. CASE h \in minted
    <3>1. base'[h] = base[h] /\ orig'[h] = orig[h] /\ anc'[h] = anc[h] BY <2>1, <1>k DEF HistEv
    <3>2. /\ base[h] \in Opt(minted) /\ anc[h] \subseteq minted
          /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
      BY <2>1 DEF Hist, HistMinted
    <3>3. orig[h] # Nil => orig'[orig[h]] = orig[orig[h]] BY <3>2 DEF HistEv, Opt
    <3>. QED BY <2>0, <3>1, <3>2, <3>3 DEF Opt
  <2>2. CASE h \notin minted
    <3>1. /\ base'[h] \in Opt(minted) /\ orig'[h] \in Opt(minted)
          /\ (orig'[h] # Nil => orig[orig'[h]] = Nil)
      BY <2>2 DEF HistEv
    <3>2. base[h] = Nil /\ anc[h] = {} BY <2>2 DEF Hist, HistNew, HistTy
    <3>3. anc'[h] \subseteq minted
      <4>1. CASE base'[h] = Nil BY <3>2, <4>1 DEF AncUpdate, HistTy
      <4>2. CASE base'[h] # Nil
        <5>1. base'[h] \in minted BY <3>1, <4>2 DEF Opt
        <5>2. anc'[h] = {base'[h]} \cup anc[base'[h]] BY <3>2, <4>2 DEF AncUpdate, AncOf, HistTy
        <5>3. anc[base'[h]] \subseteq minted BY <5>1 DEF Hist, HistMinted
        <5>. QED BY <5>1, <5>2, <5>3
      <4>. QED BY <4>1, <4>2
    <3>4. orig'[h] # Nil => orig'[orig'[h]] = orig[orig'[h]] BY <3>1 DEF HistEv, Opt
    <3>. QED BY <2>0, <3>1, <3>3, <3>4 DEF Opt
  <2>. QED BY <2>1, <2>2
<1>4. BaselineMinted'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths PROVE w'[u].baseline[p] \in Opt(minted')
    BY DEF BaselineMinted
  <2>1. w'[u].baseline[p] \in {w[u].baseline[p], Nil, doc[p], w[u].snap[p]} BY DEF BlEv
  <2>2. w[u].baseline[p] \in Opt(minted) BY DEF Hist, BaselineMinted
  <2>3. doc[p] \in Opt(minted) /\ w[u].snap[p] \in Opt(minted) BY DEF Minted, SnapMinted
  <2>. QED BY <2>1, <2>2, <2>3 DEF HistEv, Opt
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF Hist

\* THE POINT: Derives and Content between minted handles never move.
LEMMA DerivesKeep ==
  ASSUME Hist, HistEv, HistTy, AncUpdate, NEW k \in minted, NEW x \in minted
  PROVE  /\ Content(k)' = Content(k)
         /\ Derives(k, x)' = Derives(k, x)
<1>0. Nil \notin Handles BY NilHandle
<1>k. anc'[k] = anc[k] BY AncKeep
<1>a. anc[k] \subseteq minted BY DEF Hist, HistMinted
<1>c. \A m \in minted : Content(m)' = Content(m) BY DEF HistEv, Content
<1>1. Content(k)' = Content(k) BY <1>c
<1>2. Derives(k, x)' = Derives(k, x)
  <2>1. k # Nil /\ x # Nil BY <1>0 DEF HistTy
  <2>2. (\E a \in anc'[k] : a = x \/ (x # Nil /\ Content(a)' = Content(x)'))
        = (\E a \in anc[k] : a = x \/ (x # Nil /\ Content(a) = Content(x)))
    BY <1>k, <1>a, <1>c
  <2>. QED BY <2>1, <2>2, <1>c DEF Derives
<1>. QED BY <1>1, <1>2

\* The history after a step that writes none of it and no tree.
LEMMA HistAll ==
  ASSUME Hist, UNCHANGED <<minted, base, orig, anc, w>>
  PROVE  Hist'
BY DEF Hist, HistNew, HistAnc, AncOf, HistMinted, BaselineMinted

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M5 == Init => IndM5
<1>. SUFFICES ASSUME Init PROVE IndM5 OBVIOUS
<1>1. IndM4 BY Init_M4
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>t. minted \subseteq Handles BY <1>1 DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>b. base = [h \in Handles |-> Nil] /\ anc = [h \in Handles |-> {}] /\ orig = [h \in Handles |-> Nil] BY DEF Init
<1>3a. HistNew BY <1>b DEF HistNew
<1>3b. HistAnc BY <1>b DEF HistAnc, AncOf
<1>3c. HistMinted BY <1>b, <1>t DEF HistMinted, Opt
<1>3. HistNew /\ HistAnc /\ HistMinted BY <1>3a, <1>3b, <1>3c
<1>4. BaselineMinted BY <1>2 DEF BaselineMinted, WriterInit, Opt
<1>. QED BY <1>1, <1>3, <1>4 DEF IndM5, M5, Hist

'''

HEAD5 = '''<1>a. IndM4' BY {m4} DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\\ TypeOK /\\ Ghosts /\\ Minted /\\ SnapMinted /\\ Fresh
      /\\ w \\in [Writers -> Writer] /\\ doc \\in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
'''
QED5 = '<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5\n\n'
SAME = '<1>h. HistEv BY <1>c, HistSame DEF {d}, bucket, aux\n'

def gstep(name, sig, binders, hist=None, pre=''):
    """A gateway step, or one that writes no tree (under Frame)."""
    t = f'''LEMMA {name}_M5 ==
  ASSUME IndM5{binders}, {sig}, Frame
  PROVE  IndM5'
{HEAD5.format(m4=name + "_M4")}{pre}'''
    t += hist or SAME.format(d=name)
    t += f"<1>b. BlEv BY BlNone DEF {name}\n"
    return t + QED5

def wstep(name, sig, binders, restate, rec, recdef, bl='w[s].baseline', hist=None, pre='', bl_by=None, L=1, head=True):
    """A writer step: its new tree R's baseline."""
    I = '  ' * (L - 1)
    def S(n): return f'<{L}>{n}'
    t = ''
    if head:
        t += f'''LEMMA {name}_M5 ==
  ASSUME IndM5, NEW s \\in Writers{binders}, {sig}, Frame
  PROVE  IndM5'
{HEAD5.format(m4=name + "_M4")}'''
    t += pre
    t += f'{I}{S(1)}. {restate}\n'
    t += f'{I}{S(2)}. Wr(s, {rec}) BY <1>y, {S(1)}, WriteAny DEF Wr\n'
    t += f'{I}{S(3)}. {rec}.baseline = {bl} BY DEF {recdef}\n'
    t += (f'{I}{S(4)}. \\A p0 \\in Paths : {rec}.baseline[p0] \\in {{w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}}\n'
          f'{I}  BY {S(3)}{bl_by or ""}\n')
    t += f'{I}{S("b")}. BlEv BY {S(2)}, {S(4)}, BlWrite\n'
    if hist is None:
        t += f'{I}{S("h")}. HistEv BY {S(1)}, <1>c, HistSame DEF bucket, aux\n'
    else:
        t += hist.replace('<L>', f'<{L}>').replace('\n', '\n' + I).rstrip(' ')
    t += f'{I}{S("")}. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, {S("h")}, {S("b")}, HistWrite DEF IndM5, M5\n'
    return t

def W5(*a, **k):
    return wstep(*a, **k) + '\n'


def mint_hist(A, X, BX, OX, facts, handles, notminted, bx, ox):
    """HistEv for a step that mints X with base BX and orig OX: the four
    conjuncts one at a time.  `facts` proves minted', base', orig' and the
    guard; `handles`, `notminted`, `bx`, `ox` the side facts (each a list of
    lines at the sub-level, the last one the named step)."""
    N, M = A + 1, A + 2
    def n(k): return f'<{N}>{k}'
    def m(k): return f'<{M}>{k}'
    t = f'<{A}>h. HistEv\n'
    for blk in (facts, handles, notminted, bx, ox):
        t += blk
    t += f"{n('q1')}. minted \\subseteq minted' BY {n(1)}\n"
    t += (f"{n('q2')}. \\A x \\in minted : base'[x] = base[x] /\\ orig'[x] = orig[x]\n"
          f"  BY {n(1)}, {n(4)}, <1>ty DEF HistTy\n")
    t += (f"{n('q3')}. \\A x \\in minted' \\ minted : /\\ base'[x] \\in Opt(minted) /\\ orig'[x] \\in Opt(minted)\n"
          f"                                     /\\ (orig'[x] # Nil => orig[orig'[x]] = Nil)\n"
          f"  {m(1)}. SUFFICES ASSUME NEW x \\in minted' \\ minted\n"
          f"                PROVE  /\\ base'[x] \\in Opt(minted) /\\ orig'[x] \\in Opt(minted)\n"
          f"                       /\\ (orig'[x] # Nil => orig[orig'[x]] = Nil)\n"
          f"    OBVIOUS\n"
          f"  {m(2)}. x = {X} BY {n(1)}\n"
          f"  {m(3)}. base'[{X}] = {BX} /\\ orig'[{X}] = {OX} BY {n(1)}, {n(3)}, <1>ty DEF HistTy\n"
          f"  {m('')}. QED BY {m(2)}, {m(3)}, {n(5)}, {n(6)}\n")
    t += (f"{n('q4')}. \\A x \\in Handles \\ minted' : base'[x] = Nil /\\ orig'[x] = Nil\n"
          f"  {m(1)}. SUFFICES ASSUME NEW x \\in Handles \\ minted' PROVE base'[x] = Nil /\\ orig'[x] = Nil\n"
          f"    OBVIOUS\n"
          f"  {m(2)}. x # {X} /\\ x \\notin minted BY {n(1)}\n"
          f"  {m(3)}. base'[x] = base[x] /\\ orig'[x] = orig[x] BY {m(2)}, {n(1)}, <1>ty DEF HistTy\n"
          f"  {m('')}. QED BY {m(2)}, {m(3)}, <1>c DEF Hist, HistNew\n")
    t += f"{n('')}. QED BY {n('q1')}, {n('q2')}, {n('q3')}, {n('q4')} DEF HistEv\n"
    return t

KEEP = "UNCHANGED <<minted, base, orig, doc>>"
BODY = '------------------------------------------------------------------------------\n(* M5: the gateway.                                                         *)\n\n'

# GPut mints h = <<p, nextGen>> with base doc[p].
BODY += gstep('GPut', 'GPut(p)', ', NEW p \\in Paths', hist=mint_hist(1, 'h', 'doc[p]', 'orig[h]',
    """  <2>. DEFINE h == <<p, nextGen>>
  <2>1. /\\ minted' = minted \\cup {h} /\\ base' = [base EXCEPT ![h] = doc[p]] /\\ orig' = orig
        /\\ nextGen <= MaxMint
    BY DEF GPut, aux
  <2>2. nextGen \\in Nat /\\ nextGen > Seed BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
""",
    "  <2>3. h \\in Handles BY <2>1, <2>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed\n",
    """  <2>4. h \\notin minted
    <3>1. \\A m \\in minted : Gen(m) < nextGen \\/ Gen(m) > MaxMint BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>. QED BY <3>1, <2>1, <2>2, MaxMintNat DEF Gen
""",
    "  <2>5. doc[p] \\in Opt(minted) BY <1>y DEF Minted\n",
    "  <2>6. orig[h] \\in Opt(minted) /\\ (orig[h] # Nil => orig[orig[h]] = Nil) BY <2>3, <2>4, <1>c DEF Hist, HistNew, Opt\n"))
BODY += gstep('GCas', 'GCas(p)', ', NEW p \\in Paths')
BODY += gstep('GRename', 'GRename(p, q)', ', NEW p \\in Paths, NEW q \\in Paths')
BODY += '''LEMMA GRenameFinish_M5 ==
  ASSUME IndM5, GRenameFinish, Frame
  PROVE  IndM5'
<1>1. mv = Nil BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

'''
BODY += gstep('GDelete', 'GDelete(p)', ', NEW p \\in Paths')
BODY += gstep('Sweep', 'Sweep(s, h)', ', NEW s \\in Writers, NEW h \\in Handles')

BODY += '------------------------------------------------------------------------------\n(* M5: the agent, the commit section, the sync, the rescope, the reader.    *)\n\n'
# Edit mints h = <<p, nextGen>> with base the tree's baseline.
BODY += W5('Edit', 'Edit(s, p)', ', NEW p \\in Paths', "w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit", 'EditW(s, p)', 'EditW',
           hist=mint_hist(1, 'h', 'w[s].baseline[p]', 'orig[h]',
    """  <2>. DEFINE h == <<p, nextGen>>
  <2>1. /\\ minted' = minted \\cup {h} /\\ base' = [base EXCEPT ![h] = w[s].baseline[p]] /\\ orig' = orig
        /\\ nextGen <= MaxMint
    BY DEF Edit, aux
  <2>2. nextGen \\in Nat /\\ nextGen > Seed BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
""",
    "  <2>3. h \\in Handles BY <2>1, <2>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed\n",
    """  <2>4. h \\notin minted
    <3>1. \\A m \\in minted : Gen(m) < nextGen \\/ Gen(m) > MaxMint BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>. QED BY <3>1, <2>1, <2>2, MaxMintNat DEF Gen
""",
    "  <2>5. w[s].baseline[p] \\in Opt(minted) BY <1>c DEF Hist, BaselineMinted\n",
    "  <2>6. orig[h] \\in Opt(minted) /\\ (orig[h] # Nil => orig[orig[h]] = Nil) BY <2>3, <2>4, <1>c DEF Hist, HistNew, Opt\n").replace('<1>h.', '<L>h.'))
BODY += W5('Delete', 'Delete(s, p)', ', NEW p \\in Paths', f"w' = [w EXCEPT ![s] = DeleteW(s, p)] /\\ {KEEP} BY DEF Delete, bucket, aux", 'DeleteW(s, p)', 'DeleteW')
BODY += W5('Checkout', 'Checkout(s)', '', f"PICK T \\in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\\ {KEEP} BY DEF Checkout, bucket, aux",
           'CheckoutW(s, T)', 'CheckoutW', bl='CheckoutHeld(s, T)', bl_by=' DEF CheckoutHeld')

# Consume: two trees.
BODY += f'''LEMMA Consume_M5 ==
  ASSUME IndM5, NEW s \\in Writers, Consume(s), Frame
  PROVE  IndM5'
{HEAD5.format(m4="Consume_M4")}<1>1. CASE CheapPath(s)
'''
BODY += wstep('Consume', 'Consume(s)', '', f"w' = [w EXCEPT ![s] = ConsumeCheapW(s)] /\\ {KEEP} BY <1>1 DEF Consume, bucket, aux",
              'ConsumeCheapW(s)', 'ConsumeCheapW', L=2, head=False)
BODY += '<1>2. CASE ~CheapPath(s)\n'
BODY += wstep('Consume', 'Consume(s)', '', f"PICK fail \\in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] /\\ {KEEP} BY <1>2 DEF Consume, bucket, aux",
              'ConsumeW(s, fail)', 'ConsumeW', bl='[q \\in Paths |-> IF q \\in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]]', L=2, head=False)
BODY += '<1>. QED BY <1>1, <1>2\n\n'

BODY += W5('Scan', 'Scan(s)', '', f"PICK dels \\in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\\ {KEEP} BY DEF Scan, bucket, aux", 'ScanW(s, dels)', 'ScanW')
BODY += W5('Skip', 'Skip(s)', '', f"w' = [w EXCEPT ![s] = SkipW(s)] /\\ {KEEP} BY DEF Skip, bucket, aux", 'SkipW(s)', 'SkipW')

# Upload: a first PUT, or a copy c of bytes PUT once (base[h], Content(h)).
BODY += f'''LEMMA Upload_M5 ==
  ASSUME IndM5, NEW s \\in Writers, NEW p \\in Paths, Upload(s, p), Frame
  PROVE  IndM5'
{HEAD5.format(m4="Upload_M4")}<1>1. CASE w[s].snap[p] \\notin upped
'''
BODY += wstep('Upload', 'Upload(s, p)', '', f"w' = [w EXCEPT ![s] = UploadW(s, p)] /\\ UNCHANGED <<minted, base, orig, doc>> BY <1>1 DEF Upload",
              'UploadW(s, p)', 'UploadW', L=2, head=False)
BODY += '<1>2. CASE w[s].snap[p] \\in upped\n  <2>. DEFINE c == <<p, MaxMint + copies + 1>>\n  <2>. DEFINE h == w[s].snap[p]\n'
BODY += wstep('Upload', 'Upload(s, p)', '',
              "/\\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\\ doc' = doc\n"
              "      /\\ minted' = minted \\cup {c} /\\ base' = [base EXCEPT ![c] = base[h]]\n"
              "      /\\ orig' = [orig EXCEPT ![c] = Content(h)] /\\ copies < MaxCopies\n"
              "    BY <1>2 DEF Upload",
              'UploadCopyW(s, p, c)', 'UploadCopyW', L=2, head=False,
              hist=mint_hist(2, 'c', 'base[h]', 'Content(h)',
    """  <3>1. /\\ minted' = minted \\cup {c} /\\ base' = [base EXCEPT ![c] = base[h]]
        /\\ orig' = [orig EXCEPT ![c] = Content(h)] /\\ copies < MaxCopies
    BY <2>1
  <3>2. copies \\in Nat /\\ nextGen \\in Nat /\\ nextGen <= MaxMint + 1
    BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh, Ghosts, IndM1, IndTypeOK, TypeOK
""",
    "  <3>3. c \\in Handles BY <3>1, <3>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed\n",
    """  <3>4. c \\notin minted
    <4>1. \\A m \\in minted : Gen(m) < nextGen \\/ (Gen(m) > MaxMint /\\ Gen(m) <= MaxMint + copies)
      BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <4>. QED BY <4>1, <3>2, MaxMintNat DEF Gen
""",
    """  <3>h0. h \\in minted BY <1>2 DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <3>h1. base[h] \\in Opt(minted) /\\ orig[h] \\in Opt(minted) /\\ (orig[h] # Nil => orig[orig[h]] = Nil)
    BY <3>h0, <1>c DEF Hist, HistMinted
  <3>5. base[h] \\in Opt(minted) BY <3>h1
""",
    """  <3>6. Content(h) \\in Opt(minted) /\\ (Content(h) # Nil => orig[Content(h)] = Nil)
    <4>0. h # Nil BY <3>h0, NilHandle, <1>ty DEF HistTy
    <4>1. CASE orig[h] # Nil BY <4>0, <4>1, <3>h1 DEF Content, Opt
    <4>2. CASE orig[h] = Nil BY <4>0, <4>2, <3>h0 DEF Content, Opt
    <4>. QED BY <4>1, <4>2
""").replace('<2>h.', '<L>h.'))
BODY += '<1>. QED BY <1>1, <1>2\n\n'

for nm, rec, extra in [('PullOnly', 'PullOnlyW(s)', ''), ('Claim', 'ClaimW(s)', ''), ('Verify', 'VerifyW(s)', ''),
                       ('Install', 'InstallW(s)', ''), ('Collect', 'CollectW(s)', '')]:
    BODY += W5(nm, f'{nm}(s)', '', f"w' = [w EXCEPT ![s] = {rec}] /\\ UNCHANGED <<minted, base, orig>> BY DEF {nm}, bucket, aux",
               rec, rec.split('(')[0])
BODY += W5('Finish', 'Finish(s)', '', f"w' = [w EXCEPT ![s] = FinishW(s)] /\\ UNCHANGED <<minted, base, orig>> BY DEF Finish, bucket, aux",
           'FinishW(s)', 'FinishW',
           bl='[p0 \\in Paths |-> IF p0 \\in w[s].uploads \\cap w[s].upDone THEN w[s].snap[p0]\n'
              '                         ELSE IF p0 \\in w[s].deletes /\\ w[s].inst[p0] = Nil THEN Nil\n'
              '                         ELSE w[s].baseline[p0]]')
BODY += W5('Restart', 'Restart(s)', '', f"w' = [w EXCEPT ![s] = RestartW(s)] /\\ UNCHANGED <<minted, base, orig>> BY DEF Restart, bucket, aux",
           'RestartW(s)', 'RestartW')
BODY += W5('Sync', 'Sync(s)', '', f"PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\\ UNCHANGED <<minted, base, orig>> BY DEF Sync, bucket, aux",
           'SyncW(s, fail)', 'SyncW', bl='SyncBl(s, fail)', bl_by=' DEF SyncBl')
BODY += W5('RescopeBegin', 'RescopeBegin(s)', '', f"PICK T \\in Scopes \\ {{w[s].scope}} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\\ UNCHANGED <<minted, base, orig>> BY DEF RescopeBegin, bucket, aux",
           'RescopeBeginW(s, T)', 'RescopeBeginW')
BODY += W5('RescopeFirst', 'RescopeFirst(s)', '', f"w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\\ UNCHANGED <<minted, base, orig>> BY DEF RescopeFirst, bucket, aux",
           'RescopeFirstW(s)', 'RescopeFirstW',
           bl='IF RescopeUnciteFirst\n'
              '                THEN [p0 \\in Paths |-> IF p0 \\in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[p0]]\n'
              '                ELSE w[s].baseline')
BODY += W5('RescopeSecond', 'RescopeSecond(s)', '',
           "PICK wfail \\in SUBSET {q \\in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \\/ ~WidenKeepsLocal} :\n"
           f"      w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\\ UNCHANGED <<minted, base, orig>>\n    BY DEF RescopeSecond, bucket, aux",
           'RescopeSecondW(s, wfail)', 'RescopeSecondW',
           bl='[p0 \\in Paths |-> IF p0 \\in RescopeFetch(s, wfail) THEN doc[p0] ELSE RescopeBase1(s)[p0]]',
           bl_by=' DEF RescopeBase1')
BODY += W5('RPullRead', 'RPullRead(s)', '', f"w' = [w EXCEPT ![s] = RPullReadW(s)] /\\ UNCHANGED <<minted, base, orig>> BY DEF RPullRead, bucket, aux",
           'RPullReadW(s)', 'RPullReadW')
BODY += W5('RPullSync', 'RPullSync(s)', '', f"PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\\ UNCHANGED <<minted, base, orig>> BY DEF RPullSync, bucket, aux",
           'RPullSyncW(s, fail)', 'RPullSyncW', bl='SyncBl(s, fail)', bl_by=' DEF SyncBl')

BODY += r'''------------------------------------------------------------------------------
(* M5: the retire age and the reader's load.                                *)

LEMMA Age_M5 ==
  ASSUME IndM5, Age, UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY Age_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, Age, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

LEMMA Reap_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY Reap_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, Reap, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

LEMMA RLoad_M5 ==
  ASSUME IndM5, RLoad, UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY RLoad_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, RLoad, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

'''

TAIL5 = m4['TAIL4']
_s = TAIL5.index('LEMMA Next_M4')
_e = TAIL5.index('THEOREM ReaderFetches')
TAIL5 = TAIL5[_s:_e].replace('IndM4', 'IndM5').replace('_M4', '_M5').replace('M4Invariant', 'M5Invariant')
_o = ("    <3>5. IndM5 /\\ UNCHANGED vars => M4' BY M4Same DEF IndM5, vars, ret\n"
      "    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5 DEF IndM5, IndM3, IndM2, IndM1\n")
assert TAIL5.count(_o) == 1, TAIL5[-1500:]
TAIL5 = TAIL5.replace(_o,
      "    <3>5. IndM5 /\\ UNCHANGED vars => M4' BY M4Same DEF IndM5, IndM4, vars, ret\n"
      "    <3>6. IndM5 /\\ UNCHANGED vars => M5' BY HistAll DEF IndM5, M5, vars, bucket, aux\n"
      "    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6 DEF IndM5, IndM4, IndM3, IndM2, IndM1\n")
for a, b in [("BY DEF IndM5, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret",
              "BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret"),
             ("BY M1Keep DEF IndM5, IndM3, IndM2, IndM1, vars", "BY M1Keep DEF IndM5, IndM4, IndM3, IndM2, IndM1, vars"),
             ("BY M2Same DEF IndM5, IndM3, IndM2, vars, aux, ret", "BY M2Same DEF IndM5, IndM4, IndM3, IndM2, vars, aux, ret"),
             ("BY M3Same DEF IndM5, IndM3, vars, bucket", "BY M3Same DEF IndM5, IndM4, IndM3, vars, bucket")]:
    assert TAIL5.count(a) == 1, a
    TAIL5 = TAIL5.replace(a, b)
TAIL5 = '------------------------------------------------------------------------------\n(* M5: the step, and the invariant.                                         *)\n\n' + TAIL5

PARTB = r"""------------------------------------------------------------------------------
(* M5, part B: Prop_NoSilentRevert.  A step replaces a published version  *)
(* with another at only two places: the gateway's CAS (GCas), which lands  *)
(* only over the version the save read -- so the new one derives from it  *)
(* through `anc` (HistAnc) -- and a commit (Install), which publishes over *)
(* a version not its own baseline only by recording it (CommitSurfaces-   *)
(* Foreign).  No other M5 claim is used: part A and the typing suffice.   *)

NoRev == \A p \in Paths : ~SilentRevert(p)

\* A step that, at every path, keeps the version, clears it, or fills an
\* empty path, reverts nothing.
LEMMA RevClear ==
  (\A p \in Paths : doc'[p] = doc[p] \/ doc'[p] = Nil \/ doc[p] = Nil) => NoRev
  BY DEF NoRev, SilentRevert

LEMMA GCas_Rev ==
  ASSUME IndM5, NEW p0 \in Paths, GCas(p0)
  PROVE  NoRev
<1>. DEFINE h == gw[p0]
<1>y. /\ doc \in [Paths -> Opt(Handles)] /\ gw \in [Paths -> Opt(Handles)] /\ minted \subseteq Handles
      /\ Minted
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. /\ h # Nil
      /\ doc' = IF doc[p0] = base[h] THEN [doc EXCEPT ![p0] = h] ELSE doc
  BY ShippedShape DEF GCas, Shipped
<1>2. h \in minted /\ h \in Handles BY <1>1, <1>y DEF Minted, Opt
<1>3. SUFFICES ASSUME NEW p \in Paths, SilentRevert(p) PROVE FALSE BY DEF NoRev
<1>4. /\ doc[p] # Nil /\ doc'[p] # doc[p]
      /\ ~Supersedes(doc'[p], doc[p], p)
  BY <1>3 DEF SilentRevert
<1>5. p = p0 /\ doc[p0] = base[h] /\ doc'[p] = h BY <1>1, <1>4, <1>y
<1>6. anc[h] = {base[h]} \cup anc[base[h]]
  BY <1>2, <1>4, <1>5 DEF IndM5, M5, Hist, HistAnc, AncOf
<1>7. Derives(h, doc[p]) BY <1>1, <1>5, <1>6 DEF Derives
<1>. QED BY <1>4, <1>5, <1>7 DEF Supersedes

LEMMA Install_Rev ==
  ASSUME IndM5, NEW s \in Writers, Install(s)
  PROVE  NoRev
<1>1. /\ w[s].pc = "claimed" /\ doc' = InstallInst(s)
      /\ {<<q, doc[q]>> : q \in InstallContested(s)} \subseteq conflicts'
  BY DEF Install
<1>2. SUFFICES ASSUME NEW p \in Paths, SilentRevert(p) PROVE FALSE BY DEF NoRev
<1>3. /\ doc[p] # Nil /\ doc'[p] # Nil /\ doc'[p] # doc[p]
      /\ ~\E c \in conflicts' : c[2] = doc[p]
      /\ ~\E t \in Writers : w[t].pc = "claimed"
                             /\ (w[t].baseline[p] = doc[p] \/ Gen(doc[p]) \in took[t][p])
  BY <1>2 DEF SilentRevert
<1>4. p \in InstallMine(s) /\ p \notin w[s].gone BY <1>1, <1>3 DEF InstallInst
<1>5. CASE Foreign(s, p)
  <2>1. p \in InstallContested(s) BY <1>3, <1>4, <1>5, ShippedShape DEF InstallContested, Shipped
  <2>2. <<p, doc[p]>> \in conflicts' BY <1>1, <2>1
  <2>. QED BY <1>3, <2>2
<1>6. CASE ~Foreign(s, p)
  <2>1. w[s].baseline[p] = doc[p] BY <1>6 DEF Foreign
  <2>. QED BY <1>1, <1>3, <2>1
<1>. QED BY <1>5, <1>6

LEMMA Next_Rev == IndM5 /\ Next => NoRev
<1>. SUFFICES ASSUME IndM5, Next PROVE NoRev OBVIOUS
<1>y. doc \in [Paths -> Opt(Handles)] BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. CASE GatewayStep
  <2>1. ASSUME NEW p \in Paths, GPut(p) PROVE NoRev BY <2>1, RevClear DEF GPut
  <2>2. ASSUME NEW p \in Paths, GCas(p) PROVE NoRev BY <2>2, GCas_Rev
  <2>3. ASSUME NEW p \in Paths, GDelete(p) PROVE NoRev BY <2>3, <1>y, RevClear DEF GDelete
  <2>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE NoRev
    BY <2>4, <1>y, RevClear, ShippedShape DEF GRename, Shipped
  <2>5. CASE GRenameFinish BY <2>5, <1>y, RevClear DEF GRenameFinish
  <2>. QED BY <1>1, <2>1, <2>2, <2>3, <2>4, <2>5 DEF GatewayStep
<1>2. CASE WriterStep
"""
PARTB += '''  <2>1. ASSUME NEW s \\in Writers, Checkout(s) PROVE NoRev BY <2>1, RevClear DEF Checkout, bucket
  <2>2. ASSUME NEW s \\in Writers, Consume(s) PROVE NoRev BY <2>2, RevClear DEF Consume, bucket
  <2>3. ASSUME NEW s \\in Writers, Scan(s) PROVE NoRev BY <2>3, RevClear DEF Scan, bucket
  <2>4. ASSUME NEW s \\in Writers, Skip(s) PROVE NoRev BY <2>4, RevClear DEF Skip, bucket
  <2>5. ASSUME NEW s \\in Writers, PullOnly(s) PROVE NoRev BY <2>5, RevClear DEF PullOnly, bucket
  <2>6. ASSUME NEW s \\in Writers, Claim(s) PROVE NoRev BY <2>6, RevClear DEF Claim, bucket
  <2>7. ASSUME NEW s \\in Writers, Verify(s) PROVE NoRev BY <2>7, RevClear DEF Verify, bucket
  <2>8. ASSUME NEW s \\in Writers, Collect(s) PROVE NoRev BY <2>8, RevClear DEF Collect, bucket
  <2>9. ASSUME NEW s \\in Writers, Finish(s) PROVE NoRev BY <2>9, RevClear DEF Finish, bucket
  <2>10. ASSUME NEW s \\in Writers, Restart(s) PROVE NoRev BY <2>10, RevClear DEF Restart, bucket
  <2>11. ASSUME NEW s \\in Writers, Sync(s) PROVE NoRev BY <2>11, RevClear DEF Sync, bucket
  <2>12. ASSUME NEW s \\in Writers, RescopeBegin(s) PROVE NoRev BY <2>12, RevClear DEF RescopeBegin, bucket
  <2>13. ASSUME NEW s \\in Writers, RescopeFirst(s) PROVE NoRev BY <2>13, RevClear DEF RescopeFirst, bucket
  <2>14. ASSUME NEW s \\in Writers, RescopeSecond(s) PROVE NoRev BY <2>14, RevClear DEF RescopeSecond, bucket
  <2>15. ASSUME NEW s \\in Writers, RPullRead(s) PROVE NoRev BY <2>15, RevClear DEF RPullRead, bucket
  <2>16. ASSUME NEW s \\in Writers, RPullSync(s) PROVE NoRev BY <2>16, RevClear DEF RPullSync, bucket
  <2>17. ASSUME NEW s \\in Writers, NEW p \\in Paths, Edit(s, p) PROVE NoRev BY <2>17, RevClear DEF Edit, bucket
  <2>18. ASSUME NEW s \\in Writers, NEW p \\in Paths, Delete(s, p) PROVE NoRev BY <2>18, RevClear DEF Delete, bucket
  <2>19. ASSUME NEW s \\in Writers, NEW p \\in Paths, Upload(s, p) PROVE NoRev BY <2>19, RevClear DEF Upload, bucket
  <2>20. ASSUME NEW s \\in Writers, NEW h \\in Handles, Sweep(s, h) PROVE NoRev BY <2>20, RevClear DEF Sweep, bucket
  <2>21. ASSUME NEW s \\in Writers, Install(s) PROVE NoRev BY <2>21, Install_Rev
  <2>. QED BY <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>18, <2>19, <2>20, <2>21 DEF WriterStep
'''
PARTB += r"""<1>3. CASE Age BY <1>3, RevClear DEF Age
<1>4. CASE RLoad BY <1>4, RevClear DEF RLoad
<1>5. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE NoRev BY <1>5, RevClear DEF Reap
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Next

THEOREM NoSilentRevert == Spec => Prop_NoSilentRevert
<1>1. IndM5 /\ [Next]_vars => [NoRev]_vars
  <2>1. IndM5 /\ Next => NoRev BY Next_Rev
  <2>. QED BY <2>1
<1>. QED BY M5Invariant, <1>1, PTL DEF Spec, Prop_NoSilentRevert, NoRev

"""

open(OUT, 'w').write(PRE + M5 + BODY + TAIL5 + PARTB + END)
print('written', OUT, len((PRE + M5 + BODY + TAIL5 + PARTB + END).splitlines()), 'lines')
