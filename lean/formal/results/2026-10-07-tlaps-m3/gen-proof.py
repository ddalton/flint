#!/usr/bin/env python3
"""Emit lean/formal/LeanP1Proof.tla: M0 + M1 + M2 (results/2026-10-07-tlaps-m2/gen-proof.py)
followed by M3 -- Inv_ShortcutSound and Inv_ReaderSound over IndM3 = IndM2 + the
cheap path's record and what a cased writer carries to its finish
(results/2026-10-07-tlaps-m3/MCLeanP1M3.tla, TLC-checked; NOTES.txt).  Every
step states one event (SeqEv: the pointer never goes back and moves when the
document does) and its written tree R; one lemma (M3Write) turns five facts
about R into M3'.
Run from lean/formal:  python3 results/2026-10-07-tlaps-m3/gen-proof.py LeanP1Proof.tla"""
import sys, runpy, os, tempfile
OUT = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
_argv = sys.argv
sys.argv = [_argv[0], os.path.join(tempfile.mkdtemp(), 'm2.tla')]
m2 = runpy.run_path(os.path.join(HERE, '..', '2026-10-07-tlaps-m2', 'gen-proof.py'), run_name='m2')
sys.argv = _argv
END = m2['END']
PRE = m2['PRE'] + m2['M2'] + m2['BODY'] + m2['TAIL2']
_old = "   and one lemma per conjunct consumes them.                              *)\n"
assert PRE.count(_old) == 1
PRE = PRE.replace(_old,
    "   and one lemma per conjunct consumes them.\n"
    "   M3 (after M2): `Inv_ShortcutSound` and `Inv_ReaderSound` over `IndM3`\n"
    "   -- IndM2 and the plan's I8-I10 as the proof needs them (results/\n"
    "   2026-10-07-tlaps-m3/NOTES.txt): while the pointer is the one a writer\n"
    "   last derived against, every held path where the document and the\n"
    "   baseline differ is skipped; a cased writer carries what its finish\n"
    "   needs from the CAS.  Each step states one event (`SeqEv`) and its\n"
    "   written tree; `M3Write` turns five facts about that tree into M3'.  *)\n")

