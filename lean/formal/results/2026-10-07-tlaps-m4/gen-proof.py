#!/usr/bin/env python3
"""Emit lean/formal/LeanP1Proof.tla: M0-M3 (results/2026-10-07-tlaps-m3/gen-proof.py)
followed by M4 -- Inv_ReaderFetches over IndM4 = IndM3 + the never-re-cited
lemma and the reader's loaded document (results/2026-10-07-tlaps-m4/
MCLeanP1M4.tla, TLC-checked; NOTES.txt).  Every frame step logs exactly what
stops being cited (RetExact, under RetireAge); one lemma (M4Write) turns two
facts about the step -- what the document may newly cite, what `live` may
lose -- into M4'.
Run from lean/formal:  python3 results/2026-10-07-tlaps-m4/gen-proof.py LeanP1Proof.tla"""
import sys, runpy, os, tempfile
OUT = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
_argv = sys.argv
sys.argv = [_argv[0], os.path.join(tempfile.mkdtemp(), 'm3.tla')]
m3 = runpy.run_path(os.path.join(HERE, '..', '2026-10-07-tlaps-m3', 'gen-proof.py'), run_name='m3')
sys.argv = _argv
END = m3['END']
PRE = m3['PRE'] + m3['M3'] + m3['BODY'] + m3['TAIL3']
_old = "   written tree; `M3Write` turns five facts about that tree into M3'.  *)\n"
assert PRE.count(_old) == 1
PRE = PRE.replace(_old,
    "   written tree; `M3Write` turns five facts about that tree into M3'.\n"
    "   M4 (after M3): `Inv_ReaderFetches` over `IndM4` -- IndM3 and the\n"
    "   plan's I11 with the never-re-cited lemma (results/2026-10-07-tlaps-m4/\n"
    "   NOTES.txt): a cited handle is never retiring or aged, and a reader\n"
    "   that loaded the document less than G ago holds handles each live,\n"
    "   not aged, and still cited or retiring.                               *)\n")

M4 = r'''
------------------------------------------------------------------------------
(* M4: Inv_ReaderFetches.                                                   *)

\* The never-re-cited lemma: a step cites a fresh handle or moves a cited
\* one, and what stops being cited is logged retiring (`RetUpdate`).
NeverReCited == \A p \in Paths : doc[p] # Nil => doc[p] \notin retiring \cup aged
\* I11: a reader that loaded the document less than G ago holds handles each
\* live, not aged, and still cited or retiring.
ReaderLive == ~rlag => \A p \in Paths : rdoc[p] # Nil =>
                /\ rdoc[p] \in live /\ rdoc[p] \notin aged
                /\ (Cited(rdoc[p]) \/ rdoc[p] \in retiring)
M4 == NeverReCited /\ ReaderLive
IndM4 == IndM3 /\ M4

\* What a step may newly cite is cited already or neither retiring nor aged.
DocNew == \A k \in Paths : doc'[k] # Nil => doc'[k] \in CitedSet \/ doc'[k] \notin retiring \cup aged
\* What `live` may lose is uncited and not retiring.
LiveLoss == \A h \in live : h \notin live' => ~Cited(h) /\ h \notin retiring

------------------------------------------------------------------------------
(* What the events give M4.                                                 *)

\* Under the retire age every frame step logs exactly what stops being cited.
LEMMA RetExact ==
  ASSUME RetireAge, RetUpdate
  PROVE  retiring' = retiring \cup (CitedSet \ CitedSet')
BY DEF RetUpdate, CitedSet

LEMMA DocSameNew == doc' = doc => DocNew
BY DEF DocNew, CitedSet

LEMMA LiveGrows == live \subseteq live' => LiveLoss
BY DEF LiveLoss

LEMMA M4Write ==
  ASSUME M4, DocNew, LiveLoss,
         retiring' = retiring \cup (CitedSet \ CitedSet'),
         aged' = aged, rdoc' = rdoc, rlag' = rlag
  PROVE  M4'
<1>1. NeverReCited'
  <2>. SUFFICES ASSUME NEW p \in Paths, doc'[p] # Nil PROVE doc'[p] \notin retiring' \cup aged'
    BY DEF NeverReCited
  <2>1. doc'[p] \in CitedSet' BY DEF CitedSet
  <2>2. CASE doc'[p] \in CitedSet
    <3>1. doc'[p] \notin retiring \cup aged BY <2>2 DEF M4, NeverReCited, CitedSet
    <3>. QED BY <2>1, <3>1
  <2>3. CASE doc'[p] \notin retiring \cup aged BY <2>1, <2>3
  <2>. QED BY <2>2, <2>3 DEF DocNew
<1>2. ReaderLive'
  <2>. SUFFICES ASSUME ~rlag, NEW p \in Paths, rdoc[p] # Nil
                PROVE  /\ rdoc[p] \in live' /\ rdoc[p] \notin aged'
                       /\ ((\E k \in Paths : doc'[k] = rdoc[p]) \/ rdoc[p] \in retiring')
    BY DEF ReaderLive, Cited
  <2>0. /\ rdoc[p] \in live /\ rdoc[p] \notin aged
        /\ ((\E k \in Paths : doc[k] = rdoc[p]) \/ rdoc[p] \in retiring)
    BY DEF M4, ReaderLive, Cited
  <2>1. rdoc[p] \in live' BY <2>0 DEF LiveLoss, Cited
  <2>2. CASE \E k \in Paths : doc[k] = rdoc[p]
    <3>1. rdoc[p] \in CitedSet BY <2>2 DEF CitedSet
    <3>. QED BY <2>0, <2>1, <3>1 DEF CitedSet
  <2>. QED BY <2>0, <2>1, <2>2
<1>. QED BY <1>1, <1>2 DEF M4

\* M4 after a step that moves nothing M4 reads.
LEMMA M4Same ==
  ASSUME M4, UNCHANGED <<doc, live, retiring, aged, rdoc, rlag>>
  PROVE  M4'
BY DEF M4, NeverReCited, ReaderLive, Cited

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M4 == Init => IndM4
<1>. SUFFICES ASSUME Init PROVE IndM4 OBVIOUS
<1>1. IndM3 BY Init_M3
<1>2. NeverReCited /\ ReaderLive BY DEF Init, NeverReCited, ReaderLive
<1>. QED BY <1>1, <1>2 DEF IndM4, M4

'''

