#!/usr/bin/env python3
"""Emit lean/formal/LeanP1Proof.tla: M0 (results/2026-10-06-tlaps-typeok/gen-proof.py,
run as is) followed by M1 -- Inv_OneHolder, Prop_DeleteSettles,
Prop_NarrowNeverDeletes over IndM1 = IndTypeOK + I1 + I6 + I7 + six facts read
off the actions (results/2026-10-07-tlaps-m1/MCLeanP1M1.tla, TLC-checked).
Run from lean/formal:  python3 results/2026-10-07-tlaps-m1/gen-proof.py LeanP1Proof.tla"""
import sys, runpy, os
OUT = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
m0 = runpy.run_path(os.path.join(HERE, '..', '2026-10-06-tlaps-typeok', 'gen-proof.py'), run_name='m0')
END = '==============================================================================\n'
assert m0['TAIL'].endswith(END)
M0TEXT = m0['HEADER'] + m0['BODY'] + m0['TAIL'][:-len(END)]
M0TEXT = M0TEXT.replace(
    "   LeanP1Anc.tla states without names.                                     *)\n",
    "   LeanP1Anc.tla states without names.\n"
    "   M1 (after the TypeOK theorem): `Inv_OneHolder`, `Prop_DeleteSettles` and\n"
    "   `Prop_NarrowNeverDeletes` over `IndM1` -- IndTypeOK and the plan's I1, I6,\n"
    "   I7 with six facts read off the actions (TLC-checked first:\n"
    "   results/2026-10-07-tlaps-m1/).  One generic lemma per conjunct takes the\n"
    "   step's new tree as a parameter; a step lemma reads the tree's fields\n"
    "   once (`<1>3`) and discharges each lemma's hypothesis from them.        *)\n")
assert 'M1 (after' in M0TEXT

M1 = r'''
------------------------------------------------------------------------------
(* M1: Inv_OneHolder, Prop_DeleteSettles, Prop_NarrowNeverDeletes.          *)

CC == {"claimed", "cased"}
\* I1: the holder is in its commit section, and on.
Holder == holder # "none" => w[holder].pc \in CC /\ w[holder].st = "on"
\* I6: at the CAS every delete landed (DeleteWinsPreserved: a delete over a
\* foreign change applies).
Cased == \A s \in Writers : w[s].pc = "cased" => \A p \in w[s].deletes : w[s].inst[p] = Nil
\* The scan's two sets are disjoint; what the verify withheld is an upload.
Mine == \A s \in Writers : w[s].deletes \cap w[s].uploads = {} /\ w[s].gone \subseteq w[s].uploads
\* From the scan to the finish, no path the barrier publishes or deletes is
\* one a rescope unlinked (such a path is clean: I7).
Ups == \A s \in Writers : w[s].pc \in {"scanned", "claimed", "cased"} =>
         (w[s].uploads \cup w[s].deletes) \cap w[s].unlinked = {}
\* A writer that is off has never run: every step but Checkout needs On(s).
Off == \A s \in Writers : w[s].st = "off" => w[s] = WriterInit
\* I7a: the barrier runs with no rescope in flight.
R1 == \A s \in Writers : w[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w[s].sStage = "none"
\* I7b: a path a rescope unlinked is clean until the agent touches it.
R2 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage = "none") => w[s].local[p] = w[s].baseline[p]
\* I7b with a rescope in flight: clean, or uncited by the first half --
\* dropped and not kept, its baseline gone, its bytes those the intent
\* recorded (so the second half unlinks it).
R3 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage \in {"saved", "mid"}) =>
          \/ w[s].local[p] = w[s].baseline[p]
          \/ /\ p \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[p] = Nil
             /\ w[s].local[p] # Nil /\ w[s].sHeld[p] = w[s].local[p]
\* Between the halves, every path the first half dropped is uncited.
R4 == \A s \in Writers : w[s].sStage = "mid" => \A p \in w[s].sDrop \ w[s].sKeep : w[s].baseline[p] = Nil
\* A reader's pull never spans the halves.
R5 == \A s \in Writers : w[s].pc = "pulling" => w[s].sStage \in {"none", "saved"}
Rescope == R1 /\ R2 /\ R3 /\ R4 /\ R5
M1 == Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope
IndM1 == IndTypeOK /\ M1
\* Prop_NarrowNeverDeletes's step formula.
NarrowOK == \A s \in Writers :
              (w[s].pc = "consumed" /\ w'[s].pc = "scanned") => w'[s].deletes \cap w[s].unlinked = {}

------------------------------------------------------------------------------
(* One lemma per conjunct: what a step's new tree R must satisfy.           *)

\* The step writes one tree: what every tree becomes.
LEMMA WriteAny ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R]
  PROVE  \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t]
BY DEF TypeOK

\* A step that writes no tree and keeps the holder keeps M1.
LEMMA M1Keep ==
  ASSUME M1, w' = w, holder' = holder
  PROVE  M1' /\ NarrowOK
<1>1. Inv_OneHolder' /\ Holder' BY DEF M1, Inv_OneHolder, Holder
<1>2. Cased' /\ Mine' /\ Ups' /\ Off' BY DEF M1, Cased, Mine, Ups, Off
<1>3. R1' /\ R2' /\ R3' /\ R4' /\ R5' BY DEF M1, Rescope, R1, R2, R3, R4, R5
<1>4. NarrowOK BY DEF NarrowOK
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF M1, Rescope

\* The holder's two conjuncts after a step that keeps the holder.
LEMMA HolderWrite ==
  ASSUME Inv_OneHolder, Holder, holder \in Writers \cup {"none"},
         NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         holder' = holder,
         (R.pc \in CC) <=> (w[s].pc \in CC),
         w[s].st = "on" => R.st = "on"
  PROVE  Inv_OneHolder' /\ Holder'
<1>1. Inv_OneHolder' BY DEF Inv_OneHolder, CC
<1>2. Holder' BY DEF Holder, CC
<1>. QED BY <1>1, <1>2

LEMMA CasedWrite ==
  ASSUME Cased, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc = "cased" => \A p \in R.deletes : R.inst[p] = Nil
  PROVE  Cased'
BY DEF Cased

LEMMA MineWrite ==
  ASSUME Mine, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.deletes \cap R.uploads = {} /\ R.gone \subseteq R.uploads
  PROVE  Mine'
BY DEF Mine

LEMMA UpsWrite ==
  ASSUME Ups, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc \in {"scanned", "claimed", "cased"} => (R.uploads \cup R.deletes) \cap R.unlinked = {}
  PROVE  Ups'
BY DEF Ups

LEMMA OffWrite ==
  ASSUME Off, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.st = "off" => R = WriterInit
  PROVE  Off'
BY DEF Off

\* Rescope after a step that writes R.
LEMMA RescopeWrite ==
  ASSUME Rescope, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc \in {"consumed", "scanned", "claimed", "cased"} => R.sStage = "none",
         \A p \in Paths : (p \in R.unlinked /\ R.sStage = "none") => R.local[p] = R.baseline[p],
         \A p \in Paths : (p \in R.unlinked /\ R.sStage \in {"saved", "mid"}) =>
           \/ R.local[p] = R.baseline[p]
           \/ /\ p \in R.sDrop \ R.sKeep /\ R.baseline[p] = Nil
              /\ R.local[p] # Nil /\ R.sHeld[p] = R.local[p],
         R.sStage = "mid" => \A p \in R.sDrop \ R.sKeep : R.baseline[p] = Nil,
         R.pc = "pulling" => R.sStage \in {"none", "saved"}
  PROVE  Rescope'
<1>1. R1' BY DEF Rescope, R1
<1>2. R2' BY DEF Rescope, R2
<1>3. R3' BY DEF Rescope, R3
<1>4. R4' BY DEF Rescope, R4
<1>5. R5' BY DEF Rescope, R5
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Rescope

\* Rescope after a step that keeps the rescope fields, except that the
\* unlinked set may shrink and the tree may change off it.
LEMMA RescopeShrink ==
  ASSUME Rescope, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.sStage = w[s].sStage, R.unlinked \subseteq w[s].unlinked,
         \A q \in R.unlinked : R.local[q] = w[s].local[q],
         R.baseline = w[s].baseline, R.sDrop = w[s].sDrop, R.sKeep = w[s].sKeep, R.sHeld = w[s].sHeld,
         R.pc \in {"consumed", "scanned", "claimed", "cased"} => R.sStage = "none",
         R.pc = "pulling" => R.sStage \in {"none", "saved"}
  PROVE  Rescope'
<1>1. \A p \in Paths : (p \in R.unlinked /\ R.sStage = "none") => R.local[p] = R.baseline[p]
  BY DEF Rescope, R2
<1>2. \A p \in Paths : (p \in R.unlinked /\ R.sStage \in {"saved", "mid"}) =>
        \/ R.local[p] = R.baseline[p]
        \/ /\ p \in R.sDrop \ R.sKeep /\ R.baseline[p] = Nil
           /\ R.local[p] # Nil /\ R.sHeld[p] = R.local[p]
  BY DEF Rescope, R3
<1>3. R.sStage = "mid" => \A p \in R.sDrop \ R.sKeep : R.baseline[p] = Nil BY DEF Rescope, R4
<1>. QED BY <1>1, <1>2, <1>3, RescopeWrite

\* Prop_NarrowNeverDeletes's step formula after a step that writes R.
LEMMA NarrowWrite ==
  ASSUME NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         (w[s].pc = "consumed" /\ R.pc = "scanned") => R.deletes \cap w[s].unlinked = {}
  PROVE  NarrowOK
BY DEF NarrowOK

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M1 == Init => IndM1
<1>. SUFFICES ASSUME Init PROVE IndM1 OBVIOUS
<1>1. IndTypeOK BY Init_TypeOK
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>2a. holder = "none" BY DEF Init
<1>3. \A s \in Writers : /\ w[s].st = "off" /\ w[s].pc = "idle"
                         /\ w[s].deletes = {} /\ w[s].uploads = {} /\ w[s].gone = {}
                         /\ w[s].unlinked = {} /\ w[s].sStage = "none"
  BY <1>2 DEF WriterInit
<1>4. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off
  BY <1>2, <1>2a, <1>3, NoneWriter DEF Inv_OneHolder, Holder, Cased, Mine, Ups, Off
<1>5. Rescope BY <1>3 DEF Rescope, R1, R2, R3, R4, R5
<1>. QED BY <1>1, <1>4, <1>5 DEF IndM1, M1

------------------------------------------------------------------------------
(* Steps that write no tree.                                                *)

'''