M3 = r'''
------------------------------------------------------------------------------
(* M3: Inv_ShortcutSound, Inv_ReaderSound.                                  *)

\* seq starts at 1, so a record of 0 ("nothing derived") never matches it.
M3Seq == seq >= 1
\* I10 and I9's bounds, for every tree (a non-reader's memo stays 0).
Bounds == \A s \in Writers :
            /\ w[s].derived <= seq /\ w[s].memo <= seq /\ w[s].rnow <= seq
            /\ (w[s].derived # 0 => w[s].memo <= w[s].derived)
\* I8: the cheap path's record.
Record == \A s \in Writers : (w[s].derived = seq /\ w[s].sStage = "none") =>
            \A p \in Paths : (Held(s, p) /\ doc[p] # w[s].baseline[p]) => p \in w[s].skipped
\* Between the CAS and the finish: the install's own paths hold the snapshot;
\* with `adv`, every other held path where the INSTALLED document differs
\* from the baseline is skipped; a record still current means the install
\* moved nothing.
CasedMine == \A s \in Writers : w[s].pc = "cased" =>
               \A p \in w[s].uploads \cap w[s].upDone : w[s].inst[p] = w[s].snap[p]
CasedAdv == \A s \in Writers : (w[s].pc = "cased" /\ w[s].adv) =>
              \A p \in Paths \ ((w[s].uploads \cap w[s].upDone) \cup w[s].deletes) :
                (Held(s, p) /\ w[s].inst[p] # w[s].baseline[p]) => p \in w[s].skipped
CasedSame == \A s \in Writers : (w[s].pc = "cased" /\ w[s].derived = seq) =>
               w[s].adv /\ w[s].inst = doc
M3 == M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame
IndM3 == IndM2 /\ M3

\* The step's event: the pointer never goes back, and it moves when the
\* document does.
SeqEv == seq' \in Nat /\ seq <= seq' /\ (seq' = seq => doc' = doc)
\* What the written tree R must satisfy, read off R (the primed pointer and
\* document are the step's).
HeldR(R, p) == R.baseline[p] # Nil \/ p \in R.scope
RB(R) == /\ R.derived <= seq' /\ R.memo <= seq' /\ R.rnow <= seq'
         /\ (R.derived # 0 => R.memo <= R.derived)
RRec(R) == (R.derived = seq' /\ R.sStage = "none") =>
             \A p \in Paths : (HeldR(R, p) /\ doc'[p] # R.baseline[p]) => p \in R.skipped
RMine(R) == R.pc = "cased" => \A p \in R.uploads \cap R.upDone : R.inst[p] = R.snap[p]
RAdv(R) == (R.pc = "cased" /\ R.adv) =>
             \A p \in Paths \ ((R.uploads \cap R.upDone) \cup R.deletes) :
               (HeldR(R, p) /\ R.inst[p] # R.baseline[p]) => p \in R.skipped
RSame(R) == (R.pc = "cased" /\ R.derived = seq') => R.adv /\ R.inst = doc'

------------------------------------------------------------------------------
(* What the event and the tree give M3.                                     *)

LEMMA M3Write ==
  ASSUME M3, seq \in Nat, SeqEv, w \in [Writers -> Writer], w' \in [Writers -> Writer],
         NEW t, NEW R, Wr(t, R),
         t \in Writers => RB(R) /\ RRec(R) /\ RMine(R) /\ RAdv(R) /\ RSame(R)
  PROVE  M3'
<1>0. seq' \in Nat /\ seq <= seq' /\ (seq' = seq => doc' = doc) BY DEF SeqEv
<1>1. M3Seq' BY <1>0 DEF M3, M3Seq
<1>n. \A u \in Writers : w[u].derived \in Nat /\ w[u].memo \in Nat /\ w[u].rnow \in Nat
  <2>. SUFFICES ASSUME NEW u \in Writers PROVE w[u].derived \in Nat /\ w[u].memo \in Nat /\ w[u].rnow \in Nat
    OBVIOUS
  <2>1. w[u] \in Writer OBVIOUS
  <2>. QED BY <2>1, WriterFields
<1>k. \A u \in Writers : u # t => w'[u] = w[u] BY DEF Wr
<1>t. t \in Writers => w'[t] = R BY DEF Wr
<1>2. Bounds'
  <2>. SUFFICES ASSUME NEW u \in Writers
                PROVE  /\ w'[u].derived <= seq' /\ w'[u].memo <= seq' /\ w'[u].rnow <= seq'
                       /\ (w'[u].derived # 0 => w'[u].memo <= w'[u].derived)
    BY DEF Bounds
  <2>1. CASE u = t BY <2>1, <1>t DEF RB
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. /\ w[u].derived <= seq /\ w[u].memo <= seq /\ w[u].rnow <= seq
          /\ (w[u].derived # 0 => w[u].memo <= w[u].derived)
      BY DEF M3, Bounds
    <3>. QED BY <3>1, <3>2, <1>0, <1>n
  <2>. QED BY <2>1, <2>2
<1>3. Record'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].derived = seq', w'[u].sStage = "none",
                       NEW p \in Paths, w'[u].baseline[p] # Nil \/ p \in w'[u].scope,
                       doc'[p] # w'[u].baseline[p]
                PROVE  p \in w'[u].skipped
    BY DEF Record, Held
  <2>1. CASE u = t BY <2>1, <1>t DEF RRec, HeldR
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. w[u].derived <= seq BY DEF M3, Bounds
    <3>3. seq' = seq /\ doc' = doc BY <3>1, <3>2, <1>0, <1>n
    <3>. QED BY <3>1, <3>3 DEF M3, Record, Held
  <2>. QED BY <2>1, <2>2
<1>4. CasedMine'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased"
                PROVE  \A p \in w'[u].uploads \cap w'[u].upDone : w'[u].inst[p] = w'[u].snap[p]
    BY DEF CasedMine
  <2>1. CASE u = t BY <2>1, <1>t DEF RMine
  <2>2. CASE u # t BY <2>2, <1>k DEF M3, CasedMine
  <2>. QED BY <2>1, <2>2
<1>5. CasedAdv'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased", w'[u].adv,
                       NEW p \in Paths \ ((w'[u].uploads \cap w'[u].upDone) \cup w'[u].deletes),
                       w'[u].baseline[p] # Nil \/ p \in w'[u].scope,
                       w'[u].inst[p] # w'[u].baseline[p]
                PROVE  p \in w'[u].skipped
    BY DEF CasedAdv, Held
  <2>1. CASE u = t BY <2>1, <1>t DEF RAdv, HeldR
  <2>2. CASE u # t BY <2>2, <1>k DEF M3, CasedAdv, Held
  <2>. QED BY <2>1, <2>2
<1>6. CasedSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased", w'[u].derived = seq'
                PROVE  w'[u].adv /\ w'[u].inst = doc'
    BY DEF CasedSame
  <2>1. CASE u = t BY <2>1, <1>t DEF RSame
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. w[u].derived <= seq BY DEF M3, Bounds
    <3>3. seq' = seq /\ doc' = doc BY <3>1, <3>2, <1>0, <1>n
    <3>. QED BY <3>1, <3>3 DEF M3, CasedSame
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF M3

\* M3 after a step that moves nothing M3 reads.
LEMMA M3Same ==
  ASSUME M3, UNCHANGED <<doc, seq, w>>
  PROVE  M3'
BY DEF M3, M3Seq, Bounds, Record, CasedMine, CasedAdv, CasedSame, Held

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M3 == Init => IndM3
<1>. SUFFICES ASSUME Init PROVE IndM3 OBVIOUS
<1>1. IndM2 BY Init_M2
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>3. \A s \in Writers : /\ w[s].pc = "idle" /\ w[s].derived = 0 /\ w[s].memo = 0 /\ w[s].rnow = 0
  BY <1>2 DEF WriterInit
<1>4. seq = 1 BY DEF Init
<1>5. M3Seq /\ Bounds BY <1>3, <1>4 DEF M3Seq, Bounds
<1>6. Record BY <1>3, <1>4 DEF Record
<1>7. CasedMine /\ CasedAdv /\ CasedSame BY <1>3 DEF CasedMine, CasedAdv, CasedSame
<1>. QED BY <1>1, <1>5, <1>6, <1>7 DEF IndM3, M3

'''

# ---------------------------------------------------------------------------
# Step lemmas.

HEAD3 = '''<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, {m2}
<1>c. M3Seq /\\ Bounds /\\ Record /\\ CasedMine /\\ CasedAdv /\\ CasedSame BY DEF IndM3, M3
<1>y. /\\ w \\in [Writers -> Writer] /\\ seq \\in Nat /\\ doc \\in [Paths -> Opt(Handles)] /\\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \\in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
'''