HEAD4 = '''<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, {m3}
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \\cup (CitedSet \\ CitedSet') /\\ aged' = aged /\\ rdoc' = rdoc /\\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
'''

def frame_step(name, sig, binders, doc_by=None, live=None, pre=''):
    """A gateway or writer step (under Frame)."""
    t = f'''LEMMA {name}_M4 ==
  ASSUME IndM4{binders}, {sig}, Frame
  PROVE  IndM4'
{HEAD4.format(m3=name + "_M3")}{pre}'''
    if doc_by is None:
        t += f'<1>d0. doc\' = doc BY DEF {name}, bucket\n<1>d. DocNew BY <1>d0, DocSameNew\n'
    else:
        t += f'<1>d. DocNew {doc_by}\n'
    if live is None:
        t += f'<1>l0. live \\subseteq live\' BY DEF {name}, bucket\n<1>l. LiveLoss BY <1>l0, LiveGrows\n'
    else:
        t += f'<1>l. LiveLoss {live}\n'
    t += '<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4\n\n'
    return t

W = ', NEW s \\in Writers'
BODY = '------------------------------------------------------------------------------\n(* M4: the gateway.                                                         *)\n\n'
BODY += frame_step('GPut', 'GPut(p)', ', NEW p \\in Paths')
BODY += frame_step('GCas', 'GCas(p)', ', NEW p \\in Paths',
                   doc_by='''
  <2>1. gw[p] \\notin retiring \\cup aged /\\ gw[p] # Nil BY DEF GCas, IndM4, IndM3, IndM2, M2, Flight
  <2>2. \\A k \\in Paths : doc'[k] = doc[k] \\/ doc'[k] = gw[p]
    <3>1. doc \\in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
    <3>. QED BY <3>1 DEF GCas
  <2>. QED BY <2>1, <2>2 DEF DocNew, CitedSet''')