def keep(name, sig, binders, frame='Frame'):
    return f'''LEMMA {name}_M1 ==
  ASSUME IndM1{binders}, {sig}, {frame}
  PROVE  IndM1' /\\ NarrowOK
<1>1. IndTypeOK' BY {name}_TypeOK DEF IndM1
<1>2. w' = w /\\ holder' = holder BY DEF {sig.split('(')[0]}
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

'''
M1 += keep('GPut', 'GPut(p)', ', NEW p \\in Paths')
M1 += keep('GCas', 'GCas(p)', ', NEW p \\in Paths')
M1 += keep('GRename', 'GRename(p, q)', ', NEW p \\in Paths, NEW q \\in Paths')
M1 += '''\\* Never enabled (mv = Nil).
LEMMA GRenameFinish_M1 ==
  ASSUME IndM1, GRenameFinish, Frame
  PROVE  IndM1' /\\ NarrowOK
BY DEF IndM1, IndTypeOK, GRenameFinish

'''
M1 += keep('GDelete', 'GDelete(p)', ', NEW p \\in Paths')
M1 += keep('Sweep', 'Sweep(s, h)', ', NEW s \\in Writers, NEW h \\in Handles')
M1 += keep('Age', 'Age', '', 'UNCHANGED anc')
M1 += keep('Reap', 'Reap(s, h)', ', NEW s \\in Writers, NEW h \\in Handles', 'UNCHANGED anc')
M1 += keep('RLoad', 'RLoad', '', 'UNCHANGED anc')

FIELDS = ['deletes', 'inst', 'uploads', 'gone', 'unlinked', 'sStage', 'local', 'baseline', 'sDrop', 'sKeep', 'sHeld']
RESET = {'deletes': '{}', 'uploads': '{}', 'gone': '{}', 'inst': '[p \\in Paths |-> Nil]',
         'sKeep': '{}', 'sHeld': '[p \\in Paths |-> Nil]', 'sDrop': '{}'}

def reads(rec, st, pc, changed):
    """R.st, R.pc and every field of FIELDS: the given expression or w[s].<f>."""
    out = [f'{rec}.st = {st}', f'{rec}.pc = {pc}']
    for f in FIELDS:
        out.append(f'{rec}.{f} = {changed.get(f, "w[s]." + f)}')
    return out