def nostep3(name, sig, binders, restate, frame='Frame'):
    """A step that writes no tree."""
    m2 = f'{name}_M2'
    return f'''LEMMA {name}_M3 ==
  ASSUME IndM3{binders}, {sig}, {frame}
  PROVE  IndM3'
{HEAD3.format(m2=m2)}<1>1. {restate}
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

'''

FA = ['pc', 'derived', 'memo', 'rnow', 'sStage', 'scope', 'skipped']
FB = ['baseline', 'uploads', 'upDone', 'deletes', 'snap', 'inst', 'adv']

def reads(rec, changed, fields):
    out = []
    for f in fields:
        v = changed.get(f, f'w[s].{f}')
        if v is None:
            continue
        out.append(f'{rec}.{f} = {v}')
    return out

def step3(name, sig, binders, rec, recdef, restate, guards, guard_defs, changed,
          L=1, pre='', head=True, rules='', by=None, nost=False):
    """A writer step: field reads of the new tree, then its five facts."""
    by = by or {}
    I = '  ' * (L - 1)
    def S(n): return f'<{L}>{n}'
    t = ''
    if head:
        t += f'''LEMMA {name}_M3 ==
  ASSUME IndM3, NEW s \\in Writers{binders}, {sig}, Frame
  PROVE  IndM3'
{HEAD3.format(m2=name + "_M2")}<1>w. w[s] \\in Writer BY <1>y
<1>f. /\\ w[s].derived \\in Nat /\\ w[s].memo \\in Nat /\\ w[s].rnow \\in Nat
      /\\ w[s].baseline \\in [Paths -> Opt(Handles)] /\\ w[s].local \\in [Paths -> Opt(Handles)]
      /\\ w[s].scope \\subseteq Paths /\\ w[s].skipped \\subseteq Paths
      /\\ w[s].uploads \\subseteq Paths /\\ w[s].upDone \\subseteq Paths /\\ w[s].deletes \\subseteq Paths /\\ w[s].gone \\subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\\ Off /\\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
'''
        if rules:
            t += f'<1>s. {rules} BY ShippedShape DEF Shipped\n'
    t += pre
    t += f'{I}{S(1)}. {restate}\n'
    t += f'{I}{S("g")}. {guards} BY DEF {guard_defs}\n'
    t += f'{I}{S(2)}. Wr(s, {rec}) BY <1>y, {S(1)}, WriteAny DEF Wr\n'
    ra = reads(rec, changed, FA)
    rb = reads(rec, changed, FB)
    t += f'{I}{S(3)}. /\\ ' + f'\n{I}      /\\ '.join(ra) + f'\n{I}  BY DEF {recdef}\n'
    if rb:
        t += f'{I}{S("3b")}. /\\ ' + f'\n{I}      /\\ '.join(rb) + f'\n{I}  BY DEF {recdef}\n'
    R3 = f'{S(3)}' + (f', {S("3b")}' if rb else '')
    t += f'{I}{S("e")}. SeqEv BY {by.get("e", S(1) + ", <1>y DEF SeqEv")}\n'
    t += f'{I}{S(4)}. RB({rec}) BY {by.get("rb", S(1) + ", " + R3 + ", <1>c, <1>f, <1>y DEF RB, Bounds")}\n'
    t += f'{I}{S(5)}. RRec({rec}) {by.get("rrec_proof") or ("BY " + by.get("rrec", S(1) + ", " + R3 + ", <1>c DEF RRec, Record, HeldR, Held"))}\n'
    t += f'{I}{S(6)}. RMine({rec}) {by.get("rmine_proof") or ("BY " + by.get("rmine", S(1) + ", " + S("g") + ", " + R3 + ", <1>c DEF RMine, CasedMine"))}\n'
    t += f'{I}{S(7)}. RAdv({rec}) {by.get("radv_proof") or ("BY " + by.get("radv", S(1) + ", " + S("g") + ", " + R3 + ", <1>c DEF RAdv, CasedAdv, HeldR, Held"))}\n'
    t += f'{I}{S(8)}. RSame({rec}) {by.get("rsame_proof") or ("BY " + by.get("rsame", S(1) + ", " + S("g") + ", " + R3 + ", <1>c DEF RSame, CasedSame"))}\n'
    t += f'{I}{S("")}. QED BY <1>a, <1>c, <1>y, <1>t2, {S(2)}, {S("e")}, {S(4)}, {S(5)}, {S(6)}, {S(7)}, {S(8)}, M3Write DEF IndM3, M3\n'
    return t

def W3(*a, **k):
    return step3(*a, **k) + '\n'

# Vacuous cased facts for a tree whose pc is not "cased".
def notcased(n=1):
    return {}

BODY = ''
BODY += '------------------------------------------------------------------------------\n(* M3: the gateway and the steps that write no tree.                        *)\n\n'
BODY += nostep3('GPut', 'GPut(p)', ', NEW p \\in Paths', 'w\' = w /\\ UNCHANGED <<doc, seq>> BY DEF GPut')
BODY += nostep3('GCas', 'GCas(p)', ', NEW p \\in Paths',
                "w' = w /\\ (seq' = seq + 1 \\/ (seq' = seq /\\ doc' = doc)) BY DEF GCas")