BODY += frame_step('GRename', 'GRename(p, q)', ', NEW p \\in Paths, NEW q \\in Paths',
                   pre='<1>sr. RenameAtomic BY ShippedShape DEF Shipped\n',
                   doc_by='''
  <2>1. doc \\in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. doc[p] # Nil /\\ \\A k \\in Paths : doc'[k] = doc[k] \\/ doc'[k] = Nil \\/ doc'[k] = doc[p]
    BY <1>sr, <2>1 DEF GRename
  <2>. QED BY <2>2 DEF DocNew, CitedSet''')
BODY += '''LEMMA GRenameFinish_M4 ==
  ASSUME IndM4, GRenameFinish, Frame
  PROVE  IndM4'
<1>1. mv = Nil BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

'''
BODY += frame_step('GDelete', 'GDelete(p)', ', NEW p \\in Paths',
                   doc_by='''
  <2>1. doc \\in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. \\A k \\in Paths : doc'[k] = doc[k] \\/ doc'[k] = Nil BY <2>1 DEF GDelete
  <2>. QED BY <2>2 DEF DocNew, CitedSet''')
BODY += frame_step('Sweep', 'Sweep(s, h)', W + ', NEW h \\in Handles',
                   live='''
  <2>1. live' = live \\ {h} /\\ ~Cited(h) /\\ h \\notin retiring BY <1>s DEF Sweep
  <2>. QED BY <2>1 DEF LiveLoss''')

BODY += '------------------------------------------------------------------------------\n(* M4: the agent, the commit section, the sync, the rescope, the reader.    *)\n\n'
for nm, sig, b in [('Edit', 'Edit(s, p)', W + ', NEW p \\in Paths'), ('Delete', 'Delete(s, p)', W + ', NEW p \\in Paths'),
                   ('Checkout', 'Checkout(s)', W), ('Consume', 'Consume(s)', W), ('Scan', 'Scan(s)', W),
                   ('Skip', 'Skip(s)', W), ('Upload', 'Upload(s, p)', W + ', NEW p \\in Paths'),
                   ('PullOnly', 'PullOnly(s)', W), ('Claim', 'Claim(s)', W), ('Verify', 'Verify(s)', W)]:
    BODY += frame_step(nm, sig, b)
BODY += frame_step('Install', 'Install(s)', W,
                   doc_by='''
  <2>1. \\A k \\in Paths : InstallInst(s)[k] = Nil \\/ InstallInst(s)[k] = doc[k]
                         \\/ (k \\in InstallMine(s) \\ w[s].gone /\\ InstallInst(s)[k] = w[s].snap[k])
    BY DEF InstallInst
  <2>2. \\A k \\in InstallMine(s) \\ w[s].gone : w[s].snap[k] \\notin retiring \\cup aged
    BY DEF IndM4, IndM3, IndM2, M2, Uploaded, Up, InstallMine, Install
  <2>3. doc' = InstallInst(s) BY DEF Install
  <2>. QED BY <2>1, <2>2, <2>3 DEF DocNew, CitedSet''')
BODY += frame_step('Collect', 'Collect(s)', W,
                   live='''
  <2>1. live' = live BY <1>s DEF Collect
  <2>. QED BY <2>1, LiveGrows''')
for nm, sig, b in [('Finish', 'Finish(s)', W), ('Restart', 'Restart(s)', W), ('Sync', 'Sync(s)', W),
                   ('RescopeBegin', 'RescopeBegin(s)', W), ('RescopeFirst', 'RescopeFirst(s)', W),
                   ('RescopeSecond', 'RescopeSecond(s)', W), ('RPullRead', 'RPullRead(s)', W),
                   ('RPullSync', 'RPullSync(s)', W)]:
    BODY += frame_step(nm, sig, b)