def block(L, name, sig, rec, recdef, restate, holder, holder_defs, guards, guard_defs, st, pc, changed,
          holder_proof=None, cased_by=None, mine_by=None, ups_by=None, off_by=None, rescope_proof=None,
          narrow_by=None, pre='', reads_by=None, dedent=''):
    """The body of a step lemma at level L (steps <L>1 ..), after the common facts
    <1>a <1>b <1>m <1>r <1>0 <1>f <1>t.  `restate` is the <L>1 step text."""
    I = '  ' * (L - 1)
    def S(n): return f'<{L}>{n}'
    t = ''
    t += pre
    t += f'{I}{S(1)}. {restate}\n'
    t += f'{I}{S("h")}. holder\' = {holder} BY DEF {holder_defs}\n'
    t += f'{I}{S("g")}. {guards} BY DEF {guard_defs}\n'
    t += f'{I}{S(2)}. \\A t \\in Writers : w\'[t] = IF t = s THEN {rec} ELSE w[t] BY <1>b, {S(1)}, WriteAny\n'
    rd = reads(rec, st, pc, changed)
    t += f'{I}{S(3)}. /\\ ' + f'\n{I}      /\\ '.join(rd) + f'\n{I}  BY {reads_by or "DEF " + recdef}\n'
    if holder_proof:
        t += holder_proof
    else:
        t += f'{I}{S("4a")}. ({rec}.pc \\in CC <=> w[s].pc \\in CC) /\\ (w[s].st = "on" => {rec}.st = "on") BY {S(3)}, {S("g")} DEF CC\n'
        t += f'{I}{S(4)}. Inv_OneHolder\' /\\ Holder\' BY <1>m, <1>0, {S(2)}, {S("4a")}, {S("h")}, HolderWrite\n'
    keeps = (pc == 'w[s].pc')
    t += f'{I}{S("5a")}. {rec}.pc = "cased" => \\A q \\in {rec}.deletes : {rec}.inst[q] = Nil BY {cased_by or ("<1>m, " + S(3) + " DEF Cased" if keeps else S(3))}\n'
    t += f'{I}{S(5)}. Cased\' BY <1>m, {S(2)}, {S("5a")}, CasedWrite\n'
    t += f'{I}{S("6a")}. {rec}.deletes \\cap {rec}.uploads = {{}} /\\ {rec}.gone \\subseteq {rec}.uploads BY {mine_by or ("<1>m, " + S(3) + " DEF Mine")}\n'
    t += f'{I}{S(6)}. Mine\' BY <1>m, {S(2)}, {S("6a")}, MineWrite\n'
    t += f'{I}{S("7a")}. {rec}.pc \\in {{"scanned", "claimed", "cased"}} => ({rec}.uploads \\cup {rec}.deletes) \\cap {rec}.unlinked = {{}} BY {ups_by or ("<1>m, " + S(3) + " DEF Ups" if keeps else S(3))}\n'
    t += f'{I}{S(7)}. Ups\' BY <1>m, {S(2)}, {S("7a")}, UpsWrite\n'
    t += f'{I}{S("8a")}. {rec}.st = "off" => {rec} = WriterInit BY {off_by or (S(3) + ", " + S("g") + " DEF On")}\n'
    t += f'{I}{S(8)}. Off\' BY <1>m, {S(2)}, {S("8a")}, OffWrite\n'
    if rescope_proof:
        t += rescope_proof
    else:
        t += f'{I}{S("9a")}. /\\ {rec}.sStage = w[s].sStage /\\ {rec}.unlinked \\subseteq w[s].unlinked\n'
        t += f'{I}       /\\ \\A q \\in {rec}.unlinked : {rec}.local[q] = w[s].local[q]\n'
        t += f'{I}       /\\ {rec}.baseline = w[s].baseline /\\ {rec}.sDrop = w[s].sDrop /\\ {rec}.sKeep = w[s].sKeep /\\ {rec}.sHeld = w[s].sHeld\n'
        t += f'{I}  BY {S(3)}, <1>f\n'
        t += f'{I}{S("9b")}. /\\ ({rec}.pc \\in {{"consumed", "scanned", "claimed", "cased"}} => {rec}.sStage = "none")\n'
        t += f'{I}       /\\ ({rec}.pc = "pulling" => {rec}.sStage \\in {{"none", "saved"}})\n'
        t += f'{I}  BY {S(3)}, {S("g")}, <1>r DEF R1, R5\n'
        t += f'{I}{S(9)}. Rescope\' BY <1>m, {S(2)}, {S("9a")}, {S("9b")}, RescopeShrink\n'
    t += f'{I}{S("10a")}. (w[s].pc = "consumed" /\\ {rec}.pc = "scanned") => {rec}.deletes \\cap w[s].unlinked = {{}} BY {narrow_by or S(3)}\n'
    t += f'{I}{S(10)}. NarrowOK BY {S(2)}, {S("10a")}, NarrowWrite\n'
    t += f'{I}{S("")}. QED BY <1>t, {S(4)}, {S(5)}, {S(6)}, {S(7)}, {S(8)}, {S(9)}, {S(10)} DEF IndM1, M1\n'
    return t

def head(name, sig, binders):
    return f'''LEMMA {name}_M1 ==
  ASSUME IndM1, NEW s \\in Writers{binders}, {sig}, Frame
  PROVE  IndM1' /\\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\\ Holder /\\ Cased /\\ Mine /\\ Ups /\\ Off /\\ Rescope BY DEF IndM1, M1
<1>r. R1 /\\ R2 /\\ R3 /\\ R4 /\\ R5 BY <1>m DEF Rescope
<1>0. w[s] \\in Writer /\\ holder \\in Writers \\cup {{"none"}} BY <1>b DEF TypeOK
<1>f. /\\ w[s].local \\in [Paths -> Opt(Handles)] /\\ w[s].baseline \\in [Paths -> Opt(Handles)]
      /\\ w[s].unlinked \\subseteq Paths /\\ w[s].uploads \\subseteq Paths /\\ w[s].deletes \\subseteq Paths
      /\\ w[s].sDrop \\subseteq Paths /\\ w[s].sKeep \\subseteq Paths
      /\\ w[s].sStage \\in {{"none", "saved", "mid"}}
      /\\ w[s].pc \\in {{"idle", "consumed", "scanned", "claimed", "cased", "pulling"}}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, {name}_TypeOK
'''