BODY += nostep3('GRename', 'GRename(p, q)', ', NEW p \\in Paths, NEW q \\in Paths',
                "w' = w /\\ seq' = seq + 1 BY DEF GRename")
BODY += nostep3('GRenameFinish', 'GRenameFinish', '', "w' = w /\\ seq' = seq + 1 BY DEF GRenameFinish")
BODY += nostep3('GDelete', 'GDelete(p)', ', NEW p \\in Paths', "w' = w /\\ seq' = seq + 1 BY DEF GDelete")
BODY += nostep3('Sweep', 'Sweep(s, h)', ', NEW s \\in Writers, NEW h \\in Handles',
                "w' = w /\\ UNCHANGED <<doc, seq>> BY DEF Sweep")
BODY += nostep3('Reap', 'Reap(s, h)', ', NEW s \\in Writers, NEW h \\in Handles',
                "w' = w /\\ UNCHANGED <<doc, seq>> BY DEF Reap", frame='UNCHANGED anc')
BODY += nostep3('Age', 'Age', '', "w' = w /\\ UNCHANGED <<doc, seq>> BY DEF Age", frame='UNCHANGED anc')
BODY += nostep3('RLoad', 'RLoad', '', "w' = w /\\ UNCHANGED <<doc, seq>> BY DEF RLoad", frame='UNCHANGED anc')

KEEP = "UNCHANGED <<doc, seq>>"
BODY += '------------------------------------------------------------------------------\n(* M3: the agent and the commit section.                                    *)\n\n'
BODY += W3('Edit', 'Edit(s, p)', ', NEW p \\in Paths', 'EditW(s, p)', 'EditW',
           f"w' = [w EXCEPT ![s] = EditW(s, p)] /\\ {KEEP} BY DEF Edit", 'On(s)', 'Edit', {})
BODY += W3('Delete', 'Delete(s, p)', ', NEW p \\in Paths', 'DeleteW(s, p)', 'DeleteW',
           f"w' = [w EXCEPT ![s] = DeleteW(s, p)] /\\ {KEEP} BY DEF Delete, bucket", 'On(s)', 'Delete', {})
BODY += W3('Checkout', 'Checkout(s)', '', 'CheckoutW(s, T)', 'CheckoutW',
           f"PICK T \\in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\\ {KEEP} BY DEF Checkout, bucket",
           'w[s].st = "off"', 'Checkout',
           {'derived': 'seq', 'scope': 'T', 'skipped': '{}', 'baseline': 'CheckoutHeld(s, T)', 'inst': 'doc'},
           by={'rrec': "<1>1, <1>3, <1>3b DEF RRec, HeldR, CheckoutHeld",
               # an off writer is WriterInit: idle
               'rmine': "<1>3, <1>g, <1>m DEF RMine, Off, WriterInit",
               'radv': "<1>3, <1>g, <1>m DEF RAdv, Off, WriterInit",
               'rsame': "<1>3, <1>g, <1>m DEF RSame, Off, WriterInit"})

# Consume: two trees.
BODY += f'''LEMMA Consume_M3 ==
  ASSUME IndM3, NEW s \\in Writers, Consume(s), Frame
  PROVE  IndM3'
{HEAD3.format(m2="Consume_M2")}<1>w. w[s] \\in Writer BY <1>y
<1>f. /\\ w[s].derived \\in Nat /\\ w[s].memo \\in Nat /\\ w[s].rnow \\in Nat
      /\\ w[s].baseline \\in [Paths -> Opt(Handles)] /\\ w[s].local \\in [Paths -> Opt(Handles)]
      /\\ w[s].scope \\subseteq Paths /\\ w[s].skipped \\subseteq Paths
      /\\ w[s].uploads \\subseteq Paths /\\ w[s].upDone \\subseteq Paths /\\ w[s].deletes \\subseteq Paths /\\ w[s].gone \\subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\\ Off /\\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. ConsumeKeepsLeft /\\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. CASE CheapPath(s)
'''
BODY += step3('Consume', 'Consume(s)', '', 'ConsumeCheapW(s)', 'ConsumeCheapW',
              f"w' = [w EXCEPT ![s] = ConsumeCheapW(s)] /\\ {KEEP} BY <1>1 DEF Consume",
              'On(s) /\\ w[s].pc = "idle"', 'Consume', {'pc': '"consumed"'}, L=2, head=False)
BODY += '<1>2. CASE ~CheapPath(s)\n'
_crec = '''
    <3>1. ASSUME ConsumeW(s, fail).derived = seq' PROVE fail = {}
      BY <3>1, <2>1, <2>3, <1>s, <1>c DEF M3Seq
    <3>2. ASSUME NEW p \\in Paths, fail = {},
                 HeldR(ConsumeW(s, fail), p), doc'[p] # ConsumeW(s, fail).baseline[p]
          PROVE  p \\in ConsumeW(s, fail).skipped
      <4>1. p \\notin ConsumeTaken(s, fail) BY <3>2, <2>1, <2>3b
      <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF ConsumeTaken, ConsumeOwed
      <4>3. ConsumeW(s, fail).baseline[p] = w[s].baseline[p] BY <4>1, <2>3b
      <4>4. Held(s, p) BY <3>2, <4>3, <2>3 DEF HeldR, Held
      <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <2>1, <1>s DEF Owed
      <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <2>1, <2>3
    <3>. QED BY <3>1, <3>2 DEF RRec'''