BODY += r'''------------------------------------------------------------------------------
(* M4: the retire age and the reader's load.                                *)

LEMMA Age_M4 ==
  ASSUME IndM4, Age, UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Age_M3
<1>c. NeverReCited BY DEF IndM4, M4
<1>1. aged' = aged \cup retiring /\ retiring' = {} /\ rlag' = TRUE /\ doc' = doc BY DEF Age
<1>2. NeverReCited' BY <1>1, <1>c DEF NeverReCited
<1>3. ReaderLive' BY <1>1 DEF ReaderLive
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

LEMMA Reap_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Reap_M3
<1>1. /\ h \in aged /\ live' = live \ {h} /\ aged' = aged \ {h}
      /\ UNCHANGED <<doc, retiring, rdoc, rlag>>
  BY DEF Reap
<1>2. NeverReCited' BY <1>1 DEF IndM4, M4, NeverReCited
<1>3. ReaderLive'
  <2>. SUFFICES ASSUME ~rlag, NEW p \in Paths, rdoc[p] # Nil
                PROVE  /\ rdoc[p] \in live' /\ rdoc[p] \notin aged'
                       /\ ((\E k \in Paths : doc'[k] = rdoc[p]) \/ rdoc[p] \in retiring')
    BY <1>1 DEF ReaderLive, Cited
  <2>1. rdoc[p] \in live /\ rdoc[p] \notin aged /\ ((\E k \in Paths : doc[k] = rdoc[p]) \/ rdoc[p] \in retiring)
    BY DEF IndM4, M4, ReaderLive, Cited
  <2>. QED BY <1>1, <2>1
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

LEMMA RLoad_M4 ==
  ASSUME IndM4, RLoad, UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RLoad_M3
<1>1. rdoc' = doc /\ rlag' = FALSE /\ UNCHANGED <<live, doc, retiring, aged>> BY DEF RLoad
<1>2. NeverReCited' BY <1>1 DEF IndM4, M4, NeverReCited
<1>3. ReaderLive'
  <2>. SUFFICES ASSUME NEW p \in Paths, doc[p] # Nil
                PROVE  /\ doc[p] \in live /\ doc[p] \notin aged
                       /\ ((\E k \in Paths : doc[k] = doc[p]) \/ doc[p] \in retiring)
    BY <1>1 DEF ReaderLive, Cited
  <2>1. doc[p] \in live BY DEF IndM4, IndM3, IndM2, M2, Inv_CitationsLive
  <2>2. doc[p] \notin aged BY DEF IndM4, M4, NeverReCited
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

'''

TAIL4 = m3['TAIL3']
_s = TAIL4.index('LEMMA Next_M3')
_e = TAIL4.index('\\* The cheap path skips only')
TAIL4 = TAIL4[_s:_e].replace('IndM3', 'IndM4').replace('_M3', '_M4').replace('M3Invariant', 'M4Invariant')
_o = ("    <3>4. IndM4 /\\ UNCHANGED vars => M3' BY M3Same DEF IndM4, vars, bucket\n"
      "    <3>. QED BY <3>1, <3>2, <3>3, <3>4 DEF IndM4, IndM2, IndM1\n")
assert TAIL4.count(_o) == 1, TAIL4[-1500:]
TAIL4 = TAIL4.replace(_o,
      "    <3>4. IndM4 /\\ UNCHANGED vars => M3' BY M3Same DEF IndM4, IndM3, vars, bucket\n"
      "    <3>5. IndM4 /\\ UNCHANGED vars => M4' BY M4Same DEF IndM4, vars, ret\n"
      "    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5 DEF IndM4, IndM3, IndM2, IndM1\n")
for a, b in [("BY DEF IndM4, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret",
              "BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret"),
             ("BY M1Keep DEF IndM4, IndM2, IndM1, vars", "BY M1Keep DEF IndM4, IndM3, IndM2, IndM1, vars"),
             ("BY M2Same DEF IndM4, IndM2, vars, aux, ret", "BY M2Same DEF IndM4, IndM3, IndM2, vars, aux, ret")]:
    assert TAIL4.count(a) == 1, a
    TAIL4 = TAIL4.replace(a, b)
TAIL4 = '------------------------------------------------------------------------------\n(* M4: the step, and the theorem.                                           *)\n\n' + TAIL4
TAIL4 += r'''THEOREM ReaderFetches == Spec => []Inv_ReaderFetches
<1>1. IndM4 => Inv_ReaderFetches BY DEF IndM4, M4, ReaderLive, Inv_ReaderFetches
<1>. QED BY M4Invariant, <1>1, PTL
'''

open(OUT, 'w').write(PRE + M4 + BODY + TAIL4 + END)
print('written', OUT, len((PRE + M4 + BODY + TAIL4 + END).splitlines()), 'lines')