def simple(name, sig, binders, rec, recdef, restate, holder, holder_defs, guards, guard_defs, st, pc, changed, **kw):
    return head(name, sig, binders) + block(1, name, sig, rec, recdef, restate, holder, holder_defs, guards, guard_defs, st, pc, changed, **kw) + '\n'

M1 += '------------------------------------------------------------------------------\n(* The agent.                                                               *)\n\n'
M1 += simple('Edit', 'Edit(s, p)', ', NEW p \\in Paths', 'EditW(s, p)', 'EditW',
             "w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit", 'holder', 'Edit', 'On(s)', 'Edit',
             'w[s].st', 'w[s].pc',
             {'local': '[w[s].local EXCEPT ![p] = <<p, nextGen>>]', 'unlinked': 'w[s].unlinked \\ {p}'})
M1 += simple('Delete', 'Delete(s, p)', ', NEW p \\in Paths', 'DeleteW(s, p)', 'DeleteW',
             "w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete", 'holder', 'Delete, bucket', 'On(s)', 'Delete',
             'w[s].st', 'w[s].pc',
             {'local': '[w[s].local EXCEPT ![p] = Nil]', 'unlinked': 'w[s].unlinked \\ {p}'})
M1 += simple('Checkout', 'Checkout(s)', '', 'CheckoutW(s, T)', 'CheckoutW',
             "PICK T \\in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout",
             'holder', 'Checkout, bucket', 'w[s].st = "off"', 'Checkout',
             '"on"', 'w[s].pc',
             {'inst': 'doc', 'local': 'CheckoutHeld(s, T)', 'baseline': 'CheckoutHeld(s, T)'},
             pre='<1>i. w[s] = WriterInit BY <1>m DEF Off, Checkout\n'
                 '<1>j. /\\ w[s].pc = "idle" /\\ w[s].unlinked = {} /\\ w[s].sStage = "none"\n'
                 '      /\\ w[s].deletes = {} /\\ w[s].uploads = {} /\\ w[s].gone = {} /\\ w[s].sDrop = {}\n'
                 '  BY <1>i DEF WriterInit\n',
             holder_proof='<1>4a. (CheckoutW(s, T).pc \\in CC <=> w[s].pc \\in CC) /\\ (w[s].st = "on" => CheckoutW(s, T).st = "on") BY <1>3\n'
                          '<1>4. Inv_OneHolder\' /\\ Holder\' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite\n',
             cased_by='<1>3, <1>j', ups_by='<1>3, <1>j', off_by='<1>3',
             rescope_proof='<1>9. Rescope\' BY <1>m, <1>2, <1>3, <1>j, RescopeWrite\n')

# Consume: two cases, two trees.
M1 += head('Consume', 'Consume(s)', '')
M1 += '''<1>g. On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "none" BY DEF Consume
<1>h. holder' = holder BY DEF Consume
<1>1. CASE CheapPath(s)
'''
M1 += block(2, 'Consume', 'Consume(s)', 'ConsumeCheapW(s)', 'ConsumeCheapW',
            "w' = [w EXCEPT ![s] = ConsumeCheapW(s)] BY <1>1 DEF Consume",
            'holder', 'Consume', 'On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "none"', 'Consume',
            'w[s].st', '"consumed"', {})
M1 += '''<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \\in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] BY <1>2 DEF Consume
'''
M1 += block(2, 'Consume', 'Consume(s)', 'ConsumeW(s, fail)', 'ConsumeW',
            "w' = [w EXCEPT ![s] = ConsumeW(s, fail)] BY <2>0",
            'holder', 'Consume', 'On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "none"', 'Consume',
            'w[s].st', '"consumed"',
            {'local': '[q \\in Paths |-> IF q \\in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]',
             'baseline': '[q \\in Paths |-> IF q \\in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]]'},
            rescope_proof='''  <2>9a. \\A q \\in Paths : (q \\in ConsumeW(s, fail).unlinked /\\ ConsumeW(s, fail).sStage = "none")
                           => ConsumeW(s, fail).local[q] = ConsumeW(s, fail).baseline[q]
    BY <2>3, <1>r, <1>f DEF R2
  <2>9. Rescope' BY <1>m, <2>2, <2>3, <1>g, <2>9a, RescopeWrite
''')
M1 += '<1>. QED BY <1>1, <1>2\n\n'

M1 += simple('Scan', 'Scan(s)', '', 'ScanW(s, dels)', 'ScanW',
             "PICK dels \\in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan",
             'holder', 'Scan, bucket', 'On(s) /\\ w[s].pc = "consumed"', 'Scan',
             'w[s].st', '"scanned"',
             {'uploads': 'ScanUps(s)', 'deletes': 'dels', 'gone': '{}'},
             pre='<1>k. w[s].sStage = "none" BY <1>r DEF R1, Scan\n'
                 '<1>l. \\A q \\in w[s].unlinked : w[s].local[q] = w[s].baseline[q] BY <1>r, <1>k, <1>f DEF R2\n',
             mine_by='<1>1, <1>3 DEF ScanUps, ScanAbsent, ScanDirty',
             ups_by='<1>1, <1>3, <1>l DEF ScanUps, ScanAbsent, ScanDirty',
             rescope_proof='<1>9a. /\\ ScanW(s, dels).sStage = w[s].sStage /\\ ScanW(s, dels).unlinked \\subseteq w[s].unlinked\n'
                           '       /\\ \\A q \\in ScanW(s, dels).unlinked : ScanW(s, dels).local[q] = w[s].local[q]\n'
                           '       /\\ ScanW(s, dels).baseline = w[s].baseline /\\ ScanW(s, dels).sDrop = w[s].sDrop\n'
                           '       /\\ ScanW(s, dels).sKeep = w[s].sKeep /\\ ScanW(s, dels).sHeld = w[s].sHeld\n'
                           '  BY <1>3\n'
                           '<1>9b. /\\ (ScanW(s, dels).pc \\in {"consumed", "scanned", "claimed", "cased"} => ScanW(s, dels).sStage = "none")\n'
                           '       /\\ (ScanW(s, dels).pc = "pulling" => ScanW(s, dels).sStage \\in {"none", "saved"})\n'
                           '  BY <1>3, <1>k\n'
                           '<1>9. Rescope\' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink\n',
             narrow_by='<1>1, <1>3, <1>l DEF ScanAbsent, ScanDirty')