BODY += step3('Consume', 'Consume(s)', '', 'ConsumeW(s, fail)', 'ConsumeW',
              f"PICK fail \\in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] /\\ {KEEP} BY <1>2 DEF Consume",
              'On(s) /\\ w[s].pc = "idle"', 'Consume',
              {'pc': '"consumed"',
               'derived': 'IF fail # {} /\\ ConsumeKeepsLeft THEN 0 ELSE seq',
               'skipped': '{q \\in Paths \\ ConsumeTaken(s, fail) : doc[q] # w[s].baseline[q] /\\ Held(s, q)\n'
                          '                                              /\\ w[s].local[q] # w[s].baseline[q]}',
               'baseline': '[q \\in Paths |-> IF q \\in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]]'},
              L=2, head=False,
              by={'rb': "<2>1, <2>3, <1>c, <1>f, <1>y DEF RB, Bounds", 'rrec_proof': _crec})
BODY += '<1>. QED BY <1>1, <1>2\n\n'

BODY += W3('Scan', 'Scan(s)', '', 'ScanW(s, dels)', 'ScanW',
           f"PICK dels \\in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\\ {KEEP} BY DEF Scan, bucket",
           'On(s) /\\ w[s].pc = "consumed"', 'Scan',
           {'pc': '"scanned"', 'uploads': None, 'deletes': None, 'snap': None, 'upDone': None})
BODY += W3('Skip', 'Skip(s)', '', 'SkipW(s)', 'SkipW',
           f"w' = [w EXCEPT ![s] = SkipW(s)] /\\ {KEEP} BY DEF Skip, bucket",
           'On(s) /\\ w[s].pc = "consumed"', 'Skip', {'pc': '"idle"'})

# Upload: two trees.
BODY += f'''LEMMA Upload_M3 ==
  ASSUME IndM3, NEW s \\in Writers, NEW p \\in Paths, Upload(s, p), Frame
  PROVE  IndM3'
{HEAD3.format(m2="Upload_M2")}<1>w. w[s] \\in Writer BY <1>y
<1>f. /\\ w[s].derived \\in Nat /\\ w[s].memo \\in Nat /\\ w[s].rnow \\in Nat
      /\\ w[s].baseline \\in [Paths -> Opt(Handles)] /\\ w[s].local \\in [Paths -> Opt(Handles)]
      /\\ w[s].scope \\subseteq Paths /\\ w[s].skipped \\subseteq Paths
      /\\ w[s].uploads \\subseteq Paths /\\ w[s].upDone \\subseteq Paths /\\ w[s].deletes \\subseteq Paths /\\ w[s].gone \\subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\\ Off /\\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. CASE w[s].snap[p] \\notin upped
'''
BODY += step3('Upload', 'Upload(s, p)', ', NEW p \\in Paths', 'UploadW(s, p)', 'UploadW',
              f"w' = [w EXCEPT ![s] = UploadW(s, p)] /\\ {KEEP} BY <1>1 DEF Upload",
              'On(s) /\\ w[s].pc = "scanned"', 'Upload', {'upDone': None}, L=2, head=False)
BODY += '<1>2. CASE w[s].snap[p] \\in upped\n  <2>. DEFINE c == <<p, MaxMint + copies + 1>>\n'
BODY += step3('Upload', 'Upload(s, p)', ', NEW p \\in Paths', 'UploadCopyW(s, p, c)', 'UploadCopyW',
              f"w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\\ {KEEP} BY <1>2 DEF Upload",
              'On(s) /\\ w[s].pc = "scanned"', 'Upload', {'upDone': None, 'snap': None}, L=2, head=False)
BODY += '<1>. QED BY <1>1, <1>2\n\n'

BODY += W3('PullOnly', 'PullOnly(s)', '', 'PullOnlyW(s)', 'PullOnlyW',
           f"w' = [w EXCEPT ![s] = PullOnlyW(s)] /\\ {KEEP} BY DEF PullOnly, bucket",
           'On(s) /\\ w[s].pc = "scanned"', 'PullOnly',
           {'pc': '"idle"', 'upDone': None, 'snap': None, 'inst': None})
BODY += W3('Claim', 'Claim(s)', '', 'ClaimW(s)', 'ClaimW',
           f"w' = [w EXCEPT ![s] = ClaimW(s)] /\\ {KEEP} BY DEF Claim",
           'On(s) /\\ w[s].pc = "scanned"', 'Claim', {'pc': '"claimed"'})
BODY += W3('Verify', 'Verify(s)', '', 'VerifyW(s)', 'VerifyW',
           f"w' = [w EXCEPT ![s] = VerifyW(s)] /\\ {KEEP} BY DEF Verify, bucket",
           'On(s) /\\ w[s].pc = "claimed"', 'Verify', {})

# Install: the one writer step that moves the document.
_inst_pre = '''<1>r1. w[s].sStage = "none" BY <1>m, <1>g0 DEF R1
<1>ii. \\A k \\in Paths : k \\notin (w[s].uploads \\cap (w[s].upDone \\ w[s].gone)) \\cup w[s].deletes
                       => InstallInst(s)[k] = doc[k]
  BY DEF InstallInst, InstallMine
'''
_inst_radv = '''
  <2>1. ASSUME InstallW(s).adv PROVE w[s].derived = seq BY <2>1, <1>3b, <1>s
  <2>2. ASSUME InstallW(s).adv,
               NEW p \\in Paths \\ ((InstallW(s).uploads \\cap InstallW(s).upDone) \\cup InstallW(s).deletes),
               HeldR(InstallW(s), p), InstallW(s).inst[p] # InstallW(s).baseline[p]
        PROVE  p \\in InstallW(s).skipped
    <3>1. InstallInst(s)[p] = doc[p] BY <2>2, <1>3b, <1>ii
    <3>2. Held(s, p) /\\ doc[p] # w[s].baseline[p] BY <2>2, <3>1, <1>3, <1>3b DEF HeldR, Held
    <3>. QED BY <2>1, <2>2, <3>2, <1>3, <1>r1, <1>c DEF Record
  <2>. QED BY <2>1, <2>2 DEF RAdv'''
_inst_rrec = '''
  <2>1. ASSUME InstallW(s).derived = seq' PROVE seq' = seq /\\ doc' = doc
    BY <2>1, <1>3, <1>1, <1>e, <1>c, <1>f, <1>y DEF Bounds, SeqEv
  <2>. QED BY <2>1, <1>3, <1>3b, <1>r1, <1>c DEF RRec, Record, HeldR, Held'''
_inst_rsame = '''
  <2>1. ASSUME InstallW(s).derived = seq' PROVE seq' = seq /\\ w[s].derived = seq
    BY <2>1, <1>3, <1>1, <1>e, <1>c, <1>f, <1>y DEF Bounds, SeqEv
  <2>. QED BY <2>1, <1>1, <1>3, <1>3b, <1>s DEF RSame'''
_inst_rmine = '''
  <2>. SUFFICES ASSUME NEW p \\in w[s].uploads \\cap (w[s].upDone \\ w[s].gone) PROVE InstallInst(s)[p] = w[s].snap[p]
    BY <1>3, <1>3b DEF RMine
  <2>1. p \\in Paths /\\ p \\notin w[s].gone /\\ p \\in InstallMine(s) BY <1>f DEF InstallMine
  <2>. QED BY <2>1 DEF InstallInst'''
BODY += W3('Install', 'Install(s)', '', 'InstallW(s)', 'InstallW',
           "/\\ doc' = InstallInst(s) /\\ w' = [w EXCEPT ![s] = InstallW(s)]\n"
           "      /\\ seq' = IF InstallInst(s) = doc THEN seq ELSE seq + 1\n  BY DEF Install",
           'On(s) /\\ w[s].pc = "claimed"', 'Install',
           {'pc': '"cased"', 'upDone': 'w[s].upDone \\ w[s].gone', 'inst': 'InstallInst(s)',
            'adv': 'IF CommitAdvanceGuarded THEN seq = w[s].derived ELSE TRUE'},
           rules='CommitAdvanceGuarded',
           pre='<1>g0. w[s].pc = "claimed" BY DEF Install\n' + _inst_pre,
           by={'e': "<1>1, <1>y DEF SeqEv",
               'rb': "<1>1, <1>3, <1>e, <1>c, <1>f, <1>y DEF RB, Bounds, SeqEv",
               'rrec_proof': _inst_rrec,
               'rmine_proof': _inst_rmine,
               'radv_proof': _inst_radv,
               'rsame_proof': _inst_rsame})
BODY += W3('Collect', 'Collect(s)', '', 'CollectW(s)', 'CollectW',
           f"w' = [w EXCEPT ![s] = CollectW(s)] /\\ {KEEP} BY DEF Collect",
           'On(s) /\\ w[s].pc = "cased"', 'Collect', {})

# Finish: the record advances over the installed document.
_fin_rrec = '''
  <2>1. w[s].sStage = "none" /\\ w[s].pc = "cased" BY <1>g, <1>m DEF R1
  <2>2. CASE w[s].adv /\\ w[s].inst = doc
    <3>. SUFFICES ASSUME NEW p \\in Paths, HeldR(FinishW(s), p), doc[p] # FinishW(s).baseline[p]
                  PROVE  p \\in FinishW(s).skipped
      BY <1>1 DEF RRec
    <3>1. p \\notin w[s].uploads \\cap w[s].upDone
      BY <2>1, <2>2, <1>3b, <1>c DEF CasedMine
    <3>2. p \\notin w[s].deletes
      BY <2>1, <2>2, <3>1, <1>3b, <1>m DEF Cased
    <3>3. FinishW(s).baseline[p] = w[s].baseline[p] BY <3>1, <3>2, <1>3b
    <3>4. Held(s, p) /\\ w[s].inst[p] # w[s].baseline[p] BY <2>2, <3>3, <1>3 DEF HeldR, Held
    <3>5. p \\in w[s].skipped BY <2>1, <2>2, <3>1, <3>2, <3>4, <1>c DEF CasedAdv
    <3>. QED BY <2>2, <3>1, <3>2, <3>5, <1>3
  <2>3. CASE ~(w[s].adv /\\ w[s].inst = doc)
    <3>1. w[s].derived # seq BY <2>1, <2>3, <1>c DEF CasedSame
    <3>. QED BY <2>3, <3>1, <1>1, <1>3 DEF RRec
  <2>. QED BY <2>2, <2>3'''