M1 += simple('Skip', 'Skip(s)', '', 'SkipW(s)', 'SkipW',
             "w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip", 'holder', 'Skip, bucket',
             'On(s) /\\ w[s].pc = "consumed"', 'Skip', 'w[s].st', '"idle"', {})

# Upload: two cases, two trees.
M1 += head('Upload', 'Upload(s, p)', ', NEW p \\in Paths')
M1 += '''<1>g. On(s) /\\ w[s].pc = "scanned" /\\ p \\in w[s].uploads BY DEF Upload
<1>h. holder' = holder BY DEF Upload
<1>k. p \\notin w[s].unlinked BY <1>m, <1>g DEF Ups
<1>1. CASE w[s].snap[p] \\notin upped
'''
M1 += block(2, 'Upload', 'Upload(s, p)', 'UploadW(s, p)', 'UploadW',
            "w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>1 DEF Upload",
            'holder', 'Upload', 'On(s) /\\ w[s].pc = "scanned" /\\ p \\in w[s].uploads', 'Upload',
            'w[s].st', 'w[s].pc', {})
M1 += '''<1>2. CASE w[s].snap[p] \\in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
'''
M1 += block(2, 'Upload', 'Upload(s, p)', 'UploadCopyW(s, p, c)', 'UploadCopyW',
            "w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] BY <1>2 DEF Upload",
            'holder', 'Upload', 'On(s) /\\ w[s].pc = "scanned" /\\ p \\in w[s].uploads', 'Upload',
            'w[s].st', 'w[s].pc',
            {'local': '[w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]'},
            rescope_proof='''  <2>9a. /\\ UploadCopyW(s, p, c).sStage = w[s].sStage /\\ UploadCopyW(s, p, c).unlinked \\subseteq w[s].unlinked
         /\\ \\A q \\in UploadCopyW(s, p, c).unlinked : UploadCopyW(s, p, c).local[q] = w[s].local[q]
         /\\ UploadCopyW(s, p, c).baseline = w[s].baseline /\\ UploadCopyW(s, p, c).sDrop = w[s].sDrop
         /\\ UploadCopyW(s, p, c).sKeep = w[s].sKeep /\\ UploadCopyW(s, p, c).sHeld = w[s].sHeld
    BY <2>3, <1>f, <1>k
  <2>9b. /\\ (UploadCopyW(s, p, c).pc \\in {"consumed", "scanned", "claimed", "cased"} => UploadCopyW(s, p, c).sStage = "none")
         /\\ (UploadCopyW(s, p, c).pc = "pulling" => UploadCopyW(s, p, c).sStage \\in {"none", "saved"})
    BY <2>3, <1>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
''')
M1 += '<1>. QED BY <1>1, <1>2\n\n'

M1 += '------------------------------------------------------------------------------\n(* The commit section.                                                      *)\n\n'
M1 += simple('PullOnly', 'PullOnly(s)', '', 'PullOnlyW(s)', 'PullOnlyW',
             "w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly", 'holder', 'PullOnly, bucket',
             'On(s) /\\ w[s].pc = "scanned"', 'PullOnly', 'w[s].st', '"idle"', {'inst': 'doc', 'gone': '{}'})
M1 += simple('Claim', 'Claim(s)', '', 'ClaimW(s)', 'ClaimW',
             "w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim", 's', 'Claim',
             'On(s) /\\ w[s].pc = "scanned" /\\ holder = "none"', 'Claim', 'w[s].st', '"claimed"', {},
             holder_proof='<1>4. Inv_OneHolder\' /\\ Holder\'\n'
                          '  <2>1. Inv_OneHolder\' BY <1>m, <1>0, <1>2, <1>3, <1>h, <1>g, NoneWriter DEF Inv_OneHolder\n'
                          '  <2>2. Holder\' BY <1>2, <1>3, <1>h, <1>g, NoneWriter DEF Holder, CC, On\n'
                          '  <2>. QED BY <2>1, <2>2\n',
             ups_by='<1>m, <1>3, <1>g DEF Ups')
M1 += simple('Verify', 'Verify(s)', '', 'VerifyW(s)', 'VerifyW',
             "w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify", 'holder', 'Verify, bucket',
             'On(s) /\\ w[s].pc = "claimed"', 'Verify', 'w[s].st', 'w[s].pc',
             {'gone': 'IF CommitVerifiesUploads THEN {q \\in w[s].uploads \\cap w[s].upDone : w[s].snap[q] \\notin live} ELSE {}'},
             cased_by='<1>m, <1>3 DEF Cased')
M1 += simple('Install', 'Install(s)', '', 'InstallW(s)', 'InstallW',
             "w' = [w EXCEPT ![s] = InstallW(s)] BY DEF Install", 'holder', 'Install',
             'On(s) /\\ w[s].pc = "claimed" /\\ holder = s', 'Install', 'w[s].st', '"cased"',
             {'inst': 'InstallInst(s)'},
             pre='<1>i. \\A q \\in w[s].deletes : InstallInst(s)[q] = Nil\n'
                 '  <2>1. DeleteWinsPreserved BY ShippedShape DEF Shipped\n'
                 '  <2>2. \\A q \\in w[s].deletes : q \\notin w[s].gone /\\ q \\notin InstallMine(s) BY <1>m DEF Mine, InstallMine\n'
                 '  <2>. QED BY <2>1, <2>2, <1>f DEF InstallInst\n',
             cased_by='<1>3, <1>i', ups_by='<1>m, <1>3, <1>g DEF Ups')
M1 += simple('Collect', 'Collect(s)', '', 'CollectW(s)', 'CollectW',
             "w' = [w EXCEPT ![s] = CollectW(s)] BY DEF Collect", 'holder', 'Collect',
             'On(s) /\\ w[s].pc = "cased"', 'Collect', 'w[s].st', 'w[s].pc', {},
             cased_by='<1>m, <1>3 DEF Cased')