BODY += W3('Finish', 'Finish(s)', '', 'FinishW(s)', 'FinishW',
           f"w' = [w EXCEPT ![s] = FinishW(s)] /\\ {KEEP} BY DEF Finish",
           'On(s) /\\ w[s].pc = "cased"', 'Finish',
           {'pc': '"idle"',
            'derived': 'IF w[s].adv /\\ w[s].inst = doc THEN seq ELSE w[s].derived',
            'skipped': 'IF w[s].adv /\\ w[s].inst = doc\n'
                       '                 THEN w[s].skipped \\ ((w[s].uploads \\cap w[s].upDone) \\cup {q \\in w[s].deletes : w[s].inst[q] = Nil})\n'
                       '                 ELSE w[s].skipped',
            'baseline': '[q \\in Paths |-> IF q \\in w[s].uploads \\cap w[s].upDone THEN w[s].snap[q]\n'
                        '                                ELSE IF q \\in w[s].deletes /\\ w[s].inst[q] = Nil THEN Nil\n'
                        '                                ELSE w[s].baseline[q]]',
            'uploads': None, 'upDone': None, 'deletes': None, 'snap': None, 'adv': None},
           by={'rrec_proof': _fin_rrec})

BODY += '------------------------------------------------------------------------------\n(* M3: the restart and the sync.                                            *)\n\n'
BODY += W3('Restart', 'Restart(s)', '', 'RestartW(s)', 'RestartW',
           f"w' = [w EXCEPT ![s] = RestartW(s)] /\\ {KEEP} BY DEF Restart",
           'On(s)', 'Restart',
           {'pc': '"idle"', 'sStage': 'IF w[s].sStage = "none" THEN "none" ELSE "saved"',
            'uploads': None, 'upDone': None, 'deletes': None, 'snap': None, 'inst': None, 'adv': None})

def _syncrec(rec):
    return f'''
    <3>1. ASSUME {rec}.derived = seq' PROVE fail = {{}}
      BY <3>1, <1>1, <1>3, <1>s, <1>c DEF M3Seq
    <3>2. ASSUME NEW p \\in Paths, fail = {{}},
                 HeldR({rec}, p), doc'[p] # {rec}.baseline[p]
          PROVE  p \\in {rec}.skipped
      <4>1. p \\notin SyncOwed(s, fail) BY <3>2, <1>1, <1>3b DEF SyncBl
      <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF SyncOwed, SyncAll
      <4>3. {rec}.baseline[p] = w[s].baseline[p] BY <4>1, <1>3b DEF SyncBl
      <4>4. Held(s, p) BY <3>2, <4>3, <1>3 DEF HeldR, Held
      <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <1>1, <1>s DEF Owed
      <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <1>1, <1>3 DEF SyncBl
    <3>. QED BY <3>1, <3>2 DEF RRec'''.replace('\n    ', '\n  ')

_sync_changed = {
    'derived': 'IF fail # {} /\\ SyncKeepsLeft THEN 0 ELSE seq',
    'skipped': 'IF fail # {} /\\ SyncKeepsLeft THEN {}\n'
               '              ELSE {q \\in Paths : doc[q] # SyncBl(s, fail)[q] /\\ w[s].local[q] # SyncBl(s, fail)[q] /\\ Held(s, q)}',
    'baseline': 'SyncBl(s, fail)'}
BODY += W3('Sync', 'Sync(s)', '', 'SyncW(s, fail)', 'SyncW',
           f"PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\\ {KEEP} BY DEF Sync, bucket",
           'On(s) /\\ w[s].pc = "idle"', 'Sync', _sync_changed,
           rules='SyncKeepsLeft /\\ ConsumeHonorsScope',
           by={'rb': "<1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds", 'rrec_proof': _syncrec('SyncW(s, fail)')})

BODY += '------------------------------------------------------------------------------\n(* M3: the narrow / widen verb.                                             *)\n\n'
BODY += W3('RescopeBegin', 'RescopeBegin(s)', '', 'RescopeBeginW(s, T)', 'RescopeBeginW',
           f"PICK T \\in Scopes \\ {{w[s].scope}} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\\ {KEEP} BY DEF RescopeBegin, bucket",
           'On(s) /\\ w[s].pc = "idle"', 'RescopeBegin', {'sStage': '"saved"'})
BODY += W3('RescopeFirst', 'RescopeFirst(s)', '', 'RescopeFirstW(s)', 'RescopeFirstW',
           f"w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\\ {KEEP} BY DEF RescopeFirst, bucket",
           'On(s) /\\ w[s].pc = "idle"', 'RescopeFirst', {'sStage': '"mid"', 'baseline': None})
BODY += W3('RescopeSecond', 'RescopeSecond(s)', '', 'RescopeSecondW(s, wfail)', 'RescopeSecondW',
           "PICK wfail \\in SUBSET {q \\in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \\/ ~WidenKeepsLocal} :\n"
           f"        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\\ {KEEP}\n  BY DEF RescopeSecond, bucket",
           'On(s) /\\ w[s].pc = "idle"', 'RescopeSecond',
           {'derived': '0', 'sStage': '"none"', 'scope': None, 'skipped': '{}', 'baseline': None},
           by={'rb': "<1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds",
               'rrec': "<1>1, <1>3, <1>c DEF RRec, M3Seq"})

BODY += '------------------------------------------------------------------------------\n(* M3: a reader\'s tick.                                                     *)\n\n'
BODY += W3('RPullRead', 'RPullRead(s)', '', 'RPullReadW(s)', 'RPullReadW',
           f"w' = [w EXCEPT ![s] = RPullReadW(s)] /\\ {KEEP} BY DEF RPullRead, bucket",
           'On(s) /\\ w[s].pc = "idle"', 'RPullRead', {'pc': '"pulling"', 'rnow': 'seq'})