M1 += simple('Finish', 'Finish(s)', '', 'FinishW(s)', 'FinishW',
             "w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish", '"none"', 'Finish',
             'On(s) /\\ w[s].pc = "cased"', 'Finish', 'w[s].st', '"idle"',
             {'deletes': '{}', 'uploads': '{}', 'gone': '{}',
              'baseline': '[q \\in Paths |-> IF q \\in w[s].uploads \\cap w[s].upDone THEN w[s].snap[q]\n'
                          '                                        ELSE IF q \\in w[s].deletes /\\ w[s].inst[q] = Nil THEN Nil\n'
                          '                                        ELSE w[s].baseline[q]]'},
             holder_proof='<1>4. Inv_OneHolder\' /\\ Holder\'\n'
                          '  <2>1. Inv_OneHolder\' BY <1>m, <1>2, <1>3, <1>g DEF Inv_OneHolder\n'
                          '  <2>2. Holder\' BY <1>h, NoneWriter DEF Holder\n'
                          '  <2>. QED BY <2>1, <2>2\n',
             rescope_proof='<1>9. Rescope\'\n'
                           '  <2>1. w[s].sStage = "none" BY <1>r, <1>g DEF R1\n'
                           '  <2>2. \\A q \\in w[s].unlinked : q \\notin w[s].uploads /\\ q \\notin w[s].deletes BY <1>m, <1>g DEF Ups\n'
                           '  <2>3. \\A q \\in Paths : (q \\in FinishW(s).unlinked /\\ FinishW(s).sStage = "none") => FinishW(s).local[q] = FinishW(s).baseline[q]\n'
                           '    BY <1>3, <1>r, <1>f, <2>1, <2>2 DEF R2\n'
                           '  <2>. QED BY <1>m, <1>2, <1>3, <2>1, <2>3, RescopeWrite\n')

M1 += '------------------------------------------------------------------------------\n(* The restart and the sync.                                                *)\n\n'
M1 += simple('Restart', 'Restart(s)', '', 'RestartW(s)', 'RestartW',
             "w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart", 'IF holder = s THEN "none" ELSE holder', 'Restart',
             'On(s)', 'Restart', 'w[s].st', '"idle"',
             {'deletes': '{}', 'uploads': '{}', 'gone': '{}', 'inst': '[q \\in Paths |-> Nil]',
              'sStage': 'IF w[s].sStage = "none" THEN "none" ELSE "saved"', 'sKeep': '{}'},
             holder_proof='<1>4. Inv_OneHolder\' /\\ Holder\'\n'
                          '  <2>1. Inv_OneHolder\' BY <1>m, <1>0, <1>2, <1>3, <1>h DEF Inv_OneHolder\n'
                          '  <2>2. Holder\' BY <1>m, <1>0, <1>2, <1>3, <1>h DEF Holder\n'
                          '  <2>. QED BY <2>1, <2>2\n',
             rescope_proof='<1>9. Rescope\'\n'
                           '  <2>1. \\A q \\in Paths : (q \\in RestartW(s).unlinked /\\ RestartW(s).sStage = "none") => RestartW(s).local[q] = RestartW(s).baseline[q]\n'
                           '    BY <1>3, <1>r DEF R2\n'
                           '  <2>2. \\A q \\in Paths : (q \\in RestartW(s).unlinked /\\ RestartW(s).sStage \\in {"saved", "mid"}) =>\n'
                           '          \\/ RestartW(s).local[q] = RestartW(s).baseline[q]\n'
                           '          \\/ /\\ q \\in RestartW(s).sDrop \\ RestartW(s).sKeep /\\ RestartW(s).baseline[q] = Nil\n'
                           '             /\\ RestartW(s).local[q] # Nil /\\ RestartW(s).sHeld[q] = RestartW(s).local[q]\n'
                           '    BY <1>3, <1>r, <1>f DEF R3\n'
                           '  <2>3. RestartW(s).sStage = "mid" => \\A q \\in RestartW(s).sDrop \\ RestartW(s).sKeep : RestartW(s).baseline[q] = Nil BY <1>3\n'
                           '  <2>. QED BY <1>m, <1>2, <1>3, <2>1, <2>2, <2>3, RescopeWrite\n')

def synclike(name, sig, binders, rec, recdef, restate, holder_defs, guards, pc, dstep):
    return simple(name, sig, binders, rec, recdef, restate, 'holder', holder_defs, guards, name, 'w[s].st', pc,
                  {'local': f'[q \\in Paths |-> IF q \\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]',
                   'baseline': 'SyncBl(s, fail)'},
                  rescope_proof=f'''<1>9. Rescope'
  <2>0. \\A q \\in Paths : q \\in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] BY DEF SyncOwed, SyncAll, Owed
  <2>1. \\A q \\in Paths : /\\ (w[s].local[q] = w[s].baseline[q] => {rec}.local[q] = {rec}.baseline[q])
                        /\\ (q \\notin SyncOwed(s, fail) => {rec}.local[q] = w[s].local[q] /\\ {rec}.baseline[q] = w[s].baseline[q])
    BY <1>3, <2>0 DEF SyncBl
  <2>2. \\A q \\in Paths : (q \\in {rec}.unlinked /\\ {rec}.sStage = "none") => {rec}.local[q] = {rec}.baseline[q]
    BY <1>3, <1>r, <2>1 DEF R2
  <2>3. \\A q \\in Paths : (q \\in {rec}.unlinked /\\ {rec}.sStage \\in {{"saved", "mid"}}) =>
          \\/ {rec}.local[q] = {rec}.baseline[q]
          \\/ /\\ q \\in {rec}.sDrop \\ {rec}.sKeep /\\ {rec}.baseline[q] = Nil
             /\\ {rec}.local[q] # Nil /\\ {rec}.sHeld[q] = {rec}.local[q]
    BY <1>3, <1>r, <2>0, <2>1 DEF R3
  <2>4. {rec}.sStage = "mid" => \\A q \\in {rec}.sDrop \\ {rec}.sKeep : {rec}.baseline[q] = Nil {dstep}
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, <2>4, RescopeWrite
''')
M1 += synclike('Sync', 'Sync(s)', '', 'SyncW(s, fail)', 'SyncW',
               "PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync",
               'Sync, bucket', 'On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage \\in {"none", "saved"}', 'w[s].pc',
               'BY <1>3, <1>g')

M1 += '------------------------------------------------------------------------------\n(* The narrow / widen verb.                                                 *)\n\n'
M1 += simple('RescopeBegin', 'RescopeBegin(s)', '', 'RescopeBeginW(s, T)', 'RescopeBeginW',
             "PICK T \\in Scopes \\ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin",
             'holder', 'RescopeBegin, bucket', 'On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "none"', 'RescopeBegin',
             'w[s].st', 'w[s].pc',
             {'sStage': '"saved"', 'sDrop': 'Leaving(s, T)', 'sKeep': '{}', 'sHeld': '[q \\in Paths |-> Nil]'},
             rescope_proof='<1>9. Rescope\'\n'
                           '  <2>2. \\A q \\in Paths : (q \\in RescopeBeginW(s, T).unlinked /\\ RescopeBeginW(s, T).sStage \\in {"saved", "mid"}) =>\n'
                           '          \\/ RescopeBeginW(s, T).local[q] = RescopeBeginW(s, T).baseline[q]\n'
                           '          \\/ /\\ q \\in RescopeBeginW(s, T).sDrop \\ RescopeBeginW(s, T).sKeep /\\ RescopeBeginW(s, T).baseline[q] = Nil\n'
                           '             /\\ RescopeBeginW(s, T).local[q] # Nil /\\ RescopeBeginW(s, T).sHeld[q] = RescopeBeginW(s, T).local[q]\n'
                           '    BY <1>3, <1>g, <1>r DEF R2\n'
                           '  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, RescopeWrite\n')
M1 += simple('RescopeFirst', 'RescopeFirst(s)', '', 'RescopeFirstW(s)', 'RescopeFirstW',
             "w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst",
             'holder', 'RescopeFirst, bucket', 'On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "saved"', 'RescopeFirst',
             'w[s].st', 'w[s].pc',
             {'sStage': '"mid"',
              'baseline': '[q \\in Paths |-> IF q \\in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[q]]',
              'sKeep': 'KeepSet(s)',
              'sHeld': '[q \\in Paths |-> IF q \\in RescopeFirstDd(s) /\\ w[s].baseline[q] # Nil\n'
                       '                                     THEN w[s].baseline[q] ELSE w[s].sHeld[q]]'},
             pre='<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped\n',
             reads_by='<1>u DEF RescopeFirstW',
             rescope_proof='<1>9. Rescope\'\n'
                           '  <2>d. /\\ RescopeFirstDd(s) = w[s].sDrop \\ KeepSet(s)\n'
                           '        /\\ \\A q \\in w[s].sDrop : w[s].baseline[q] = Nil => q \\notin KeepSet(s)\n'
                           '    BY DEF RescopeFirstDd, KeepSet\n'
                           '  <2>e. \\A q \\in w[s].unlinked :\n'
                           '          \\/ w[s].local[q] = w[s].baseline[q]\n'
                           '          \\/ /\\ q \\in w[s].sDrop \\ w[s].sKeep /\\ w[s].baseline[q] = Nil\n'
                           '             /\\ w[s].local[q] # Nil /\\ w[s].sHeld[q] = w[s].local[q]\n'
                           '    BY <1>r, <1>g, <1>f DEF R3\n'
                           '  <2>2. \\A q \\in Paths : (q \\in RescopeFirstW(s).unlinked /\\ RescopeFirstW(s).sStage \\in {"saved", "mid"}) =>\n'
                           '          \\/ RescopeFirstW(s).local[q] = RescopeFirstW(s).baseline[q]\n'
                           '          \\/ /\\ q \\in RescopeFirstW(s).sDrop \\ RescopeFirstW(s).sKeep /\\ RescopeFirstW(s).baseline[q] = Nil\n'
                           '             /\\ RescopeFirstW(s).local[q] # Nil /\\ RescopeFirstW(s).sHeld[q] = RescopeFirstW(s).local[q]\n'
                           '    BY <1>3, <1>f, <2>d, <2>e\n'
                           '  <2>3. RescopeFirstW(s).sStage = "mid" => \\A q \\in RescopeFirstW(s).sDrop \\ RescopeFirstW(s).sKeep : RescopeFirstW(s).baseline[q] = Nil\n'
                           '    BY <1>3, <1>f, <2>d\n'
                           '  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, RescopeWrite\n')
M1 += simple('RescopeSecond', 'RescopeSecond(s)', '', 'RescopeSecondW(s, wfail)', 'RescopeSecondW',
             "PICK wfail \\in SUBSET {q \\in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \\/ ~WidenKeepsLocal} :\n"
             "        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]\n  BY DEF RescopeSecond",
             'holder', 'RescopeSecond, bucket', 'On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "mid"', 'RescopeSecond',
             'w[s].st', 'w[s].pc',
             {'sStage': '"none"', 'sDrop': '{}', 'sKeep': '{}', 'sHeld': '[q \\in Paths |-> Nil]',
              'unlinked': '(w[s].unlinked \\cup {q \\in Paths : RescopeUnlink(s, q) /\\ RescopeUnciteFirst}) \\ RescopeFetch(s, wfail)',
              'local': '[q \\in Paths |-> IF q \\in RescopeFetch(s, wfail) /\\ (RescopeLocal1(s)[q] = Nil \\/ ~WidenKeepsLocal)\n'
                       '                                        THEN doc[q] ELSE RescopeLocal1(s)[q]]',
              'baseline': '[q \\in Paths |-> IF q \\in RescopeFetch(s, wfail) THEN doc[q] ELSE RescopeBase1(s)[q]]'},
             pre='<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped\n',
             ups_by='<1>3, <1>g',
             rescope_proof='<1>9. Rescope\'\n'
                           '  <2>a. /\\ RescopeLocal1(s) = [q \\in Paths |-> IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]]\n'
                           '        /\\ RescopeBase1(s) = w[s].baseline\n'
                           '    BY <1>u DEF RescopeLocal1, RescopeBase1\n'
                           '  <2>b. \\A q \\in Paths : RescopeUnlink(s, q) => q \\in w[s].sDrop \\ w[s].sKeep /\\ w[s].local[q] # Nil\n'
                           '    BY DEF RescopeUnlink, RescopeDrop\n'
                           '  <2>c. \\A q \\in Paths : (q \\in w[s].sDrop \\ w[s].sKeep /\\ w[s].local[q] # Nil /\\ w[s].sHeld[q] = w[s].local[q]) => RescopeUnlink(s, q)\n'
                           '    BY DEF RescopeUnlink, RescopeDrop\n'
                           '  <2>d. \\A q \\in w[s].sDrop \\ w[s].sKeep : w[s].baseline[q] = Nil BY <1>r, <1>g DEF R4\n'
                           '  <2>e. \\A q \\in w[s].unlinked :\n'
                           '          \\/ w[s].local[q] = w[s].baseline[q]\n'
                           '          \\/ /\\ q \\in w[s].sDrop \\ w[s].sKeep /\\ w[s].baseline[q] = Nil\n'
                           '             /\\ w[s].local[q] # Nil /\\ w[s].sHeld[q] = w[s].local[q]\n'
                           '    BY <1>r, <1>g, <1>f DEF R3\n'
                           '  <2>1. \\A q \\in Paths : (q \\in RescopeSecondW(s, wfail).unlinked /\\ RescopeSecondW(s, wfail).sStage = "none")\n'
                           '          => RescopeSecondW(s, wfail).local[q] = RescopeSecondW(s, wfail).baseline[q]\n'
                           '    <3>. SUFFICES ASSUME NEW q \\in Paths, q \\in RescopeSecondW(s, wfail).unlinked\n'
                           '                  PROVE  RescopeSecondW(s, wfail).local[q] = RescopeSecondW(s, wfail).baseline[q]\n'
                           '      OBVIOUS\n'
                           '    <3>1. q \\notin RescopeFetch(s, wfail) /\\ (q \\in w[s].unlinked \\/ RescopeUnlink(s, q)) BY <1>3, <1>u\n'
                           '    <3>2. /\\ RescopeSecondW(s, wfail).local[q] = IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]\n'
                           '          /\\ RescopeSecondW(s, wfail).baseline[q] = w[s].baseline[q]\n'
                           '      BY <1>3, <2>a, <3>1\n'
                           '    <3>3. CASE RescopeUnlink(s, q) BY <3>2, <3>3, <2>b, <2>d\n'
                           '    <3>4. CASE ~RescopeUnlink(s, q)\n'
                           '      <4>1. q \\in w[s].unlinked BY <3>1, <3>4\n'
                           '      <4>2. w[s].local[q] = w[s].baseline[q] BY <4>1, <2>e, <2>c, <3>4\n'
                           '      <4>. QED BY <3>2, <3>4, <4>2\n'
                           '    <3>. QED BY <3>3, <3>4\n'
                           '  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>1, RescopeWrite\n')