_rps = dict(_sync_changed)
_rps.update({'pc': '"idle"', 'memo': 'w[s].rnow'})
BODY += W3('RPullSync', 'RPullSync(s)', '', 'RPullSyncW(s, fail)', 'RPullSyncW',
           f"PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\\ {KEEP} BY DEF RPullSync, bucket",
           'On(s) /\\ w[s].pc = "pulling"', 'RPullSync', _rps,
           rules='SyncKeepsLeft /\\ ConsumeHonorsScope',
           by={'rb': "<1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds", 'rrec_proof': _syncrec('RPullSyncW(s, fail)')})

TAIL3 = m2['TAIL2']
# Next_M3, M3Invariant: M2's text with M2 -> M3 (the step lemmas carry the same names).
_s = TAIL3.index('LEMMA Next_M2')
_e = TAIL3.index('THEOREM CitationsLive')
TAIL3 = TAIL3[_s:_e].replace('IndM2', 'IndM3').replace('_M2', '_M3').replace('M2Invariant', 'M3Invariant')
TAIL3 = TAIL3.replace("    <3>3. IndM3 /\\ UNCHANGED vars => M2' BY M2Same DEF IndM3, vars, aux, ret\n"
                      "    <3>. QED BY <3>1, <3>2, <3>3 DEF IndM3, IndM1\n",
                      "    <3>3. IndM3 /\\ UNCHANGED vars => M2' BY M2Same DEF IndM3, IndM2, vars, aux, ret\n"
                      "    <3>4. IndM3 /\\ UNCHANGED vars => M3' BY M3Same DEF IndM3, vars, bucket\n"
                      "    <3>. QED BY <3>1, <3>2, <3>3, <3>4 DEF IndM3, IndM2, IndM1\n")
TAIL3 = TAIL3.replace("      BY DEF IndM3, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret\n",
                      "      BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret\n")
TAIL3 = TAIL3.replace("    <3>2. IndM3 /\\ UNCHANGED vars => M1' BY M1Keep DEF IndM3, IndM1, vars\n",
                      "    <3>2. IndM3 /\\ UNCHANGED vars => M1' BY M1Keep DEF IndM3, IndM2, IndM1, vars\n")
assert "M3Same" in TAIL3 and "IndM3, IndM2, IndM1, IndTypeOK" in TAIL3 and "M1Keep DEF IndM3, IndM2" in TAIL3
TAIL3 = '------------------------------------------------------------------------------\n(* M3: the step, and the theorems.                                          *)\n\n' + TAIL3
TAIL3 += r'''\* The cheap path skips only a writer owed nothing: an owed path is held and
\* differs from the baseline, so the record has it skipped, and the cheap
\* path re-checks every skipped path as dirty.
LEMMA ShortcutFromRecord == IndM3 => Inv_ShortcutSound
<1>. SUFFICES ASSUME IndM3, NEW s \in Writers, On(s), w[s].pc = "idle", w[s].sStage = "none", CheapPath(s),
                     NEW p \in Paths, Owed(s, p)
              PROVE  FALSE
  BY DEF Inv_ShortcutSound
<1>s. RecheckSkipped /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. w[s].derived = seq /\ \A q \in w[s].skipped : w[s].local[q] # w[s].baseline[q] BY <1>s DEF CheapPath
<1>2. Held(s, p) /\ doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p] BY <1>s DEF Owed
<1>3. p \in w[s].skipped BY <1>1, <1>2 DEF IndM3, M3, Record
<1>. QED BY <1>1, <1>2, <1>3

\* A reader that skips a tick has `memo = seq` and something derived, so by
\* the bounds its record is current; it re-checks every skipped path.
LEMMA ReaderFromRecord == IndM3 => Inv_ReaderSound
<1>. SUFFICES ASSUME IndM3, NEW s \in Readers, On(s), w[s].pc = "idle", w[s].sStage = "none", ReaderSkips(s),
                     NEW p \in Paths, Owed(s, p)
              PROVE  FALSE
  BY DEF Inv_ReaderSound
<1>0. s \in Writers BY ReadersWriters
<1>s. ReaderRechecksOwed /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>y. w[s] \in Writer /\ seq \in Nat BY <1>0 DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>n. w[s].derived \in Nat /\ w[s].memo \in Nat BY <1>y, WriterFields
<1>1. w[s].memo = seq /\ w[s].derived # 0 /\ \A q \in w[s].skipped : w[s].local[q] # w[s].baseline[q]
  BY <1>s DEF ReaderSkips, StillOwed
<1>2. w[s].derived = seq BY <1>0, <1>1, <1>y, <1>n DEF IndM3, M3, Bounds
<1>3. Held(s, p) /\ doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p] BY <1>s DEF Owed
<1>4. p \in w[s].skipped BY <1>0, <1>2, <1>3 DEF IndM3, M3, Record
<1>. QED BY <1>1, <1>3, <1>4

THEOREM ShortcutSound == Spec => []Inv_ShortcutSound
BY M3Invariant, ShortcutFromRecord, PTL

THEOREM ReaderSound == Spec => []Inv_ReaderSound
BY M3Invariant, ReaderFromRecord, PTL
'''

open(OUT, 'w').write(PRE + M3 + BODY + TAIL3 + END)
print('written', OUT, len((PRE + M3 + BODY + TAIL3 + END).splitlines()), 'lines')