M1 += '------------------------------------------------------------------------------\n(* A reader\'s tick.                                                         *)\n\n'
M1 += simple('RPullRead', 'RPullRead(s)', '', 'RPullReadW(s)', 'RPullReadW',
             "w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead", 'holder', 'RPullRead, bucket',
             'On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage \\in {"none", "saved"}', 'RPullRead',
             'w[s].st', '"pulling"', {})
M1 += synclike('RPullSync', 'RPullSync(s)', '', 'RPullSyncW(s, fail)', 'RPullSyncW',
               "PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync",
               'RPullSync, bucket', 'On(s) /\\ w[s].pc = "pulling"', '"idle"',
               'BY <1>3, <1>g, <1>r DEF R5')

M1 += r'''------------------------------------------------------------------------------
(* The step, and the theorems.                                              *)

LEMMA Next_M1 == IndM1 /\ Next => IndM1' /\ NarrowOK
<1>. SUFFICES ASSUME IndM1, Next PROVE IndM1' /\ NarrowOK OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM1' /\ NarrowOK BY <3>1, GPut_M1
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM1' /\ NarrowOK BY <3>2, GCas_M1
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM1' /\ NarrowOK BY <3>3, GDelete_M1
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM1' /\ NarrowOK BY <3>4, GRename_M1
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M1
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM1' /\ NarrowOK BY <3>1, Checkout_M1
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM1' /\ NarrowOK BY <3>2, Consume_M1
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM1' /\ NarrowOK BY <3>3, Scan_M1
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM1' /\ NarrowOK BY <3>4, Skip_M1
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM1' /\ NarrowOK BY <3>5, PullOnly_M1
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM1' /\ NarrowOK BY <3>6, Claim_M1
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM1' /\ NarrowOK BY <3>7, Verify_M1
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM1' /\ NarrowOK BY <3>8, Install_M1
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM1' /\ NarrowOK BY <3>9, Collect_M1
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM1' /\ NarrowOK BY <3>10, Finish_M1
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM1' /\ NarrowOK BY <3>11, Restart_M1
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM1' /\ NarrowOK BY <3>12, Sync_M1
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM1' /\ NarrowOK BY <3>13, RescopeBegin_M1
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM1' /\ NarrowOK BY <3>14, RescopeFirst_M1
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM1' /\ NarrowOK BY <3>15, RescopeSecond_M1
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM1' /\ NarrowOK BY <3>16, RPullRead_M1
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM1' /\ NarrowOK BY <3>17, RPullSync_M1
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM1' /\ NarrowOK BY <3>18, Edit_M1
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM1' /\ NarrowOK BY <3>19, Delete_M1
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM1' /\ NarrowOK BY <3>20, Upload_M1
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM1' /\ NarrowOK BY <3>21, Sweep_M1
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M1
  <2>2. CASE RLoad BY <2>2, RLoad_M1
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM1' /\ NarrowOK BY <2>3, Reap_M1
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M1Invariant == Spec => []IndM1
<1>1. Init => IndM1 BY Init_M1
<1>2. IndM1 /\ [Next]_vars => IndM1'
  <2>1. IndM1 /\ Next => IndM1' BY Next_M1
  <2>2. IndM1 /\ UNCHANGED vars => IndM1'
    <3>1. IndM1 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM1 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM1, vars
    <3>. QED BY <3>1, <3>2 DEF IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

THEOREM OneHolder == Spec => []Inv_OneHolder
<1>1. IndM1 => Inv_OneHolder BY DEF IndM1, M1
<1>. QED BY M1Invariant, <1>1, PTL

THEOREM DeleteSettles == Spec => Prop_DeleteSettles
<1>1. ASSUME IndM1, NEW s \in Writers, Finish(s)
      PROVE  \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]
  BY <1>1 DEF IndM1, M1, Cased, Finish
<1>2. IndM1 => [\A s \in Writers :
                  Finish(s) => \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]]_vars
  BY <1>1
<1>. QED BY M1Invariant, <1>2, PTL DEF Prop_DeleteSettles

THEOREM NarrowNeverDeletes == Spec => Prop_NarrowNeverDeletes
<1>1. IndM1 /\ [Next]_vars => [NarrowOK]_vars
  <2>1. IndM1 /\ Next => NarrowOK BY Next_M1
  <2>. QED BY <2>1
<1>. QED BY M1Invariant, <1>1, PTL DEF Spec, Prop_NarrowNeverDeletes, NarrowOK
'''

open(OUT, 'w').write(M0TEXT + M1 + END)
print('written', OUT, len((M0TEXT + M1 + END).splitlines()), 'lines')
